//! Cross-resume support for codex-local: a sanitizer for histories written by other
//! providers, plus the resume picker registration.

use codex_protocol::models::ResponseItem;

/// Prepares an in-memory history for resuming under a different provider.
///
/// Drops items whose payload is opaque to other providers: reasoning and compaction items
/// carrying `encrypted_content`. Clears provider-assigned item ids. Function calls are never
/// dropped (so each call stays paired with its output, even under per-item application):
/// the encrypted args are cleared, and empty arguments become `"{}"`.
/// Infallible: it never panics and never returns an error.
pub fn sanitize_history(items: Vec<ResponseItem>) -> Vec<ResponseItem> {
    items
        .into_iter()
        .filter_map(|mut item| {
            match &mut item {
                ResponseItem::Reasoning {
                    encrypted_content: Some(_),
                    ..
                }
                | ResponseItem::Compaction { .. }
                | ResponseItem::ContextCompaction {
                    encrypted_content: Some(_),
                    ..
                } => {
                    tracing::debug!("resume sanitizer: dropping item with encrypted payload");
                    return None;
                }
                ResponseItem::FunctionCall {
                    arguments,
                    encrypted_function_args,
                    ..
                } if encrypted_function_args.is_some() => {
                    *encrypted_function_args = None;
                    if arguments.is_empty() {
                        *arguments = "{}".to_string();
                    }
                }
                _ => {}
            }
            item.set_id(None);
            Some(item)
        })
        .collect()
}

/// Registers the sanitizer (H3) and the all-providers, filesystem-first picker (H4).
/// Idempotent; the first registration wins.
pub fn register() {
    let _ = codex_core::RESUME_HISTORY_TRANSFORM.set(sanitize_history);
    let _ = codex_tui::RESUME_ALL_PROVIDERS.set(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ResponseItemId;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;

    fn id(s: &str) -> Option<ResponseItemId> {
        serde_json::from_value(serde_json::json!(s)).ok()
    }

    #[test]
    fn strips_encrypted_reasoning_clears_ids_keeps_messages_and_tools() {
        let items = vec![
            ResponseItem::Reasoning {
                id: id("rs_1"),
                summary: vec![],
                content: None,
                encrypted_content: Some("blob".into()),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Message {
                id: id("msg_1"),
                role: "assistant".into(),
                content: vec![ContentItem::OutputText { text: "hi".into() }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCall {
                id: id("fc_1"),
                name: "shell".into(),
                namespace: None,
                arguments: "{}".into(),
                encrypted_function_args: None,
                call_id: "call_1".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: id("fco_1"),
                call_id: Some("call_1".into()),
                name: None,
                namespace: None,
                output: FunctionCallOutputPayload::from_text("ok".into()),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let out = sanitize_history(items);
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[1], ResponseItem::FunctionCall { encrypted_function_args: None, .. }));
        assert!(out.iter().all(|i| i.id().is_none()));
        assert!(matches!(&out[0], ResponseItem::Message { content, .. }
            if matches!(&content[0], ContentItem::OutputText { text } if text == "hi")));
        assert!(
            matches!(&out[1], ResponseItem::FunctionCall { call_id, .. } if call_id == "call_1")
        );
        assert!(
            matches!(&out[2], ResponseItem::FunctionCallOutput { call_id, .. } if call_id.as_deref() == Some("call_1"))
        );
    }

    #[test]
    fn keeps_encrypted_only_calls_with_empty_args_and_drops_compaction() {
        let items = vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "shell".into(),
                namespace: None,
                arguments: String::new(),
                encrypted_function_args: Some(vec!["blob".to_string()]),
                call_id: "call_2".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("call_2".into()),
                name: None,
                namespace: None,
                output: FunctionCallOutputPayload::from_text("ok".into()),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Compaction {
                id: None,
                encrypted_content: "blob".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::ContextCompaction {
                id: None,
                encrypted_content: Some("blob".into()),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let out = sanitize_history(items);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], ResponseItem::FunctionCall { arguments, encrypted_function_args: None, .. }
            if arguments == "{}"));
        assert!(matches!(&out[1], ResponseItem::FunctionCallOutput { .. }));
    }
}
