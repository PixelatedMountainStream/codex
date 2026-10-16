//! H9 local wire codec. Some local engines (LM Studio) reject Responses `namespace`
//! and `custom` tools. In `Flatten` mode both become plain function tools with
//! collision-checked aliases, history is translated the same way, and inbound calls are
//! mapped back to canonical items through an exact reverse map (aliases are never parsed).
//! `DottedInbound` (Ollama) sends tools unchanged and maps `ns.tool` call names back.
//!
//! Known limits (documented, not fixed): aliases appear in telemetry and raw-capture
//! because those observers run before inbound canonicalisation; one engine per
//! codex-local process (no mixed-engine threads); the per-request literal set is
//! process-wide, so concurrent sub-agents with different tool sets can race on it;
//! `AdditionalTools`/`ToolSearchOutput` are only produced for OpenAI-hosted paths
//! (responses-lite, hosted tool_search) and never reach local engines, so they are not
//! translated; streamed argument deltas of flattened custom calls arrive as function
//! argument JSON, which the SSE layer does not turn into `ToolCallInputDelta`, so such
//! calls get no live diff preview (the final item is canonical and executes normally).

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
    rev: HashMap<String, (Pair, Kind)>,
    /// Literal top-level function names of the latest request; never reverse-mapped.
    literals: HashSet<String>,
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

fn naive(ns: Option<&str>, name: &str) -> String {
    match ns {
        Some(ns) => format!("{}__{}", sanitize(ns), sanitize(name)),
        None => sanitize(name),
    }
}

/// Hashed alias: `trunc54(naive) + "_" + hex8(sha256(tag ns \0 name [\0 salt]))`.
/// `tag` separates `None` from `Some("")`; `salt == 0` hashes no salt.
fn hashed(ns: Option<&str>, name: &str, salt: u32) -> String {
    let mut h = Sha256::new();
    h.update([u8::from(ns.is_some())]);
    h.update(ns.unwrap_or(""));
    h.update([0]);
    h.update(name);
    if salt > 0 {
        h.update([0]);
        h.update(salt.to_string());
    }
    let hex: String = h
        .finalize()
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect();
    let head: String = naive(ns, name).chars().take(54).collect();
    format!("{head}_{hex}")
}

/// Pure, deterministic alias of `(ns, name)` (depends on nothing else).
///
/// Plain form `P(ns, name)`: `name` when `ns` is `None`, else `ns + "__" + name`. It is
/// used only when `ns`/`name` are unchanged by `sanitize`, `len(P) <= 64`, and:
///   * `ns == None`: `name` contains no `"__"`;
///   * `ns == Some`: `name` contains no `"__"` and does not start with `'_'`
///     (`ns` is unrestricted, so MCP namespaces `mcp__server__` work; they yield
///     `mcp__server____tool`).
/// Injectivity: a plain `None` alias never contains `"__"`, a plain `Some` alias
/// always does, so the two never collide. For `Some`, the last `"__"` in `P` starts
/// exactly at `len(ns)`: an occurrence starting later would lie inside `name` (barred)
/// or straddle the separator and `name[0]` (barred by the no-leading-`'_'` rule), and
/// `P` has no later separator. So `ns = P[..i]`, `name = P[i+2..]` with `i` the last
/// `"__"` index recovers the pair uniquely. Otherwise the hashed form is used
/// (`salt` 0), which is deterministic too; `salt > 0` is only for literal conflicts.
fn alias_of(ns: Option<&str>, name: &str, salt: u32) -> String {
    let clean = ns.is_none_or(|n| sanitize(n) == n)
        && sanitize(name) == name
        && !name.contains("__")
        && (ns.is_none() || !name.starts_with('_'));
    if salt == 0 && clean {
        let plain = naive(ns, name);
        if plain.len() <= 64 {
            return plain;
        }
    }
    hashed(ns, name, salt)
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
                self.remember(&format!("{ns}.{name}"), pair, kind_of(inner), true);
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
        // Aliases are pure; only a literal-name conflict in this request forces a salt.
        self.literals = literals;
        let mut out = Vec::new();
        for slot in slots {
            match slot {
                Ok(t) => out.push(t),
                Err(i) => {
                    let (pair, tool) = &entries[i];
                    let kind = kind_of(tool);
                    let alias = self.alias(pair.0.as_deref(), &pair.1, kind, true);
                    out.push(as_function(tool.clone(), &alias));
                }
            }
        }
        out
    }

    /// Records `alias -> pair`. The mapping is injective, so a different pair under the
    /// same alias is a bug (or a 32-bit hash collision): log it and keep the first.
    fn remember(&mut self, alias: &str, pair: Pair, kind: Kind, authoritative: bool) {
        match self.rev.get_mut(alias) {
            Some((p, k)) if *p == pair => {
                if authoritative {
                    *k = kind;
                }
            }
            Some((p, _)) => {
                tracing::error!(alias, existing = ?p, new = ?pair, "codec alias conflict");
                debug_assert!(false, "codec alias conflict for {alias}");
            }
            None => {
                self.rev.insert(alias.to_string(), (pair, kind));
            }
        }
    }

    /// Alias for tool definitions and history alike: the pure alias, salted only while it
    /// equals a literal top-level function name of the current request.
    fn alias(&mut self, ns: Option<&str>, name: &str, kind: Kind, tool_def: bool) -> String {
        let mut salt = 0;
        let mut a = alias_of(ns, name, salt);
        while self.literals.contains(&a) {
            salt += 1;
            a = alias_of(ns, name, salt);
        }
        let pair = (ns.map(str::to_string), name.to_string());
        self.remember(&a, pair, kind, tool_def);
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
                let name = self.alias(Some(&ns), &name, Kind::Function, false);
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
                let name = name.map(|n| self.alias(Some(&ns), &n, Kind::Function, false));
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
                let name = self.alias(namespace.as_deref(), &name, Kind::Custom, false);
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
        // A literal function name of this request is never redirected.
        if self.literals.contains(name) {
            return item;
        }
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
    fn aliases_are_pure_and_stable_across_requests() {
        let mut c = Codec::default();
        let first = flatten(&mut c, json!([ns_tool("mcp__s__", &["t"])]));
        let other = flatten(
            &mut c,
            json!([ns_tool("x", &["y"]), ns_tool("mcp__s__", &["t"])]),
        );
        assert_eq!(first[0]["name"], other[1]["name"]);
        assert_eq!(first[0]["name"], "mcp__s____t");
        assert_eq!(first[0]["name"], alias_of(Some("mcp__s__"), "t", 0));
    }

    #[test]
    fn colliding_pairs_get_distinct_stable_aliases_across_requests() {
        let mut c = Codec::default();
        let r1 = flatten(&mut c, json!([ns_tool("a", &["b__c"])]));
        let r2 = flatten(&mut c, json!([ns_tool("a__b", &["c"])]));
        let (a, b) = (
            r1[0]["name"].as_str().unwrap(),
            r2[0]["name"].as_str().unwrap(),
        );
        assert_ne!(a, b);
        assert_eq!(
            c.inbound(Mode::Flatten, call(None, a)),
            call(Some("a"), "b__c")
        );
        assert_eq!(
            c.inbound(Mode::Flatten, call(None, b)),
            call(Some("a__b"), "c")
        );
        let again = flatten(&mut c, json!([ns_tool("a", &["b__c"])]));
        assert_eq!(again[0]["name"], a);
    }

    #[test]
    fn literal_equal_to_alias_is_salted_and_never_redirected() {
        let mut c = Codec::default();
        let hashed_lit = alias_of(Some("a.b"), "x", 0);
        let lit =
            |n: &str| json!({"type": "function", "name": n, "parameters": {"type": "object"}});
        let out = flatten(
            &mut c,
            json!([
                ns_tool("a", &["b"]),
                lit("a__b"),
                ns_tool("a.b", &["x"]),
                lit(&hashed_lit)
            ]),
        );
        let (plain, salted) = (
            out[0]["name"].as_str().unwrap(),
            out[2]["name"].as_str().unwrap(),
        );
        assert_ne!(plain, "a__b");
        assert_ne!(salted, hashed_lit);
        assert_eq!(
            c.inbound(Mode::Flatten, call(None, plain)),
            call(Some("a"), "b")
        );
        assert_eq!(
            c.inbound(Mode::Flatten, call(None, salted)),
            call(Some("a.b"), "x")
        );
        for l in ["a__b", hashed_lit.as_str()] {
            assert_eq!(c.inbound(Mode::Flatten, call(None, l)), call(None, l));
        }
    }

    #[test]
    fn flattened_custom_call_maps_back_to_custom_tool_call() {
        let mut c = Codec::default();
        let custom = json!({"type": "custom", "name": "apply_patch", "description": "p"});
        flatten(&mut c, json!([custom]));
        let mut fc = call(None, "apply_patch");
        if let ResponseItem::FunctionCall { arguments, .. } = &mut fc {
            *arguments = json!({"input": "*** Begin"}).to_string();
        }
        // router.rs maps `CustomToolCall` straight to `ToolPayload::Custom { input }`.
        assert!(matches!(
            c.inbound(Mode::Flatten, fc),
            ResponseItem::CustomToolCall { namespace: None, name, input, .. }
                if name == "apply_patch" && input == "*** Begin"
        ));
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
