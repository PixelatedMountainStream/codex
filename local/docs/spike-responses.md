# Phase 0 spike: Responses API against local endpoints

**Decision: keep the Responses API and drop the chat adapter.**

## Setup

- Stock upstream `rust-v0.162.1`, debug build.
- Endpoints: plain Ollama `http://127.0.0.1:11435` and PAIR `http://localhost:11434`.
- Model `qwen3.8:27b` (user-confirmed canonical tag; `qwen3.8:27B-mlx` is a different model).
- Custom provider with `wire_api="responses"` and `requires_openai_auth=false`.

## Results

| capability | Ollama | PAIR |
|---|---|---|
| chat | 3/3 | 3/3 |
| shell | 5/5 | 4/5 |
| apply_patch | 5/5 via shell interception (0 via the real tool) | 0/5 |
| spawn_agent | 0/3 | 3/3 |
| resume | 3/3 | 3/3 |
| resume_fallback | not verified | 0/2 (500) |
| apply_patch, derived freeform catalog | 0/2 (600s hang) | skipped |

Durations are informational only, because PAIR routes by load.

## Findings

- **H8 (spawn_agent on Ollama):** Ollama 0.32.15 flattens namespaced tools to `ns.name`. H8 splits the name on a registry miss. Verified live with 2 spawn_agent calls on Ollama.
- **apply_patch:** only shell-intercepted apply_patch is used. The freeform tool hangs on Ollama (a function_call with no output) and there is no function variant at this tag. codex-local doesn't advertise freeform.

## PAIR findings (deferred to plan Phase 9)

- PAIR forwards requests verbatim. The 500 comes from the llama.cpp Qwen Jinja template when codex adds developer messages mid-history: on cross-home resume (the skills catalog path), on `/model`, on permission changes and on compaction. Fixed by https://github.com/PixelatedMountainStream/Personal-AI-Router/pull/1.
- Turns hop between nodes, which forces a full prompt recompute. Fixed by thread-id affinity in https://github.com/PixelatedMountainStream/Personal-AI-Router/pull/2.
- Neither PR is deployed yet: macOS App Management blocks swapping the binary inside the signed `/Applications/NVIDIA PAIR.app`.
- Node .146 (16 GB, IQ4_XS, 8k context) silently truncates prompts over 8k. It is offline for now.
- Both 64 GB Macs serve Q4_K_M (digest aaee06).

## Open items

- The Ollama filename-fallback resume path is still unverified.
- The PAIR shell run ended with an empty final message (1 of 5).
- The throughput benchmark hasn't run (Phase 9).

## Reproduce

The spike harness lives outside the repo and is not committed. It ran each capability N times per endpoint against stock `codex exec` with the provider above, then scored the JSON event streams (completed tool events and final file contents) with a small analysis script.
