//! Tool reverse map: translates Responses-API tool definitions into Chat
//! Completions function definitions and tracks how to reverse-map the encoded
//! names back to their original protocol types.

use std::collections::HashMap;

use codex_api::ApiError;
use serde_json::Value;
use tracing::warn;

/// Separator used to embed the namespace into an encoded function name.
const NS_SEP: &str = "__NS__";

/// The kind of tool that an encoded function name maps back to.
#[derive(Debug, Clone)]
pub enum ToolKind {
    /// A normal `function` tool, optionally scoped under a namespace.
    Function { namespace: Option<String> },
    /// A custom (freeform) tool whose input is wrapped in `{"input": "..."}`.
    Custom,
}

/// Maps encoded Chat-Completions function names back to their [`ToolKind`].
///
/// Callers must hold a `ToolReverseMap` alongside the request so that the SSE
/// response parser can translate tool-call results back into Responses-API
/// items.
#[derive(Debug, Default)]
pub struct ToolReverseMap(HashMap<String, ToolKind>);

impl ToolReverseMap {
    /// Create an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up an encoded function name.
    pub fn get(&self, encoded_name: &str) -> Option<&ToolKind> {
        self.0.get(encoded_name)
    }

    /// Encode a (namespace, name) pair into a single Chat-Completions function
    /// name, register it in the map, and return the encoded name.
    ///
    /// Returns `Err` on collision with an already-registered name.
    fn register_function(
        &mut self,
        namespace: Option<&str>,
        name: &str,
        kind: ToolKind,
    ) -> Result<String, ApiError> {
        let encoded = encode_name(namespace, name);
        if self.0.contains_key(&encoded) {
            return Err(ApiError::InvalidRequest {
                message: format!(
                    "tool name collision: encoded name '{encoded}' is already registered"
                ),
            });
        }
        self.0.insert(encoded.clone(), kind);
        Ok(encoded)
    }
}

/// Encode a namespace + name pair into a single string.
pub fn encode_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{ns}{NS_SEP}{name}"),
        _ => name.to_owned(),
    }
}

/// Decode an encoded function name back to (namespace, plain_name).
pub fn decode_name(encoded: &str) -> (Option<String>, String) {
    if let Some(idx) = encoded.find(NS_SEP) {
        let ns = &encoded[..idx];
        let name = &encoded[idx + NS_SEP.len()..];
        (Some(ns.to_owned()), name.to_owned())
    } else {
        (None, encoded.to_owned())
    }
}

/// Convert a slice of Responses-API tool definitions into Chat-Completions
/// `{"type":"function",...}` objects, populating `map` for later reverse
/// lookup.
///
/// Tool types that cannot be represented as Chat-Completions functions
/// (`namespace`, `tool_search`, `image_generation`, `web_search`) are silently
/// skipped with a warning.
pub fn translate_tools(tools: &[Value], map: &mut ToolReverseMap) -> Result<Vec<Value>, ApiError> {
    let mut out = Vec::new();

    for tool in tools {
        let kind = tool.get("type").and_then(Value::as_str).unwrap_or_default();

        match kind {
            "function" => {
                let func = tool
                    .get("function")
                    .ok_or_else(|| ApiError::InvalidRequest {
                        message: "function tool missing 'function' field".into(),
                    })?;
                let name = func.get("name").and_then(Value::as_str).ok_or_else(|| {
                    ApiError::InvalidRequest {
                        message: "function tool missing 'function.name'".into(),
                    }
                })?;
                let namespace = func
                    .get("namespace")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                let encoded = map.register_function(
                    namespace,
                    name,
                    ToolKind::Function {
                        namespace: namespace.map(str::to_owned),
                    },
                )?;

                let mut function_obj = serde_json::Map::new();
                function_obj.insert("name".into(), Value::String(encoded));
                if let Some(desc) = func.get("description") {
                    function_obj.insert("description".into(), desc.clone());
                }
                if let Some(params) = func.get("parameters") {
                    function_obj.insert("parameters".into(), params.clone());
                }

                out.push(serde_json::json!({
                    "type": "function",
                    "function": function_obj
                }));
            }

            "custom" => {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ApiError::InvalidRequest {
                        message: "custom tool missing 'name' field".into(),
                    })?
                    .to_owned();

                let desc_base = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();

                // Embed the format definition (if present) into the
                // description so the model knows the expected schema.
                let format_def = tool
                    .get("format")
                    .and_then(|f| f.get("definition"))
                    .map(|d| {
                        if d.is_string() {
                            d.as_str().unwrap_or_default().to_owned()
                        } else {
                            d.to_string()
                        }
                    });

                let description = if let Some(def) = format_def {
                    format!("{desc_base}\n\n{def}")
                } else {
                    desc_base.to_owned()
                };

                let encoded = map.register_function(None, &name, ToolKind::Custom)?;

                out.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": encoded,
                        "description": description,
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "input": { "type": "string" }
                            },
                            "required": ["input"]
                        }
                    }
                }));
            }

            "namespace" | "tool_search" | "image_generation" | "web_search" => {
                warn!("chat-completions adapter: skipping unsupported tool type '{kind}'");
            }

            other => {
                warn!("chat-completions adapter: unknown tool type '{other}', skipping");
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encode_decode_no_namespace() {
        let encoded = encode_name(None, "my_tool");
        assert_eq!(encoded, "my_tool");
        let (ns, name) = decode_name(&encoded);
        assert!(ns.is_none());
        assert_eq!(name, "my_tool");
    }

    #[test]
    fn encode_decode_with_namespace() {
        let encoded = encode_name(Some("myns"), "my_tool");
        assert_eq!(encoded, "myns__NS__my_tool");
        let (ns, name) = decode_name(&encoded);
        assert_eq!(ns.as_deref(), Some("myns"));
        assert_eq!(name, "my_tool");
    }

    #[test]
    fn translate_function_tool() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "calc",
                "description": "A calculator",
                "parameters": { "type": "object", "properties": {} }
            }
        })];
        let mut map = ToolReverseMap::new();
        let out = translate_tools(&tools, &mut map).expect("should succeed");
        assert_eq!(out.len(), 1);
        let fname = out[0]["function"]["name"].as_str().unwrap();
        assert_eq!(fname, "calc");
        assert!(matches!(
            map.get("calc"),
            Some(ToolKind::Function { namespace: None })
        ));
    }

    #[test]
    fn translate_function_tool_with_namespace() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "search",
                "namespace": "web",
                "description": "Web search",
                "parameters": { "type": "object", "properties": {} }
            }
        })];
        let mut map = ToolReverseMap::new();
        let out = translate_tools(&tools, &mut map).expect("should succeed");
        assert_eq!(out.len(), 1);
        let fname = out[0]["function"]["name"].as_str().unwrap();
        assert_eq!(fname, "web__NS__search");
        assert!(matches!(
            map.get("web__NS__search"),
            Some(ToolKind::Function { namespace: Some(ns) }) if ns == "web"
        ));
    }

    #[test]
    fn translate_custom_tool() {
        let tools = vec![json!({
            "type": "custom",
            "name": "my_custom",
            "description": "Does something",
            "format": {
                "definition": "{\"schema\": \"v1\"}"
            }
        })];
        let mut map = ToolReverseMap::new();
        let out = translate_tools(&tools, &mut map).expect("should succeed");
        assert_eq!(out.len(), 1);
        let fname = out[0]["function"]["name"].as_str().unwrap();
        assert_eq!(fname, "my_custom");
        let desc = out[0]["function"]["description"].as_str().unwrap();
        assert!(desc.contains("Does something"));
        assert!(desc.contains("{\"schema\": \"v1\"}"));
        assert!(matches!(map.get("my_custom"), Some(ToolKind::Custom)));
    }

    #[test]
    fn collision_returns_error() {
        let tools = vec![
            json!({
                "type": "function",
                "function": { "name": "dup", "parameters": {} }
            }),
            json!({
                "type": "function",
                "function": { "name": "dup", "parameters": {} }
            }),
        ];
        let mut map = ToolReverseMap::new();
        let result = translate_tools(&tools, &mut map);
        assert!(result.is_err());
        if let Err(ApiError::InvalidRequest { message }) = result {
            assert!(message.contains("collision"));
        } else {
            panic!("expected InvalidRequest error");
        }
    }

    #[test]
    fn skip_unsupported_tool_types() {
        let tools = vec![
            json!({ "type": "namespace" }),
            json!({ "type": "tool_search" }),
            json!({ "type": "image_generation" }),
            json!({ "type": "web_search" }),
        ];
        let mut map = ToolReverseMap::new();
        let out = translate_tools(&tools, &mut map).expect("should succeed");
        assert!(out.is_empty());
    }
}
