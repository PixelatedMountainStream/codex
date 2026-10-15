use std::ffi::OsString;

use anyhow::Result;
use anyhow::bail;

use crate::EnvFn;
use crate::PROVIDER_ID;
use crate::env_ollama_root;
use crate::resolve_ollama_root;

pub(crate) const MAX_RETRIES: u64 = 2;

/// Strip `--ollama-url` (ours, unknown to upstream) and inject `-c` overrides right
/// after argv[0] (root-level, so they reach every subcommand).
///
/// Precedence: flags (`-m`, `--ollama-url`) > `CODEX_LOCAL_MODEL` >
/// `CODEX_LOCAL_OLLAMA_URL` > `OLLAMA_HOST` > config.toml > built-ins. `-m` is left to
/// upstream (it already wins), so the env model is injected only when no model flag is
/// present; config and built-ins need no injection.
pub fn apply_overrides(args: Vec<OsString>, env: EnvFn) -> Result<Vec<OsString>> {
    let mut rest = Vec::with_capacity(args.len());
    let mut flag_url: Option<String> = None;
    let mut has_model_flag = false;
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
        } else if s == "--ollama-url" {
            match it.next() {
                Some(v) => flag_url = Some(v.to_string_lossy().into_owned()),
                None => bail!("--ollama-url requires a value"),
            }
            continue;
        } else if let Some(v) = s.strip_prefix("--ollama-url=") {
            flag_url = Some(v.to_string());
            continue;
        } else if s == "-m" || s == "--model" || s.starts_with("--model=") || s.starts_with("-m=") {
            has_model_flag = true;
        } else if s.starts_with("model=") && rest.last().is_some_and(|p| p == "-c") {
            has_model_flag = true;
        }
        rest.push(a);
    }

    let root = match flag_url {
        Some(u) => Some(resolve_ollama_root(&u)?),
        None => env_ollama_root(env)?,
    };
    let mut out: Vec<OsString> = argv0.into_iter().collect();
    let mut push_c = |kv: String| {
        out.push("-c".into());
        out.push(kv.into());
    };
    if !has_model_flag && let Some(m) = env("CODEX_LOCAL_MODEL") {
        push_c(format!("model={}", toml::Value::String(m)));
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
    // Ollama fails the built-in server-side web_search tool; also covers old installs.
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
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str], env: &[(&str, &str)]) -> Vec<String> {
        let f = |k: &str| {
            env.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        };
        let a = args.iter().map(OsString::from).collect();
        apply_overrides(a, &f)
            .unwrap()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    const RQ: &str = "model_providers.codex-local.request_max_retries=2";
    const ST: &str = "model_providers.codex-local.stream_max_retries=2";
    const WS: &str = "web_search=\"disabled\"";
    const DA: &str = "features.daemon_auto_start=false";
    const URL_KEY: &str = "model_providers.codex-local.base_url";

    #[test]
    fn nothing_set_changes_nothing() {
        let want = [
            "x",
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
        assert_eq!(run(&["x", "exec"], &[(k, "8")])[10], ma);
        assert_eq!(run(&["x", "exec"], &[(k, "8")]).len(), 12);
        for bad in ["0", "-1", "abc", ""] {
            assert_eq!(run(&["x", "exec"], &[(k, bad)]).len(), 10);
        }
        assert_eq!(run(&["x", "exec"], &[]).len(), 10);
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
        assert_eq!(run(&["x", "exec"], &env)[2], want("http://h1:1"));
        assert_eq!(
            run(&["x", "exec"], &[("OLLAMA_HOST", "0.0.0.0")])[2],
            want("http://127.0.0.1:11434")
        );
    }

    #[test]
    fn model_flag_beats_env_and_dd_is_respected() {
        let env = [("CODEX_LOCAL_MODEL", "envm")];
        assert_eq!(run(&["x", "exec"], &env)[2], "model=\"envm\"");
        assert_eq!(
            run(&["x", "-m", "flagm", "exec"], &env),
            [
                "x",
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
