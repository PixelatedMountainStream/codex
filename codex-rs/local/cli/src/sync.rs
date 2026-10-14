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
    if let Ok(text) = fs::read_to_string(h.stock.join("config.toml")) {
        let stock: DocumentMut = text.parse().context("parse stock config.toml")?;
        for key in ["mcp_servers", "projects"] {
            if let Some(item) = stock.get(key) {
                local[key] = item.clone();
                copied.push(key);
            }
        }
    }
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
