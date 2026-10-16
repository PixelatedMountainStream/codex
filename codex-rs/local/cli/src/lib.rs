//! Logic for the `codex-local` binary. The binary itself (`main.rs`) only wires this
//! to upstream's CLI entry (hook H1); everything testable lives here.

mod homes;
mod links;
mod local_cfg;
mod overrides;
mod preflight;
mod seed;
mod sync;
mod url;

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;

pub use codex_local_models::Engine;
pub use homes::Homes;
pub use homes::resolve_homes;
pub use links::ensure_home;
pub use links::refresh_skills;
pub use overrides::Applied;
pub use overrides::apply_overrides;
pub use preflight::check as preflight_check;
pub use preflight::engine_hint;
pub use preflight::failure_message as preflight_failure_message;
pub use seed::seed_config;
pub use sync::sync_config;
pub use url::resolve_ollama_root;

/// Seeded model: the default engine's (LM Studio) placeholder model.
pub const DEFAULT_MODEL: &str = codex_local_models::LMSTUDIO_DEFAULT_MODEL;
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
        engine: Engine,
        argv: Vec<OsString>,
        codex_home: PathBuf,
        /// Engine root to probe before delegating; `None` skips the preflight.
        preflight_root: Option<String>,
        /// Model to look for on the other engines' default ports when the engine was
        /// never chosen explicitly; `None` skips the probe.
        probe_model: Option<String>,
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
    // Seed from env/built-ins only; a one-off --ollama-url/--engine flag must not persist.
    // The seeded URL is always the default engine's (LM Studio; any engine's default port is
    // injected at run time, after the winning engine is known, so a seeded default root reads
    // as "unset"), and a bad env engine must not abort here.
    let seed_engine = env("CODEX_LOCAL_ENGINE")
        .and_then(|v| v.parse::<Engine>().ok())
        .unwrap_or_default();
    let root = env_ollama_root(env)?.unwrap_or_else(|| Engine::default().default_root());
    seed_config(&homes, &root, seed_engine)?;
    let config_engine = local_cfg::engine(&homes)?;
    if first == Some("sync-config") {
        sync_config(&homes)?;
        return Ok(Action::SyncConfig);
    }
    let args_snapshot = args.clone();
    let want_preflight = preflight::needs_preflight(&args, env);
    let cfg = std::fs::read_to_string(homes.local.join("config.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok());
    let config_model = cfg
        .as_ref()
        .and_then(|c| c.get("model").and_then(|m| m.as_str()).map(str::to_string))
        .or_else(|| Some(DEFAULT_MODEL.to_string()));
    let config_root = config_root(cfg.as_ref());
    let mut applied = apply_overrides(args, env, config_model, config_engine, config_root)?;
    overrides::inject_default_effort(&mut applied.argv, cfg.as_ref());
    let preflight_root = want_preflight.then(|| effective_root(&applied.argv, &homes, &root));
    // llama.cpp serves whatever it loaded: default to the first `/v1/models` id. This is
    // model selection, so it runs even when the preflight itself is skipped.
    if applied.engine == Engine::LlamaCpp
        && applied.effective_model.is_none()
        && preflight::uses_model(&args_snapshot)
    {
        let r = preflight_root
            .clone()
            .unwrap_or_else(|| effective_root(&applied.argv, &homes, &root));
        if let Some(id) = preflight::first_model_id(&r) {
            applied.argv.insert(1, "-c".into());
            applied
                .argv
                .insert(2, format!("model={}", toml::Value::String(id)).into());
        }
    }
    let probe_model = (!applied.engine_explicit && !applied.url_explicit)
        .then(|| applied.effective_model.clone())
        .flatten()
        .filter(|_| want_preflight);
    Ok(Action::Run {
        engine: applied.engine,
        argv: applied.argv,
        codex_home: homes.local,
        preflight_root,
        probe_model,
    })
}

/// Wire-codec mode for an engine. llama.cpp's namespace handling is unverified, and
/// flatten is always safe, so only Ollama gets the dotted-inbound mode.
pub fn codec_mode(engine: Engine) -> codex_local_models::codec::Mode {
    use codex_local_models::codec::Mode;
    match engine {
        Engine::Ollama => Mode::DottedInbound,
        Engine::LmStudio | Engine::LlamaCpp => Mode::Flatten,
    }
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
    config_root(
        std::fs::read_to_string(homes.local.join("config.toml"))
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok())
            .as_ref(),
    )
    .unwrap_or_else(|| fallback.to_string())
}

/// Provider root from config.toml, unless it is just a seeded engine-default root (which
/// must not beat the winning engine's default port). This also covers homes seeded with
/// Ollama's root before LM Studio became the default.
fn config_root(cfg: Option<&toml::Table>) -> Option<String> {
    let url = cfg?
        .get("model_providers")?
        .get(PROVIDER_ID)?
        .get("base_url")?
        .as_str()?;
    let root = url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string();
    (!Engine::is_default_root(&root)).then_some(root)
}

pub(crate) fn env_ollama_root(env: EnvFn) -> Result<Option<String>> {
    for key in ["CODEX_LOCAL_URL", "CODEX_LOCAL_OLLAMA_URL", "OLLAMA_HOST"] {
        if let Some(v) = env(key) {
            return resolve_ollama_root(&v).map(Some);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    #[test]
    fn engine_to_codec_mode() {
        use codex_local_models::codec::Mode;
        assert_eq!(codec_mode(Engine::Ollama), Mode::DottedInbound);
        assert_eq!(codec_mode(Engine::LmStudio), Mode::Flatten);
        assert_eq!(codec_mode(Engine::LlamaCpp), Mode::Flatten);
    }

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
            Some("http://127.0.0.1:1234/v1")
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

    fn run_env(tmp: &std::path::Path, extra: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let mut m: Vec<(String, String)> = vec![
            (
                "CODEX_HOME".into(),
                tmp.join("stock").to_string_lossy().into(),
            ),
            (
                "CODEX_LOCAL_HOME".into(),
                tmp.join("local").to_string_lossy().into(),
            ),
        ];
        m.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        move |k: &str| m.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    fn base_url_arg(a: &Action) -> Option<String> {
        let Action::Run { argv, .. } = a else {
            panic!()
        };
        argv.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .find(|s| s.contains(".base_url="))
    }

    #[test]
    fn flag_engine_beats_bad_env_engine_and_seed_never_bakes_a_port() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("stock")).unwrap();
        let env = run_env(
            tmp.path(),
            &[
                ("CODEX_LOCAL_ENGINE", "bogus"),
                ("CODEX_LOCAL_SKIP_PREFLIGHT", "1"),
            ],
        );
        let a = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        let act = prepare(a(&["x", "--engine", "ollama", "exec"]), &env, None).unwrap();
        // The seeded LM Studio root reads as unset, so Ollama's own port is injected.
        assert!(base_url_arg(&act).unwrap().contains(":11434/v1"));
        let env = run_env(
            tmp.path(),
            &[
                ("CODEX_LOCAL_ENGINE", "llamacpp"),
                ("CODEX_LOCAL_SKIP_PREFLIGHT", "1"),
            ],
        );
        let cfg = fs::read_to_string(tmp.path().join("local/config.toml")).unwrap();
        assert!(cfg.contains("127.0.0.1:1234/v1"), "{cfg}");
        let act = prepare(a(&["x", "exec"]), &env, None).unwrap();
        assert!(base_url_arg(&act).unwrap().contains(":8080/v1"));
    }

    #[test]
    fn legacy_local_table_migrates_to_codex_local_toml() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("stock")).unwrap();
        let env = run_env(tmp.path(), &[("CODEX_LOCAL_SKIP_PREFLIGHT", "1")]);
        let a = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        prepare(a(&["x", "exec"]), &env, None).unwrap();
        let own = fs::read_to_string(tmp.path().join("local/codex-local.toml")).unwrap();
        assert!(!own.contains("\nengine"), "seeded unset: {own}");
        let cfg_path = tmp.path().join("local/config.toml");
        let cfg = fs::read_to_string(&cfg_path).unwrap();
        fs::write(
            &cfg_path,
            format!("{cfg}\n[local]\nengine = \"lmstudio\"\n"),
        )
        .unwrap();
        let act = prepare(a(&["x", "exec"]), &env, None).unwrap();
        assert!(base_url_arg(&act).unwrap().contains(":1234/v1"));
        assert!(!fs::read_to_string(&cfg_path).unwrap().contains("[local]"));
        let own = fs::read_to_string(tmp.path().join("local/codex-local.toml")).unwrap();
        assert!(own.contains("engine = \"lmstudio\""), "{own}");
    }
}
