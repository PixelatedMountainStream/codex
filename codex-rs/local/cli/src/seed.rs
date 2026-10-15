use std::fs;

use anyhow::Context;
use anyhow::Result;
use toml::Table;
use toml::Value;

use crate::DEFAULT_MODEL;
use crate::Homes;
use crate::PROVIDER_ID;

fn table(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Table(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

pub(crate) fn seed_toml(root: &str, stock_home: &str) -> Result<String> {
    let s = |v: &str| Value::String(v.to_string());
    let provider = table([
        ("name", s("codex-local")),
        ("base_url", s(&format!("{root}/v1"))),
        ("wire_api", s("responses")),
        ("requires_openai_auth", Value::Boolean(false)),
        ("request_max_retries", Value::Integer(2)),
        ("stream_max_retries", Value::Integer(2)),
    ]);
    let mut t = Table::new();
    t.insert("model".into(), s(DEFAULT_MODEL));
    t.insert("model_provider".into(), s(PROVIDER_ID));
    t.insert("check_for_update_on_startup".into(), Value::Boolean(false));
    t.insert("model_providers".into(), table([(PROVIDER_ID, provider)]));
    t.insert(
        "analytics".into(),
        table([("enabled", Value::Boolean(false))]),
    );
    t.insert(
        "features".into(),
        table([
            ("multi_agent", Value::Boolean(true)),
            // Gates the startup chatgpt.com featured/recommended-plugins fetches.
            ("plugins", Value::Boolean(false)),
        ]),
    );
    t.insert("agents".into(), table([("max_threads", Value::Integer(4))]));
    t.insert(
        "shell_environment_policy".into(),
        table([("set", table([("CODEX_HOME", s(stock_home))]))]),
    );
    Ok(toml::to_string(&t)?)
}

/// Write codex-local's `config.toml` if absent. Returns whether it was written.
pub fn seed_config(h: &Homes, root: &str) -> Result<bool> {
    let path = h.local.join("config.toml");
    if path.exists() {
        return Ok(false);
    }
    let body = seed_toml(root, &h.stock.to_string_lossy())?;
    fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}
