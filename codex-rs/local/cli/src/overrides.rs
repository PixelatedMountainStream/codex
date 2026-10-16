use std::ffi::OsString;

use anyhow::Result;
use anyhow::bail;

use codex_local_models::Engine;
use codex_local_models::resolve_engine;

use crate::EnvFn;
use crate::PROVIDER_ID;
use crate::env_ollama_root;
use crate::resolve_ollama_root;

pub(crate) const MAX_RETRIES: u64 = 2;

/// Result of [`apply_overrides`].
#[derive(Debug)]
pub struct Applied {
    pub argv: Vec<OsString>,
    pub engine: Engine,
    /// Flag, env or config set the engine (else it is the `lmstudio` default).
    pub engine_explicit: bool,
    /// A URL flag, env var or non-default config provider URL chose the root.
    pub url_explicit: bool,
    /// The model we injected (`None` when `-m`/`-c model=` was given, or llama.cpp
    /// still needs its first `/v1/models` id).
    pub model: Option<String>,
    /// The model the run will use: the `-m`/`-c model=` value, else the injected one.
    pub effective_model: Option<String>,
}

/// Strip `--engine`, `--base-url`/`--ollama-url` (ours, unknown to upstream) and inject `-c` overrides right
/// after argv[0] (root-level, so they reach every subcommand).
///
/// Precedence: flags (`-m`, `--base-url`/`--ollama-url`) > `CODEX_LOCAL_MODEL` >
/// `CODEX_LOCAL_URL` > `CODEX_LOCAL_OLLAMA_URL` > `OLLAMA_HOST` > config.toml > built-ins.
/// The engine is `--engine` > `CODEX_LOCAL_ENGINE` > config `[local] engine` > `lmstudio`;
/// the winning engine with no URL override gets its default port and default model. `-m` is left to
/// upstream (it already wins), so the env model is injected only when no model flag is
/// present, else the configured model. Both the provider and the model are always
/// injected: upstream's TUI only overrides a resumed session's rollout provider/model
/// (e.g. a stock `openai`/`gpt-*` session) when a CLI layer sets them, and codex-local
/// must always continue on its own provider and model.
pub fn apply_overrides(
    args: Vec<OsString>,
    env: EnvFn,
    config_model: Option<String>,
    config_engine: Option<String>,
    config_root: Option<String>,
) -> Result<Applied> {
    let mut rest = Vec::with_capacity(args.len());
    let mut flag_url: Option<String> = None;
    // `--ollama-url` is an alias of `--base-url`, but only it implies the Ollama engine.
    let mut flag_url_is_ollama = false;
    let mut flag_engine: Option<String> = None;
    let mut has_model_flag = false;
    let mut flag_model: Option<String> = None;
    let mut it = args.into_iter();
    let argv0 = it.next();
    let mut after_dd = false;
    while let Some(a) = it.next() {
        if after_dd {
            rest.push(a);
            continue;
        }
        let s = a.to_string_lossy().into_owned();
        if s == "--" {
            after_dd = true;
        } else if s == "--ollama-url" || s == "--base-url" {
            flag_url_is_ollama = s == "--ollama-url";
            match it.next() {
                Some(v) => flag_url = Some(v.to_string_lossy().into_owned()),
                None => bail!("{s} requires a value"),
            }
            continue;
        } else if let Some(v) = s
            .strip_prefix("--ollama-url=")
            .or_else(|| s.strip_prefix("--base-url="))
        {
            flag_url_is_ollama = s.starts_with("--ollama-url=");
            flag_url = Some(v.to_string());
            continue;
        } else if s == "--engine" {
            match it.next() {
                Some(v) => flag_engine = Some(v.to_string_lossy().into_owned()),
                None => bail!("--engine requires a value"),
            }
            continue;
        } else if let Some(v) = s.strip_prefix("--engine=") {
            flag_engine = Some(v.to_string());
            continue;
        } else if s == "-m" || s == "--model" {
            has_model_flag = true;
            rest.push(a);
            if let Some(v) = it.next() {
                flag_model = Some(v.to_string_lossy().into_owned());
                rest.push(v);
            }
            continue;
        } else if let Some(v) = s.strip_prefix("--model=").or_else(|| s.strip_prefix("-m=")) {
            has_model_flag = true;
            flag_model = Some(v.to_string());
        } else if let Some(v) = s.strip_prefix("model=")
            && rest.last().is_some_and(|p| p == "-c")
        {
            has_model_flag = true;
            flag_model = Some(v.trim_matches('"').to_string());
        }
        rest.push(a);
    }

    let (mut engine, mut engine_explicit) = resolve_engine(
        flag_engine.as_deref(),
        env("CODEX_LOCAL_ENGINE").as_deref(),
        config_engine.as_deref(),
    )
    .map_err(anyhow::Error::msg)?;
    // An Ollama-specific URL input implies Ollama unless an engine was set explicitly
    // (the generic `--base-url`/`CODEX_LOCAL_URL` imply nothing).
    if !engine_explicit
        && (flag_url_is_ollama
            || env("CODEX_LOCAL_OLLAMA_URL").is_some()
            || env("OLLAMA_HOST").is_some())
    {
        engine = Engine::Ollama;
        engine_explicit = true;
    }
    let explicit_root = match flag_url {
        Some(u) => Some(resolve_ollama_root(&u)?),
        None => env_ollama_root(env)?,
    };
    // Explicit URLs (flag, env, non-default config provider URL) beat engine default ports.
    let url_explicit = explicit_root.is_some() || config_root.is_some();
    let root = explicit_root.or_else(|| config_root.is_none().then(|| engine.default_root()));
    // A seeded default model of another engine (LM Studio's, or Ollama's in homes seeded
    // before LM Studio became the default) is wrong for the winning engine.
    let config_model = config_model
        .filter(|m| engine.default_model() == Some(m.as_str()) || !Engine::is_default_model(m));
    let model = if has_model_flag {
        None
    } else {
        env("CODEX_LOCAL_MODEL")
            .or(config_model)
            .or_else(|| engine.default_model().map(str::to_string))
    };
    let effective_model = flag_model.or_else(|| model.clone());
    let mut out: Vec<OsString> = argv0.into_iter().collect();
    let mut push_c = |kv: String| {
        out.push("-c".into());
        out.push(kv.into());
    };
    push_c(format!("model_provider=\"{PROVIDER_ID}\""));
    if let Some(m) = &model {
        push_c(format!("model={}", toml::Value::String(m.clone())));
    }
    if let Some(root) = root {
        let v = toml::Value::String(format!("{root}/v1"));
        push_c(format!("model_providers.{PROVIDER_ID}.base_url={v}"));
    }
    // Bound upstream's retry loops so an outage ends in an error, not an endless
    // "Reconnecting..." (config seeding only covers new homes).
    push_c(format!(
        "model_providers.{PROVIDER_ID}.request_max_retries={MAX_RETRIES}"
    ));
    push_c(format!(
        "model_providers.{PROVIDER_ID}.stream_max_retries={MAX_RETRIES}"
    ));
    // Local engines fail the built-in server-side web_search tool; also covers old installs.
    push_c("web_search=\"disabled\"".to_string());
    // No app-server daemon (needs a package we don't ship; also skips our in-process hooks).
    push_c("features.daemon_auto_start=false".to_string());
    // `CODEX_LOCAL_MAX_AGENTS` beats the seeded `[agents] max_threads` in config.toml.
    if let Some(raw) = env("CODEX_LOCAL_MAX_AGENTS") {
        match raw.trim().parse::<u64>() {
            Ok(n) if n > 0 => push_c(format!("agents.max_threads={n}")),
            _ => eprintln!("codex-local: ignoring invalid CODEX_LOCAL_MAX_AGENTS={raw:?}"),
        }
    }
    out.extend(rest);
    Ok(Applied {
        argv: out,
        engine,
        engine_explicit,
        url_explicit,
        model,
        effective_model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_MODEL;

    fn run(args: &[&str], env: &[(&str, &str)]) -> Vec<String> {
        let f = |k: &str| {
            env.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        };
        let a = args.iter().map(OsString::from).collect();
        // A custom config URL keeps the default engine's own root out of these argv shapes.
        apply_overrides(
            a,
            &f,
            Some("cfgm".to_string()),
            None,
            Some("http://cfg:1".into()),
        )
        .unwrap()
        .argv
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
    }

    const RQ: &str = "model_providers.codex-local.request_max_retries=2";
    const ST: &str = "model_providers.codex-local.stream_max_retries=2";
    const WS: &str = "web_search=\"disabled\"";
    const MP: &str = "model_provider=\"codex-local\"";
    const MD: &str = "model=\"cfgm\"";
    const DA: &str = "features.daemon_auto_start=false";
    const URL_KEY: &str = "model_providers.codex-local.base_url";

    fn applied(args: &[&str], env: &[(&str, &str)], cfg_engine: Option<&str>) -> Applied {
        let f = |k: &str| {
            env.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        };
        let a = args.iter().map(OsString::from).collect();
        apply_overrides(
            a,
            &f,
            Some(DEFAULT_MODEL.to_string()),
            cfg_engine.map(str::to_string),
            None,
        )
        .unwrap()
    }

    fn has(a: &Applied, kv: &str) -> bool {
        a.argv.iter().any(|s| s == kv)
    }

    #[test]
    fn ollama_url_inputs_imply_ollama_engine() {
        let a = applied(&["x", "--ollama-url", "h:1"], &[], None);
        assert_eq!(a.engine, Engine::Ollama);
        assert!(has(&a, "model=\"qwen3.8:27b\""));
        let a = applied(&["x"], &[("OLLAMA_HOST", "h:1")], None);
        assert_eq!(a.engine, Engine::Ollama);
        let a = applied(
            &["x", "--engine", "lmstudio", "--ollama-url", "h:1"],
            &[],
            None,
        );
        assert_eq!(a.engine, Engine::LmStudio);
        let a = applied(&["x", "--base-url", "h:1"], &[], None);
        assert_eq!(a.engine, Engine::LmStudio);
    }

    #[test]
    fn engine_precedence_flag_env_config_default() {
        let a = applied(&["x", "exec"], &[], None);
        assert_eq!((a.engine, a.engine_explicit), (Engine::LmStudio, false));
        assert!(has(
            &a,
            "model_providers.codex-local.base_url=\"http://127.0.0.1:1234/v1\""
        ));
        assert!(has(&a, &format!("model=\"{DEFAULT_MODEL}\"")));
        // Explicit ollama still resolves (here the config carries Ollama's old seed).
        let f = |_: &str| None;
        let o = apply_overrides(
            vec!["x".into(), "--engine=ollama".into()],
            &f,
            Some("qwen3.8:27b".into()),
            None,
            None,
        )
        .unwrap();
        assert_eq!((o.engine, o.engine_explicit), (Engine::Ollama, true));
        assert!(
            o.argv
                .iter()
                .any(|s| s == "model_providers.codex-local.base_url=\"http://127.0.0.1:11434/v1\"")
        );
        // Old Ollama-seeded config, no explicit engine: now LM Studio.
        let m =
            apply_overrides(vec!["x".into()], &f, Some("qwen3.8:27b".into()), None, None).unwrap();
        assert_eq!(m.engine, Engine::LmStudio);
        assert_eq!(m.model.as_deref(), Some(DEFAULT_MODEL));
        let a = applied(&["x", "exec"], &[], Some("lmstudio"));
        assert_eq!((a.engine, a.engine_explicit), (Engine::LmStudio, true));
        let a = applied(
            &["x", "exec"],
            &[("CODEX_LOCAL_ENGINE", "llamacpp")],
            Some("lmstudio"),
        );
        assert_eq!(a.engine, Engine::LlamaCpp);
        let a = applied(
            &["x", "--engine", "ollama", "exec"],
            &[("CODEX_LOCAL_ENGINE", "llamacpp")],
            Some("lmstudio"),
        );
        assert_eq!(a.engine, Engine::Ollama);
        assert!(!a.argv.iter().any(|s| s == "--engine" || s == "ollama"));
        assert!(
            apply_overrides(
                vec!["x".into(), "--engine=bogus".into()],
                &|_| None,
                None,
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn non_ollama_engine_gets_port_and_model_unless_overridden() {
        let a = applied(&["x", "--engine=lmstudio", "exec"], &[], None);
        assert!(has(
            &a,
            "model_providers.codex-local.base_url=\"http://127.0.0.1:1234/v1\""
        ));
        assert!(has(&a, "model=\"qwen/qwen3.8-27b\""));
        assert!(!a.url_explicit);
        let a = applied(
            &["x", "--engine=lmstudio", "--base-url", "h:9", "exec"],
            &[],
            None,
        );
        assert!(has(
            &a,
            "model_providers.codex-local.base_url=\"http://h:9/v1\""
        ));
        assert!(a.url_explicit);
        let a = applied(
            &["x", "--engine=llamacpp", "exec"],
            &[("CODEX_LOCAL_URL", "h:8")],
            None,
        );
        assert!(has(
            &a,
            "model_providers.codex-local.base_url=\"http://h:8/v1\""
        ));
        assert_eq!(a.model, None);
        // A custom config provider URL beats the engine's default port.
        let f = |_: &str| None;
        let a = apply_overrides(
            vec!["x".into(), "--engine=lmstudio".into()],
            &f,
            None,
            None,
            Some("http://remote:9".into()),
        )
        .unwrap();
        assert!(
            a.url_explicit
                && !a
                    .argv
                    .iter()
                    .any(|s| s.to_string_lossy().contains("base_url"))
        );
    }

    #[test]
    fn nothing_set_changes_nothing() {
        let want = [
            "x",
            "-c",
            MP,
            "-c",
            MD,
            "-c",
            "model_providers.codex-local.request_max_retries=2",
            "-c",
            "model_providers.codex-local.stream_max_retries=2",
            "-c",
            "web_search=\"disabled\"",
            "-c",
            "features.daemon_auto_start=false",
            "exec",
            "hi",
        ];
        assert_eq!(run(&["x", "exec", "hi"], &[]), want);
    }

    #[test]
    fn max_agents_env_valid_invalid_unset() {
        let k = "CODEX_LOCAL_MAX_AGENTS";
        let ma = "agents.max_threads=8";
        assert_eq!(run(&["x", "exec"], &[(k, "8")])[14], ma);
        assert_eq!(run(&["x", "exec"], &[(k, "8")]).len(), 16);
        for bad in ["0", "-1", "abc", ""] {
            assert_eq!(run(&["x", "exec"], &[(k, bad)]).len(), 14);
        }
        assert_eq!(run(&["x", "exec"], &[]).len(), 14);
    }

    #[test]
    fn url_precedence_flag_then_env_chain() {
        let env = [("CODEX_LOCAL_OLLAMA_URL", "h1:1"), ("OLLAMA_HOST", "h2:2")];
        let want = |u: &str| format!("{URL_KEY}=\"{u}/v1\"");
        assert_eq!(
            run(&["x", "--ollama-url", "http://f:3", "exec"], &env),
            [
                "x",
                "-c",
                MP,
                "-c",
                MD,
                "-c",
                &want("http://f:3"),
                "-c",
                RQ,
                "-c",
                ST,
                "-c",
                WS,
                "-c",
                DA,
                "exec"
            ]
        );
        assert_eq!(run(&["x", "exec"], &env)[6], want("http://h1:1"));
        assert_eq!(
            run(&["x", "exec"], &[("OLLAMA_HOST", "0.0.0.0")])[6],
            want("http://127.0.0.1:11434")
        );
    }

    #[test]
    fn model_flag_beats_env_and_dd_is_respected() {
        let env = [("CODEX_LOCAL_MODEL", "envm")];
        assert_eq!(run(&["x", "exec"], &env)[2], MP);
        assert_eq!(run(&["x", "exec"], &env)[4], "model=\"envm\"");
        assert_eq!(run(&["x", "exec"], &[])[4], "model=\"cfgm\"");
        assert_eq!(
            run(&["x", "-m", "flagm", "exec"], &env),
            [
                "x",
                "-c",
                MP,
                "-c",
                "model_providers.codex-local.request_max_retries=2",
                "-c",
                "model_providers.codex-local.stream_max_retries=2",
                "-c",
                "web_search=\"disabled\"",
                "-c",
                "features.daemon_auto_start=false",
                "-m",
                "flagm",
                "exec"
            ]
        );
        assert_eq!(
            run(&["x", "exec", "--", "--ollama-url", "-m"], &[]),
            [
                "x",
                "-c",
                MP,
                "-c",
                MD,
                "-c",
                "model_providers.codex-local.request_max_retries=2",
                "-c",
                "model_providers.codex-local.stream_max_retries=2",
                "-c",
                "web_search=\"disabled\"",
                "-c",
                "features.daemon_auto_start=false",
                "exec",
                "--",
                "--ollama-url",
                "-m"
            ]
        );
    }
}
