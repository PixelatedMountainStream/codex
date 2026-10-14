#!/usr/bin/env python3
"""Install codex-local beside stock codex.

  install.py [--repo OWNER/REPO] [--tag local-vX.Y.Z] [--dry-run] [--no-cleanup]
             [--release-dir DIR] [--bin-dir DIR] [--stock-home DIR] [--local-home DIR]

* Picks the newest release whose tag starts with `local-v` (ignores `rust-v*`), downloads
  `codex-local-<triple>[.exe]`, verifies it against SHA256SUMS, installs to ~/.local/bin
  (Windows: %USERPROFILE%\\.local\\bin plus the user PATH).
* Never touches the stock `codex` binary; only warns if it is missing.
* Runs `codex-local sync-config` to create the home and links (no link logic here).
* Legacy cleanup (the ONLY code that writes to stock's home): matched by parsed TOML keys (never by text),
  backed up to <local-home>/legacy-backup/<timestamp>/, then removed. Interactive runs
  ask first; non-interactive runs and --dry-run only report.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
try:
    import tomllib
except ImportError:   # Python < 3.11: legacy detection is disabled (nothing is touched)
    tomllib = None
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _lib  # noqa: E402

_lib.make_output_safe()

WIN_HELPERS = ("codex-windows-sandbox-setup", "codex-command-runner")
# Line locators used ONLY to find candidate lines to delete in config.toml; whether a file is
# legacy is decided from the parsed TOML, and every edit is re-parsed and verified.
CHAT_LINE_RE = re.compile(r'^[ \t]*wire_api[ \t]*=[ \t]*(?:"chat"|\'chat\')[ \t]*(?:#.*)?$')
PATCH_LINE_RE = re.compile(r'^[ \t]*model_apply_patch_tool_type[ \t]*=[ \t]*(?:"[^"\n]*"|\'[^\'\n]*\'|[^\s#]+)[ \t]*(?:#.*)?$')
PATCH_KEY = "model_apply_patch_tool_type"
TUNNEL_PORT = "127.0.0.1:11434"   # legacy fleet_lib.tunnel_argv: ssh -N ... -L <port>:127.0.0.1:11434


# ---------------------------------------------------------------- release selection

def select_release(releases: list[dict], tag: str | None = None) -> dict | None:
    """Newest non-draft release with a `local-v` tag (or exactly `tag`)."""
    cands = [r for r in releases
             if str(r.get("tag_name", "")).startswith(_lib.TAG_PREFIX) and not r.get("draft")]
    if tag:
        cands = [r for r in cands if r["tag_name"] == tag]
    cands.sort(key=lambda r: r.get("published_at") or r.get("created_at") or "", reverse=True)
    return cands[0] if cands else None


def find_asset(release: dict, name: str) -> dict | None:
    return next((a for a in release.get("assets", []) if a.get("name") == name), None)


def _token() -> str | None:
    return os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")


def _http(url: str, accept: str = "application/vnd.github+json") -> bytes:
    h = {"Accept": accept, "User-Agent": "codex-local-install"}
    if _token():
        h["Authorization"] = f"Bearer {_token()}"
    with urllib.request.urlopen(urllib.request.Request(url, headers=h), timeout=60) as r:
        return r.read()


def fetch_releases(repo: str) -> list[dict]:
    return json.loads(_http(f"https://api.github.com/repos/{repo}/releases?per_page=100"))


def fetch_asset(asset: dict) -> bytes:
    if _token():   # API url + octet-stream also works for private repos
        return _http(asset["url"], "application/octet-stream")
    return _http(asset["browser_download_url"], "application/octet-stream")


def verify_asset(data: bytes, name: str, sums_text: str) -> str:
    """Return the hex digest if `data` matches SHA256SUMS for `name`; raise otherwise."""
    import hashlib
    sums = _lib.parse_sha256sums(sums_text)
    if name not in sums:
        raise ValueError(f"{name} is not listed in SHA256SUMS")
    got = hashlib.sha256(data).hexdigest()
    if got != sums[name]:
        raise ValueError(f"SHA256 mismatch for {name}: expected {sums[name]}, got {got}")
    return got


# ---------------------------------------------------------------- binary placement

def place_binary(data: bytes, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_name(dest.name + ".new")
    tmp.write_bytes(data)
    if not _lib.IS_WINDOWS:
        os.chmod(tmp, 0o755)
    os.replace(tmp, dest)
    if sys.platform == "darwin":
        run = subprocess.run
        run(["xattr", "-d", "com.apple.quarantine", str(dest)], capture_output=True)
        run(["codesign", "--remove-signature", str(dest)], capture_output=True)
        run(["codesign", "-s", "-", "-f", str(dest)], capture_output=True)


def ensure_on_user_path(directory: Path) -> None:
    """Windows: add `directory` to the persistent User PATH (.NET API, never setx)."""
    target = os.path.normcase(os.path.normpath(str(directory)))
    rc, out, _ = _lib.run(["powershell", "-NoProfile", "-Command",
                           "[Environment]::GetEnvironmentVariable('Path','User')"])
    if rc != 0:
        print(f"    WARN: could not read the user PATH; add {directory} to PATH yourself")
        return
    entries = [e for e in out.strip().split(";") if e]
    if any(os.path.normcase(os.path.normpath(e)) == target for e in entries):
        print(f"    {directory} is already on the user PATH")
    else:
        env = {**os.environ, "CLH_NEW_PATH": ";".join(entries + [str(directory)])}
        rc, _, _ = _lib.run(["powershell", "-NoProfile", "-Command",
                             "[Environment]::SetEnvironmentVariable('Path', $env:CLH_NEW_PATH, 'User')"],
                            env=env)
        print(f"    added {directory} to the user PATH; open a NEW shell" if rc == 0
              else f"    WARN: could not update the user PATH; add {directory} yourself")
    if target not in [os.path.normcase(os.path.normpath(p))
                      for p in os.environ.get("PATH", "").split(os.pathsep) if p]:
        os.environ["PATH"] = f"{directory}{os.pathsep}{os.environ.get('PATH', '')}"


# ---------------------------------------------------------------- TOML-aware matching

def _parse(text: str):
    if tomllib is None:
        return None
    try:
        return tomllib.loads(text)
    except (tomllib.TOMLDecodeError, ValueError):
        return None


def _is_legacy_key(k, v) -> bool:
    return k == PATCH_KEY or (k == "wire_api" and v == "chat")


def count_legacy_keys(data) -> int:
    """Number of real `wire_api = "chat"` / `model_apply_patch_tool_type` keys, at any depth."""
    if isinstance(data, dict):
        return sum(1 if _is_legacy_key(k, v) else count_legacy_keys(v) for k, v in data.items())
    if isinstance(data, list):
        return sum(count_legacy_keys(v) for v in data)
    return 0


def _without_legacy_keys(data):
    if isinstance(data, dict):
        return {k: _without_legacy_keys(v) for k, v in data.items() if not _is_legacy_key(k, v)}
    if isinstance(data, list):
        return [_without_legacy_keys(v) for v in data]
    return data


def is_legacy_toml(text: str) -> bool:
    data = _parse(text)
    return data is not None and count_legacy_keys(data) > 0


def _ml_state_after(line: str, delim: str | None) -> str | None:
    """Track whether a TOML multi-line string (triple quotes) is open at the end of `line`.
    Naive on purpose: any misjudgement is caught by the re-parse verification."""
    i = 0
    while i < len(line):
        if delim:
            j = line.find(delim, i)
            if j < 0:
                return delim
            i, delim = j + 3, None
        else:
            hits = [(line.find(d, i), d) for d in ('"""', "'''")]
            hits = [(j, d) for j, d in hits if j >= 0]
            hash_at = line.find("#", i)
            if not hits or (0 <= hash_at < min(j for j, _ in hits)):
                return None
            j, delim = min(hits)
            i = j + 3
    return delim


def strip_config_keys(text: str) -> str | None:
    """Remove only the legacy key lines. Returns None (caller must skip) unless re-parsing the
    result proves that exactly those keys disappeared and everything else is equal."""
    data = _parse(text)
    if data is None:
        return None
    keep, delim = [], None
    for ln in text.splitlines(keepends=True):
        bare = ln.rstrip("\r\n")
        if delim is None and (CHAT_LINE_RE.match(bare) or PATCH_LINE_RE.match(bare)):
            continue   # starts outside any multi-line string, so it is a real assignment line
        keep.append(ln)
        delim = _ml_state_after(bare, delim)
    new = "".join(keep)
    if _parse(new) != _without_legacy_keys(data):
        return None
    return new


# ---------------------------------------------------------------- legacy cleanup

def find_tunnel_procs(ps_lines: list[str] | None = None) -> list[tuple[int, str]]:
    """Legacy fleet tunnels: `ssh -N ... -L <port>:127.0.0.1:11434 host`. [(pid, cmdline)]."""
    if ps_lines is None:
        if _lib.IS_WINDOWS:
            _, out, _ = _lib.run(["powershell", "-NoProfile", "-Command",
                                  "Get-CimInstance Win32_Process -Filter \"Name='ssh.exe'\" | "
                                  "ForEach-Object { \"$($_.ProcessId) $($_.CommandLine)\" }"])
        else:
            _, out, _ = _lib.run(["ps", "-axo", "pid=,command="])
        ps_lines = out.splitlines()
    found = []
    for line in ps_lines:
        m = re.match(r"\s*(\d+)\s+(.*)$", line)
        if not m:
            continue
        cmd = m.group(2)
        if (re.match(r'^"?([^"]*[\\/])?ssh(\.exe)?"?\s', cmd + " ") and re.search(r"\s-N\b", cmd)
                and re.search(r"-L\s*\S*:" + re.escape(TUNNEL_PORT), cmd)):
            found.append((int(m.group(1)), cmd))
    return found


def scan_legacy(stock: Path, ps_lines: list[str] | None = None) -> list[dict]:
    """Findings: {kind: file|config-keys|process, path/pid, detail}. Parsed-TOML key matching."""
    out: list[dict] = []

    def read(p: Path) -> str:
        try:
            return p.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""

    agents = stock / "agents"
    for p in sorted(agents.glob("*.toml")) if agents.is_dir() else []:
        if is_legacy_toml(read(p)):
            out.append({"kind": "file", "path": p, "detail": "agent toml with fork-only keys"})
    p = stock / "ollama.config.toml"
    if p.is_file() and is_legacy_toml(read(p)):
        out.append({"kind": "file", "path": p, "detail": "legacy ollama profile"})
    p = stock / "config.toml"
    if p.is_file():
        t = read(p)
        data = _parse(t)
        n = count_legacy_keys(data) if data is not None else 0
        if n and strip_config_keys(t) is not None:
            out.append({"kind": "config-keys", "path": p,
                        "detail": f"{n} fork-only key(s); only those lines are removed"})
        elif n:
            out.append({"kind": "config-skip", "path": p,
                        "detail": f"{n} fork-only key(s) not safely removable by line; "
                                  "left untouched, edit by hand"})
    for pid, cmd in find_tunnel_procs(ps_lines):
        out.append({"kind": "process", "pid": pid, "detail": cmd[:120]})
    return out


def backup_and_remove(findings: list[dict], stock: Path, backup: Path, kill=None) -> None:
    backup.mkdir(parents=True, exist_ok=True)
    for f in findings:                       # back up everything first
        if f["kind"] in ("file", "config-keys"):
            dst = backup / f["path"].relative_to(stock)
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(f["path"], dst)
    procs = [f for f in findings if f["kind"] == "process"]
    if procs:
        (backup / "tunnel-processes.txt").write_text(
            "\n".join(f"{f['pid']} {f['detail']}" for f in procs) + "\n", encoding="utf-8")
    for f in findings:
        if f["kind"] == "file":
            f["path"].unlink()
        elif f["kind"] == "config-keys":
            p = f["path"]
            new = strip_config_keys(p.read_text(encoding="utf-8", errors="replace"))
            if new is None:   # verification failed: report and skip, never write
                print(f"    SKIP: {p} failed post-edit verification; left untouched")
                continue
            p.write_text(new, encoding="utf-8")
        elif f["kind"] == "process":
            (kill or _kill)(f["pid"])


def _kill(pid: int) -> None:
    if _lib.IS_WINDOWS:
        _lib.run(["taskkill", "/PID", str(pid), "/F"])
    else:
        try:
            os.kill(pid, 15)
        except OSError:
            pass


def describe(f: dict) -> str:
    where = f["path"] if "path" in f else f"pid {f['pid']}"
    return f"{f['kind']:<12} {where}  ({f['detail']})"


def legacy_cleanup(stock: Path, local: Path, dry_run: bool, interactive: bool,
                   ps_lines=None) -> list[dict]:
    findings = scan_legacy(stock, ps_lines)
    if not findings:
        print("    no legacy artifacts found")
        return findings
    for f in findings:
        print(f"    legacy: {describe(f)}")
    if not any(f["kind"] != "config-skip" for f in findings):
        return findings
    if dry_run:
        print("    dry-run: nothing changed")
        return findings
    if not interactive:
        print("    non-interactive: reported only; re-run in a terminal to clean up")
        return findings
    if input("    Back up and remove these? [y/N] ").strip().lower() != "y":
        print("    skipped")
        return findings
    backup = local / "legacy-backup" / time.strftime("%Y%m%d-%H%M%S")
    backup_and_remove(findings, stock, backup)
    print(f"    backed up to {backup} and removed")
    return findings


# ---------------------------------------------------------------- main

def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="Install codex-local beside stock codex.")
    ap.add_argument("--repo", default=os.environ.get(_lib.REPO_ENV) or _lib.DEFAULT_REPO)
    ap.add_argument("--tag", help="exact local-v* release tag (default: newest)")
    ap.add_argument("--release-dir", help="offline: dir holding the asset(s) and SHA256SUMS")
    ap.add_argument("--bin-dir", default=None)
    ap.add_argument("--stock-home", default=None)
    ap.add_argument("--local-home", default=None)
    ap.add_argument("--dry-run", action="store_true",
                    help="report what would happen; change nothing")
    ap.add_argument("--no-cleanup", action="store_true", help="skip legacy cleanup")
    ap.add_argument("--no-verify", action="store_true", help="skip the final verify --quick")
    a = ap.parse_args(argv)

    stock = Path(a.stock_home) if a.stock_home else _lib.stock_home()
    local = Path(a.local_home) if a.local_home else _lib.local_home()
    bin_dir = Path(a.bin_dir) if a.bin_dir else _lib.default_bin_dir()
    target = _lib.triple()
    name = _lib.asset_name(target)
    dest = bin_dir / _lib.exe_name()
    env = {**os.environ, "CODEX_LOCAL_HOME": str(local), "CODEX_HOME": str(stock)}

    print("==> Stock codex (never modified)")
    exe = _lib.stock_codex()
    print(f"    stock codex: {exe}" if exe else
          "    WARN: stock `codex` not found on PATH; codex-local works without it, "
          "but cross-resume needs it (npm i -g @openai/codex)")

    print(f"==> Release ({a.repo}, asset {name})")
    if a.release_dir:
        d = Path(a.release_dir)
        try:
            data = (d / name).read_bytes()
            sums = (d / "SHA256SUMS").read_text(encoding="utf-8")
        except OSError as e:
            print(f"    ERROR: cannot read release files from {d}: {e}; "
                  f"expected {name} and SHA256SUMS")
            return 1
        tag, helpers = "local", {}
        if a.dry_run:
            print(f"    dry-run: would install {name} from {d} to {dest}")
            data = None
        for h in WIN_HELPERS if (_lib.IS_WINDOWS and data is not None) else ():
            hp = d / f"{h}-{target}.exe"
            if hp.is_file():
                helpers[h] = (hp.name, hp.read_bytes())
            else:
                print(f"    ERROR: {hp.name} is required on Windows but missing from {d}")
                return 1
    else:
        try:
            rel = select_release(fetch_releases(a.repo), a.tag)
        except Exception as e:
            print(f"    ERROR: could not list releases for {a.repo}: {e}")
            return 1
        if not rel:
            print(f"    ERROR: no release with tag {a.tag or _lib.TAG_PREFIX + '*'} in {a.repo}")
            return 1
        tag = rel["tag_name"]
        asset, sums_asset = find_asset(rel, name), find_asset(rel, "SHA256SUMS")
        if not asset or not sums_asset:
            print(f"    ERROR: {tag} lacks {name if not asset else 'SHA256SUMS'}")
            return 1
        print(f"    {tag}")
        if a.dry_run:
            print(f"    dry-run: would install {name} to {dest}")
            data = sums = None
        else:
            data = fetch_asset(asset)
            sums = fetch_asset(sums_asset).decode("utf-8", "replace")
            helpers = {}
            for h in WIN_HELPERS if _lib.IS_WINDOWS else ():
                ha = find_asset(rel, f"{h}-{target}.exe")
                if not ha:
                    print(f"    ERROR: {tag} lacks {h}-{target}.exe (required on Windows)")
                    return 1
                helpers[h] = (ha["name"], fetch_asset(ha))

    if data is not None:
        try:
            digest = verify_asset(data, name, sums)
            for hname, hdata in helpers.values():
                verify_asset(hdata, hname, sums)
        except ValueError as e:
            print(f"    ERROR: {e}")
            return 1
        print(f"    SHA256 ok {digest}")
        print(f"==> Installing {dest}")
        try:
            place_binary(data, dest)
            for h, (_, hdata) in helpers.items():
                place_binary(hdata, bin_dir / f"{h}.exe")
        except OSError as e:
            print(f"    ERROR: could not write {dest}: {e} (is codex-local running?)")
            return 1
        if _lib.IS_WINDOWS:
            ensure_on_user_path(bin_dir)
        elif str(bin_dir) not in os.environ.get("PATH", "").split(os.pathsep):
            print(f"    NOTE: add {bin_dir} to PATH")
        local.mkdir(parents=True, exist_ok=True)
        (local / "install.json").write_text(json.dumps(
            {"tag": tag, "asset": name, "sha256": digest,
             "installed_sha256": _lib.sha256_file(dest), "path": str(dest)}, indent=2),
            encoding="utf-8")

        print("==> Home and links (codex-local sync-config)")
        rc, out, err = _lib.run([dest, "sync-config"], env=env, timeout=120)
        for line in (out + err).strip().splitlines():
            print(f"    {line}")
        if rc != 0:
            print(f"    ERROR: sync-config exited {rc}")
            return 1

    if not a.no_cleanup:
        print("==> Legacy cleanup (stock home: only fork-only content is touched)")
        interactive = sys.stdin.isatty() and sys.stdout.isatty()
        legacy_cleanup(stock, local, a.dry_run, interactive)

    if a.dry_run or data is None:
        return 0
    if not a.no_verify:
        print("==> Verifying (verify_install.py --quick)")
        rc, out, err = _lib.run([sys.executable, Path(__file__).with_name("verify_install.py"),
                                 "--quick", "--binary", dest], env=env, timeout=300)
        for line in (out + err).splitlines():
            print(f"    {line}")
        if rc != 0:
            print("\nINSTALL FAILED verification. Not reporting success.")
            return 1
    print(f"\nDone. Run `{dest.name} --version`; full check: verify_install.py")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
