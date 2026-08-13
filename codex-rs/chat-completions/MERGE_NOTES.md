# Upstream merge notes

Running record of what it costs to merge `openai/codex` into this fork, and what we
should restructure to make the next merge cheaper. The governing rule: **changes to
upstream files stay additive, localized, and few** — our value lives in the
`codex-chat-completions` crate, not in edits to upstream code.

---

## 2026-08-12 — merge of `origin/main` @ `361fe2d202`

Previous base `94427aaf46` (2026-06-12). **1784 upstream commits**, two months of drift.
Merged (not rebased) onto scratch branch `merge/upstream-2026-08`.

### Cost summary

| | |
|---|---|
| Files conflicting | 7 of ~20 upstream files we touch |
| Conflicts needing hand re-derivation | 2 (`spec_plan.rs`, `provider.rs`) |
| Compile errors after a clean textual merge | ~20, none predictable from the conflict list |
| New footprint added to upstream files | **none** |

The headline lesson: **the conflict count told us almost nothing about the real cost.**
`client.rs` was predicted hardest and turned out to be a one-hunk import conflict. The
expensive work was semantic drift in files that auto-merged perfectly and then failed to
compile.

### Conflicts, easiest to hardest

- `core/tests/suite/mod.rs`, `models-manager/src/config.rs` — pure "keep both" (a module
  line, an import). Trivial.
- `core/src/client.rs` — **one conflict, in the import block only.** All three of our
  seams (the `WireApi::Chat` dispatch arm, `stream_chat_completions_api`, the
  `responses_websocket_enabled` guard) survived textually.
- `core/src/tools/handlers/apply_patch.rs` — comment/insertion-order collision around a
  new upstream `apply_patch_file_update_mode` fn. Our `tool_type` field, `new_with_type`,
  and `ToolPayload::Function` widening auto-merged.
- `core/src/tools/handlers/shell_tests.rs` — import-block collision; our alias tests
  auto-merged intact at EOF.
- `core/src/tools/spec_plan.rs` — **re-derived by hand.** Upstream renamed
  `planned_tools` → `ToolRegistry` and replaced the old dispatch-only concept with
  `registry.add_with_exposure(handler, ToolExposure::Hidden)`. Shell registration is now
  gated on `supports_shell_command`. Our `exec` alias re-applied onto the new API.
- `model-provider/src/provider.rs` — **re-derived by hand.** Upstream added a Bedrock
  provider path and reworked `capabilities()` around `remote_compaction`. Our `wire_api`
  gating re-applied as a wrapper that spreads upstream's computed defaults.

### Semantic drift — the part that actually cost time

None of this appears as a conflict. It only surfaces at `cargo build`.

1. **`ResponsesApiRequest.tools` became opaque** — `Vec<Value>` → `Option<ResponsesApiTools>`,
   an `Arc<RawValue>` newtype whose `as_raw_value()` is `pub(crate)`.
   **Resolution: read it through its public `Serialize` impl** (`serde_json::to_value`)
   from inside our crate. The obvious fix — adding a `pub` accessor to `codex-api` —
   was rejected because it would add a permanent line of upstream footprint to re-merge
   forever. Serializing costs one allocation per request, not per token.
2. **`strip_images_when_unsupported` now takes `&mut [ResponseItemEnvelope]`** while
   `request.input` is still `Vec<ResponseItem>`. Resolved with upstream's own
   `ResponseItemEnvelope::new` / `into_item` around the call.
3. **`ResponseItem.id` became the `ResponseItemId` newtype.** We use
   `ResponseItemId::from_server(..)` for the IDs we synthesize on behalf of a local model
   that supplies none.
4. **New required fields on `ResponseItem` variants** — `internal_chat_message_metadata_passthrough`,
   `encrypted_function_args` (FunctionCall), `namespace` (CustomToolCall). All `None` for
   a local model.
5. **Four `client.rs` helper signatures moved** — transport construction is now
   `build_api_transport(&provider, endpoint)`; `AuthRequestTelemetryContext::new`,
   `map_response_events`, and `handle_unauthorized` each gained a parameter, and
   `build_responses_request` lost one. Every one had an exact analogue in upstream's
   current Responses path in the same file.
6. Assorted new variants/fields: `ContentItem::InputAudio`, `ResponseItem::AdditionalTools`,
   `CompactionTrigger {}` as a struct variant, `FunctionCallOutput.id`,
   `ResponsesApiRequest.stream_options`, `ModelProviderInfo.supports_standalone_web_search`.

### Behavioral regression caught only by live validation

Everything compiled and every test passed, and the local node worked — but the **remote
node failed with `unsupported call: shell`**.

Cause: **upstream renamed the shell tool `shell` → `shell_command`.** Our fork already
aliased `exec` (which is what qwen3-coder:30b happened to emit, so the local node passed),
but nothing aliased `shell` — and qwen3-coder:latest emits `shell`. Before the merge,
`shell` was the real upstream tool name, so this worked natively; after the merge it
resolved to nothing.

Fix: `LOCAL_MODEL_SHELL_ALIASES = ["exec", "shell"]` in `core/src/tools/spec_plan.rs`,
registered as hidden aliases at both shell-registration sites, with a regression test.

**Lesson for next time: a green test suite does not prove the local-LLM path works.**
Nothing in the mocked suite covers which tool *name* a real model emits. Always run the
live two-node validation, and validate on **more than one model tag** — `:30b` and
`:latest` behaved differently and only one of them exposed the bug.

### Model-side traps (not merge issues, but they will look like failures)

- qwen writes GNU `sed -i`, which fails on macOS BSD sed, leaving the file untouched while
  the tool call itself succeeds. Steer prompts to `python3` explicitly and forbid `sed`.
- qwen sometimes emits a tool call missing the required `command` field; re-running
  usually gets a well-formed call. Sampling variance, not a wire-format bug.
- Verify by artifact (git diff + verifier exit code). Both of the above return `codex_rc=0`.

### Not our problem, but will bite again

- **`v8` fails to build in this environment.** Upstream's `code-mode-runtime` and
  `v8-poc` crates depend on `v8 150.4.0`, whose build script downloads a prebuilt
  `librusty_v8` archive; that URL 404s. It is upstream's own dependency (present in
  upstream's `Cargo.lock`), unrelated to our changes. Exclude those three crates when
  building/testing locally.
- **One upstream test overflows the debug stack** —
  `session::tests::guardian_tests::strict_auto_review_turn_grant_forces_guardian_for_shell_command_policy_skip`.
  Passes with `RUST_MIN_STACK=16777216`. Use that env var for the test suite.

### What to restructure before the next merge

- `stream_chat_completions_api` in `core/src/client.rs` is our largest liability: ~170
  lines that **mirror** upstream's Responses-path retry/auth-recovery loop. Every upstream
  refactor of that loop breaks us. Worth investigating whether the loop can be factored so
  both wire APIs share it with the endpoint and body-builder as parameters — that would
  turn a recurring re-derivation into a one-line match arm.
- Everything else is already at minimum footprint. Keep it that way: prefer solving drift
  inside `codex-rs/chat-completions/` even at a small runtime cost, as with the `tools`
  serialization above.

### Reproducing the assessment before a merge

```bash
git fetch origin main
# read-only conflict prediction, does not touch the worktree:
git merge-tree --write-tree --name-only HEAD origin/main
# upstream churn in the files we touch:
for f in $(git diff --name-only $(git merge-base HEAD origin/main)...HEAD); do
  echo "$(git log --oneline $(git merge-base HEAD origin/main)..origin/main -- "$f" | wc -l) $f"
done | sort -rn
```

Then assume the compile errors will outnumber the conflicts, and budget accordingly.
