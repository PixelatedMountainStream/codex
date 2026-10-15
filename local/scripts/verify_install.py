#!/usr/bin/env python3
"""Verify a codex-local install. One line per check; exits non-zero on any FAIL.

  verify_install.py            full: stock codex + config, /api/tags, version skew,
                               links into stock's home, a live one-shot exec
  verify_install.py --quick    offline only: binary runs, version format, SHA256,
                               home/links in a temp CODEX_LOCAL_HOME, one-shot exec
                               against a stdlib mock Ollama (mock_ollama.py)

Options: --binary PATH, --sums SHA256SUMS (quick SHA256 source; default is the
install record <local-home>/install.json). All output is plain ASCII (cp1252-safe).
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tempfile
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _lib  # noqa: E402
import mock_ollama  # noqa: E402

_lib.make_output_safe()

EXEC_ARGS = ["exec", "--skip-git-repo-check", "Reply with the single word OK."]


class Report:
    def __init__(self) -> None:
        self.failed = 0

    def _p(self, mark, label, detail):
        print(f"{mark:<5} {label:<18} {detail}")

    def ok(self, l, d): self._p("PASS", l, d)
    def warn(self, l, d): self._p("WARN", l, d)
    def info(self, l, d): self._p("--", l, d)

    def fail(self, l, d):
        self._p("FAIL", l, d)
        self.failed += 1


def find_binary(arg: str | None) -> str | None:
    if arg:   # resolve now: later steps run the binary with a different cwd
        return str(Path(arg).expanduser().resolve())
    return shutil.which("codex-local")


def check_binary(rep, exe) -> tuple | None:
    if not exe:
        rep.fail("binary", "codex-local not found (run install.py)")
        return None
    rc, ver, raw = _lib.probe_version(exe)
    if rc != 0 or not ver:
        rep.fail("binary", f"`{exe} --version` rc={rc}: {raw[:150] or '<no output>'}")
        return None
    rep.ok("binary", f"{exe} -> {raw.splitlines()[0]}")
    if not ver[1]:
        rep.fail("version format", f"{ver[0]} has no +local stamp; not a codex-local build")
        return None
    rep.ok("version format", f"{ver[0]}{ver[1]}")
    return ver


def check_sha(rep, exe, local: Path, sums: str | None) -> None:
    got = _lib.sha256_file(Path(exe))
    want = src = None
    if sums and sys.platform == "darwin":
        rep.warn("sha256", "--sums skipped on macOS: install re-signs the binary, so it "
                           "cannot match the release SHA256SUMS; verify via install.json")
        return
    if sums:
        want = _lib.parse_sha256sums(Path(sums).read_text(encoding="utf-8")).get(
            _lib.asset_name(_lib.triple()))
        src = sums
    elif (local / "install.json").is_file():
        rec = json.loads((local / "install.json").read_text(encoding="utf-8"))
        want, src = rec.get("installed_sha256") or rec.get("sha256"), "install record"
    if not want:
        rep.warn("sha256", f"no recorded checksum to compare ({got[:12]}...); pass --sums")
    elif got == want:
        rep.ok("sha256", f"matches {src}")
    else:
        rep.fail("sha256", f"binary {got[:12]}... != {src} {want[:12]}...")


def check_links(rep, stock: Path, local: Path) -> None:
    def same(a: Path, b: Path) -> bool:
        return a.exists() and b.exists() and os.path.realpath(a) == os.path.realpath(b)

    bad = []
    for d in ("sessions", "archived_sessions"):
        if (stock / d).exists() and not same(local / d, stock / d):
            bad.append(d)
    if bad:
        rep.fail("links", f"{', '.join(bad)} in {local} do not resolve to {stock}")
    else:
        rep.ok("links", "sessions/ and archived_sessions/ resolve to stock's home")
    if (stock / "AGENTS.md").exists():
        a = local / "AGENTS.md"
        if same(a, stock / "AGENTS.md"):
            rep.ok("AGENTS.md", "linked to stock")
        elif a.is_file():
            rep.warn("AGENTS.md", "copy fallback in use (not a link); edits in stock do not propagate live")
        else:
            rep.fail("AGENTS.md", "missing in codex-local home")
    skills = stock / "skills"
    if skills.is_dir():
        miss = [c.name for c in skills.iterdir()
                if c.name != ".system" and not same(local / "skills" / c.name, c)]
        if miss:
            rep.fail("skill links", "not linked: " + ", ".join(sorted(miss)))
        else:
            rep.ok("skill links", "user skills resolve to stock's")


def one_shot(rep, label, exe, env, cwd) -> None:
    rc, out, err = _lib.run([exe] + EXEC_ARGS, env=env, cwd=cwd, timeout=300)
    text = (out + err).strip()
    if rc != 0:
        rep.fail(label, f"exec rc={rc}: {text[-200:]}")
    elif not out.strip():
        rep.fail(label, "exec exited 0 but printed no reply")
    else:
        rep.ok(label, "reply: " + out.strip().splitlines()[-1][:60])


def quick(rep, exe, sums) -> None:
    ver = check_binary(rep, exe)
    if not ver:
        return
    with tempfile.TemporaryDirectory() as td:
        td = Path(td)
        stock, local, work = td / "stock", td / "local", td / "work"
        for d in (stock / "sessions", stock / "archived_sessions", stock / "skills" / "demo", work):
            d.mkdir(parents=True)
        (stock / "AGENTS.md").write_text("# demo\n", encoding="utf-8")
        check_sha(rep, exe, _lib.local_home(), sums)
        srv, url = mock_ollama.start()
        try:
            env = {**os.environ, "CODEX_HOME": str(stock), "CODEX_LOCAL_HOME": str(local),
                   "CODEX_LOCAL_OLLAMA_URL": url}
            env.pop("OLLAMA_HOST", None)
            rc, out, err = _lib.run([exe, "sync-config"], env=env)
            if rc != 0:
                rep.fail("home setup", f"sync-config rc={rc}: {(out + err).strip()[-200:]}")
                return
            rep.ok("home setup", f"sync-config created {local}")
            check_links(rep, stock, local)
            one_shot(rep, "mock exec", exe, env, work)
        finally:
            srv.shutdown()


def full(rep, exe, sums) -> None:
    stock, local = _lib.stock_home(), _lib.local_home()
    ver = check_binary(rep, exe)
    sexe = _lib.stock_codex()
    sver = None
    if not sexe:
        rep.warn("stock codex", "not found on PATH; cross-resume unavailable")
    else:
        rc, sver, raw = _lib.probe_version(sexe)
        (rep.ok if rc == 0 and sver else rep.fail)("stock codex", f"{sexe} -> {raw[:80]}")
    if _lib.web_search_runner_missing():
        rep.warn("web search", _lib.WEB_SEARCH_HINT)
    cfg = stock / "config.toml"
    if cfg.is_file():
        try:
            import tomllib
            tomllib.loads(cfg.read_text(encoding="utf-8"))
            rep.ok("stock config", f"{cfg} parses")
        except ImportError:
            rep.info("stock config", "tomllib unavailable (Python < 3.11); not parsed")
        except Exception as e:
            rep.fail("stock config", f"{cfg}: {e}")
    else:
        rep.info("stock config", f"{cfg} absent")
    root = _lib.ollama_root(local_cfg=local / "config.toml")
    try:
        with urllib.request.urlopen(root + "/api/tags", timeout=5) as r:
            n = len(json.loads(r.read().decode()).get("models", []))
        rep.ok("ollama", f"{root}/api/tags reachable, {n} model(s)")
    except Exception as e:
        rep.fail("ollama", f"{root}/api/tags unreachable: {e}")
    if ver and sver:
        if ver[0] == sver[0]:
            rep.ok("version skew", f"upstream {ver[0]} on both")
        else:
            rep.warn("version skew", f"codex-local upstream {ver[0]} vs stock {sver[0]}; "
                                     "cross-resume may misbehave")
    check_sha(rep, exe, local, sums) if ver else None
    if local.is_dir():
        check_links(rep, stock, local)
    else:
        rep.fail("links", f"{local} missing; run `codex-local sync-config`")
    if ver:
        env = {**os.environ, "CODEX_HOME": str(stock), "CODEX_LOCAL_HOME": str(local)}
        with tempfile.TemporaryDirectory() as td:
            one_shot(rep, "live exec", exe, env, td)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="Verify a codex-local install.")
    ap.add_argument("--quick", action="store_true", help="offline checks only")
    ap.add_argument("--binary", help="path to codex-local (default: PATH)")
    ap.add_argument("--sums", help="SHA256SUMS file to check the binary against")
    a = ap.parse_args(argv)
    rep = Report()
    exe = find_binary(a.binary)
    (quick if a.quick else full)(rep, exe, a.sums)
    if rep.failed:
        print(f"\n{rep.failed} check(s) FAILED.")
        return 1
    print(f"\nAll checks passed ({'quick' if a.quick else 'full'}).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
