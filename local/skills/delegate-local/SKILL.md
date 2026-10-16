---
name: delegate-local
description: >
  Delegate bounded, token-heavy, judgment-light sub-tasks to the local model (LM Studio by default)
  (qwen3.8:27b) via `codex-local exec` to cut cloud-token usage. Use when the user asks to
  "delegate locally", "offload this", "run it on the local model", runs /delegate-local, or
  when a task has large mechanical read / edit / summarize / search / verify work that the
  orchestrator should hand off instead of doing in-context.
---

# Delegate to a local model

Offload narrow, mechanical work to the free local model and ingest only a **compact,
verified** result, never the raw transcript. You stay the orchestrator: you decompose,
decide what is safe to delegate, and consume the signal.

**Map locally, reduce in cloud.** The local model is slow and a weak agentic driver. It is
reliable ONLY on narrow, fully-specified, single-shot units paired with a cheap verifier.

## Setup (zero-config)

`codex-local` is installed beside stock `codex` (`local/scripts/install.py`). It defaults to
engine LM Studio, model `qwen/qwen3.8-27b` (4-bit MLX; the 8-bit build is `-m qwen3.8-27b-mlx`) on `http://127.0.0.1:1234` (PAIR's LM Studio facade). Ollama (`qwen3.8:27b`, port 11434) and llama.cpp are one flag away. Overrides:
`-m MODEL`, `--base-url URL` (alias `--ollama-url`), `--engine ollama|lmstudio|llamacpp`, env
`CODEX_LOCAL_MODEL` / `CODEX_LOCAL_URL` / `CODEX_LOCAL_OLLAMA_URL` / `OLLAMA_HOST` /
`CODEX_LOCAL_ENGINE`. The engine can also be set in codex-local's config.toml
(`[local]` table, `engine = "lmstudio"`); precedence is flag > env > config > `lmstudio`. Homes seeded with Ollama's URL/model and no explicit engine now resolve to LM Studio; keep Ollama with `engine = "ollama"`, `--engine ollama` or `CODEX_LOCAL_ENGINE=ollama`. No profiles, tunnels, fleet files or node locks. Check with
`verify_install.py` (full) if a unit fails oddly.

## Green / Yellow / Red decisioning

- GREEN, delegate: narrow, fully specified, single-shot, cheaply verifiable. E.g. rename
  symbol X to Y across these files then `cargo build`; summarize each of 8 files in 2 lines;
  count call sites of `foo`; boilerplate from a template.
- YELLOW, pin down first: multi-file edits with a clear spec AND a compile/test gate. Split
  into smaller green units, each with its own verifier.
- RED, keep in cloud: open-ended design, architecture, ambiguous requirements, judgment, or
  anything with no cheap verifier.

When unsure, treat it as Red.

## How to delegate

1. **Decompose** into green units: a self-contained prompt, a working directory, and a
   verifier command (exit 0 = pass).
2. **One `codex-local exec` per unit.** Independent units may run as parallel processes:
   each issues its own request, and a PAIR proxy distributes them. Never share a working
   directory between concurrent units that touch the same files.
3. **Write the prompt to a file** and pipe it (avoids shell-escaping):
   ```bash
   codex-local exec --skip-git-repo-check -C /abs/workspace -s workspace-write \
     --output-last-message /tmp/unit1.out - < /tmp/unit1.txt
   ```
   Then run the verifier yourself: `cd /abs/workspace && <verify cmd>`; trust its exit code,
   not the model's prose. Retrying rarely helps: re-scope the prompt instead.
4. **Prompt rules** (each fixed an observed failure):
   - "INVOKE THE SHELL TOOL and run the command now. Do NOT just print the command."
     Without it the model often narrates a correct command and changes nothing.
   - Never delegate `apply_patch`; the model cannot produce valid hunks. Tell it to edit with
     a `python3 -c "..."` one-liner (portable across macOS, Windows, Linux). `sed -i` differs
     between BSD and GNU; always `python3`, never bare `python`.
   - State the exact change and tell it to act without asking questions.
5. **Consume only the compact result**: the `--output-last-message` file plus
   `git diff --stat` and the verifier exit code.

## Verification per task class

| Task class | Verifier |
|------------|----------|
| Code edit / refactor | project build/test: `cargo build`, `cargo test -q`, `npm test` |
| Pattern refactor | `grep -q 'new' file` and/or `! grep -rq 'old' --include='*.py' .` |
| Summary | a `grep` that a cited fact really appears |
| Search / inventory | `grep -c` sanity, or the output file is non-empty |

Scope verifiers tightly (explicit paths or `--include`); stale artifacts such as
`__pycache__` make broad greps falsely fail. If a unit has no cheap verifier, it was Red.

## Compression boundary

- Never paste the raw transcript or full diffs into your context. Look at a log only for
  triage, and do not echo it back.
- On verifier failure: fix it in cloud or re-scope into smaller green units.
- Use `git diff --stat` to see what landed; re-read specific changed lines yourself if you
  need certainty.

## Triage

- Verifier fails and nothing changed: the model narrated instead of acting. Re-issue with the
  shell-tool rule.
- Files changed but the verifier fails: it acted wrongly, or the verifier is mis-scoped.
  Check the verifier first, then the diff.
- `codex-local exec` exits non-zero or hangs: check the engine is up (`curl $URL/v1/models`),
  then escalate or split the unit.

## Honesty about savings

Savings are real for read / summarize / mechanical-edit / search, and marginal-to-negative
for open-ended or architectural work. Delegate Green freely; keep Red in cloud.
