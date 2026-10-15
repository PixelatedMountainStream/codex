use std::fs;

use anyhow::Context;
use anyhow::Result;
use toml::Table;
use toml::Value;

use crate::DEFAULT_MODEL;
use crate::Homes;
use crate::PROVIDER_ID;

pub(crate) const WEB_SEARCH_ID: &str = "web-search";
/// Pinned DuckDuckGo MCP server (tools: search, fetch_content); needs no API key.
pub(crate) const WEB_SEARCH_PKG: &str = "duckduckgo-mcp-server==0.7.0";

fn table(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Table(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

/// The `[mcp_servers.web-search]` entry. Both tools are read-only, so approve them
/// (otherwise `approval_policy = never` runs reject every call).
pub(crate) fn web_search_server() -> Value {
    let s = |v: &str| Value::String(v.to_string());
    table([
        ("command", s("uvx")),
        ("args", Value::Array(vec![s(WEB_SEARCH_PKG)])),
        ("default_tools_approval_mode", s("approve")),
    ])
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
    // Ollama can't run the server-side `web_search` tool; use the local MCP server.
    t.insert("web_search".into(), s("disabled"));
    t.insert(
        "mcp_servers".into(),
        table([(WEB_SEARCH_ID, web_search_server())]),
    );
    t.insert("agents".into(), table([("max_threads", Value::Integer(6))]));
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

#[cfg(test)]
mod tests {
    #[test]
    fn seeds_six_agent_threads() {
        let body = super::seed_toml("http://127.0.0.1:11434", "/h").unwrap();
        let v: toml::Value = toml::from_str(&body).unwrap();
        assert_eq!(v["agents"]["max_threads"].as_integer(), Some(6));
        assert_eq!(v["web_search"].as_str(), Some("disabled"));
        let ws = &v["mcp_servers"]["web-search"];
        assert_eq!(ws["command"].as_str(), Some("uvx"));
        assert_eq!(ws["default_tools_approval_mode"].as_str(), Some("approve"));
        assert_eq!(ws["args"][0].as_str(), Some(super::WEB_SEARCH_PKG));
    }
}
