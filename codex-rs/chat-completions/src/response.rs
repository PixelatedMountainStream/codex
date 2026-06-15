//! Parse a Chat-Completions SSE byte stream into a stream of
//! [`ResponseEvent`] values following the Responses-API event model.

use std::collections::HashMap;

use bytes::Bytes;
use codex_api::ApiError;
use codex_api::ResponseEvent;
use codex_protocol::models::ContentItem;
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
    let mut text_accumulator = String::new();
    // Ordered list of builders — preserves arrival / emission order.
    let mut tool_call_builders: Vec<ToolCallBuilder> = Vec::new();
    // Maps SSE `index` field -> position of the MOST-RECENTLY-STARTED builder
    // at that index.  When a delta arrives with a new id we push a new builder;
    // when it carries no id (or the same id) we continue the mapped builder.
    let mut index_to_builder: HashMap<usize, usize> = HashMap::new();
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
                                &mut text_accumulator,
                                &mut tool_call_builders,
                                &mut index_to_builder,
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
                &mut text_accumulator,
                &mut tool_call_builders,
                &mut index_to_builder,
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

    // XML fallback: if no structured tool_calls were produced but the assistant
    // text contains qwen3-coder's native XML tool dialect, synthesize tool-call
    // events from the XML.
    //
    // Streaming caveat: text deltas were already sent to the client as
    // OutputTextDelta events before we reach this point.  We cannot retroactively
    // un-stream those deltas.  The synthesized ResponseItem::FunctionCall /
    // CustomToolCall emitted here is what actually triggers tool execution in
    // non-interactive `exec` mode, so the fallback is still effective even though
    // the raw XML text has already been streamed.
    let mut xml_fallback_fired = false;
    if tool_call_builders.is_empty() && !text_accumulator.is_empty() {
        let xml_calls = parse_xml_tool_calls(&text_accumulator);
        if !xml_calls.is_empty() {
            xml_fallback_fired = true;
            debug!(
                "chat-completions SSE: no structured tool_calls; falling back to XML parser, \
                 found {} call(s)",
                xml_calls.len()
            );
            // Suppress the raw XML text message.  The model sometimes emits
            // leading prose ("I'll create the file...") followed by the XML
            // block.  We drop all surrounding text here to avoid presenting
            // the XML or preamble as a visible assistant message — the tool
            // execution is what matters.  If callers need the preamble prose,
            // the text deltas have already been streamed.
            active_text_item_id = None;
            text_accumulator = String::new();

            for (i, call) in xml_calls.into_iter().enumerate() {
                let item_id = format!("xmlcall_{i}");
                let call_id = format!("xmlcall_{i}");

                match tool_map.get(&call.name) {
                    Some(ToolKind::Function { namespace }) => {
                        let (decoded_ns, decoded_name) = decode_name(&call.name);
                        let effective_ns =
                            namespace.clone().or(decoded_ns).filter(|s| !s.is_empty());
                        let call_item = ResponseItem::FunctionCall {
                            id: Some(item_id),
                            name: decoded_name,
                            namespace: effective_ns,
                            arguments: call.arguments,
                            call_id,
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
                        let input = extract_custom_input(&call.arguments);
                        let call_item = ResponseItem::CustomToolCall {
                            id: Some(item_id),
                            status: None,
                            call_id,
                            name: call.name,
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
                        // Unknown tool name — emit as FunctionCall (best-effort),
                        // matching the existing unknown-name path for structured calls.
                        debug!(
                            "chat-completions SSE XML fallback: unknown tool name '{}', \
                             emitting as FunctionCall",
                            call.name
                        );
                        let call_item = ResponseItem::FunctionCall {
                            id: Some(item_id),
                            name: call.name,
                            namespace: None,
                            arguments: call.arguments,
                            call_id,
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
        }
    }

    // Emit OutputItemDone for the text item if present (only when structured
    // tool calls fired OR no XML fallback was triggered — see fallback block
    // above which clears active_text_item_id when it takes over).
    if let Some(text_id) = active_text_item_id.take() {
        // Carry the accumulated streamed text into the finalized item so the
        // assistant message is persisted (deltas alone leave content empty).
        let content = if text_accumulator.is_empty() {
            vec![]
        } else {
            vec![ContentItem::OutputText {
                text: std::mem::take(&mut text_accumulator),
            }]
        };
        let done_item = ResponseItem::Message {
            id: Some(text_id),
            role: "assistant".into(),
            content,
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
    // end_turn = false when there are structured tool calls OR the XML fallback fired.
    let had_tool_calls = !tool_call_builders.is_empty() || xml_fallback_fired;

    // Emit tool call events in insertion order (Vec preserves arrival order).
    for builder in tool_call_builders.drain(..) {
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
#[allow(clippy::too_many_arguments)]
async fn process_chunk(
    data: &str,
    active_text_item_id: &mut Option<String>,
    text_accumulator: &mut String,
    tool_call_builders: &mut Vec<ToolCallBuilder>,
    index_to_builder: &mut HashMap<usize, usize>,
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
            text_accumulator.push_str(&content);
            tx.send(Ok(ResponseEvent::OutputTextDelta(content)))
                .await
                .map_err(|_| ApiError::Stream("channel closed".into()))?;
        }

        // Handle tool call fragments.
        //
        // Two streaming conventions must be handled:
        //
        // OpenAI: parallel calls use distinct `index` values (0, 1, 2, …).
        //   The first chunk for each call carries an `id`; continuation chunks
        //   carry the same `index` and no `id`.
        //
        // Ollama: ALL parallel calls reuse `index: 0` but carry distinct `id`s
        //   on every chunk (including argument-continuation chunks for the same
        //   call).
        //
        // Strategy: a NEW builder is started whenever a delta carries a non-empty
        // `id` that differs from the id of the builder currently mapped to that
        // `index`.  Continuation chunks (no id, or same id) are routed to the
        // most-recently-started builder for that `index`.
        if let Some(tc_deltas) = delta.tool_calls {
            for tc in tc_deltas {
                let index = tc.index.unwrap_or(0);
                let incoming_id = tc.id.as_deref().unwrap_or("").to_owned();

                // Determine whether this delta starts a new tool call.
                let is_new_call = if incoming_id.is_empty() {
                    // No id — this is a continuation; route to current builder.
                    false
                } else {
                    // Check if the id differs from the builder currently mapped
                    // to this index (or if no builder is mapped yet).
                    match index_to_builder.get(&index) {
                        Some(&pos) => tool_call_builders[pos].id != incoming_id,
                        None => true,
                    }
                };

                if is_new_call {
                    // Start a fresh builder and register it for this index.
                    let pos = tool_call_builders.len();
                    tool_call_builders.push(ToolCallBuilder {
                        id: incoming_id,
                        ..Default::default()
                    });
                    index_to_builder.insert(index, pos);
                }

                // Route to the current builder for this index (guaranteed to
                // exist now — either we just created it or it existed before).
                if let Some(&pos) = index_to_builder.get(&index) {
                    let builder = &mut tool_call_builders[pos];
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
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// XML tool-call fallback parser
// ---------------------------------------------------------------------------

/// A parsed `<function=NAME>...</function>` block extracted from raw assistant
/// text when the model emitted its native XML tool dialect instead of
/// structured `tool_calls`.
#[derive(Debug, PartialEq)]
struct ParsedXmlCall {
    /// Unencoded tool name as it appears in `<function=NAME>`.
    name: String,
    /// Parameters serialized as a JSON object string, e.g. `{"input":"..."}`.
    arguments: String,
}

/// Parse zero or more `<function=NAME>...<parameter=K>V</parameter>...</function>`
/// blocks from `text`.
///
/// Rules (matching observed qwen3-coder output):
/// - Opening tag: `<function=NAME>` — NAME ends at `>`.
/// - Parameter blocks: `<parameter=KEY>\nVALUE\n</parameter>` — exactly one
///   leading and one trailing newline around VALUE is stripped; internal
///   whitespace is preserved verbatim.
/// - Closing tag: `</function>`.
/// - Stray `</tool_call>` or other unmatched tags between/after blocks are
///   silently ignored.
/// - Prose before/after/between blocks is ignored (we only parse the function
///   blocks; see finalization logic for why we suppress all surrounding text).
/// - Never panics on malformed input; returns whatever could be parsed.
fn parse_xml_tool_calls(text: &str) -> Vec<ParsedXmlCall> {
    let mut calls = Vec::new();
    let mut remaining = text;

    while let Some(func_start) = remaining.find("<function=") {
        let after_open = &remaining[func_start + "<function=".len()..];

        // NAME ends at the first '>'.
        let Some(name_end) = after_open.find('>') else {
            break;
        };
        let name = after_open[..name_end].trim().to_owned();
        if name.is_empty() {
            // Malformed tag — skip past it.
            remaining = &remaining[func_start + 1..];
            continue;
        }

        let after_name = &after_open[name_end + 1..];

        // Find the matching </function>.
        let Some(func_body_end) = after_name.find("</function>") else {
            // No closing tag — stop parsing; do not emit partial call.
            break;
        };
        let body = &after_name[..func_body_end];

        // Advance remaining past this entire block.
        remaining = &after_name[func_body_end + "</function>".len()..];

        // Parse <parameter=KEY>VALUE</parameter> blocks from body.
        let mut params = serde_json::Map::new();
        let mut param_remaining = body;

        while let Some(p_start) = param_remaining.find("<parameter=") {
            let after_ptag = &param_remaining[p_start + "<parameter=".len()..];
            let Some(key_end) = after_ptag.find('>') else {
                break;
            };
            let key = after_ptag[..key_end].trim().to_owned();
            if key.is_empty() {
                param_remaining = &after_ptag[key_end + 1..];
                continue;
            }
            let after_key = &after_ptag[key_end + 1..];

            let Some(val_end) = after_key.find("</parameter>") else {
                break;
            };
            let raw_value = &after_key[..val_end];

            // Strip exactly one leading newline and one trailing newline.
            let value = raw_value
                .strip_prefix('\n')
                .unwrap_or(raw_value)
                .strip_suffix('\n')
                .unwrap_or(raw_value.strip_prefix('\n').unwrap_or(raw_value));

            params.insert(key, serde_json::Value::String(value.to_owned()));
            param_remaining = &after_key[val_end + "</parameter>".len()..];
        }

        // Serialize the parameters map as a JSON object.
        let arguments = serde_json::Value::Object(params).to_string();

        calls.push(ParsedXmlCall { name, arguments });
    }

    calls
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
        // The finalized item must carry the accumulated text, not an empty
        // content shell — otherwise the persisted assistant message is blank.
        match it.next() {
            Some(Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. }))) => {
                assert_eq!(content.len(), 1);
                assert!(matches!(
                    &content[0],
                    ContentItem::OutputText { text } if text == "Hello world"
                ));
            }
            other => panic!("expected OutputItemDone(Message), got {other:?}"),
        }
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
                "name": "my_func",
                "parameters": {}
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
                "name": "ollamatool",
                "parameters": {}
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

    // -----------------------------------------------------------------------
    // Parallel-tool-call aggregation tests
    // -----------------------------------------------------------------------

    /// Ollama parallel shape: two tool calls both carrying `index:0` but
    /// distinct ids.  Must produce TWO separate FunctionCall events with
    /// correct names and arguments (not concatenated).
    #[tokio::test]
    async fn ollama_parallel_same_index_distinct_ids() {
        let mut tool_map = ToolReverseMap::new();
        crate::tool_map::translate_tools(
            &[
                serde_json::json!({"type":"function","name":"get_goal","parameters":{}}),
                serde_json::json!({"type":"function","name":"apply_patch","parameters":{}}),
            ],
            &mut tool_map,
        )
        .unwrap();

        // Ollama emits both calls at index 0 with distinct ids in a single chunk.
        // NOTE: must be a single line — SSE splits on newlines.
        let chunk1 = r#"{"id":"o1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_meu10l5v","type":"function","function":{"name":"get_goal","arguments":"{}"}},{"index":0,"id":"call_jw1pnw92","type":"function","function":{"name":"apply_patch","arguments":"{\"input\":\"patch\"}"}}]},"finish_reason":null}]}"#;
        let chunk2 =
            r#"{"id":"o1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let body = build_sse(&[
            &format!("data: {chunk1}"),
            "",
            &format!("data: {chunk2}"),
            "",
            "data: [DONE]",
            "",
        ]);

        let events: Vec<_> = {
            let mut s = Box::pin(parse_chat_sse_stream(sse_bytes_owned(body), tool_map));
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };

        // Collect all OutputItemAdded FunctionCall names.
        let names: Vec<&str> = events
            .iter()
            .filter_map(|e| {
                if let Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
                    name, ..
                })) = e
                {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            names,
            vec!["get_goal", "apply_patch"],
            "expected two separate function calls, got: {names:?}"
        );
    }

    /// OpenAI streaming shape: one tool call where chunk0 carries id+name and
    /// subsequent chunks carry only arguments at the same index with no id.
    /// Must produce ONE FunctionCall with the full name (not duplicated) and
    /// correctly concatenated arguments.
    #[tokio::test]
    async fn openai_streaming_single_call_argument_continuation() {
        let mut tool_map = ToolReverseMap::new();
        crate::tool_map::translate_tools(
            &[serde_json::json!({"type":"function","name":"my_func","parameters":{}})],
            &mut tool_map,
        )
        .unwrap();

        // chunk0: id + name, empty arguments string.
        let chunk0 = r#"{"id":"s1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"tc_1","type":"function","function":{"name":"my_func","arguments":""}}]},"finish_reason":null}]}"#;
        // chunk1: no id, argument fragment 1.
        let chunk1 = r#"{"id":"s1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]},"finish_reason":null}]}"#;
        // chunk2: no id, argument fragment 2.
        let chunk2 = r#"{"id":"s1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":null}]}"#;
        let chunk3 =
            r#"{"id":"s1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let body = build_sse(&[
            &format!("data: {chunk0}"),
            "",
            &format!("data: {chunk1}"),
            "",
            &format!("data: {chunk2}"),
            "",
            &format!("data: {chunk3}"),
            "",
            "data: [DONE]",
            "",
        ]);

        let events: Vec<_> = {
            let mut s = Box::pin(parse_chat_sse_stream(sse_bytes_owned(body), tool_map));
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };

        let added_calls: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
                    name,
                    arguments,
                    ..
                })) = e
                {
                    Some((name.as_str(), arguments.as_str()))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            added_calls.len(),
            1,
            "expected exactly one function call, got {added_calls:?}"
        );
        assert_eq!(added_calls[0].0, "my_func", "name must not be duplicated");
        assert_eq!(
            added_calls[0].1, "{\"x\":1}",
            "arguments must be fully concatenated"
        );
    }

    /// OpenAI-style parallel calls with distinct indices 0 and 1.
    /// Must produce two correct separate function calls.
    #[tokio::test]
    async fn openai_parallel_distinct_indices() {
        let mut tool_map = ToolReverseMap::new();
        crate::tool_map::translate_tools(
            &[
                serde_json::json!({"type":"function","name":"func_a","parameters":{}}),
                serde_json::json!({"type":"function","name":"func_b","parameters":{}}),
            ],
            &mut tool_map,
        )
        .unwrap();

        // Each call gets its own index; ids appear only on the first chunk.
        // NOTE: must be a single line — SSE splits on newlines.
        let chunk0 = r#"{"id":"p1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"tc_a","type":"function","function":{"name":"func_a","arguments":""}},{"index":1,"id":"tc_b","type":"function","function":{"name":"func_b","arguments":""}}]},"finish_reason":null}]}"#;
        // Argument continuation for index 0, no id.
        let chunk1 = r#"{"id":"p1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":1}"}}]},"finish_reason":null}]}"#;
        // Argument continuation for index 1, no id.
        let chunk2 = r#"{"id":"p1","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"b\":2}"}}]},"finish_reason":null}]}"#;
        let chunk3 =
            r#"{"id":"p1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let body = build_sse(&[
            &format!("data: {chunk0}"),
            "",
            &format!("data: {chunk1}"),
            "",
            &format!("data: {chunk2}"),
            "",
            &format!("data: {chunk3}"),
            "",
            "data: [DONE]",
            "",
        ]);

        let events: Vec<_> = {
            let mut s = Box::pin(parse_chat_sse_stream(sse_bytes_owned(body), tool_map));
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };

        let added_calls: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
                    name,
                    arguments,
                    ..
                })) = e
                {
                    Some((name.as_str(), arguments.as_str()))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            added_calls.len(),
            2,
            "expected two function calls, got {added_calls:?}"
        );
        assert_eq!(added_calls[0], ("func_a", "{\"a\":1}"));
        assert_eq!(added_calls[1], ("func_b", "{\"b\":2}"));
    }

    // -----------------------------------------------------------------------
    // XML tool-call fallback tests
    // -----------------------------------------------------------------------

    /// parse_xml_tool_calls: two function blocks in one string -> two calls in
    /// order with correctly escaped JSON arguments.
    #[test]
    fn parse_xml_two_function_blocks() {
        let text = concat!(
            "<function=tool_a>\n",
            "<parameter=x>\nvalue_x\n</parameter>\n",
            "</function>\n",
            "<function=tool_b>\n",
            "<parameter=y>\nvalue_y\n</parameter>\n",
            "</function>\n",
        );
        let calls = parse_xml_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "tool_a");
        assert_eq!(calls[1].name, "tool_b");

        let a: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(a["x"], "value_x");
        let b: serde_json::Value = serde_json::from_str(&calls[1].arguments).unwrap();
        assert_eq!(b["y"], "value_y");
    }

    /// parse_xml_tool_calls: stray </tool_call> and leading prose are ignored.
    #[test]
    fn parse_xml_prose_and_stray_tags() {
        let text = concat!(
            "I'll create the file for you.\n",
            "<function=apply_patch>\n",
            "<parameter=input>\n",
            "*** Begin Patch\n",
            "*** Add File: hello.txt\n",
            "+hello world\n",
            "*** End Patch\n",
            "</parameter>\n",
            "</function>\n",
            "</tool_call>\n",
        );
        let calls = parse_xml_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apply_patch");

        let v: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        let input = v["input"].as_str().unwrap();
        assert!(
            input.contains("*** Begin Patch"),
            "patch envelope must be preserved"
        );
        assert!(
            input.contains("+hello world"),
            "patch content must be preserved"
        );
        // Newlines within the value must be preserved.
        assert!(input.contains('\n'), "internal newlines must be kept");
    }

    /// SSE stream with no structured tool_calls but XML in assistant text ->
    /// synthesizes FunctionCall, does NOT emit the raw XML as a message item.
    #[tokio::test]
    async fn xml_fallback_synthesizes_function_call() {
        let mut tool_map = ToolReverseMap::new();
        crate::tool_map::translate_tools(
            &[serde_json::json!({
                "type": "function",
                "name": "apply_patch",
                "parameters": {}
            })],
            &mut tool_map,
        )
        .unwrap();

        // The assistant streams the XML as plain content (no tool_calls field).
        let xml_content = concat!(
            "<function=apply_patch>",
            "<parameter=input>",
            "*** Begin Patch\n*** Add File: hello.txt\n+hello world\n*** End Patch",
            "</parameter>",
            "</function>",
            "</tool_call>",
        );
        // SSE chunk with content, no tool_calls.
        let chunk1 = format!(
            r#"{{"id":"x1","choices":[{{"index":0,"delta":{{"content":"{}"}},"finish_reason":null}}]}}"#,
            xml_content
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
        );
        let chunk2 = r#"{"id":"x1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;

        let body = build_sse(&[
            &format!("data: {chunk1}"),
            "",
            &format!("data: {chunk2}"),
            "",
            "data: [DONE]",
            "",
        ]);

        let events: Vec<_> = {
            let mut s = Box::pin(parse_chat_sse_stream(sse_bytes_owned(body), tool_map));
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };

        // Must have exactly one OutputItemAdded for a FunctionCall named apply_patch.
        let function_calls: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
                    name,
                    arguments,
                    ..
                })) = e
                {
                    Some((name.as_str(), arguments.as_str()))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            function_calls.len(),
            1,
            "expected exactly one synthesized FunctionCall, got: {events:?}"
        );
        assert_eq!(function_calls[0].0, "apply_patch");

        // arguments must be valid JSON with `input` key preserving the patch text.
        let args: serde_json::Value =
            serde_json::from_str(function_calls[0].1).expect("arguments must be valid JSON");
        let input = args["input"].as_str().expect("input must be a string");
        assert!(
            input.contains("*** Begin Patch"),
            "patch envelope must be in input"
        );
        assert!(
            input.contains("+hello world"),
            "patch content must be in input"
        );

        // Raw XML must NOT appear as a Message item.
        let xml_in_message = events.iter().any(|e| match e {
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. })) => {
                content.iter().any(|c| {
                    if let ContentItem::OutputText { text } = c {
                        text.contains("<function=")
                    } else {
                        false
                    }
                })
            }
            _ => false,
        });
        assert!(
            !xml_in_message,
            "raw XML must not be emitted as a message item"
        );
    }

    /// SSE stream WITH structured tool_call AND incidental text -> XML fallback
    /// does NOT fire; only the structured call is emitted.
    #[tokio::test]
    async fn structured_tool_call_suppresses_xml_fallback() {
        let mut tool_map = ToolReverseMap::new();
        crate::tool_map::translate_tools(
            &[serde_json::json!({
                "type": "function",
                "name": "my_func",
                "parameters": {}
            })],
            &mut tool_map,
        )
        .unwrap();

        // Text chunk with some XML-like content streamed alongside a real tool call.
        let chunk_text = r#"{"id":"s1","choices":[{"index":0,"delta":{"content":"<function=my_func><parameter=x>v</parameter></function>"},"finish_reason":null}]}"#;
        let chunk_tc = r#"{"id":"s1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"tc_real","type":"function","function":{"name":"my_func","arguments":"{\"x\":1}"}}]},"finish_reason":null}]}"#;
        let chunk_done =
            r#"{"id":"s1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

        let body = build_sse(&[
            &format!("data: {chunk_text}"),
            "",
            &format!("data: {chunk_tc}"),
            "",
            &format!("data: {chunk_done}"),
            "",
            "data: [DONE]",
            "",
        ]);

        let events: Vec<_> = {
            let mut s = Box::pin(parse_chat_sse_stream(sse_bytes_owned(body), tool_map));
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };

        let function_calls: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let Ok(ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
                    name,
                    call_id,
                    ..
                })) = e
                {
                    Some((name.as_str(), call_id.as_str()))
                } else {
                    None
                }
            })
            .collect();

        // Exactly one call from the structured path, not from XML fallback.
        assert_eq!(
            function_calls.len(),
            1,
            "expected exactly one function call, got: {events:?}"
        );
        // The structured call carries the real id "tc_real", not an xmlcall_ id.
        assert_eq!(
            function_calls[0].1, "tc_real",
            "expected structured call id, not XML fallback id"
        );
    }
}
