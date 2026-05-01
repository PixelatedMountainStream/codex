use codex_models_manager::collaboration_mode_presets::CollaborationModesConfig;
use codex_models_manager::collaboration_mode_presets::builtin_collaboration_mode_presets;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use std::convert::Infallible;

const OLLAMA_PROVIDER_ID: &str = "ollama";

#[derive(Debug, Clone)]
pub(crate) struct ModelCatalog {
    models: Vec<ModelPreset>,
    collaboration_modes_config: CollaborationModesConfig,
    configured_provider_ids: Vec<String>,
}

impl ModelCatalog {
    pub(crate) fn new(
        models: Vec<ModelPreset>,
        collaboration_modes_config: CollaborationModesConfig,
        configured_provider_ids: Vec<String>,
    ) -> Self {
        Self {
            models,
            collaboration_modes_config,
            configured_provider_ids,
        }
    }

    pub(crate) fn try_list_models(&self) -> Result<Vec<ModelPreset>, Infallible> {
        let mut models = self.models.clone();
        // Milestone-1 stub: one hard-coded local Ollama preset so the provider
        // switching pipeline can be validated end-to-end before real catalog
        // discovery (fetch_local_presets) ships in Milestone 2.
        // Only surfaced when the running config actually has an `ollama`
        // entry in its merged provider map — otherwise selecting it would
        // fail at runtime with an unknown-provider error.
        if self
            .configured_provider_ids
            .iter()
            .any(|id| id == OLLAMA_PROVIDER_ID)
        {
            models.push(local_ollama_gemma4_preset());
        }
        Ok(models)
    }

    pub(crate) fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets(self.collaboration_modes_config)
    }
}

/// Hard-coded stub for the local Ollama Gemma 4 26B-A4B preset.
///
/// Milestone 2 replaces this with a live `fetch_local_presets` call that
/// queries `GET http://localhost:11434/api/tags` and synthesises presets for
/// every model tag returned.  Until then this single entry validates the
/// end-to-end provider-switching pipeline without network discovery.
fn local_ollama_gemma4_preset() -> ModelPreset {
    ModelPreset {
        id: "ollama-gemma4-26b-a4b".into(),
        model: "gemma4:26b-a4b-it-q4_K_M".into(),
        display_name: "Gemma 4 26B-A4B (local Ollama)".into(),
        description: "Local model via Ollama — requires `ollama` running on localhost:11434."
            .into(),
        default_reasoning_effort: ReasoningEffort::None,
        supported_reasoning_efforts: vec![ReasoningEffortPreset {
            effort: ReasoningEffort::None,
            description: "No reasoning".into(),
        }],
        supports_personality: false,
        additional_speed_tiers: vec![],
        is_default: false,
        upgrade: None,
        show_in_picker: true,
        availability_nux: None,
        supported_in_api: true,
        input_modalities: codex_protocol::openai_models::default_input_modalities(),
        provider_id: Some("ollama".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn list_collaboration_modes_matches_core_presets() {
        let collaboration_modes_config = CollaborationModesConfig {
            default_mode_request_user_input: true,
        };
        let catalog = ModelCatalog::new(Vec::new(), collaboration_modes_config, Vec::new());

        assert_eq!(
            catalog.list_collaboration_modes(),
            builtin_collaboration_mode_presets(collaboration_modes_config)
        );
    }

    #[test]
    fn try_list_models_includes_ollama_stub_when_ollama_configured() {
        let collab_cfg = CollaborationModesConfig {
            default_mode_request_user_input: false,
        };
        let catalog =
            ModelCatalog::new(Vec::new(), collab_cfg, vec![OLLAMA_PROVIDER_ID.to_string()]);

        let presets = catalog.try_list_models().expect("infallible");
        assert_eq!(presets.len(), 1, "exactly one stub preset expected");
        let preset = &presets[0];
        assert_eq!(
            preset.provider_id,
            Some(OLLAMA_PROVIDER_ID.to_string()),
            "preset provider_id should be ollama",
        );
        assert_eq!(
            preset.model, "gemma4:26b-a4b-it-q4_K_M",
            "preset model slug should match the hard-coded stub",
        );
    }

    #[test]
    fn try_list_models_omits_ollama_stub_when_ollama_not_configured() {
        let collab_cfg = CollaborationModesConfig {
            default_mode_request_user_input: false,
        };
        let catalog = ModelCatalog::new(Vec::new(), collab_cfg, Vec::new());

        let presets = catalog.try_list_models().expect("infallible");
        assert!(
            presets
                .iter()
                .all(|p| p.provider_id.as_deref() != Some(OLLAMA_PROVIDER_ID)),
            "no ollama preset should be present when ollama is not in configured_provider_ids",
        );
    }
}
