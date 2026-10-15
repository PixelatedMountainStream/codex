//! Logic for the `codex-local` binary. The binary itself (`main.rs`) only wires this
//! to upstream's CLI entry (hook H1); everything testable lives here.

mod homes;
mod links;
mod overrides;
mod preflight;
mod seed;
mod sync;
mod url;

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;

pub use homes::Homes;
pub use homes::resolve_homes;
pub use links::ensure_home;
pub use links::refresh_skills;
pub use overrides::apply_overrides;
pub use preflight::check as preflight_check;
pub use preflight::failure_message as preflight_failure_message;
pub use seed::seed_config;
pub use sync::sync_config;
pub use url::resolve_ollama_root;

pub const DEFAULT_MODEL: &str = "qwen3.8:27b";
pub const DEFAULT_OLLAMA_ROOT: &str = "http://localhost:11434";
pub const PROVIDER_ID: &str = "codex-local";

/// `<upstream-version>+local.<sha7>`; the sha comes from `build.rs`.
pub fn version_string() -> String {
    format!(
        "{}+local.{}",
        env!("CARGO_PKG_VERSION"),
        env!("CODEX_LOCAL_SHA7")
    )
}

/// Read-only environment lookup, injected so tests never touch the process env.
pub type EnvFn<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

pub fn user_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

pub enum Action {
    /// Print this string and exit.
    Version(String),
    /// `codex-local sync-config` finished.
    SyncConfig,
    /// Delegate to upstream with this argv; the caller must first set
    /// `CODEX_HOME` to `codex_home`.
    Run {
        argv: Vec<OsString>,
        codex_home: PathBuf,
        /// Ollama root to probe before delegating; `None` skips the preflight.
        preflight_root: Option<String>,
    },
}

/// Everything `main` does before delegating to upstream, minus touching the process env.
pub fn prepare(args: Vec<OsString>, env: EnvFn, user_home: Option<PathBuf>) -> Result<Action> {
    let first = args.get(1).and_then(|a| a.to_str());
    if matches!(first, Some("--version" | "-V")) {
        return Ok(Action::Version(format!("codex-local {}", version_string())));
    }
    let homes = resolve_homes(env, user_home)?;
    ensure_home(&homes)?;
    // Seed from env/built-ins only; a one-off --ollama-url flag must not persist.
    let root = env_ollama_root(env)?.unwrap_or_else(|| DEFAULT_OLLAMA_ROOT.to_string());
    seed_config(&homes, &root)?;
    if first == Some("sync-config") {
        sync_config(&homes)?;
        return Ok(Action::SyncConfig);
    }
    let want_preflight = preflight::needs_preflight(&args, env);
    let config_model = std::fs::read_to_string(homes.local.join("config.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|c| c.get("model").and_then(|m| m.as_str()).map(str::to_string))
        .or_else(|| Some(DEFAULT_MODEL.to_string()));
    let argv = apply_overrides(args, env, config_model)?;
    let preflight_root = want_preflight.then(|| effective_root(&argv, &homes, &root));
    Ok(Action::Run {
        argv,
        codex_home: homes.local,
        preflight_root,
    })
}

/// Root that upstream will end up using: the injected base_url override, else the
/// seeded config's base_url, else the env/built-in root.
fn effective_root(argv: &[OsString], homes: &Homes, fallback: &str) -> String {
    let key = format!("model_providers.{PROVIDER_ID}.base_url=");
    let strip = |u: &str| u.trim_end_matches('/').trim_end_matches("/v1").to_string();
    for w in argv.windows(2) {
        if w[0] == "-c"
            && let Some(v) = w[1].to_string_lossy().strip_prefix(&key)
        {
            return strip(v.trim_matches('"'));
        }
    }
    let cfg = std::fs::read_to_string(homes.local.join("config.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok());
    cfg.and_then(|c| {
        c["model_providers"][PROVIDER_ID]["base_url"]
            .as_str()
            .map(strip)
    })
    .unwrap_or_else(|| fallback.to_string())
}

pub(crate) fn env_ollama_root(env: EnvFn) -> Result<Option<String>> {
    for key in ["CODEX_LOCAL_OLLAMA_URL", "OLLAMA_HOST"] {
        if let Some(v) = env(key) {
            return resolve_ollama_root(&v).map(Some);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn home_links_seed_and_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let (stock, local) = (tmp.path().join("stock"), tmp.path().join("local"));
        fs::create_dir_all(stock.join("skills/.system")).unwrap();
        fs::create_dir_all(stock.join("skills/mine")).unwrap();
        fs::write(stock.join("AGENTS.md"), "hi").unwrap();
        fs::write(
            stock.join("config.toml"),
            "[mcp_servers.a]\ncommand=\"x\"\n[projects.\"/p\"]\ntrust_level=\"trusted\"\n",
        )
        .unwrap();
        let (s, l) = (
            stock.to_string_lossy().into_owned(),
            local.to_string_lossy().into_owned(),
        );
        let env = move |k: &str| match k {
            "CODEX_HOME" => Some(s.clone()),
            "CODEX_LOCAL_HOME" => Some(l.clone()),
            _ => None,
        };
        let args = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        let Action::Run { codex_home, .. } = prepare(args(&["x", "exec"]), &env, None).unwrap()
        else {
            panic!()
        };
        assert_eq!(codex_home, local);
        for d in ["sessions", "archived_sessions", "AGENTS.md", "skills/mine"] {
            assert!(
                local
                    .join(d)
                    .symlink_metadata()
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{d}"
            );
        }
        assert!(
            !local
                .join("skills")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!local.join("skills/.system").exists());
        let cfg: toml::Table = fs::read_to_string(local.join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(cfg["model"].as_str(), Some(DEFAULT_MODEL));
        assert_eq!(
            cfg["model_providers"][PROVIDER_ID]["base_url"].as_str(),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(
            cfg["shell_environment_policy"]["set"]["CODEX_HOME"].as_str(),
            Some(stock.to_str().unwrap())
        );
        assert!(matches!(
            prepare(args(&["x", "sync-config"]), &env, None).unwrap(),
            Action::SyncConfig
        ));
        let cfg: toml::Table = fs::read_to_string(local.join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(cfg["mcp_servers"]["a"]["command"].as_str(), Some("x"));
        assert_eq!(
            cfg["mcp_servers"]["web-search"]["command"].as_str(),
            Some("uvx")
        );
        assert_eq!(
            cfg["projects"]["/p"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(
            fs::read_to_string(stock.join("config.toml"))
                .unwrap()
                .matches("mcp_servers")
                .count(),
            1
        );
    }
}
