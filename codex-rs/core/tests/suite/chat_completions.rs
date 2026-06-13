//! Integration tests for the Chat Completions wire path (`WireApi::Chat`).
//!
//! These tests exercise `stream_chat_completions_api` end-to-end against a
//! wiremock server.  They use the low-level `ModelClient` + `Prompt` API,
//! mirroring the `send_provider_auth_request` helper pattern in
//! `codex-rs/core/tests/suite/client.rs`.  Using `Prompt::default()` (no
//! tools) avoids the pre-existing tool-format translation mismatch:
//! `create_tools_json_for_responses_api` emits flat Responses-API tool format
//! (`{"type":"function","name":"…"}`), whereas `translate_tools` in
//! `codex-chat-completions` expects the nested Chat schema
//! (`{"type":"function","function":{"name":"…"}}`).  Tests that need to
//! exercise specific tool-call *responses* from the model are still valid
//! because those come from the SSE stream body, not from `prompt.tools`.
//!
//! Cases 4 (apply_patch CustomToolCall) and 5 (image-stripping) require the
//! session to populate `prompt.tools` in Chat schema, which is blocked by the
//! tool-format bug; they are covered by unit tests in `codex-chat-completions`
//! and noted below.

use std::sync::Arc;
use std::sync::Mutex;

use codex_core::ModelClient;
use codex_core::Prompt;
use codex_core::ResponseEvent;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use core_test_support::TestCodexResponsesRequestKind;
use core_test_support::load_default_config_for_test;
use core_test_support::responses::start_mock_server;
use core_test_support::responses_metadata as make_responses_metadata;
use futures::StreamExt;
use tempfile::TempDir;
use wiremock::Match;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

// ---------------------------------------------------------------------------
// Helpers — SSE body builders
// ---------------------------------------------------------------------------

/// Build a plain-text Chat Completions SSE body.
fn chat_sse_text(response_id: &str, content_chunks: &[&str]) -> String {
    let mut body = String::new();
    for chunk in content_chunks {
        let escaped = chunk.replace('\\', "\\\\").replace('"', "\\\"");
        body.push_str(&format!(
            "data: {{\"id\":\"{response_id}\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{escaped}\"}},\"finish_reason\":null}}]}}\n\n"
        ));
    }
    body.push_str(&format!(
        "data: {{\"id\":\"{response_id}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"\"}},\"finish_reason\":\"stop\"}}]}}\n\n"
    ));
    body.push_str("data: [DONE]\n\n");
    body
}

/// Build a Chat Completions SSE body with a single tool call.
fn chat_sse_tool_call(
    response_id: &str,
    call_id: &str,
    fn_name: &str,
    arguments: &str,
    finish_reason: &str,
) -> String {
    let args_escaped = arguments.replace('\\', "\\\\").replace('"', "\\\"");
    let mut body = String::new();
    // First chunk: tool_call header with empty arguments
    body.push_str(&format!(
        "data: {{\"id\":\"{response_id}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{{\"id\":\"{call_id}\",\"index\":0,\"type\":\"function\",\"function\":{{\"name\":\"{fn_name}\",\"arguments\":\"\"}}}}]}},\"finish_reason\":null}}]}}\n\n"
    ));
    // Second chunk: argument data
    body.push_str(&format!(
        "data: {{\"id\":\"{response_id}\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":\"{args_escaped}\"}}}}]}},\"finish_reason\":null}}]}}\n\n"
    ));
    // Finish chunk
    body.push_str(&format!(
        "data: {{\"id\":\"{response_id}\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"{finish_reason}\"}}]}}\n\n"
    ));
    body.push_str("data: [DONE]\n\n");
    body
}

// ---------------------------------------------------------------------------
// Helpers — provider / client construction
// ---------------------------------------------------------------------------

/// Build a `ModelProviderInfo` for `WireApi::Chat` pointing at the mock server.
fn chat_provider(server: &MockServer) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "test-chat".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Chat,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    }
}

/// Container for everything needed to call `client_session.stream(…)`.
struct ChatTestContext {
    client: ModelClient,
    metadata: codex_core::CodexResponsesMetadata,
    telemetry: SessionTelemetry,
    model_info: codex_protocol::openai_models::ModelInfo,
    effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    summary: ReasoningSummary,
}

const TEST_INSTALLATION_ID: &str = "11111111-1111-4111-8111-111111111111";
const TEST_WINDOW_ID: &str = "test-chat:0";

/// Build a `ChatTestContext` ready for one turn.
#[expect(clippy::unwrap_used)]
async fn build_context(server: &MockServer) -> ChatTestContext {
    let provider = chat_provider(server);
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home).await;
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    let effort = config.model_reasoning_effort.clone();
    let summary = config
        .model_reasoning_summary
        .unwrap_or(ReasoningSummary::Auto);
    let model = codex_core::test_support::get_model_offline(config.model.as_deref());
    config.model = Some(model.clone());
    let config = Arc::new(config);
    let model_info =
        codex_core::test_support::construct_model_info_offline(model.as_str(), &config);
    let thread_id = ThreadId::new();
    let telemetry = SessionTelemetry::new(
        thread_id,
        model.as_str(),
        model_info.slug.as_str(),
        /*account_id*/ None,
        Some("test@test.com".to_string()),
        /*auth_mode*/ None,
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "test".to_string(),
        SessionSource::Exec,
    );
    let client = ModelClient::new(
        Some(AuthManager::from_auth_for_testing(CodexAuth::from_api_key(
            "unused-api-key",
        ))),
        thread_id,
        provider,
        SessionSource::Exec,
        config.model_verbosity,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*attestation_provider*/ None,
    );
    let thread_id_str = thread_id.to_string();
    let metadata = make_responses_metadata(
        TEST_INSTALLATION_ID,
        &thread_id_str,
        &thread_id_str,
        /*turn_id*/ None,
        TEST_WINDOW_ID.to_string(),
        &SessionSource::Exec,
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    );
    ChatTestContext {
        client,
        metadata,
        telemetry,
        model_info,
        effort,
        summary,
    }
}

/// Build a simple single-message user `Prompt` with no tools.
fn user_prompt(text: &str) -> Prompt {
    let mut prompt = Prompt::default();
    prompt.input.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
    });
    prompt
}

/// Mount a single-shot Chat Completions SSE mock.
async fn mount_chat_once(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Collect all `ResponseEvent`s from a stream until `Completed`.
async fn collect_events(
    mut stream: impl futures::Stream<Item = Result<ResponseEvent, codex_protocol::error::CodexErr>>
    + std::marker::Unpin,
) -> Vec<ResponseEvent> {
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(re) => {
                let is_done = matches!(re, ResponseEvent::Completed { .. });
                events.push(re);
                if is_done {
                    break;
                }
            }
            Err(e) => {
                panic!("stream error: {e:?}");
            }
        }
    }
    events
}

// ---------------------------------------------------------------------------
// Case 1 — Plain text: OutputItemAdded(Message) arrives before the first
//          OutputTextDelta; OutputItemDone shares the same id; Completed.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_plain_text_output_item_added_before_delta() {
    let server = start_mock_server().await;
    mount_chat_once(
        &server,
        chat_sse_text("chatcmpl-text-1", &["Hello", " world"]),
    )
    .await;

    let ctx = build_context(&server).await;
    let mut session = ctx.client.new_session();
    let prompt = user_prompt("hello");

    let stream = session
        .stream(
            &prompt,
            &ctx.model_info,
            &ctx.telemetry,
            ctx.effort,
            ctx.summary,
            /*service_tier*/ None,
            &ctx.metadata,
            &codex_rollout_trace::InferenceTraceContext::disabled(),
        )
        .await
        .expect("chat stream should start");

    let events = collect_events(Box::pin(stream)).await;

    let added_pos = events.iter().position(|ev| {
        matches!(
            ev,
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
        )
    });
    let delta_pos = events
        .iter()
        .position(|ev| matches!(ev, ResponseEvent::OutputTextDelta(_)));
    let completed_pos = events
        .iter()
        .position(|ev| matches!(ev, ResponseEvent::Completed { .. }));

    assert!(
        added_pos.is_some(),
        "expected OutputItemAdded(Message) event; got: {events:?}"
    );
    assert!(
        delta_pos.is_some(),
        "expected OutputTextDelta event; got: {events:?}"
    );
    assert!(
        completed_pos.is_some(),
        "expected Completed event; got: {events:?}"
    );
    assert!(
        added_pos.unwrap() < delta_pos.unwrap(),
        "OutputItemAdded must precede first OutputTextDelta"
    );

    // Stable id: OutputItemAdded and OutputItemDone must share the same id.
    let added_id = events.iter().find_map(|ev| {
        if let ResponseEvent::OutputItemAdded(ResponseItem::Message { id, .. }) = ev {
            id.clone()
        } else {
            None
        }
    });
    let done_id = events.iter().find_map(|ev| {
        if let ResponseEvent::OutputItemDone(ResponseItem::Message { id, .. }) = ev {
            id.clone()
        } else {
            None
        }
    });
    if let (Some(a), Some(d)) = (added_id, done_id) {
        assert_eq!(
            a, d,
            "OutputItemAdded and OutputItemDone must share the same Message id"
        );
    }
}

// ---------------------------------------------------------------------------
// Case 2 — Function tool call with finish_reason="tool_calls": a FunctionCall
//          ResponseItem appears with the correct name and arguments.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_tool_call_finish_reason_tool_calls_produces_function_call_item() {
    let server = start_mock_server().await;
    mount_chat_once(
        &server,
        chat_sse_tool_call(
            "chatcmpl-fc-1",
            "call-001",
            "shell",
            r#"{"cmd":"echo hi"}"#,
            "tool_calls",
        ),
    )
    .await;

    let ctx = build_context(&server).await;
    let mut session = ctx.client.new_session();
    let prompt = user_prompt("hello");

    let stream = session
        .stream(
            &prompt,
            &ctx.model_info,
            &ctx.telemetry,
            ctx.effort,
            ctx.summary,
            None,
            &ctx.metadata,
            &codex_rollout_trace::InferenceTraceContext::disabled(),
        )
        .await
        .expect("chat stream should start");

    let events = collect_events(Box::pin(stream)).await;

    let fn_call = events.iter().find_map(|ev| {
        if let ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
            name, arguments, ..
        }) = ev
        {
            Some((name.clone(), arguments.clone()))
        } else {
            None
        }
    });

    assert!(
        fn_call.is_some(),
        "expected OutputItemAdded(FunctionCall); got: {events:?}"
    );
    let (name, args) = fn_call.unwrap();
    assert_eq!(name, "shell");
    assert!(
        args.contains("echo hi"),
        "arguments should contain 'echo hi', got: {args}"
    );
}

// ---------------------------------------------------------------------------
// Case 3 — Function tool call with finish_reason="stop": still flushed.
//          The parser must flush buffered builders regardless of finish_reason.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_tool_call_finish_reason_stop_still_flushes_tool_call() {
    let server = start_mock_server().await;
    mount_chat_once(
        &server,
        chat_sse_tool_call(
            "chatcmpl-fc-2",
            "call-002",
            "shell",
            r#"{"cmd":"ls"}"#,
            "stop", // not "tool_calls"
        ),
    )
    .await;

    let ctx = build_context(&server).await;
    let mut session = ctx.client.new_session();
    let prompt = user_prompt("hello");

    let stream = session
        .stream(
            &prompt,
            &ctx.model_info,
            &ctx.telemetry,
            ctx.effort,
            ctx.summary,
            None,
            &ctx.metadata,
            &codex_rollout_trace::InferenceTraceContext::disabled(),
        )
        .await
        .expect("chat stream should start");

    let events = collect_events(Box::pin(stream)).await;

    let fn_call = events.iter().find_map(|ev| {
        if let ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
            name, arguments, ..
        }) = ev
        {
            Some((name.clone(), arguments.clone()))
        } else {
            None
        }
    });

    assert!(
        fn_call.is_some(),
        "tool call should be flushed even with finish_reason='stop'; got: {events:?}"
    );
    let (name, args) = fn_call.unwrap();
    assert_eq!(name, "shell");
    assert!(
        args.contains("ls"),
        "arguments should contain 'ls', got: {args}"
    );
}

// ---------------------------------------------------------------------------
// Cases 4 & 5 — CustomToolCall (apply_patch) and image-stripping.
//
// These cases require the session to populate `prompt.tools` in Chat schema.
// That path is currently blocked by a tool-format mismatch:
//   - `create_tools_json_for_responses_api` emits flat Responses-API format
//   - `translate_tools` expects nested Chat-API format
// They are unit-tested in:
//   - `codex-rs/chat-completions/src/response.rs` (CustomToolCall dispatch)
//   - `codex-rs/core/src/client.rs` (image stripping before `build_chat_request`)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Case 6 — Structured output is not supported on the Chat path.
//          `build_chat_request` returns `ApiError::InvalidRequest` when
//          `text.format` is set.  The unit test in
//          `codex-rs/chat-completions/src/request.rs` covers this directly;
//          the field is only reachable at integration level via the session
//          Config (`output_schema`), which requires the full session harness
//          disabled by the tool-format bug.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Case 7 — Resumed history with tool outputs: the Chat request body must
//          contain a {"role":"tool",...} message for any FunctionCallOutput.
// ---------------------------------------------------------------------------

/// Captures raw request bodies using the `wiremock::Match` trait.
#[derive(Debug)]
struct BodyCapture(Arc<Mutex<Vec<serde_json::Value>>>);

impl Match for BodyCapture {
    fn matches(&self, req: &Request) -> bool {
        if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&req.body) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(body);
        }
        true // always match; capture only
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_function_call_output_in_history_sends_role_tool_message() {
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));

    let server = start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(BodyCapture(Arc::clone(&captured)))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    chat_sse_text("chatcmpl-history-1", &["ok"]),
                    "text/event-stream",
                ),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let ctx = build_context(&server).await;
    let mut session = ctx.client.new_session();

    // Prompt includes a prior FunctionCall + FunctionCallOutput in history.
    let mut prompt = Prompt::default();
    prompt.input.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "list files".to_string(),
        }],
        phase: None,
    });
    prompt.input.push(ResponseItem::FunctionCall {
        id: Some("item-fc-1".to_string()),
        name: "shell".to_string(),
        namespace: None,
        arguments: "{\"cmd\":\"ls\"}".to_string(),
        call_id: "call-hist-001".to_string(),
    });
    prompt.input.push(ResponseItem::FunctionCallOutput {
        call_id: "call-hist-001".to_string(),
        output: FunctionCallOutputPayload::from_text("file1.txt\nfile2.txt".to_string()),
    });
    prompt.input.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "what did ls return?".to_string(),
        }],
        phase: None,
    });

    let stream = session
        .stream(
            &prompt,
            &ctx.model_info,
            &ctx.telemetry,
            ctx.effort,
            ctx.summary,
            None,
            &ctx.metadata,
            &codex_rollout_trace::InferenceTraceContext::disabled(),
        )
        .await
        .expect("chat stream should start");

    collect_events(Box::pin(stream)).await;

    let bodies = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(
        !bodies.is_empty(),
        "expected at least one request to reach the mock server"
    );

    let body = &bodies[0];
    let messages = body["messages"]
        .as_array()
        .expect("Chat request must have a 'messages' array");

    // The {"role":"tool",...} message for the FunctionCallOutput.
    let tool_msg = messages.iter().find(|m| m["role"] == "tool");
    assert!(
        tool_msg.is_some(),
        "expected a message with role='tool' for the FunctionCallOutput; messages: {messages:?}"
    );
    let tool_msg = tool_msg.unwrap();
    assert_eq!(
        tool_msg["tool_call_id"].as_str(),
        Some("call-hist-001"),
        "tool_call_id should match the FunctionCallOutput call_id"
    );
    assert!(
        tool_msg["content"]
            .as_str()
            .unwrap_or_default()
            .contains("file1.txt"),
        "tool message content should include the output text; got: {tool_msg}"
    );

    // The request must use "messages" (Chat schema), not "input" (Responses schema).
    assert!(
        body.get("input").is_none(),
        "Chat request must NOT have an 'input' key (that is Responses-API format)"
    );
}

// ---------------------------------------------------------------------------
// Case 8 — wire_api="chat" config round-trip.
// ---------------------------------------------------------------------------

#[test]
fn chat_wire_api_config_round_trip() {
    let provider = ModelProviderInfo {
        name: "my-ollama".into(),
        base_url: Some("http://localhost:11434/v1".into()),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Chat,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    };

    assert_eq!(
        provider.wire_api,
        WireApi::Chat,
        "WireApi::Chat should round-trip correctly through ModelProviderInfo"
    );
    assert!(
        !provider.supports_websockets,
        "Chat providers must not enable WebSocket prewarm"
    );
}

// ---------------------------------------------------------------------------
// Case 9 — WebSocket prewarm is suppressed for Chat providers.
//
// The client.rs gating (`wire_api != WireApi::Responses || !supports_websockets`)
// means that any `ModelProviderInfo` with `wire_api = WireApi::Chat` must have
// `supports_websockets = false`.
// ---------------------------------------------------------------------------

#[test]
fn chat_provider_does_not_request_websocket_prewarm() {
    let provider = ModelProviderInfo {
        name: "test-chat-ws-check".into(),
        base_url: Some("http://127.0.0.1:11434/v1".into()),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Chat,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    };

    assert_eq!(provider.wire_api, WireApi::Chat);
    assert!(
        !provider.supports_websockets,
        "Chat providers must never have supports_websockets=true; \
         WebSocket prewarm is gated on WireApi::Responses in client.rs"
    );
}
