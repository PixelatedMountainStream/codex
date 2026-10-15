# Plan deltas

Where the build differs from the approved plan.

- **GitHub home:** PixelatedMountainStream/codex, with stacked PRs on base `codex-local/main`. The Radiant-Vision-Systems fork is deferred, because org members can't create public repos.
- **Boundary check:** sc-lint boundary records were replaced by a `cargo metadata` check in `check_fork_surface.py`. sc-lint has no wildcard mode and would need a static list of 161 crates.
- **New hook H8:** in `core/src/tools/router.rs` (splits flattened `ns.name` tool names on a registry miss).
- **H1:** uses `include!` of upstream `cli/src/main.rs` under `cfg(codex_local)`. `codex-local-cli`'s dependencies must mirror upstream cli's.
- **H4:** also drops the provider filter for named lookups when registered.
- **Hook budget:** 135 lines / 12 files (actual 122/11), above the plan's estimate of about 85/7.
- **Plugins:** `features.plugins=false` is set to stop the chatgpt.com plugin warm-up. This also disables plugin-bundled skills.
- **Session affinity and the mid-history fix:** live in PAIR (Phase 9), not in codex-local.
- **Models catalog:** `codex-local-models` builds its manager with `.with_provider_catalog()`, so Ollama's catalog is authoritative and no bundled gpt-* models are merged in; `ensure_loaded` lazily fetches because the initial list is empty.
- **Agent threads:** seeded `[agents] max_threads` is now 6 (existing configs keep 4, never rewritten); override with `CODEX_LOCAL_MAX_AGENTS=<n>` (positive int, beats config.toml, invalid warns and is ignored) or the `agents.max_threads` config key.
- **UPSTREAM_SYNC_TOKEN:** the upstream-sync workflow needs this secret (PAT or App token with workflows scope) to push and open the PR whenever the merge touches `.github/workflows/**`; without it that run fails early, otherwise it falls back to GITHUB_TOKEN.
- **Local web search:** Ollama fails the built-in server-side `web_search` tool, so config seeds `web_search = "disabled"` (also forced via `-c` on every run) and `[mcp_servers.web-search]` runs `uvx duckduckgo-mcp-server==0.7.0` (search + fetch_content, no key); `sync-config` keeps that entry; install/verify warn if `uvx` is missing.
- **No app-server daemon:** upstream auto-starts/discovers a daemon for the TUI that needs a full `codex-package.json` package (a bare binary fails with "no complete local package") and would bypass in-process registrations; codex-local injects `--no-daemon` (plain TUI/resume/fork, unless `--remote`) plus `-c features.daemon_auto_start=false`, and seeds that feature off; stock is untouched.
