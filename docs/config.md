# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

MCP tools default to serialized calls. To mark every tool exposed by one server
as eligible for parallel tool calls, set `supports_parallel_tool_calls` on that
server:

```toml
[mcp_servers.docs]
command = "docs-server"
supports_parallel_tool_calls = true
```

Only enable parallel calls for MCP servers whose tools are safe to run at the
same time. If tools read and write shared state, files, databases, or external
resources, review those read/write race conditions before enabling this setting.

## MCP tool approvals

Codex stores approval defaults and per-tool overrides for custom MCP servers
under `mcp_servers` in `~/.codex/config.toml`. Set
`default_tools_approval_mode` on the server to apply a default to every tool,
and use per-tool `approval_mode` entries for exceptions:

```toml
[mcp_servers.docs]
command = "docs-server"
default_tools_approval_mode = "approve"

[mcp_servers.docs.tools.search]
approval_mode = "prompt"
```

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

Codex stores "never show again" choices for tool suggestions in `config.toml`:

```toml
[tool_suggest]
disabled_tools = [
  { type = "plugin", id = "slack@openai-curated" },
  { type = "connector", id = "connector_google_calendar" },
]
```

## Notify

Codex can run a notification hook when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

When Codex knows which client started the turn, the legacy notify JSON payload also includes a top-level `client` field. The TUI reports `codex-tui`, and the app server reports the `clientInfo.name` value from `initialize`.

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## SQLite State DB

Codex stores the SQLite-backed state DB under `sqlite_home` (config key) or the
`CODEX_SQLITE_HOME` environment variable. When unset, WorkspaceWrite sandbox
sessions default to a temp directory; other modes default to `CODEX_HOME`.

## Custom CA Certificates

Codex can trust a custom root CA bundle for outbound HTTPS and secure websocket
connections when enterprise proxies or gateways intercept TLS. This applies to
login flows and to Codex's other external connections, including Codex
components that build reqwest clients or secure websocket clients through the
shared `codex-client` CA-loading path and remote MCP connections that use it.

Set `CODEX_CA_CERTIFICATE` to the path of a PEM file containing one or more
certificate blocks to use a Codex-specific CA bundle. If
`CODEX_CA_CERTIFICATE` is unset, Codex falls back to `SSL_CERT_FILE`. If
neither variable is set, Codex uses the system root certificates.

`CODEX_CA_CERTIFICATE` takes precedence over `SSL_CERT_FILE`. Empty values are
treated as unset.

The PEM file may contain multiple certificates. Codex also tolerates OpenSSL
`TRUSTED CERTIFICATE` labels and ignores well-formed `X509 CRL` sections in the
same bundle. If the file is empty, unreadable, or malformed, the affected Codex
HTTP or secure websocket connection reports a user-facing error that points
back to these environment variables.

## Notices

Codex stores "do not show again" flags for some UI prompts under the `[notice]` table.

## Plan mode defaults

`plan_mode_reasoning_effort` lets you set a Plan-mode-specific default reasoning
effort override. When unset, Plan mode uses the built-in Plan preset default
(currently `medium`). When explicitly set (including `none`), it overrides the
Plan preset. The string value `none` means "no reasoning" (an explicit Plan
override), not "inherit the global default". There is currently no separate
config value for "follow the global default in Plan mode".

## Switching providers between sessions

To run the main agent against a local provider, start (or resume) Codex with
both `--oss` and `--local-provider`. `--local-provider` only takes effect
under `--oss`; passing it alone exits with an error pointing to the correct
flag combination.

```bash
# Start a new session against local Ollama.
codex --oss --local-provider ollama --model gemma4:26b-a4b-it-q4_K_M

# Resume an existing thread on the same provider/model.
codex resume <thread-id> --oss --local-provider ollama --model gemma4:26b-a4b-it-q4_K_M
```

Per-task routing for compaction, `/review`, and memories is configured via
TOML (see below) and is independent of the main-agent provider chosen at
startup.

## Per-task model and provider overrides

Some background tasks can be routed to a different model and provider than
the main agent. Each override is a pair: setting `*_provider` without the
matching `*_model` is rejected at config load.

```toml
# ~/.codex/config.toml — main agent stays on its default model/provider.

# Conversation compaction (auto-compact and /compact).
compact_provider = "ollama"
compact_model    = "gemma4:26b-a4b-it-q4_K_M"

# Inline /review and detached /review (app-server). Detached reviews fork
# from the parent thread's full effective config and inherit these.
review_provider  = "ollama"
review_model     = "gemma4:26b-a4b-it-q4_K_M"

[memories]
extract_provider       = "ollama"
extract_model          = "gemma4:26b-a4b-it-q4_K_M"
consolidation_provider = "ollama"
consolidation_model    = "gemma4:26b-a4b-it-q4_K_M"
```

The provider id must reference a provider already known to Codex (built-in
or from `[model_providers.<id>]`). The built-in `ollama` provider speaks
the Responses wire API and requires Ollama ≥ 0.13.4.

## Delegating to a local LLM agent

A child agent role can pin its own provider and model. The main agent can
then call `spawn_agent(agent_type = "local_llm", ...)` to delegate work to
it without changing its own model.

```toml
# ~/.codex/config.toml
[agents.local_llm]
description         = "Delegate to local Gemma 4 26B-A4B (Ollama)."
config_file         = "./agents/local_llm.toml"
nickname_candidates = ["Gemma"]
```

```toml
# ~/.codex/agents/local_llm.toml
model_provider = "ollama"
model          = "gemma4:26b-a4b-it-q4_K_M"
```

Local quantized models do best on prose-shaped tasks (summarisation,
classification, drafting). Tool-heavy delegations may degrade.

## Realtime start instructions

`experimental_realtime_start_instructions` lets you replace the built-in
developer message Codex inserts when realtime becomes active. It only affects
the realtime start message in prompt history and does not change websocket
backend prompt settings or the realtime end/inactive message.

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).
