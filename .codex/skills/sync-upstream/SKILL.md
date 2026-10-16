---
name: sync-upstream
description: Merge a new openai/codex rust-v* release into codex-local, repair conflicts and breakage within the allowed edit set, and escalate otherwise. Use for the upstream-sync issue/PR or when asked to sync upstream.
---

# sync-upstream

Driver: `python3 local/scripts/sync_upstream.py <subcommand>`. Each prints ONE JSON
object `{ok, step, conflicts[], build_errors[], test_failures[], surface_violations[], next_action}`.
Exit codes: **0** ok, **1** needs repair (you fix it), **2** escalate (you stop).
Always do what `next_action` says. Never guess past it.

## Checklist

1. `status` -> if `new_release` is false, stop (nothing to do). Else note `latest`.
2. `merge <latest>` -> creates `upstream-sync/<tag>`, merges the tag, takes upstream's
   `codex-rs/Cargo.lock` and regenerates it, bumps `local/UPSTREAM_VERSION`.
   Exit 0: go to step 4. Exit 1: step 3. Exit 2: ESCALATE.
3. `conflicts` -> repair each listed path by `kind`, `git add` it, rerun `conflicts` until
   exit 0, then `git commit --no-edit`.
   - `hook`: take UPSTREAM's version (`git checkout --theirs <path>`), then re-apply the hook
     by following that hook's recipe in `local/FORK_SURFACE.md`. Keep the edit as small as the
     recorded line count. Do not hand-merge the old hunk.
   - `local`: keep ours unless the file only exists upstream-side.
   - `generated` (Cargo.lock): `git checkout --theirs`, then `cargo update --workspace` in `codex-rs/`.
   - `other`: ESCALATE (see below).
4. `check` -> runs build, local crate tests, hook tests, `check_fork_surface.py`, and
   the workflow check. Loop while exit is 1 (see Repair loop). Exit 0: go to step 5.
5. `check > /tmp/check.json; report --check-json /tmp/check.json --out /tmp/report.md`.
   Use `markdown` from the report as the PR body (success) or issue comment (failure).
   Push the branch and open the PR to `codex-local/main` only when `check` exited 0.

## Repair loop (exit 1 from `check`)

Fix in this order, rerun `check` after each batch:
- `build_errors`: upstream API drift. Typical causes: a hook call site no longer matches a
  renamed/re-signatured upstream function, or a `codex-rs/local/*` crate uses a changed
  upstream API. Read the compiler error, fix the call site (hook file) or the local crate.
- `test_failures`: fix the local crate or re-apply the hook; never delete or skip a test.
- `surface_violations`: a file outside the allowlist or over budget is modified. Revert it
  to upstream's version (`git checkout <tag> -- <path>`). If a hook legitimately needs a
  bigger edit, that is an escalation, not a budget bump.
- `workflows_to_disable`: run `gh workflow disable <name> -R <owner/repo>`; not a code edit.

## Allowed edits (nothing else)

1. Allowlisted hook files (the `allow` list in `local/fork-surface.toml`), re-applied from the
   `FORK_SURFACE.md` recipe onto upstream's version.
2. `codex-rs/local/**` and `local/**`.
3. Generated files (`codex-rs/Cargo.lock`).

Never edit other upstream files to make things compile. Never add `codex-local-*`
dependencies to upstream crates. Never lower tests, loosen the budget, or edit the allowlist
to hide a violation.

## Escalate (exit 2: stop, do not keep trying)

- Any conflict with `kind: other` (outside the allowlist).
- A hook's target symbol (named in `FORK_SURFACE.md`) no longer exists or moved so the recipe
  cannot be applied as written.
- `check` still exits 1 after 3 repair rounds, or the work needs more than ~150 changed lines
  outside `codex-rs/local/**`.
- Any command exits 2.

To escalate: leave the branch as is, run `report` (with `--check-json` if you have one), and
comment on the open `upstream-sync` issue with: the tag, the failing step, the JSON
`conflicts`/errors, and which rule above triggered. Then stop.

## Notes

- `check` skips cargo steps if `cargo` is missing and says so under `skipped`; a skipped step
  is not a pass (`check` exits 1). `check_fork_surface.py` missing is also reported as skipped.
- Hook unit tests run if `FORK_SURFACE.md` has a line like ``Tests: `cargo test -p codex-core router` ``.
- Exit 2 from `check`: a fork-surface budget violation or a non-allowlisted path. Escalate.
- `check` exits 1 if any new upstream workflow could not be disabled (listed in
  `workflows_to_disable`).
- `--tag`/`merge <tag>` must match `^rust-v\d+\.\d+\.\d+$`; anything else exits 2.
- **Secret `UPSTREAM_SYNC_TOKEN`** (PAT or GitHub App token with `workflows` scope): the
  upstream-sync workflow uses it to push and open the PR when set. When a merge touches
  `.github/workflows/**` and it is NOT set, the workflow fails early with a clear message
  (GITHUB_TOKEN cannot push workflow-file changes). Otherwise it falls back to GITHUB_TOKEN.
- Options: `--upstream-remote NAME` (default `upstream`, else `origin` if it is openai/codex);
  `check --repo OWNER/NAME` disables new upstream workflows via `gh`.
