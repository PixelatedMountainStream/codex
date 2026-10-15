//! Fail-fast check that Ollama answers before upstream starts its retry loops.

use std::ffi::OsString;
use std::time::Duration;

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

/// `GET <root>/api/tags`; the error is a short human string.
pub fn check(root: &str) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let r = reqwest::Client::new()
            .get(format!("{root}/api/tags"))
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
        "codex-local: can't reach Ollama at {root} ({err}). Is it running? \
         Set --ollama-url or CODEX_LOCAL_OLLAMA_URL."
    )
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
}
