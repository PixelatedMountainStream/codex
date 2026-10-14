"""Shared helpers for install.py and verify_install.py (stdlib only, cp1252-safe output)."""

from __future__ import annotations

import hashlib
import os
import platform
import re
import shutil
import subprocess
import sys
from pathlib import Path

DEFAULT_REPO = "PixelatedMountainStream/codex"
REPO_ENV = "CODEX_LOCAL_REPO"
TAG_PREFIX = "local-v"
IS_WINDOWS = os.name == "nt"
DEFAULT_OLLAMA_URL = "http://localhost:11434"


def make_output_safe() -> None:
    """Windows consoles default to cp1252; never let a stray glyph kill the run.

    Keeps the console encoding but replaces unencodable characters. All of our own
    messages are plain ASCII anyway; this covers text echoed from child processes.
    """
    for s in (sys.stdout, sys.stderr):
        try:
            s.reconfigure(errors="replace")
        except Exception:
            pass


def triple() -> str:
    sysname, mach = platform.system(), platform.machine().lower()
    arm = mach in ("arm64", "aarch64")
    if sysname == "Darwin":
        return "aarch64-apple-darwin" if arm else "x86_64-apple-darwin"
    if sysname == "Linux":
        return f"{'aarch64' if arm else 'x86_64'}-unknown-linux-musl"
    if sysname == "Windows":
        return "aarch64-pc-windows-msvc" if arm else "x86_64-pc-windows-msvc"
    return f"{mach}-{sysname.lower()}"


def asset_name(target: str) -> str:
    return f"codex-local-{target}" + (".exe" if "windows" in target else "")


def exe_name() -> str:
    return "codex-local.exe" if IS_WINDOWS else "codex-local"


def default_bin_dir() -> Path:
    return Path.home() / ".local" / "bin"


def stock_home(env=None) -> Path:
    env = os.environ if env is None else env
    return Path(env.get("CODEX_HOME") or Path.home() / ".codex")


def local_home(env=None) -> Path:
    """CODEX_LOCAL_HOME only; an inherited CODEX_HOME is never the local home."""
    env = os.environ if env is None else env
    return Path(env.get("CODEX_LOCAL_HOME") or Path.home() / ".codex" / "local")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def parse_sha256sums(text: str) -> dict[str, str]:
    """`<hex>  <name>` or `<hex> *<name>` lines -> {name: hex}."""
    out: dict[str, str] = {}
    for line in text.splitlines():
        m = re.match(r"^([0-9a-fA-F]{64})\s+\*?(.+?)\s*$", line)
        if m:
            out[m.group(2)] = m.group(1).lower()
    return out


_VER_RE = re.compile(r"(\d+\.\d+\.\d+(?:-[0-9A-Za-z.\-]+)?)(\+local[0-9A-Za-z.\-]*)?")


def parse_version(output: str) -> tuple[str, str] | None:
    """`codex-cli 0.162.1+local.abc` -> ('0.162.1', '+local.abc'); None if no version."""
    m = _VER_RE.search(output or "")
    return (m.group(1), m.group(2) or "") if m else None


def run(argv, timeout=60, env=None, cwd=None):
    """subprocess.run that never raises on missing/slow binaries; returns (rc, out, err)."""
    try:
        r = subprocess.run([str(a) for a in argv], capture_output=True, text=True,
                           encoding="utf-8", errors="replace", timeout=timeout,
                           stdin=subprocess.DEVNULL, env=env, cwd=cwd)
        return r.returncode, r.stdout or "", r.stderr or ""
    except subprocess.TimeoutExpired:
        return 124, "", f"timed out after {timeout}s"
    except OSError as e:
        return 127, "", str(e)


def probe_version(exe: str, timeout=30):
    rc, out, err = run([exe, "--version"], timeout=timeout)
    return rc, parse_version(out), (out + err).strip()


def stock_codex() -> str | None:
    """Stock `codex` on PATH, ignoring a binary carrying the `+local` stamp."""
    exe = shutil.which("codex")
    if not exe:
        return None
    _, ver, _ = probe_version(exe)
    if ver and ver[1]:
        return None
    return exe


def ollama_root(env=None, local_cfg: Path | None = None) -> str:
    """Resolve the Ollama root: env CODEX_LOCAL_OLLAMA_URL, OLLAMA_HOST, then `base_url` of
    [model_providers.codex-local] in config.toml (codex-local keeps no `ollama_url` key), default."""
    env = os.environ if env is None else env
    raw = env.get("CODEX_LOCAL_OLLAMA_URL") or env.get("OLLAMA_HOST")
    if not raw and local_cfg and local_cfg.is_file():
        m = re.search(r'^\s*base_url\s*=\s*"([^"]+)"',
                      local_cfg.read_text(encoding="utf-8", errors="replace"), re.M)
        raw = m.group(1) if m else None
    raw = (raw or DEFAULT_OLLAMA_URL).strip()
    if "://" not in raw:
        raw = "http://" + raw
    raw = raw.rstrip("/")
    return raw[:-3] if raw.endswith("/v1") else raw
