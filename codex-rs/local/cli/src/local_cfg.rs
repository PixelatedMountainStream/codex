//! codex-local's own settings file, `codex-local.toml` in the local `CODEX_HOME`.
//!
//! Upstream warns on (and `--strict-config` rejects) unknown `config.toml` tables, so
//! `[local] engine = ...` lives here, where only codex-local reads it.

use std::fs;

use anyhow::Context;
use anyhow::Result;
use toml_edit::DocumentMut;

use crate::Homes;

pub(crate) const FILE: &str = "codex-local.toml";

const TEMPLATE: &str = "# codex-local settings (read only by codex-local).\n\
# Engine: ollama | lmstudio | llamacpp. Unset means ollama, with a hint if the model is\n\
# found on another engine's default port.\n\
# [local]\n\
# engine = \"ollama\"\n";

/// Creates `codex-local.toml` if absent, folds a legacy `[local]` table out of
/// `config.toml` (once, with a warning), and returns the configured engine.
pub(crate) fn engine(h: &Homes) -> Result<Option<String>> {
    let path = h.local.join(FILE);
    if !path.exists() {
        fs::write(&path, TEMPLATE).with_context(|| format!("write {}", path.display()))?;
    }
    let mut doc: DocumentMut = fs::read_to_string(&path)?
        .parse()
        .with_context(|| format!("parse {}", path.display()))?;
    let read = |d: &DocumentMut| {
        d.get("local")
            .and_then(|l| l.get("engine"))
            .and_then(|e| e.as_str())
            .map(str::to_string)
    };
    let cfg_path = h.local.join("config.toml");
    if let Ok(text) = fs::read_to_string(&cfg_path)
        && let Ok(mut cfg) = text.parse::<DocumentMut>()
        && let Some(legacy) = cfg.remove("local")
    {
        if read(&doc).is_none()
            && let Some(e) = legacy.get("engine").and_then(|e| e.as_str())
        {
            doc["local"]["engine"] = toml_edit::value(e);
            fs::write(&path, doc.to_string())?;
        }
        fs::write(&cfg_path, cfg.to_string())?;
        eprintln!(
            "codex-local: moved `[local]` from config.toml to {FILE} (upstream rejects unknown tables)"
        );
    }
    Ok(read(&doc))
}
