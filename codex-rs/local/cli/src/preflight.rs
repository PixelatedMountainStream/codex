//! Fail-fast check that the local engine answers before upstream starts its retry loops.

use std::ffi::OsString;
use std::time::Duration;

use codex_local_models::Engine;
use codex_local_models::V1Model;
use codex_local_models::list_v1_models;

use crate::EnvFn;

const TIMEOUT: Duration = Duration::from_secs(3);

/// Subcommands that never talk to the model.
const SKIP: &[&str] = &[
    "sync-config",
    "login",
    "logout",
    "completion",
    "debug",
    "mcp",
    "mcp-server",
    "plugin",
    "update",
    "doctor",
    "sandbox",
    "execpolicy",
    "apply",
    "a",
    "app-server",
    "remote-control",
    "app",
    "archive",
    "unarchive",
    "delete",
    "migrate-rollouts",
    "queue",
    "tcp-tunnel",
    "agents",
    "help",
    "features",
    "cloud",
    "responses-api-proxy",
    "stdio-to-uds",
];

/// Flags (of the root command) that consume the next argv element.
const VALUE_FLAGS: &[&str] = &[
    "-c",
    "--config",
    "-m",
    "--model",
    "-p",
    "--profile",
    "-C",
    "--cd",
    "-i",
    "--image",
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "--ollama-url",
    "--base-url",
    "--engine",
    "--local-provider",
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
];

/// Whether this invocation needs a reachable Ollama (exec, review, resume, the TUI, ...).
pub(crate) fn needs_preflight(args: &[OsString], env: EnvFn) -> bool {
    if env("CODEX_LOCAL_SKIP_PREFLIGHT").is_some_and(|v| v != "0") {
        return false;
    }
    let mut it = args.iter().skip(1).map(|a| a.to_string_lossy());
    while let Some(a) = it.next() {
        match a.as_ref() {
            "--" => return true,
            "-h" | "--help" | "-V" | "--version" => return false,
            f if VALUE_FLAGS.contains(&f) => {
                it.next();
            }
            f if f.starts_with('-') => {}
            sub => return !SKIP.contains(&sub),
        }
    }
    true
}

/// `GET <root>/v1/models`; the error is a short human string.
pub fn check(root: &str) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let r = reqwest::Client::new()
            .get(format!("{root}/v1/models"))
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    "timed out after 3s".to_string()
                } else if e.is_connect() {
                    "connection refused or unreachable".to_string()
                } else {
                    e.without_url().to_string()
                }
            })?;
        // Any HTTP answer means something is listening.
        drop(r);
        Ok(())
    })
}

pub fn failure_message(root: &str, err: &str) -> String {
    format!(
        "codex-local: can't reach the local model server at {root} ({err}). Is it running? \
         Set --engine, --base-url or CODEX_LOCAL_URL."
    )
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
        .map(|rt| rt.block_on(f))
}

fn list(root: &str, timeout: Duration) -> Result<Vec<V1Model>, String> {
    block_on(list_v1_models(&reqwest::Client::new(), root, timeout))
        .unwrap_or_else(|| Err("no runtime".into()))
}

/// First `/v1/models` id (llama.cpp's default model).
pub(crate) fn first_model_id(root: &str) -> Option<String> {
    list(root, TIMEOUT).ok()?.into_iter().next().map(|m| m.id)
}

/// First candidate engine whose root lists `model`; each probe gets `timeout`.
pub(crate) fn find_model_elsewhere(
    model: &str,
    candidates: &[(Engine, String)],
    timeout: Duration,
) -> Option<Engine> {
    candidates.iter().find_map(|(e, root)| {
        list(root, timeout)
            .ok()?
            .iter()
            .any(|m| m.id == model)
            .then_some(*e)
    })
}

/// Engine never chosen explicitly: if `root` answers but lacks `model`, probe the other
/// two engines' default ports (1 s each) and return a hint to print.
pub fn engine_hint(root: &str, model: &str) -> Option<String> {
    let others: Vec<_> = Engine::ALL
        .into_iter()
        .filter(|e| *e != Engine::Ollama)
        .map(|e| (e, format!("http://127.0.0.1:{}", e.default_port())))
        .collect();
    hint_with(root, model, &others)
}

fn hint_with(root: &str, model: &str, others: &[(Engine, String)]) -> Option<String> {
    let here = list(root, TIMEOUT).ok()?;
    if here.iter().any(|m| m.id == model) {
        return None;
    }
    let e = find_model_elsewhere(model, others, Duration::from_secs(1))?;
    Some(format!(
        "codex-local: model `{model}` not found at {root}, but {e} serves it; \
         use --engine {e} or set `[local] engine = \"{e}\"` in config.toml."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn allowlist_decision() {
        let none = |_: &str| None;
        assert!(needs_preflight(&a(&["x"]), &none));
        assert!(needs_preflight(&a(&["x", "exec", "hi"]), &none));
        assert!(needs_preflight(&a(&["x", "-m", "login", "exec"]), &none));
        assert!(!needs_preflight(&a(&["x", "login"]), &none));
        assert!(!needs_preflight(
            &a(&["x", "-c", "a=b", "mcp", "list"]),
            &none
        ));
        assert!(!needs_preflight(&a(&["x", "--help"]), &none));
        let skip = |k: &str| (k == "CODEX_LOCAL_SKIP_PREFLIGHT").then(|| "1".to_string());
        assert!(!needs_preflight(&a(&["x", "exec"]), &skip));
    }

    #[test]
    fn closed_port_fails_fast() {
        let t = std::time::Instant::now();
        let e = check("http://127.0.0.1:1").unwrap_err();
        assert!(t.elapsed() < Duration::from_secs(4), "{e}");
    }

    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    async fn serve(ids: &[&str]) -> MockServer {
        let s = MockServer::start().await;
        let data: Vec<_> = ids.iter().map(|i| serde_json::json!({"id": i})).collect();
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": data})),
            )
            .mount(&s)
            .await;
        s
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn preflight_uses_v1_models_and_two_port_probe_finds_engine() {
        let (here, lm, llama) = (
            serve(&["a"]).await,
            serve(&["m"]).await,
            serve(&["z"]).await,
        );
        let (h, l, p) = (here.uri(), lm.uri(), llama.uri());
        let out = tokio::task::spawn_blocking(move || {
            assert!(check(&h).is_ok());
            assert_eq!(first_model_id(&p).as_deref(), Some("z"));
            let others = [(Engine::LmStudio, l), (Engine::LlamaCpp, p)];
            let found = hint_with(&h, "m", &others);
            assert!(hint_with(&h, "a", &others).is_none());
            assert!(hint_with(&h, "nope", &others).is_none());
            found
        })
        .await
        .unwrap();
        assert!(out.unwrap().contains("--engine lmstudio"));
    }
}
