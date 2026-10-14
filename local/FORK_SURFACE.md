# Fork surface

| Hook | Upstream file | Purpose | Status |
|------|---------------|---------|--------|
| H1 | `codex-rs/cli/src/main.rs` (+1 line in `codex-rs/cli/build.rs`) | `cli_main` takes argv (`parse_from`) so `codex-local-cli`, which `include!`s this file as its crate root, can call it with its own argv. Stock `main` is gated `#[cfg(not(codex_local))]`. 11 lines added, 2 removed in main.rs (the `Box::pin(cli_main(..))` call grew from 1 to 6 lines via rustfmt); build.rs declares the cfg. | Done |
| H2 | `codex-rs/model-provider/src/provider.rs` (+ 2 `pub use` lines in `lib.rs`) | `OnceLock` `LOCAL_MODELS_FACTORY` + `register_local_models_factory`; `create_models_manager` consults it only when provider `name == "codex-local"` (no provider id exists at this layer). Unregistered: stock behaviour. Lines: 17 hook + 19 unit test. Re-apply: add the static, `local_models_manager()` helper and the 3-line early return after the `config_model_catalog` check in `create_models_manager`; re-export from lib.rs. | done |
| H3 | TODO | TODO | TODO |
| H4 | TODO | TODO | TODO |
| H5 | TODO | TODO | TODO |
| H6 | TODO | TODO | TODO |
| H7 | TODO | TODO | TODO |
| H8 | `codex-rs/core/src/tools/router.rs` | `ToolRouter::unflatten_tool_name` (+1 call in `dispatch_tool_call_with_code_mode_result_inner`): when a call has no/default namespace, its dotted name misses the registry, and the split at the first dot hits, dispatch under the split `(namespace, name)`. Works around Ollama flattening Responses `namespace` into the name (`multi_agent_v1.spawn_agent`). Unconditional: OpenAI names match `^[a-zA-Z0-9_-]+$`, so stock behaviour is unchanged. History keeps the model's original item. 16 upstream lines in router.rs (tests in router_tests.rs). Re-apply: add the method and call it on `tool_name` right after destructuring `call`. | done |

## H1 re-apply recipe

1. In `codex-rs/cli/src/main.rs`: add `#[cfg(not(codex_local))]` above `fn main`; change `cli_main` to take a leading `argv: Vec<std::ffi::OsString>`; replace `MultitoolCli::parse()` with `MultitoolCli::parse_from(argv)`; pass `std::env::args_os().collect()` from `main`.
2. In `codex-rs/cli/build.rs`, add `println!("cargo:rustc-check-cfg=cfg(codex_local)");`.
3. Re-sync the `[dependencies]` and target-specific dependency blocks of `codex-rs/local/cli/Cargo.toml` with `codex-rs/cli/Cargo.toml` (path deps get one more `../`); `local/cli/src/main.rs` includes upstream's main.rs, so any new upstream dependency must be copied. If upstream changes `main()`/`cli_main` setup, mirror it in `local/cli/src/main.rs`.
