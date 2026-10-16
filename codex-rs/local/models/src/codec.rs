//! H9 local wire codec. Some local engines (LM Studio) reject Responses `namespace`
//! and `custom` tools. In `Flatten` mode both become plain function tools with
//! collision-checked aliases, history is translated the same way, and inbound calls are
//! mapped back to canonical items through an exact reverse map (aliases are never parsed).
//! `DottedInbound` (Ollama) sends tools unchanged and maps `ns.tool` call names back.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use codex_model_provider::LocalWireCodec;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use serde_json::json;
use serde_json::value::RawValue;
use sha2::Digest;
use sha2::Sha256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Flatten,
    DottedInbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Function,
    Custom,
}

type Pair = (Option<String>, String);

#[derive(Default)]
struct Codec {
    fwd: HashMap<Pair, String>,
    rev: HashMap<String, (Pair, Kind)>,
}

static MODE: OnceLock<Mode> = OnceLock::new();
static CODEC: Mutex<Option<Codec>> = Mutex::new(None);

/// Registers the codec (once, before the first request).
pub fn register(mode: Mode) {
    if MODE.set(mode).is_ok() {
        codex_model_provider::register_local_codec(LocalWireCodec { outbound, inbound });
    }
}

fn with_codec<R>(f: impl FnOnce(Mode, &mut Codec) -> R) -> R {
    let mut guard = CODEC.lock().unwrap_or_else(|e| e.into_inner());
    f(
        MODE.get().copied().unwrap_or(Mode::DottedInbound),
        guard.get_or_insert_with(Codec::default),
    )
}

fn outbound(
    tools: &mut Option<Arc<RawValue>>,
    input: &mut Vec<ResponseItem>,
) -> serde_json::Result<()> {
    with_codec(|mode, c| c.outbound(mode, tools, input))
}

fn inbound(item: ResponseItem) -> ResponseItem {
    with_codec(|mode, c| c.inbound(mode, item))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn hashed(ns: Option<&str>, name: &str, naive: &str) -> String {
    let digest = Sha256::digest(format!("{}\0{name}", ns.unwrap_or("")));
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let head: String = naive.chars().take(54).collect();
    format!("{head}_{hex}")
}

/// `Some(alias)` when the plain alias is usable as-is.
fn plain_alias(ns: Option<&str>, name: &str) -> Option<String> {
    let alias = match ns {
        Some(ns) => format!("{}__{}", sanitize(ns), sanitize(name)),
        None => sanitize(name),
    };
    let clean = ns.is_none_or(|n| sanitize(n) == n) && sanitize(name) == name;
    (clean && alias.len() <= 64).then_some(alias)
}

fn kind_of(tool: &Value) -> Kind {
    if tool["type"] == "custom" {
        Kind::Custom
    } else {
        Kind::Function
    }
}

fn as_function(mut tool: Value, alias: &str) -> Value {
    if kind_of(&tool) == Kind::Custom {
        let desc = tool["description"].as_str().unwrap_or("");
        tool = json!({
            "type": "function",
            "description": format!("{desc}\nPass the full raw tool input as the `input` string."),
            "parameters": {
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"],
                "additionalProperties": false,
            },
        });
    }
    tool["name"] = json!(alias);
    tool
}

impl Codec {
    fn outbound(
        &mut self,
        mode: Mode,
        tools: &mut Option<Arc<RawValue>>,
        input: &mut Vec<ResponseItem>,
    ) -> serde_json::Result<()> {
        if let Some(raw) = tools.as_ref() {
            let list: Vec<Value> = serde_json::from_str(raw.get())?;
            match mode {
                Mode::Flatten => {
                    let flat = self.flatten_tools(list);
                    *tools = Some(Arc::from(serde_json::value::to_raw_value(&flat)?));
                }
                Mode::DottedInbound => self.index_dotted(&list),
            }
        }
        if mode == Mode::Flatten {
            for item in input.iter_mut() {
                let taken = std::mem::replace(item, ResponseItem::Other);
                *item = self.out_item(taken);
            }
        }
        Ok(())
    }

    /// Dotted names are looked up as `ns.tool` through the same exact map.
    fn index_dotted(&mut self, list: &[Value]) {
        for t in list.iter().filter(|t| t["type"] == "namespace") {
            let ns = t["name"].as_str().unwrap_or_default();
            for inner in t["tools"].as_array().into_iter().flatten() {
                let name = inner["name"].as_str().unwrap_or_default();
                let pair = (Some(ns.to_string()), name.to_string());
                self.rev
                    .insert(format!("{ns}.{name}"), (pair, kind_of(inner)));
            }
        }
    }

    fn flatten_tools(&mut self, list: Vec<Value>) -> Vec<Value> {
        // Pass 1: collect (ns, name, tool) to translate, and literal top-level names.
        let mut literals: HashSet<String> = HashSet::new();
        let mut entries: Vec<(Pair, Value)> = Vec::new();
        let mut slots: Vec<Result<Value, usize>> = Vec::new();
        for t in list {
            let name = t["name"].as_str().unwrap_or_default().to_string();
            if t["type"] == "namespace" {
                for inner in t["tools"].as_array().cloned().unwrap_or_default() {
                    let n = inner["name"].as_str().unwrap_or_default().to_string();
                    slots.push(Err(entries.len()));
                    entries.push(((Some(name.clone()), n), inner));
                }
            } else if t["type"] == "custom" {
                slots.push(Err(entries.len()));
                entries.push(((None, name), t));
            } else {
                literals.insert(name);
                slots.push(Ok(t));
            }
        }
        // Pass 2: an alias is plain only if clean, short, unique and not a literal tool.
        let mut owners: HashMap<String, HashSet<&Pair>> = HashMap::new();
        for (pair, _) in &entries {
            if let Some(a) = plain_alias(pair.0.as_deref(), &pair.1) {
                owners.entry(a).or_default().insert(pair);
            }
        }
        let aliases: Vec<String> = entries
            .iter()
            .map(|(pair, _)| {
                let (ns, name) = (pair.0.as_deref(), pair.1.as_str());
                plain_alias(ns, name)
                    .filter(|a| owners[a].len() == 1 && !literals.contains(a))
                    .unwrap_or_else(|| {
                        let naive = match ns {
                            Some(ns) => format!("{}__{}", sanitize(ns), sanitize(name)),
                            None => sanitize(name),
                        };
                        hashed(ns, name, &naive)
                    })
            })
            .collect();
        let mut out = Vec::new();
        for slot in slots {
            match slot {
                Ok(t) => out.push(t),
                Err(i) => {
                    let (pair, tool) = &entries[i];
                    let kind = kind_of(tool);
                    self.fwd.insert(pair.clone(), aliases[i].clone());
                    self.rev.insert(aliases[i].clone(), (pair.clone(), kind));
                    out.push(as_function(tool.clone(), &aliases[i]));
                }
            }
        }
        out
    }

    fn alias(&mut self, ns: &Option<String>, name: &str) -> String {
        let key = (ns.clone(), name.to_string());
        if let Some(a) = self.fwd.get(&key) {
            return a.clone();
        }
        let a = plain_alias(ns.as_deref(), name)
            .filter(|a| self.rev.get(a).is_none_or(|(p, _)| *p == key))
            .unwrap_or_else(|| hashed(ns.as_deref(), name, &sanitize(name)));
        self.fwd.insert(key.clone(), a.clone());
        self.rev.insert(a.clone(), (key, Kind::Function));
        a
    }

    fn out_item(&mut self, item: ResponseItem) -> ResponseItem {
        match item {
            ResponseItem::FunctionCall {
                namespace: Some(ns),
                name,
                id,
                arguments,
                encrypted_function_args,
                call_id,
                internal_chat_message_metadata_passthrough,
            } => {
                let name = self.alias(&Some(ns), &name);
                ResponseItem::FunctionCall {
                    id,
                    name,
                    namespace: None,
                    arguments,
                    encrypted_function_args,
                    call_id,
                    internal_chat_message_metadata_passthrough,
                }
            }
            ResponseItem::FunctionCallOutput {
                id,
                call_id,
                name,
                namespace: Some(ns),
                output,
                internal_chat_message_metadata_passthrough,
            } => {
                let name = name.map(|n| self.alias(&Some(ns), &n));
                ResponseItem::FunctionCallOutput {
                    id,
                    call_id,
                    name,
                    namespace: None,
                    output,
                    internal_chat_message_metadata_passthrough,
                }
            }
            ResponseItem::CustomToolCall {
                id,
                call_id,
                name,
                namespace,
                input,
                internal_chat_message_metadata_passthrough,
                ..
            } => {
                let name = self.alias(&namespace, &name);
                ResponseItem::FunctionCall {
                    id,
                    name,
                    namespace: None,
                    arguments: json!({ "input": input }).to_string(),
                    encrypted_function_args: None,
                    call_id,
                    internal_chat_message_metadata_passthrough,
                }
            }
            ResponseItem::CustomToolCallOutput {
                id,
                call_id,
                name,
                output,
                internal_chat_message_metadata_passthrough,
            } => ResponseItem::FunctionCallOutput {
                id,
                call_id: Some(call_id),
                name,
                namespace: None,
                output,
                internal_chat_message_metadata_passthrough,
            },
            other => other,
        }
    }

    fn inbound(&self, _mode: Mode, item: ResponseItem) -> ResponseItem {
        let ResponseItem::FunctionCall {
            namespace: None,
            name,
            ..
        } = &item
        else {
            return item;
        };
        // Flatten maps aliases; DottedInbound maps `ns.tool` (both live in `rev`).
        let Some(((ns, orig), kind)) = self.rev.get(name).cloned() else {
            return item;
        };
        let ResponseItem::FunctionCall {
            id,
            arguments,
            call_id,
            internal_chat_message_metadata_passthrough,
            encrypted_function_args,
            ..
        } = item
        else {
            unreachable!()
        };
        match kind {
            Kind::Function => ResponseItem::FunctionCall {
                id,
                name: orig,
                namespace: ns,
                arguments,
                encrypted_function_args,
                call_id,
                internal_chat_message_metadata_passthrough,
            },
            Kind::Custom => {
                let input = serde_json::from_str::<Value>(&arguments)
                    .ok()
                    .and_then(|v| v["input"].as_str().map(str::to_string))
                    .unwrap_or(arguments);
                ResponseItem::CustomToolCall {
                    id,
                    status: None,
                    call_id,
                    name: orig,
                    namespace: ns,
                    input,
                    internal_chat_message_metadata_passthrough,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(v: Value) -> Option<Arc<RawValue>> {
        Some(Arc::from(serde_json::value::to_raw_value(&v).unwrap()))
    }

    fn ns_tool(ns: &str, names: &[&str]) -> Value {
        let tools: Vec<Value> = names
            .iter()
            .map(|n| json!({"type": "function", "name": n, "description": "d", "parameters": {"type": "object"}}))
            .collect();
        json!({"type": "namespace", "name": ns, "description": "", "tools": tools})
    }

    fn sent(tools: &Option<Arc<RawValue>>) -> Vec<Value> {
        serde_json::from_str(tools.as_ref().unwrap().get()).unwrap()
    }

    fn call(ns: Option<&str>, name: &str) -> ResponseItem {
        ResponseItem::FunctionCall {
            id: None,
            name: name.into(),
            namespace: ns.map(Into::into),
            arguments: "{}".into(),
            encrypted_function_args: None,
            call_id: "c1".into(),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn flatten(c: &mut Codec, tools: Value) -> Vec<Value> {
        let mut t = raw(tools);
        c.outbound(Mode::Flatten, &mut t, &mut Vec::new()).unwrap();
        sent(&t)
    }

    #[test]
    fn collision_pair_gets_distinct_aliases_with_exact_reverse() {
        let mut c = Codec::default();
        let out = flatten(
            &mut c,
            json!([ns_tool("mcp__s", &["a__b"]), ns_tool("mcp__s__a", &["b"])]),
        );
        let (a, b) = (
            out[0]["name"].as_str().unwrap(),
            out[1]["name"].as_str().unwrap(),
        );
        assert_ne!(a, b);
        assert!(out.iter().all(|t| t["type"] == "function"));
        let back = c.inbound(Mode::Flatten, call(None, a));
        assert_eq!(back, call(Some("mcp__s"), "a__b"));
        let back = c.inbound(Mode::Flatten, call(None, b));
        assert_eq!(back, call(Some("mcp__s__a"), "b"));
    }

    #[test]
    fn long_and_dotted_names_get_hashed_aliases_that_round_trip() {
        let mut c = Codec::default();
        let long = "x".repeat(80);
        let out = flatten(&mut c, json!([ns_tool("mcp__s", &[&long, "a.b"])]));
        for (t, name) in out.iter().zip([long.as_str(), "a.b"]) {
            let alias = t["name"].as_str().unwrap();
            assert!(
                alias.len() <= 64
                    && alias
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            );
            assert_ne!(alias, name);
            assert_eq!(
                c.inbound(Mode::Flatten, call(None, alias)),
                call(Some("mcp__s"), name)
            );
        }
    }

    #[test]
    fn custom_tool_round_trips_and_history_keeps_call_id() {
        let mut c = Codec::default();
        let custom = json!({"type": "custom", "name": "apply_patch", "description": "p", "format": {"type": "grammar"}});
        let mut tools = raw(json!([custom, ns_tool("multi", &["spawn"])]));
        let hist_call = ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "c9".into(),
            name: "apply_patch".into(),
            namespace: None,
            input: "*** Begin".into(),
            internal_chat_message_metadata_passthrough: None,
        };
        let mut input = vec![hist_call.clone(), call(Some("multi"), "spawn")];
        c.outbound(Mode::Flatten, &mut tools, &mut input).unwrap();
        assert_eq!(sent(&tools)[0]["parameters"]["required"], json!(["input"]));
        let ResponseItem::FunctionCall {
            call_id,
            namespace: None,
            ..
        } = &input[0]
        else {
            panic!("{:?}", input[0])
        };
        assert_eq!(call_id, "c9");
        assert!(
            matches!(&input[1], ResponseItem::FunctionCall { namespace: None, call_id, .. } if call_id == "c1")
        );
        assert_eq!(c.inbound(Mode::Flatten, input[0].clone()), hist_call);
    }

    #[test]
    fn dotted_inbound_maps_only_sent_namespaces() {
        let mut c = Codec::default();
        let mut tools = raw(json!([ns_tool("multi_agent_v1", &["spawn_agent"])]));
        let before = tools.clone();
        c.outbound(Mode::DottedInbound, &mut tools, &mut Vec::new())
            .unwrap();
        assert_eq!(tools.unwrap().get(), before.unwrap().get());
        let got = c.inbound(
            Mode::DottedInbound,
            call(None, "multi_agent_v1.spawn_agent"),
        );
        assert_eq!(got, call(Some("multi_agent_v1"), "spawn_agent"));
        let unknown = call(None, "nope.spawn_agent");
        assert_eq!(c.inbound(Mode::DottedInbound, unknown.clone()), unknown);
    }
}
