//! Ollama-backed model catalog for `codex-local` (`/model` picker, req 8).
//!
//! Lists models via `GET <root>/api/tags` (PAIR merges the cluster's models) and reads each
//! context window via `POST <root>/api/show`.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use codex_http_client::HttpClientFactory;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::ModelsEndpointClient;
use codex_models_manager::manager::ModelsEndpointFuture;
use codex_models_manager::manager::ModelsEndpointResponse;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CoreResult;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use serde_json::Value;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Matches upstream's fallback metadata when `/api/show` gives no context length.
const FALLBACK_CONTEXT_WINDOW: i64 = 272_000;

#[derive(Debug)]
pub struct OllamaModelsEndpoint {
    root: String,
    freeform_apply_patch: bool,
    client: reqwest::Client,
}

impl OllamaModelsEndpoint {
    /// `root` is the Ollama root (no `/v1`). `freeform_apply_patch` defaults to off.
    pub fn new(root: impl Into<String>, freeform_apply_patch: bool) -> Self {
        Self {
            root: root.into().trim_end_matches('/').to_string(),
            freeform_apply_patch,
            client: reqwest::Client::new(),
        }
    }

    async fn context_window(&self, name: &str) -> Option<i64> {
        let resp = self
            .client
            .post(format!("{}/api/show", self.root))
            .timeout(REQUEST_TIMEOUT)
            .json(&serde_json::json!({ "model": name }))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let body: Value = resp.json().await.ok()?;
        body.get("model_info")?
            .as_object()?
            .iter()
            .find(|(k, _)| k.ends_with(".context_length"))
            .and_then(|(_, v)| v.as_i64())
    }

    async fn fetch(&self) -> CoreResult<Vec<ModelInfo>> {
        let invalid = |e: &dyn fmt::Display| CodexErr::InvalidRequest(format!("ollama: {e}"));
        let body: Value = self
            .client
            .get(format!("{}/api/tags", self.root))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| invalid(&e))?
            .json()
            .await
            .map_err(|e| invalid(&e))?;
        let mut names: Vec<String> = Vec::new();
        for m in body["models"].as_array().into_iter().flatten() {
            if let Some(n) = m["name"].as_str().or_else(|| m["model"].as_str())
                && !names.iter().any(|x| x == n)
            {
                names.push(n.to_string());
            }
        }
        let mut set = tokio::task::JoinSet::new();
        for (i, n) in names.iter().enumerate() {
            let (this, n) = (self.clone_light(), n.clone());
            set.spawn(async move { (i, this.context_window(&n).await) });
        }
        let mut windows = vec![None; names.len()];
        while let Some(Ok((i, w))) = set.join_next().await {
            windows[i] = w;
        }
        Ok(names
            .into_iter()
            .zip(windows)
            .enumerate()
            .map(|(i, (name, w))| self.model_info(&name, w, i))
            .collect())
    }

    fn clone_light(&self) -> Self {
        Self {
            root: self.root.clone(),
            freeform_apply_patch: self.freeform_apply_patch,
            client: self.client.clone(),
        }
    }

    fn model_info(&self, name: &str, window: Option<i64>, index: usize) -> ModelInfo {
        let mut info = model_info_from_slug(name);
        let window = window.unwrap_or(FALLBACK_CONTEXT_WINDOW);
        info.visibility = ModelVisibility::List;
        info.priority = i32::try_from(index).unwrap_or(i32::MAX);
        info.context_window = Some(window);
        info.max_context_window = Some(window);
        info.used_fallback_model_metadata = false;
        if self.freeform_apply_patch {
            info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
        }
        info
    }
}

impl ModelsEndpointClient for OllamaModelsEndpoint {
    fn identity(&self) -> Option<String> {
        Some(format!("codex-local:{}", self.root))
    }

    fn has_command_auth(&self) -> bool {
        false
    }

    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(async { false })
    }

    fn list_models<'a>(
        &'a self,
        _client_version: &'a str,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<ModelsEndpointResponse>> {
        Box::pin(async move {
            Ok(ModelsEndpointResponse {
                models: self.fetch().await?,
                etag: None,
                identity: format!("codex-local:{}", self.root),
            })
        })
    }
}

fn factory(info: &ModelProviderInfo) -> SharedModelsManager {
    let base = info.base_url.as_deref().unwrap_or("http://127.0.0.1:11434");
    let root = base.trim_end_matches('/').trim_end_matches("/v1");
    Arc::new(OpenAiModelsManager::new_without_cache(
        Arc::new(OllamaModelsEndpoint::new(root, false)),
        None,
    ))
}

/// Registers the H2 factory. Call once, before creating the `codex-local` provider.
pub fn register() {
    codex_model_provider::register_local_models_factory(factory);
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_partial_json;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[tokio::test]
    async fn lists_tags_with_context_windows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"name": "a:1"}, {"name": "b:2"}, {"name": "c:3"}, {"name": "a:1"}]
            })))
            .mount(&server)
            .await;
        for (n, ctx) in [("a:1", 1000), ("b:2", 2000), ("c:3", 3000)] {
            Mock::given(method("POST"))
                .and(path("/api/show"))
                .and(body_partial_json(serde_json::json!({"model": n})))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"model_info": {"general.architecture": "x", "x.context_length": ctx}}),
                ))
                .mount(&server)
                .await;
        }
        let ep = OllamaModelsEndpoint::new(server.uri(), true);
        let models = ep.fetch().await.unwrap();
        let got: Vec<_> = models
            .iter()
            .map(|m| (m.slug.as_str(), m.context_window.unwrap()))
            .collect();
        assert_eq!(got, vec![("a:1", 1000), ("b:2", 2000), ("c:3", 3000)]);
        assert!(models.iter().all(|m| m.apply_patch_tool_type.is_some()));
    }
}
