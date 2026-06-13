//! Parse a Chat-Completions SSE byte stream into a stream of
//! [`ResponseEvent`] values following the Responses-API event model.

use std::collections::HashMap;

use bytes::Bytes;
use codex_api::ApiError;
use codex_api::ResponseEvent;
use codex_protocol::models::ResponseItem;
use futures::Stream;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::trace;
use uuid::Uuid;

use crate::tool_map::ToolKind;
use crate::tool_map::ToolReverseMap;
use crate::tool_map::decode_name;

// ---------------------------------------------------------------------------
// Chunk deserialisation types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ChatChunk {
    id: Option<String>,
    choices: Option<Vec<ChatChoice>>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    delta: Option<ChatDelta>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatDelta {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: Option<usize>,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

// ---------------------------------------------------------------------------
// Accumulated tool-call builder
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a raw Chat-Completions SSE byte stream into a stream of
/// [`ResponseEvent`] items.
///
/// The returned stream emits events in the Responses-API ordering:
/// 1. `Created`
/// 2. For text: `OutputItemAdded(Message)` then one or more `OutputTextDelta`
/// 3. `OutputItemDone` for any buffered items (text + tool calls)
/// 4. `Completed`
///
/// The generic bound deliberately avoids any specific transport error type so
/// this crate has no dependency on `reqwest`.
pub fn parse_chat_sse_stream<S, E>(
    byte_stream: S,
    tool_map: ToolReverseMap,
) -> impl Stream<Item = Result<ResponseEvent, ApiError>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(256);
    tokio::spawn(drive_stream(byte_stream, tool_map, tx));
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

async fn drive_stream<S, E>(
    byte_stream: S,
    tool_map: ToolReverseMap,
    tx: mpsc::Sender<Result<ResponseEvent, ApiError>>,
) where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + 'static,
{
    // State
    let mut active_text_item_id: Option<String> = None;
    let mut tool_call_builders: HashMap<usize, ToolCallBuilder> = HashMap::new();
    let mut stream_id: Option<String> = None;
    let mut finish_reason_seen: Option<String> = None;

    // Emit Created first.
    if tx.send(Ok(ResponseEvent::Created)).await.is_err() {
        return;
    }

    // We accumulate partial SSE lines across Bytes chunks.
    let mut line_buf = String::new();
    // The current SSE data line being assembled (across multiple "data: " lines).
    let mut data_buf = String::new();

    let mut stream = Box::pin(byte_stream);

    loop {
        let maybe_chunk = stream.next().await;

        match maybe_chunk {
            None => {
                // Stream ended — flush any pending tool calls even without
                // finish_reason.
                break;
            }
            Some(Err(e)) => {
                let _ = tx
                    .send(Err(ApiError::Stream(format!("byte stream error: {e}"))))
                    .await;
                return;
            }
            Some(Ok(bytes)) => {
                let text = match std::str::from_utf8(&bytes) {
                    Ok(s) => s.to_owned(),
                    Err(e) => {
                        let _ = tx
                            .send(Err(ApiError::Stream(format!("UTF-8 error: {e}"))))
                            .await;
                        return;
                    }
                };

                line_buf.push_str(&text);

                // Process complete lines.
                while let Some(newline_pos) = line_buf.find('\n') {
                    let line = line_buf[..newline_pos].trim_end_matches('\r').to_owned();
                    line_buf = line_buf[newline_pos + 1..].to_owned();

                    if line.is_empty() {
                        // Blank line = dispatch the buffered data.
                        if !data_buf.is_empty() {
                            let data = std::mem::take(&mut data_buf);
                            if data.trim() == "[DONE]" {
                                // SSE stream finished — proceed to flush.
                                break;
                            }
                            if let Err(e) = process_chunk(
                                &data,
                                &mut active_text_item_id,
                                &mut tool_call_builders,
                                &mut stream_id,
                                &mut finish_reason_seen,
                                &tx,
                            )
                            .await
                            {
                                let _ = tx.send(Err(e)).await;
                                return;
                            }
                        }
                        continue;
                    }

                    // Accumulate data lines.
                    if let Some(payload) = line.strip_prefix("data:") {
                        let payload = payload.trim_start();
                        if !data_buf.is_empty() {
                            data_buf.push('\n');
                        }
                        data_buf.push_str(payload);
                    }
                    // event: / id: / comment lines are ignored.
                }

                // Check if we hit [DONE] (it ends a data block and sets
                // finish, so after breaking from the inner loop we flush).
                if finish_reason_seen.is_some() || data_buf.trim() == "[DONE]" {
                    // The break above only exits the while; continue processing.
                }
            }
        }

        // Flush when finish_reason seen and no remaining partial data.
        if finish_reason_seen.is_some() && data_buf.is_empty() && line_buf.is_empty() {
            break;
        }
    }

    // Flush remaining data_buf if any.
    if !data_buf.is_empty() {
        let data = std::mem::take(&mut data_buf);
        if data.trim() != "[DONE]"
            && let Err(e) = process_chunk(
                &data,
                &mut active_text_item_id,
                &mut tool_call_builders,
                &mut stream_id,
                &mut finish_reason_seen,
                &tx,
            )
            .await
        {
            let _ = tx.send(Err(e)).await;
            return;
        }
    }

    // Emit OutputItemDone for the text item if present.
    if let Some(text_id) = active_text_item_id.take() {
        let done_item = ResponseItem::Message {
            id: Some(text_id),
            role: "assistant".into(),
            content: vec![],
            phase: None,
        };
        if tx
            .send(Ok(ResponseEvent::OutputItemDone(done_item)))
            .await
            .is_err()
        {
            return;
        }
    }

    // Determine end_turn BEFORE draining builders.
    // end_turn = false means tool calls are expected; true means the model ended its turn.
    let had_tool_calls = !tool_call_builders.is_empty();

    // Emit tool call events sorted by index for deterministic ordering.
    let mut indices: Vec<usize> = tool_call_builders.keys().copied().collect();
    indices.sort_unstable();

    for idx in indices {
        let builder = match tool_call_builders.remove(&idx) {
            Some(b) => b,
            None => continue,
        };

        let item_id = Uuid::new_v4().to_string();

        match tool_map.get(&builder.name) {
            Some(ToolKind::Function { namespace }) => {
                let (decoded_ns, decoded_name) = decode_name(&builder.name);
                let effective_ns = namespace.clone().or(decoded_ns).filter(|s| !s.is_empty());
                let call_item = ResponseItem::FunctionCall {
                    id: Some(item_id.clone()),
                    name: decoded_name,
                    namespace: effective_ns,
                    arguments: builder.arguments,
                    call_id: builder.id,
                };
                if tx
                    .send(Ok(ResponseEvent::OutputItemAdded(call_item.clone())))
                    .await
                    .is_err()
                {
                    return;
                }
                if tx
                    .send(Ok(ResponseEvent::OutputItemDone(call_item)))
                    .await
                    .is_err()
                {
                    return;
                }
            }

            Some(ToolKind::Custom) => {
                // Extract `input` from `{"input": "..."}` arguments JSON.
                let input = extract_custom_input(&builder.arguments);
                let call_item = ResponseItem::CustomToolCall {
                    id: Some(item_id.clone()),
                    status: None,
                    call_id: builder.id,
                    name: builder.name.clone(),
                    input,
                };
                if tx
                    .send(Ok(ResponseEvent::OutputItemAdded(call_item.clone())))
                    .await
                    .is_err()
                {
                    return;
                }
                if tx
                    .send(Ok(ResponseEvent::OutputItemDone(call_item)))
                    .await
                    .is_err()
                {
                    return;
                }
            }

            None => {
                // Unknown tool name — treat as function call (best-effort).
                debug!(
                    "chat-completions SSE: unknown tool name '{}', emitting as FunctionCall",
                    builder.name
                );
                let call_item = ResponseItem::FunctionCall {
                    id: Some(item_id.clone()),
                    name: builder.name,
                    namespace: None,
                    arguments: builder.arguments,
                    call_id: builder.id,
                };
                if tx
                    .send(Ok(ResponseEvent::OutputItemAdded(call_item.clone())))
                    .await
                    .is_err()
                {
                    return;
                }
                if tx
                    .send(Ok(ResponseEvent::OutputItemDone(call_item)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }

    let response_id = stream_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    // end_turn is true only when there were no tool calls.
    let end_turn = Some(!had_tool_calls);
    let _ = tx
        .send(Ok(ResponseEvent::Completed {
            response_id,
            token_usage: None,
            end_turn,
        }))
        .await;
}

/// Process a single JSON data payload from the SSE stream.
async fn process_chunk(
    data: &str,
    active_text_item_id: &mut Option<String>,
    tool_call_builders: &mut HashMap<usize, ToolCallBuilder>,
    stream_id: &mut Option<String>,
    finish_reason_seen: &mut Option<String>,
    tx: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
) -> Result<(), ApiError> {
    trace!("chat SSE chunk: {data}");

    let chunk: ChatChunk = match serde_json::from_str(data) {
        Ok(c) => c,
        Err(e) => {
            debug!("chat SSE: failed to parse chunk: {e}, data: {data}");
            return Ok(());
        }
    };

    if let Some(id) = chunk.id
        && stream_id.is_none()
    {
        *stream_id = Some(id);
    }

    let choices = match chunk.choices {
        Some(c) => c,
        None => return Ok(()),
    };

    for choice in choices {
        // Record finish_reason when present.
        if let Some(reason) = &choice.finish_reason
            && !reason.is_empty()
        {
            *finish_reason_seen = Some(reason.clone());
        }

        let delta = match choice.delta {
            Some(d) => d,
            None => continue,
        };

        // Handle text content.
        if let Some(content) = delta.content
            && !content.is_empty()
        {
            if active_text_item_id.is_none() {
                // First text chunk — emit OutputItemAdded before the delta.
                let text_id = Uuid::new_v4().to_string();
                let added_item = ResponseItem::Message {
                    id: Some(text_id.clone()),
                    role: "assistant".into(),
                    content: vec![],
                    phase: None,
                };
                tx.send(Ok(ResponseEvent::OutputItemAdded(added_item)))
                    .await
                    .map_err(|_| ApiError::Stream("channel closed".into()))?;
                *active_text_item_id = Some(text_id);
            }
            tx.send(Ok(ResponseEvent::OutputTextDelta(content)))
                .await
                .map_err(|_| ApiError::Stream("channel closed".into()))?;
        }

        // Handle tool call fragments.
        if let Some(tc_deltas) = delta.tool_calls {
            for tc in tc_deltas {
                let index = tc.index.unwrap_or(0);
                let builder = tool_call_builders.entry(index).or_default();

                if let Some(id) = tc.id
                    && builder.id.is_empty()
                {
                    builder.id = id;
                }
                if let Some(func) = tc.function {
                    if let Some(name) = func.name {
                        builder.name.push_str(&name);
                    }
                    if let Some(args) = func.arguments {
                        builder.arguments.push_str(&args);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Extract the `input` field value from a `{"input": "..."}` JSON string.
/// Falls back to the raw `arguments` string on parse failure.
fn extract_custom_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|v| v.get("input").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| arguments.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream;

    /// Build an SSE byte stream from a list of lines (each terminated by `\n`).
    fn sse_bytes_owned(
        body: String,
    ) -> impl Stream<Item = Result<Bytes, std::io::Error>> + 'static {
        stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(body))])
    }

    fn build_sse(lines: &[&str]) -> String {
        let mut body = String::new();
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        body
    }

    async fn collect_events(
        s: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    ) -> Vec<Result<ResponseEvent, ApiError>> {
        let map = ToolReverseMap::new();
        let mut stream = Box::pin(parse_chat_sse_stream(s, map));
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            events.push(ev);
        }
        events
    }

    fn chunk_json(id: &str, content: Option<&str>, finish: Option<&str>) -> String {
        let content_val = match content {
            Some(c) => format!("\"{}\"", c.replace('"', "\\\"")),
            None => "null".into(),
        };
        let finish_val = match finish {
            Some(f) => format!("\"{f}\""),
            None => "null".into(),
        };
        format!(
            r#"{{"id":"{id}","choices":[{{"index":0,"delta":{{"role":"assistant","content":{content_val}}},"finish_reason":{finish_val}}}]}}"#
        )
    }

    #[tokio::test]
    async fn plain_text_stream() {
        let c1 = chunk_json("stream-1", Some("Hello"), None);
        let c2 = chunk_json("stream-1", Some(" world"), None);
        let c3 = chunk_json("stream-1", None, Some("stop"));

        let body = build_sse(&[
            &format!("data: {c1}"),
            "",
            &format!("data: {c2}"),
            "",
            &format!("data: {c3}"),
            "",
            "data: [DONE]",
            "",
        ]);
        let stream = sse_bytes_owned(body);
        let events = collect_events(stream).await;

        // Expected: Created, OutputItemAdded, OutputTextDelta x2, OutputItemDone, Completed
        let mut it = events.iter();

        assert!(matches!(it.next(), Some(Ok(ResponseEvent::Created))));
        assert!(matches!(
            it.next(),
            Some(Ok(ResponseEvent::OutputItemAdded(
                ResponseItem::Message { .. }
            )))
        ));
        assert!(matches!(
            it.next(),
            Some(Ok(ResponseEvent::OutputTextDelta(s))) if s == "Hello"
        ));
        assert!(matches!(
            it.next(),
            Some(Ok(ResponseEvent::OutputTextDelta(s))) if s == " world"
        ));
        assert!(matches!(
            it.next(),
            Some(Ok(ResponseEvent::OutputItemDone(
                ResponseItem::Message { .. }
            )))
        ));
        let completed = it.next().unwrap();
        assert!(matches!(
            completed,
            Ok(ResponseEvent::Completed {
                end_turn: Some(true),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn function_tool_call_finish_reason_tool_calls() {
        // Build a tool map with a function tool registered.
        let mut tool_map = ToolReverseMap::new();
        crate::tool_map::translate_tools(
            &[serde_json::json!({
                "type": "function",
                "function": {
                    "name": "my_func",
                    "parameters": {}
                }
            })],
            &mut tool_map,
        )
        .unwrap();

        let chunk1 = r#"{"id":"s1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"tc_1","type":"function","function":{"name":"my_func","arguments":""}}]},"finish_reason":null}]}"#;
        let chunk2 = r#"{"id":"s1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":1}"}}]},"finish_reason":null}]}"#;
        let chunk3 =
            r#"{"id":"s1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let body = build_sse(&[
            &format!("data: {chunk1}"),
            "",
            &format!("data: {chunk2}"),
            "",
            &format!("data: {chunk3}"),
            "",
            "data: [DONE]",
            "",
        ]);
        let stream = sse_bytes_owned(body);

        let events: Vec<_> = {
            let mut s = Box::pin(parse_chat_sse_stream(stream, tool_map));
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };

        let mut it = events.iter();
        assert!(matches!(it.next(), Some(Ok(ResponseEvent::Created))));
        assert!(matches!(
            it.next(),
            Some(Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
                name,
                ..
            }))) if name == "my_func"
        ));
        assert!(matches!(
            it.next(),
            Some(Ok(ResponseEvent::OutputItemDone(
                ResponseItem::FunctionCall { .. }
            )))
        ));
        // Completed with end_turn=false because there were tool calls.
        assert!(matches!(
            it.next(),
            Some(Ok(ResponseEvent::Completed {
                end_turn: Some(false),
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn function_tool_call_finish_reason_stop_ollama() {
        // Ollama sends "stop" even when tool calls are buffered.
        let mut tool_map = ToolReverseMap::new();
        crate::tool_map::translate_tools(
            &[serde_json::json!({
                "type": "function",
                "function": { "name": "ollamatool", "parameters": {} }
            })],
            &mut tool_map,
        )
        .unwrap();

        let chunk1 = r#"{"id":"o1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"tc_o1","type":"function","function":{"name":"ollamatool","arguments":""}}]},"finish_reason":null}]}"#;
        let chunk2 = r#"{"id":"o1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]},"finish_reason":null}]}"#;
        // Ollama sends "stop" instead of "tool_calls".
        let chunk3 = r#"{"id":"o1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;

        let body = build_sse(&[
            &format!("data: {chunk1}"),
            "",
            &format!("data: {chunk2}"),
            "",
            &format!("data: {chunk3}"),
            "",
            "data: [DONE]",
            "",
        ]);
        let stream = sse_bytes_owned(body);

        let events: Vec<_> = {
            let mut s = Box::pin(parse_chat_sse_stream(stream, tool_map));
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };

        // Should still flush tool call builders even though finish_reason = "stop".
        let has_added = events.iter().any(|e| {
            matches!(
                e,
                Ok(ResponseEvent::OutputItemAdded(
                    ResponseItem::FunctionCall { .. }
                ))
            )
        });
        assert!(has_added, "expected OutputItemAdded for tool call");
    }

    #[tokio::test]
    async fn custom_tool_call() {
        let mut tool_map = ToolReverseMap::new();
        crate::tool_map::translate_tools(
            &[serde_json::json!({
                "type": "custom",
                "name": "my_custom",
                "description": "A custom tool"
            })],
            &mut tool_map,
        )
        .unwrap();

        let chunk1 = r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"tc_c1","type":"function","function":{"name":"my_custom","arguments":""}}]},"finish_reason":null}]}"#;
        let chunk2 = r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello world\"}"}}]},"finish_reason":null}]}"#;
        let chunk3 =
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let body = build_sse(&[
            &format!("data: {chunk1}"),
            "",
            &format!("data: {chunk2}"),
            "",
            &format!("data: {chunk3}"),
            "",
            "data: [DONE]",
            "",
        ]);
        let stream = sse_bytes_owned(body);

        let events: Vec<_> = {
            let mut s = Box::pin(parse_chat_sse_stream(stream, tool_map));
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };

        let added = events.iter().find(|e| {
            matches!(
                e,
                Ok(ResponseEvent::OutputItemAdded(
                    ResponseItem::CustomToolCall { .. }
                ))
            )
        });
        assert!(
            added.is_some(),
            "expected OutputItemAdded for custom tool call"
        );

        if let Some(Ok(ResponseEvent::OutputItemAdded(ResponseItem::CustomToolCall {
            input,
            name,
            ..
        }))) = added
        {
            assert_eq!(name, "my_custom");
            assert_eq!(input, "hello world");
        }
    }
}
