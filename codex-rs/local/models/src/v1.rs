//! `GET <root>/v1/models`, the one catalog endpoint every engine serves.

use std::time::Duration;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V1Model {
    pub id: String,
    /// Per-slot context window when the engine reports it (PAIR/LM Studio facade).
    pub context_length: Option<i64>,
    /// Not recognisably another engine's body (llama.cpp `meta`/`owned_by`, LM Studio
    /// `publisher`), so Ollama's `/api/show` may answer.
    pub ollama_shaped: bool,
}

/// `root` has no `/v1`. Errors are short human strings.
pub async fn list_v1_models(
    client: &reqwest::Client,
    root: &str,
    timeout: Duration,
) -> Result<Vec<V1Model>, String> {
    let body: Value = client
        .get(format!("{}/v1/models", root.trim_end_matches('/')))
        .timeout(timeout)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| e.without_url().to_string())?
        .json()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let mut out: Vec<V1Model> = Vec::new();
    for m in body["data"].as_array().into_iter().flatten() {
        if let Some(id) = m["id"].as_str()
            && !out.iter().any(|x| x.id == id)
        {
            out.push(V1Model {
                id: id.to_string(),
                context_length: m["context_length"].as_i64().filter(|n| *n > 0),
                ollama_shaped: m.get("meta").is_none()
                    && m.get("publisher").is_none()
                    && m["owned_by"].as_str() != Some("llamacpp"),
            });
        }
    }
    Ok(out)
}
