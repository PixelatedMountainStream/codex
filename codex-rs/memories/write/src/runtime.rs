use codex_core::CodexThread;
use codex_core::ModelClient;
use codex_core::NewThread;
use codex_core::Prompt;
use codex_core::ResponseEvent;
use codex_core::StartThreadOptions;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::content_items_to_text;
use codex_core::resolve_installation_id;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth_env_telemetry::collect_auth_env_telemetry;
use codex_login::default_client::originator;
use codex_otel::SessionTelemetry;
use codex_otel::TelemetryAuthMode;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::user_input::UserInput;
use codex_rollout_trace::InferenceTraceContext;
use codex_state::StateRuntime;
use codex_terminal_detection::user_agent;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct SpawnedConsolidationAgent {
    pub(crate) thread_id: ThreadId,
    pub(crate) thread: Arc<CodexThread>,
}

#[derive(Clone, Debug)]
pub(crate) struct StageOneRequestContext {
    pub(crate) model_info: ModelInfo,
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) reasoning_summary: ReasoningSummary,
    pub(crate) service_tier: Option<ServiceTier>,
    pub(crate) turn_metadata_header: Option<String>,
}

impl StageOneRequestContext {
    pub(crate) fn start_timer(&self, name: &str) -> Option<codex_otel::Timer> {
        self.session_telemetry.start_timer(name, &[]).ok()
    }

    pub(crate) fn counter(&self, name: &str, inc: i64, tags: &[(&str, &str)]) {
        self.session_telemetry.counter(name, inc, tags);
    }

    pub(crate) fn histogram(&self, name: &str, value: i64, tags: &[(&str, &str)]) {
        self.session_telemetry.histogram(name, value, tags);
    }
}

pub(crate) struct MemoryStartupContext {
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    thread_manager: Arc<ThreadManager>,
    auth_manager: Arc<AuthManager>,
    session_telemetry: SessionTelemetry,
}

impl MemoryStartupContext {
    pub(crate) fn new(
        thread_manager: Arc<ThreadManager>,
        auth_manager: Arc<AuthManager>,
        thread_id: ThreadId,
        thread: Arc<CodexThread>,
        config: &Config,
        source: SessionSource,
    ) -> Self {
        let auth = auth_manager.auth_cached();
        let auth = auth.as_ref();
        let auth_mode = auth.map(CodexAuth::auth_mode).map(TelemetryAuthMode::from);
        let account_id = auth.and_then(CodexAuth::get_account_id);
        let account_email = auth.and_then(CodexAuth::get_account_email);
        let model = config.model.as_deref().unwrap_or("unknown");
        let auth_env_telemetry = collect_auth_env_telemetry(
            &config.model_provider,
            auth_manager.codex_api_key_env_enabled(),
        );
        let session_telemetry = SessionTelemetry::new(
            thread_id,
            model,
            model,
            account_id,
            account_email,
            auth_mode,
            originator().value,
            config.otel.log_user_prompt,
            user_agent(),
            source,
        )
        .with_auth_env(auth_env_telemetry.to_otel_metadata());

        Self {
            thread_id,
            thread,
            thread_manager,
            auth_manager,
            session_telemetry,
        }
    }

    pub(crate) fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    pub(crate) fn state_db(&self) -> Option<Arc<StateRuntime>> {
        self.thread.state_db()
    }

    pub(crate) fn counter(&self, name: &str, inc: i64, tags: &[(&str, &str)]) {
        self.session_telemetry.counter(name, inc, tags);
    }

    pub(crate) fn histogram(&self, name: &str, value: i64, tags: &[(&str, &str)]) {
        self.session_telemetry.histogram(name, value, tags);
    }

    pub(crate) fn start_timer(&self, name: &str) -> Option<codex_otel::Timer> {
        self.session_telemetry.start_timer(name, &[]).ok()
    }

    pub(crate) async fn stage_one_request_context(
        &self,
        config: &Config,
        model_name: &str,
        reasoning_effort: ReasoningEffort,
    ) -> StageOneRequestContext {
        let config_snapshot = self.thread.config_snapshot().await;
        let model_info = self
            .thread_manager
            .get_models_manager()
            .get_model_info(model_name, &config.to_models_manager_config())
            .await;
        let turn_metadata_header =
            codex_core::build_turn_metadata_header(&config.cwd, /*sandbox*/ None).await;
        let reasoning_summary = config
            .model_reasoning_summary
            .unwrap_or(model_info.default_reasoning_summary);

        StageOneRequestContext {
            model_info,
            turn_metadata_header,
            session_telemetry: self
                .session_telemetry
                .clone()
                .with_model(model_name, model_name),
            reasoning_effort: Some(reasoning_effort),
            reasoning_summary,
            service_tier: config_snapshot.service_tier,
        }
    }

    pub(crate) async fn stream_stage_one_prompt(
        &self,
        config: &Config,
        prompt: &Prompt,
        context: &StageOneRequestContext,
    ) -> anyhow::Result<(String, Option<TokenUsage>)> {
        let installation_id = resolve_installation_id(&config.codex_home).await?;
        let session_source = self.thread.config_snapshot().await.session_source;
        let model_provider = resolve_extract_provider(config)?;
        let model_client = ModelClient::new(
            Some(Arc::clone(&self.auth_manager)),
            self.thread_id,
            installation_id,
            model_provider,
            session_source,
            config.model_verbosity,
            config.features.enabled(Feature::EnableRequestCompression),
            config.features.enabled(Feature::RuntimeMetrics),
            /*beta_features_header*/ None,
        );

        let mut client_session = model_client.new_session();
        let mut stream = client_session
            .stream(
                prompt,
                &context.model_info,
                &context.session_telemetry,
                context.reasoning_effort,
                context.reasoning_summary,
                context.service_tier,
                context.turn_metadata_header.as_deref(),
                &InferenceTraceContext::disabled(),
            )
            .await?;

        let mut result = String::new();
        let mut token_usage = None;
        while let Some(message) = stream.next().await.transpose()? {
            match message {
                ResponseEvent::OutputTextDelta(delta) => result.push_str(&delta),
                ResponseEvent::OutputItemDone(item) => {
                    if result.is_empty()
                        && let codex_protocol::models::ResponseItem::Message { content, .. } = item
                        && let Some(text) = content_items_to_text(&content)
                    {
                        result.push_str(&text);
                    }
                }
                ResponseEvent::Completed {
                    token_usage: usage, ..
                } => {
                    token_usage = usage;
                    break;
                }
                _ => {}
            }
        }

        Ok((result, token_usage))
    }

    pub(crate) async fn spawn_consolidation_agent(
        &self,
        config: Config,
        prompt: Vec<UserInput>,
    ) -> anyhow::Result<SpawnedConsolidationAgent> {
        let environments = self
            .thread_manager
            .default_environment_selections(&config.cwd);
        let NewThread {
            thread_id, thread, ..
        } = self
            .thread_manager
            .start_thread_with_options(StartThreadOptions {
                config,
                initial_history: InitialHistory::New,
                session_source: Some(SessionSource::Internal(
                    InternalSessionSource::MemoryConsolidation,
                )),
                dynamic_tools: Vec::new(),
                persist_extended_history: false,
                metrics_service_name: None,
                parent_trace: None,
                environments,
            })
            .await?;

        let agent = SpawnedConsolidationAgent { thread_id, thread };
        if let Err(err) = agent
            .thread
            .submit(Op::UserInput {
                items: prompt,
                environments: None,
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
            })
            .await
        {
            if let Err(shutdown_err) = self.shutdown_consolidation_agent(agent).await {
                tracing::warn!(
                    "failed to shut down consolidation agent after submit error: {shutdown_err}"
                );
            }
            return Err(err.into());
        }

        Ok(agent)
    }

    pub(crate) async fn shutdown_consolidation_agent(
        &self,
        agent: SpawnedConsolidationAgent,
    ) -> anyhow::Result<()> {
        let SpawnedConsolidationAgent { thread_id, thread } = agent;
        let thread = self
            .thread_manager
            .remove_thread(&thread_id)
            .await
            .unwrap_or(thread);

        tokio::time::timeout(Duration::from_secs(10), thread.shutdown_and_wait())
            .await
            .map_err(|_| {
                anyhow::anyhow!("memory consolidation agent {thread_id} shutdown timed out")
            })??;

        Ok(())
    }
}

/// Resolve the `ModelProviderInfo` to use for the phase-1 (extract) one-shot
/// `ModelClient`. When `memories.extract_provider` is set, look it up in the
/// merged provider map; otherwise fall back to the active session provider.
pub fn resolve_extract_provider(
    config: &Config,
) -> anyhow::Result<codex_model_provider_info::ModelProviderInfo> {
    match config.memories.extract_provider.as_deref() {
        Some(provider_id) => config
            .model_providers
            .get(provider_id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown memories.extract_provider '{provider_id}' (not present in model_providers)"
                )
            }),
        None => Ok(config.model_provider.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;
    use core_test_support::load_default_config_for_test;
    use tempfile::tempdir;

    #[tokio::test]
    async fn resolve_extract_provider_returns_session_provider_when_unset() {
        let codex_home = tempdir().expect("tempdir");
        let config = load_default_config_for_test(&codex_home).await;
        assert!(config.memories.extract_provider.is_none());

        let resolved = resolve_extract_provider(&config).expect("resolve should succeed");
        assert_eq!(resolved, config.model_provider);
    }

    #[tokio::test]
    async fn resolve_extract_provider_returns_override_provider_info() {
        let codex_home = tempdir().expect("tempdir");
        let mut config = load_default_config_for_test(&codex_home).await;
        // Sanity check — the default fixture must not already be on ollama.
        assert_ne!(config.model_provider_id, OLLAMA_OSS_PROVIDER_ID);
        let expected = config
            .model_providers
            .get(OLLAMA_OSS_PROVIDER_ID)
            .cloned()
            .expect("ollama provider must be a registered built-in");

        config.memories.extract_provider = Some(OLLAMA_OSS_PROVIDER_ID.to_string());

        let resolved = resolve_extract_provider(&config).expect("resolve should succeed");
        assert_eq!(resolved, expected);
        assert_ne!(
            resolved, config.model_provider,
            "override must differ from the parent session provider"
        );
    }

    #[tokio::test]
    async fn resolve_extract_provider_unknown_id_errors() {
        let codex_home = tempdir().expect("tempdir");
        let mut config = load_default_config_for_test(&codex_home).await;
        config.memories.extract_provider = Some("definitely-not-a-real-provider".to_string());

        let err =
            resolve_extract_provider(&config).expect_err("unknown provider should be rejected");
        assert!(
            err.to_string()
                .contains("unknown memories.extract_provider"),
            "unexpected error: {err}"
        );
    }
}
