use std::fs;

use anyhow::Context;
use anyhow::Result;
use toml_edit::DocumentMut;

use crate::Homes;
use crate::links::refresh_agents_copy;
use crate::refresh_skills;

/// Copy `mcp_servers` and `[projects]` from stock's config.toml into codex-local's,
/// then refresh skill links (and a copied AGENTS.md). Stock is only read.
pub fn sync_config(h: &Homes) -> Result<()> {
    let local_path = h.local.join("config.toml");
    let mut local: DocumentMut = fs::read_to_string(&local_path)
        .with_context(|| format!("read {}", local_path.display()))?
        .parse()
        .with_context(|| format!("parse {}", local_path.display()))?;
    let mut copied = Vec::new();
    // codex-local's own web-search server must survive the stock copy.
    let own_search = local
        .get("mcp_servers")
        .and_then(|m| m.get(crate::seed::WEB_SEARCH_ID))
        .cloned();
    if let Ok(text) = fs::read_to_string(h.stock.join("config.toml")) {
        let stock: DocumentMut = text.parse().context("parse stock config.toml")?;
        for key in ["mcp_servers", "projects"] {
            if let Some(item) = stock.get(key) {
                local[key] = item.clone();
                copied.push(key);
            }
        }
    }
    // Keep the existing entry (user edits win); add the default if an older home lacks it.
    let own_search = own_search.unwrap_or_else(|| {
        let doc: DocumentMut = toml::to_string(&crate::seed::web_search_server())
            .unwrap_or_default()
            .parse()
            .unwrap_or_default();
        toml_edit::Item::Table(doc.as_table().clone())
    });
    if local.get("mcp_servers").is_none() {
        local["mcp_servers"] = toml_edit::table();
    }
    local["mcp_servers"][crate::seed::WEB_SEARCH_ID] = own_search;
    fs::write(&local_path, local.to_string())
        .with_context(|| format!("write {}", local_path.display()))?;
    refresh_skills(h)?;
    refresh_agents_copy(h)?;
    eprintln!(
        "codex-local sync-config: copied [{}]; skill links refreshed",
        copied.join(", ")
    );
    Ok(())
}
