# Using this build with Ollama (qwen3-coder) as your installed `codex`

This branch (`feat/chat-completions-wire-adapter`) lets Codex talk to Ollama
via the Chat Completions wire API, with working `apply_patch` tool calls for
qwen3-coder (see PR #3, issue #4).

The official `codex` is installed via npm as a prebuilt native binary. There is
no plugin mechanism — to run *this* code you build the branch and swap the
built binary in place of the vendored one. These are the steps to set that up
and keep it working.

## 1. Build

```bash
cd codex-rs
cargo build --release -p codex-cli
# -> codex-rs/target/release/codex
```

## 2. Swap into the npm install (+ re-sign)

The npm launcher (`@openai/codex/bin/codex.js`) spawns a vendored native binary
from the platform package. On Apple Silicon:

```bash
V="$HOME/.npm-global/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin"

# back up the stock binary once
[ -f "$V/codex.bak" ] || cp "$V/codex" "$V/codex.bak"

# swap in the branch build
cp codex-rs/target/release/codex "$V/codex"

# REQUIRED on arm64: cp invalidates the signature; re-sign ad-hoc or macOS
# SIGKILLs it ("Killed: 9" / exit 137).
codesign --remove-signature "$V/codex" 2>/dev/null
codesign -s - -f "$V/codex"

codex --version   # -> codex-cli 0.0.0  (the branch build)
```

> The exact vendor path depends on platform/arch. If `@openai/codex-darwin-arm64`
> differs on your machine, find it with:
> `node -e "console.log(require.resolve('@openai/codex-darwin-arm64/package.json'))"`
> then look under `vendor/<triple>/bin/codex`.

## 3. Configure the Ollama provider

In `~/.codex/config.toml` (additive — does not change your default model):

```toml
[model_providers.ollama-tunnel]
name = "Ollama qwen3-coder (tunnel:11435)"
base_url = "http://localhost:11435/v1"
wire_api = "chat"
requires_openai_auth = false
supports_websockets = false
```

If Ollama is remote, forward it: `ssh -fN -L 11435:127.0.0.1:11434 <host>`.

## 4. Run

```bash
codex -c model_provider=ollama-tunnel \
      -c model=qwen3-coder:latest \
      -c model_apply_patch_tool_type=function
```

`model_apply_patch_tool_type=function` is what makes `apply_patch` work with
qwen3-coder (it emits valid JSON `tool_calls` for function tools but its XML
dialect for the freeform/grammar variant). Passing it per-command keeps your
default model (e.g. gpt-5.4) untouched. To make it permanent for this provider,
set `model_apply_patch_tool_type = "function"` at the top level of config.toml.

## Revert to stock codex

```bash
V="$HOME/.npm-global/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin"
cp "$V/codex.bak" "$V/codex"
```

## Caveat: npm updates overwrite the swap

`npm install -g @openai/codex@latest` (or any reinstall) replaces the vendored
binary with the stock one. After an update, re-run step 2. If you'd rather not
deal with that, install the branch build as its own command earlier on `PATH`
(e.g. symlink `target/release/codex` into `~/.local/bin/codex`) instead of
swapping the npm vendor binary.
