#!/usr/bin/env python3
"""Enforce the codex-local fork surface (plan Phase 6). Stdlib only.

Prints one JSON object {ok, upstream, files[], total_lines, violations[]} and
exits 0 when ok, 1 on violations, 2 on a tool error (bad config, missing tag).
The policy schema is documented at the top of local/fork-surface.toml.
Crate/feature dependency edges are not checked here: sc-lint owns them
(codex-rs/sc-lint.toml + codex-rs/boundaries/). This script only verifies each
codex-local-* crate has a boundary record.
"""
import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# Fork-owned paths: never counted, never need an allowlist entry.
BUILTIN_IGNORE = [
    "codex-rs/local/**",
    "local/**",
    "boundaries/**",
    "codex-rs/boundaries/**",
    "codex-rs/sc-lint.toml",
    ".claude/**",
    ".codex/skills/**",
    ".github/workflows/local-*.yml",
    ".github/workflows/upstream-sync.yml",
]


def glob_re(pattern):
    out, i = [], 0
    while i < len(pattern):
        c = pattern[i]
        if pattern.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
            continue
        if pattern.startswith("**", i):
            out.append(".*")
            i += 2
            continue
        out.append("[^/]*" if c == "*" else "[^/]" if c == "?" else re.escape(c))
        i += 1
    return re.compile("".join(out) + r"\Z")


def matches(path, patterns):
    return any(glob_re(p).match(path) for p in patterns)


def git(root, *args):
    r = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    if r.returncode:
        raise RuntimeError(f"git {' '.join(args)}: {r.stderr.strip()}")
    return r.stdout


def resolve_ref(root, tag):
    for ref in (tag, f"upstream/{tag}"):
        try:
            git(root, "rev-parse", "--verify", "--quiet", ref + "^{commit}")
            return ref
        except RuntimeError:
            pass
    raise RuntimeError(f"upstream ref not found: {tag} (also tried upstream/{tag})")


def boundary_record_violations(root):
    """Every codex-local-* crate under codex-rs/local/ needs a boundary record
    directory codex-rs/boundaries/<name>/ with at least one .toml file.

    The dependency/feature edges themselves are enforced by sc-lint
    (codex-rs/sc-lint.toml, run in local-ci); this only guards coverage.
    """
    local = Path(root) / "codex-rs" / "local"
    out = []
    for manifest in sorted(local.glob("*/Cargo.toml")):
        name = tomllib.loads(manifest.read_text()).get("package", {}).get("name", "")
        if not name.startswith("codex-local-"):
            continue
        rec = Path(root) / "codex-rs" / "boundaries" / name
        if not any(rec.glob("*.toml")):
            out.append({"kind": "missing_boundary_record", "crate": name})
    return out


def check(root):
    root = Path(root)
    tag = (root / "local/UPSTREAM_VERSION").read_text().strip()
    cfg = tomllib.loads((root / "local/fork-surface.toml").read_text())
    allow = cfg.get("allow", cfg.get("allowlist", []))
    generated = cfg.get("generated", [])
    ignore = BUILTIN_IGNORE + cfg.get("ignore", [])
    budget = cfg.get("budget", {})
    ref = resolve_ref(root, tag)

    files, violations, total = [], [], 0
    for line in git(root, "diff", "--numstat", "--no-renames", f"{ref}...HEAD").splitlines():
        added, removed, path = line.split("\t", 2)
        if matches(path, ignore) or matches(path, generated):
            continue
        # Binary files report "-"; they count as a file but add no lines.
        n = (int(added) + int(removed)) if added != "-" else 0
        files.append({"path": path, "lines": n})
        total += n
        if not matches(path, allow):
            violations.append({"kind": "not_allowlisted", "path": path})
    files.sort(key=lambda f: f["path"])

    max_lines, max_files = budget.get("max_lines"), budget.get("max_files")
    if max_lines is not None and total > max_lines:
        violations.append({"kind": "over_line_budget", "lines": total, "max": max_lines})
    if max_files is not None and len(files) > max_files:
        violations.append({"kind": "over_file_budget", "files": len(files), "max": max_files})
    violations += boundary_record_violations(root)
    return {
        "ok": not violations,
        "upstream": tag,
        "files": files,
        "total_lines": total,
        "violations": violations,
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", default=None, help="repo root (default: parent of local/)")
    args = ap.parse_args()
    root = args.root or Path(__file__).resolve().parents[2]
    try:
        result = check(root)
    except (RuntimeError, OSError, tomllib.TOMLDecodeError) as e:
        print(json.dumps({"ok": False, "error": str(e), "violations": []}))
        return 2
    print(json.dumps(result))
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
