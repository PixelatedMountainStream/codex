use super::*;
use crate::config_toml::ConfigToml;
use crate::types::MemoriesToml;
use pretty_assertions::assert_eq;

fn parse_toml(value: &str) -> TomlValue {
    toml::from_str(value).expect("TOML should parse")
}

#[test]
fn merge_toml_values_normalizes_legacy_key_from_base_layer() {
    let mut base = parse_toml(
        r#"
[memories]
no_memories_if_mcp_or_web_search = false
"#,
    );
    let overlay = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );
    assert_eq!(base, expected);

    let config: ConfigToml = base.try_into().expect("merged config should deserialize");
    assert_eq!(
        config.memories,
        Some(MemoriesToml {
            disable_on_external_context: Some(true),
            ..Default::default()
        })
    );
}

#[test]
fn merge_toml_values_normalizes_legacy_key_from_overlay_layer() {
    let mut base = parse_toml(
        r#"
[memories]
disable_on_external_context = false
"#,
    );
    let overlay = parse_toml(
        r#"
[memories]
no_memories_if_mcp_or_web_search = true
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );
    assert_eq!(base, expected);

    let config: ConfigToml = base.try_into().expect("merged config should deserialize");
    assert_eq!(
        config.memories,
        Some(MemoriesToml {
            disable_on_external_context: Some(true),
            ..Default::default()
        })
    );
}

#[test]
fn parses_top_level_provider_overrides() {
    let cfg: ConfigToml = toml::from_str(
        r#"
review_model    = "gpt-5-mini"
review_provider = "ollama"
compact_model    = "gemma4:26b"
compact_provider = "ollama"
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(cfg.review_model.as_deref(), Some("gpt-5-mini"));
    assert_eq!(cfg.review_provider.as_deref(), Some("ollama"));
    assert_eq!(cfg.compact_model.as_deref(), Some("gemma4:26b"));
    assert_eq!(cfg.compact_provider.as_deref(), Some("ollama"));
}

#[test]
fn parses_memories_provider_overrides() {
    let cfg: ConfigToml = toml::from_str(
        r#"
[memories]
extract_model            = "gpt-5-mini"
extract_provider         = "ollama"
consolidation_model      = "gpt-5.2"
consolidation_provider   = "ollama"
"#,
    )
    .expect("TOML deserialization should succeed");

    let memories = cfg.memories.expect("memories should parse");
    assert_eq!(memories.extract_provider.as_deref(), Some("ollama"));
    assert_eq!(memories.consolidation_provider.as_deref(), Some("ollama"));
    assert_eq!(memories.extract_model.as_deref(), Some("gpt-5-mini"));
    assert_eq!(memories.consolidation_model.as_deref(), Some("gpt-5.2"));
}

#[test]
fn merge_toml_values_overlay_overrides_provider_fields() {
    let mut base = parse_toml(
        r#"
review_provider = "openai"
compact_provider = "openai"

[memories]
extract_provider       = "openai"
consolidation_provider = "openai"
"#,
    );
    let overlay = parse_toml(
        r#"
review_provider = "ollama"
compact_provider = "ollama"

[memories]
extract_provider       = "ollama"
consolidation_provider = "ollama"
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let merged: ConfigToml = base.try_into().expect("merged config should deserialize");
    assert_eq!(merged.review_provider.as_deref(), Some("ollama"));
    assert_eq!(merged.compact_provider.as_deref(), Some("ollama"));
    let memories = merged.memories.expect("memories should round-trip");
    assert_eq!(memories.extract_provider.as_deref(), Some("ollama"));
    assert_eq!(memories.consolidation_provider.as_deref(), Some("ollama"));
}

#[test]
fn merge_toml_values_prefers_canonical_key_when_one_layer_has_both_names() {
    let mut base = TomlValue::Table(toml::map::Map::new());
    let overlay = parse_toml(
        r#"
[memories]
disable_on_external_context = true
no_memories_if_mcp_or_web_search = false
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );
    assert_eq!(base, expected);
}
