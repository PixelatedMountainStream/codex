#!/usr/bin/env python3
"""Upstream tracking for codex-local (plan Phase 7). Stdlib only.

Subcommands: status | merge <tag> | conflicts | check | report.
Each prints ONE JSON object:
  {ok, step, conflicts[], build_errors[], test_failures[], surface_violations[], next_action}
(plus a few step-specific extra keys). Exit codes: 0 ok, 1 needs repair, 2 escalate.
"""
import argparse
import fnmatch
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
STABLE = re.compile(r"^rust-v(\d+)\.(\d+)\.(\d+)$")
LOCKFILE = "codex-rs/Cargo.lock"
FORK_WORKFLOWS = ["local-*.yml", "upstream-sync.yml"]
ESCALATE_KINDS = {"over_line_budget", "over_file_budget", "not_allowlisted"}
LOCAL_CRATES = ["codex-local-cli", "codex-local-models", "codex-local-resume"]


class Escalate(Exception):
    pass


def emit(step, ok, code, next_action, **kw):
    out = {"ok": ok, "step": step, "conflicts": [], "build_errors": [],
           "test_failures": [], "surface_violations": [], "next_action": next_action}
    out.update(kw)
    print(json.dumps(out, indent=2))
    return code


def run(cmd, root, check=False):
    p = subprocess.run(cmd, cwd=root, capture_output=True, text=True)
    if check and p.returncode:
        raise Escalate(f"{' '.join(cmd)} failed: {p.stderr.strip()[:300]}")
    return p


def git(root, *args, check=True):
    return run(["git", *args], root, check)


def version_file(root):
    return Path(root) / "local" / "UPSTREAM_VERSION"


def current_version(root):
    return version_file(root).read_text().strip()


TAG_RE = re.compile(r"rust-v\d+\.\d+\.\d+", re.ASCII)   # matched with fullmatch: ^...$


def validate_tag(tag):
    if not isinstance(tag, str) or not TAG_RE.fullmatch(tag):
        raise Escalate(f"{tag!r} is not a stable tag (expected rust-vN.N.N)")
    return tag


def tag_key(tag):
    m = STABLE.match(tag)
    return tuple(int(x) for x in m.groups()) if m else None


def resolve_remote(root, wanted):
    remotes = git(root, "remote").stdout.split()
    if wanted:
        if wanted not in remotes:
            raise Escalate(f"remote {wanted!r} not found")
        return wanted
    if "upstream" in remotes:
        return "upstream"
    if "origin" in remotes:
        url = git(root, "remote", "get-url", "origin").stdout
        if re.search(r"openai/codex(\.git)?\s*$", url):
            return "origin"
    raise Escalate("no upstream remote (add one or pass --upstream-remote)")


def newest_stable(root, remote):
    out = git(root, "ls-remote", "--tags", remote, "refs/tags/rust-v*").stdout
    tags = set()
    for line in out.splitlines():
        ref = line.split("\t")[-1].removeprefix("refs/tags/").removesuffix("^{}")
        if tag_key(ref):
            tags.add(ref)
    return max(tags, key=tag_key, default=None)


# ---- globs / classification -------------------------------------------------
def glob_match(pattern, path):
    rx = re.escape(pattern).replace(r"\*\*", "\0").replace(r"\*", "[^/]*").replace(r"\?", "[^/]")
    return re.fullmatch(rx.replace("\0", ".*"), path) is not None


def load_policy(root):
    p = Path(root) / "local" / "fork-surface.toml"
    try:
        import tomllib
        d = tomllib.loads(p.read_text())
    except Exception:
        return [], [LOCKFILE]
    return d.get("allow", d.get("allowlist", [])), d.get("generated", [LOCKFILE])


def classify(path, allow, generated):
    if path.startswith(("local/", "codex-rs/local/")):
        return "local"
    if any(glob_match(g, path) for g in generated) or path == LOCKFILE:
        return "generated"
    if any(glob_match(g, path) for g in allow):
        return "hook"
    return "other"


def unmerged(root):
    # index stage entries are the authoritative record of unmerged paths
    out = git(root, "ls-files", "-u", "-z").stdout
    paths = sorted({e.split("\t", 1)[1] for e in out.split("\0") if "\t" in e})
    allow, generated = load_policy(root)
    return [{"path": p, "kind": classify(p, allow, generated)} for p in paths]


def conflict_result(step, root):
    cs = unmerged(root)
    if any(c["kind"] == "other" for c in cs):
        return emit(step, False, 2, "ESCALATE: conflict outside the allowlist; stop and comment on the issue", conflicts=cs)
    if cs:
        return emit(step, False, 1, "resolve conflicts: hook -> re-apply FORK_SURFACE.md recipe on upstream's version; "
                    "generated -> take upstream and regenerate; then git add and run check", conflicts=cs)
    return emit(step, True, 0, "run: sync_upstream.py check")


# ---- subcommands ------------------------------------------------------------
def cmd_status(a, root):
    remote = resolve_remote(root, a.upstream_remote)
    cur = current_version(root)
    newest = newest_stable(root, remote)
    if a.tag:
        newest = validate_tag(a.tag)
    new = bool(newest) and (a.force or (tag_key(newest) or ()) > (tag_key(cur) or ()))
    return emit("status", True, 0, f"merge {newest}" if new else "none",
                current=cur, latest=newest, new_release=new, remote=remote)


def cmd_merge(a, root):
    tag = validate_tag(a.tag)
    remote = resolve_remote(root, a.upstream_remote)
    if git(root, "rev-parse", "-q", "--verify", f"refs/tags/{tag}", check=False).returncode:
        git(root, "fetch", remote, f"refs/tags/{tag}:refs/tags/{tag}")
    branch = f"upstream-sync/{tag}"
    git(root, "checkout", "-b", branch)
    m = git(root, "merge", "--no-commit", "--no-ff", tag, check=False)
    if m.returncode and "CONFLICT" not in m.stdout and not unmerged(root):
        raise Escalate(f"merge failed: {(m.stderr or m.stdout).strip()[:300]}")
    if LOCKFILE in [c["path"] for c in unmerged(root)]:
        git(root, "checkout", f"{tag}", "--", LOCKFILE)  # take upstream's
        git(root, "add", LOCKFILE)
    if not a.no_regen and shutil.which("cargo") and (root / "codex-rs").is_dir():
        run(["cargo", "update", "--workspace"], root / "codex-rs")
        git(root, "add", LOCKFILE, check=False)
    version_file(root).write_text(tag + "\n")
    git(root, "add", "local/UPSTREAM_VERSION")
    if unmerged(root):
        return conflict_result("merge", root)
    git(root, "commit", "-m", f"Merge {tag} into {branch}", "--no-verify")
    return emit("merge", True, 0, "run: sync_upstream.py check", branch=branch)


def cmd_conflicts(a, root):
    return conflict_result("conflicts", root)


def tail_matches(text, rx, limit=30):
    return [l.strip() for l in text.splitlines() if re.search(rx, l)][:limit]


def hook_tests(root):
    p = Path(root) / "local" / "FORK_SURFACE.md"
    if not p.exists():
        return []
    return [m.group(1).split() for m in re.finditer(r"^\W*Tests?:\s*`(cargo test [^`]+)`", p.read_text(), re.M)]


def new_workflows(root, base):
    if base is None:
        base = "HEAD^1" if git(root, "rev-parse", "-q", "--verify", "HEAD^2", check=False).returncode == 0 else None
    if base is None:
        return []
    out = git(root, "diff", "--name-only", "--diff-filter=A", base, "HEAD", "--", ".github/workflows", check=False).stdout
    names = [Path(p).name for p in out.splitlines() if p.endswith(".yml")]
    return sorted(n for n in names if not any(fnmatch.fnmatch(n, g) for g in FORK_WORKFLOWS))


def cmd_check(a, root):
    d = {"build_errors": [], "test_failures": [], "surface_violations": []}
    steps, skipped, escalate = [], [], []
    have_cargo = shutil.which("cargo") and (root / "codex-rs").is_dir()
    cargo_dir = root / "codex-rs"

    def cargo(name, args, key, rx):
        if not have_cargo:
            skipped.append(name)
            return
        p = run(["cargo", *args], cargo_dir)
        steps.append({"step": name, "ok": p.returncode == 0})
        if p.returncode:
            d[key] += tail_matches(p.stdout + p.stderr, rx) or [f"{name} exited {p.returncode}"]

    cargo("build", ["check", "-p", "codex-local-cli"], "build_errors", r"^error")
    cargo("local-tests", ["test", *sum((["-p", c] for c in LOCAL_CRATES), [])], "test_failures", r"FAILED|^error|panicked")
    for i, cmd in enumerate(hook_tests(root)):
        cargo(f"hook-test-{i}", cmd[1:], "test_failures", r"FAILED|^error|panicked")
    fs = root / "local" / "scripts" / "check_fork_surface.py"
    if fs.exists():
        p = run([sys.executable, str(fs), "--root", str(root)], root)
        try:
            v = json.loads(p.stdout).get("violations", [])
        except ValueError:
            v = [p.stderr.strip()[:300] or "check_fork_surface.py gave no JSON"]
        if p.returncode and not v:
            v = [f"check_fork_surface.py exited {p.returncode}"]
        escalate += [x for x in v if isinstance(x, dict) and x.get("kind") in ESCALATE_KINDS]
        d["surface_violations"] += [x if isinstance(x, str) else json.dumps(x) for x in v]
        steps.append({"step": "fork-surface", "ok": p.returncode == 0})
    else:
        skipped.append("fork-surface (check_fork_surface.py not present)")
    nw = new_workflows(root, a.base)
    disabled, failed_disable = [], []
    if nw:
        for n in nw:
            if a.repo and shutil.which("gh") and \
                    run(["gh", "workflow", "disable", n, "-R", a.repo], root).returncode == 0:
                disabled.append(n)
            else:
                failed_disable.append(n)   # no repo, no gh, or gh failed: still enabled
    steps.append({"step": "workflows", "ok": not failed_disable})
    ok = not (d["build_errors"] or d["test_failures"] or d["surface_violations"]
              or skipped or failed_disable)
    if escalate:
        code, nxt = 2, ("ESCALATE: fork-surface budget exceeded or non-allowlisted path touched; "
                        "do not auto-repair, comment on the upstream-sync issue for a human decision")
    elif ok:
        code, nxt = 0, "run: sync_upstream.py report"
    else:
        code = 1
        nxt = ("repair (allowed edits only): fix build_errors first, then test_failures, then "
               "surface_violations; rerun check")
        if skipped:
            nxt = f"install the missing tool(s) so required steps run (skipped: {', '.join(skipped)}); " + nxt
        if failed_disable:
            nxt = ("disable the listed workflows by hand (gh workflow disable <name>); " + nxt)
    return emit("check", ok, code, nxt, steps=steps, skipped=skipped, new_workflows=nw,
                workflows_disabled=disabled, workflows_to_disable=failed_disable, **d)


def cmd_report(a, root):
    ver = current_version(root)
    chk = {}
    if a.check_json:
        try:
            chk = json.loads(Path(a.check_json).read_text())
            if not isinstance(chk, dict):
                raise ValueError("expected a JSON object")
        except (OSError, ValueError) as e:
            return emit("report", False, 2, f"ESCALATE: --check-json {a.check_json!r} unusable: {e}",
                        error={"kind": "bad_check_json", "path": str(a.check_json), "message": str(e)})
    stat = git(root, "diff", "--stat", "HEAD^1", "HEAD", "--", ".", f":!{LOCKFILE}", check=False).stdout.strip().splitlines()
    ok = chk.get("ok", True)
    L = [f"## Upstream sync: {ver}", "", f"Result: {'PASS' if ok else 'FAIL'}", ""]
    for title, key in [("Conflicts", "conflicts"), ("Build errors", "build_errors"),
                       ("Test failures", "test_failures"), ("Fork-surface violations", "surface_violations")]:
        items = chk.get(key) or []
        lines = [f"- `{x}`" if isinstance(x, str) else f"- `{x['path']}` ({x['kind']})" for x in items]
        L += [f"### {title}", *(lines or ["- none"]), ""]
    for key, title in [("skipped", "Skipped steps"), ("new_workflows", "New upstream workflows"),
                       ("workflows_disabled", "Workflows disabled"), ("workflows_to_disable", "Workflows still to disable")]:
        if chk.get(key):
            L += [f"### {title}", *(f"- {x}" for x in chk[key]), ""]
    L += ["### Diffstat (merge vs first parent, Cargo.lock excluded)", "```", *stat[-15:], "```"]
    md = "\n".join(L) + "\n"
    if a.out:
        Path(a.out).write_text(md)
    return emit("report", True, 0, "open PR" if ok else "open or update the upstream-sync issue", markdown=md)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", default=str(ROOT))
    ap.add_argument("--upstream-remote", default=None)
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("status"); s.set_defaults(fn=cmd_status)
    s.add_argument("--tag"); s.add_argument("--force", action="store_true")
    m = sub.add_parser("merge"); m.set_defaults(fn=cmd_merge)
    m.add_argument("tag"); m.add_argument("--no-regen", action="store_true")
    sub.add_parser("conflicts").set_defaults(fn=cmd_conflicts)
    c = sub.add_parser("check"); c.set_defaults(fn=cmd_check)
    c.add_argument("--repo"); c.add_argument("--base")
    r = sub.add_parser("report"); r.set_defaults(fn=cmd_report)
    r.add_argument("--check-json"); r.add_argument("--out")
    a = ap.parse_args(argv)
    try:
        return a.fn(a, Path(a.root))
    except Escalate as e:
        return emit(a.cmd, False, 2, f"ESCALATE: {e}")


if __name__ == "__main__":
    sys.exit(main())
