//! Which local inference engine codex-local talks to.

use std::fmt;
use std::str::FromStr;

/// Default model for LM Studio (the default engine): the 4-bit MLX build
/// `lmstudio-community/Qwen3.8-27B-MLX-4bit`. The 8-bit build stays usable with
/// `-m qwen3.8-27b-mlx`. Change it here only.
pub const LMSTUDIO_DEFAULT_MODEL: &str = "qwen/qwen3.8-27b";

/// Earlier LM Studio default (the 8-bit build). Homes seeded with it are treated as unset.
pub const LMSTUDIO_LEGACY_DEFAULT_MODEL: &str = "qwen3.8-27b-mlx";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Engine {
    Ollama,
    #[default]
    LmStudio,
    LlamaCpp,
}

impl Engine {
    pub const ALL: [Engine; 3] = [Engine::Ollama, Engine::LmStudio, Engine::LlamaCpp];

    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Ollama => "ollama",
            Engine::LmStudio => "lmstudio",
            Engine::LlamaCpp => "llamacpp",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Engine::Ollama => 11434,
            Engine::LmStudio => 1234,
            Engine::LlamaCpp => 8080,
        }
    }

    pub fn default_root(self) -> String {
        format!("http://127.0.0.1:{}", self.default_port())
    }

    /// Whether `root` is some engine's built-in default root (either loopback spelling;
    /// older homes were seeded with `localhost`). Such a root in `config.toml` is a seed,
    /// not a user choice, so it must not beat the winning engine's own port.
    pub fn is_default_root(root: &str) -> bool {
        Engine::ALL.iter().any(|e| {
            let p = e.default_port();
            root == format!("http://127.0.0.1:{p}") || root == format!("http://localhost:{p}")
        })
    }

    /// Whether `model` is the built-in default model of some engine (a seed, not a choice).
    pub fn is_default_model(model: &str) -> bool {
        model == LMSTUDIO_LEGACY_DEFAULT_MODEL
            || Engine::ALL.iter().any(|e| e.default_model() == Some(model))
    }

    /// `None` for llama.cpp: it serves whatever it loaded (first `/v1/models` id).
    pub fn default_model(self) -> Option<&'static str> {
        match self {
            Engine::Ollama => Some("qwen3.8:27b"),
            Engine::LmStudio => Some(LMSTUDIO_DEFAULT_MODEL),
            Engine::LlamaCpp => None,
        }
    }

    /// Seeded `[agents] max_threads`.
    pub fn default_max_threads(self) -> i64 {
        if self == Engine::LmStudio { 7 } else { 6 }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Engine {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ollama" => Ok(Engine::Ollama),
            "lmstudio" | "lm-studio" => Ok(Engine::LmStudio),
            "llamacpp" | "llama.cpp" | "llama-cpp" => Ok(Engine::LlamaCpp),
            other => Err(format!(
                "unknown engine `{other}` (expected ollama, lmstudio or llamacpp)"
            )),
        }
    }
}

/// Precedence: flag > env > config > default (`LmStudio`). The bool is whether any
/// source set it explicitly.
pub fn resolve_engine(
    flag: Option<&str>,
    env: Option<&str>,
    config: Option<&str>,
) -> Result<(Engine, bool), String> {
    match flag.or(env).or(config) {
        Some(v) => v.parse().map(|e| (e, true)),
        None => Ok((Engine::default(), false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_new_lmstudio_defaults_both_read_as_seeds() {
        assert!(Engine::is_default_model("qwen3.8-27b-mlx"));
        assert!(Engine::is_default_model(LMSTUDIO_DEFAULT_MODEL));
        assert_eq!(LMSTUDIO_DEFAULT_MODEL, "qwen/qwen3.8-27b");
        assert!(!Engine::is_default_model("something-else"));
    }

    #[test]
    fn parse_and_precedence() {
        assert_eq!("LMStudio".parse(), Ok(Engine::LmStudio));
        assert!("vllm".parse::<Engine>().is_err());
        let r = resolve_engine;
        assert_eq!(r(None, None, None), Ok((Engine::LmStudio, false)));
        assert_eq!(r(None, None, Some("ollama")), Ok((Engine::Ollama, true)));
        assert_eq!(
            r(None, None, Some("lmstudio")),
            Ok((Engine::LmStudio, true))
        );
        assert_eq!(
            r(None, Some("llamacpp"), Some("lmstudio")),
            Ok((Engine::LlamaCpp, true))
        );
        assert_eq!(
            r(Some("ollama"), Some("llamacpp"), Some("lmstudio")),
            Ok((Engine::Ollama, true))
        );
        assert!(r(None, None, Some("bogus")).is_err());
    }
}
