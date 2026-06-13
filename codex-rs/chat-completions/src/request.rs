//! Build a Chat Completions JSON request body from a [`ResponsesApiRequest`].

use codex_api::ApiError;
use codex_api::ResponsesApiRequest;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use serde_json::json;

use crate::tool_map::ToolReverseMap;
use crate::tool_map::encode_name;
use crate::tool_map::translate_tools;

/// Build a Chat Completions JSON request body from a Responses-API request.
///
/// # Errors
///
/// Returns [`ApiError::InvalidRequest`] when:
/// - `text.format` (structured output) is requested — not supported.
/// - A tool definition is malformed or contains a name collision.
pub fn build_chat_request(
    req: &ResponsesApiRequest,
    tool_map: &mut ToolReverseMap,
) -> Result<Value, ApiError> {
    // Fail-fast: structured output is not supported over the chat wire format.
    if req.text.as_ref().and_then(|t| t.format.as_ref()).is_some() {
        return Err(ApiError::InvalidRequest {
            message: "structured output (text.format) is not supported for wire_api = chat".into(),
        });
    }

    // Translate tools and populate the reverse map.
    let chat_tools = translate_tools(&req.tools, tool_map)?;

    // Build messages.
    let mut messages: Vec<Value> = Vec::new();

    // System message from instructions.
    if !req.instructions.is_empty() {
        messages.push(json!({
            "role": "system",
            "content": req.instructions
        }));
    }

    for item in &req.input {
        match item {
            ResponseItem::Message { role, content, .. } => {
                let text = join_content_text(content);
                messages.push(json!({ "role": role, "content": text }));
            }

            ResponseItem::FunctionCall {
                call_id,
                namespace,
                name,
                arguments,
                ..
            } => {
                let encoded = encode_name(namespace.as_deref(), name);
                messages.push(json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": encoded,
                            "arguments": arguments
                        }
                    }]
                }));
            }

            ResponseItem::FunctionCallOutput { call_id, output } => {
                let content = output.body.to_text().unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content
                }));
            }

            ResponseItem::CustomToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                let arguments = json!({"input": input}).to_string();
                messages.push(json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments
                        }
                    }]
                }));
            }

            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                let content = output.body.to_text().unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content
                }));
            }

            // Reasoning items must not leak encrypted_content — skip entirely.
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::Other => {}
        }
    }

    let mut body = json!({
        "model": req.model,
        "stream": true,
        "messages": messages,
    });

    if !chat_tools.is_empty() {
        body["tools"] = Value::Array(chat_tools);
    }

    if !req.tool_choice.is_empty() {
        body["tool_choice"] = Value::String(req.tool_choice.clone());
    }

    if let Some(tier) = &req.service_tier {
        body["service_tier"] = Value::String(tier.clone());
    }

    Ok(body)
}

/// Join the text parts of a content item slice into a single string.
///
/// Image parts are expected to have been pre-normalized to placeholder text
/// by upstream callers; we simply join whatever text we find.
fn join_content_text(content: &[ContentItem]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            ContentItem::InputImage { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;

    fn minimal_req(input: Vec<ResponseItem>) -> ResponsesApiRequest {
        ResponsesApiRequest {
            model: "gpt-4o".into(),
            instructions: String::new(),
            input,
            tools: vec![],
            tool_choice: String::new(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            client_metadata: None,
        }
    }

    #[test]
    fn builds_minimal_request() {
        let req = minimal_req(vec![ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "Hello".into(),
            }],
            phase: None,
        }]);
        let mut map = ToolReverseMap::new();
        let body = build_chat_request(&req, &mut map).expect("should succeed");
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "Hello");
    }

    #[test]
    fn prepends_system_message() {
        let mut req = minimal_req(vec![]);
        req.instructions = "You are helpful.".into();
        let mut map = ToolReverseMap::new();
        let body = build_chat_request(&req, &mut map).expect("should succeed");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are helpful.");
    }

    #[test]
    fn no_text_format_does_not_fail() {
        // When text.format is None, the request should succeed even when
        // a TextControls is provided. (The fail-fast is only triggered when
        // format is Some; TextFormat is not publicly constructible from
        // outside codex-api, so we verify the happy path here.)
        let mut req = minimal_req(vec![]);
        req.text = Some(codex_api::TextControls {
            verbosity: None,
            format: None,
        });
        let mut map = ToolReverseMap::new();
        let result = build_chat_request(&req, &mut map);
        assert!(result.is_ok());
    }

    #[test]
    fn translates_function_call_output() {
        let item = ResponseItem::FunctionCallOutput {
            call_id: "call_1".into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("result text".into()),
                success: None,
            },
        };
        let req = minimal_req(vec![item]);
        let mut map = ToolReverseMap::new();
        let body = build_chat_request(&req, &mut map).expect("should succeed");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "call_1");
        assert_eq!(msgs[0]["content"], "result text");
    }

    #[test]
    fn translates_function_call_with_namespace() {
        let item = ResponseItem::FunctionCall {
            id: None,
            name: "my_fn".into(),
            namespace: Some("myns".into()),
            arguments: "{}".into(),
            call_id: "call_ns_1".into(),
        };
        let req = minimal_req(vec![item]);
        let mut map = ToolReverseMap::new();
        let body = build_chat_request(&req, &mut map).expect("should succeed");
        let msgs = body["messages"].as_array().unwrap();
        let tc = &msgs[0]["tool_calls"][0];
        assert_eq!(tc["function"]["name"], "myns__NS__my_fn");
    }

    #[test]
    fn service_tier_is_included_when_set() {
        let mut req = minimal_req(vec![]);
        req.service_tier = Some("auto".into());
        let mut map = ToolReverseMap::new();
        let body = build_chat_request(&req, &mut map).expect("should succeed");
        assert_eq!(body["service_tier"], "auto");
    }

    #[test]
    fn service_tier_absent_when_none() {
        let req = minimal_req(vec![]);
        let mut map = ToolReverseMap::new();
        let body = build_chat_request(&req, &mut map).expect("should succeed");
        assert!(body.get("service_tier").is_none());
    }
}
