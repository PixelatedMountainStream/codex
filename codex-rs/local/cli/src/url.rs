use anyhow::Result;
use anyhow::bail;

/// Single owner of Ollama URL normalization: default scheme `http`, `0.0.0.0` becomes
/// `127.0.0.1`, a trailing `/` or `/v1` is stripped. A scheme-less value with no port
/// gets Ollama's default port 11434 (matching `OLLAMA_HOST` semantics).
pub fn resolve_ollama_root(raw: &str) -> Result<String> {
    let t = raw.trim();
    if t.is_empty() {
        bail!("empty Ollama URL");
    }
    let (scheme, rest, had_scheme) = match t.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r, true),
        None => ("http".to_string(), t, false),
    };
    let (auth, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let (userinfo, hostport) = match auth.rfind('@') {
        Some(i) => (&auth[..=i], &auth[i + 1..]),
        None => ("", auth),
    };
    let (host, port) = split_host_port(hostport);
    if host.is_empty() {
        bail!("Ollama URL `{raw}` has no host");
    }
    let host = match host {
        "0.0.0.0" => "127.0.0.1",
        "[::]" => "[::1]",
        h => h,
    };
    let port = match port {
        Some(p) => format!(":{p}"),
        None if !had_scheme => ":11434".to_string(),
        None => String::new(),
    };
    let mut path = path.trim_end_matches('/');
    if let Some(p) = path.strip_suffix("/v1") {
        path = p.trim_end_matches('/');
    }
    Ok(format!("{scheme}://{userinfo}{host}{port}{path}"))
}

fn split_host_port(hp: &str) -> (&str, Option<&str>) {
    if hp.starts_with('[') {
        return match hp.find(']') {
            Some(i) => (&hp[..=i], hp[i + 1..].strip_prefix(':')),
            None => (hp, None),
        };
    }
    match hp.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() => (h, Some(p)),
        Some((h, _)) => (h, None),
        None => (hp, None),
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_ollama_root as r;

    #[test]
    fn normalizes() {
        let cases = [
            ("localhost:11434", "http://localhost:11434"),
            ("127.0.0.1", "http://127.0.0.1:11434"),
            ("0.0.0.0:11435", "http://127.0.0.1:11435"),
            ("http://0.0.0.0:11434/", "http://127.0.0.1:11434"),
            ("http://h:1/v1", "http://h:1"),
            ("http://h:1/v1/", "http://h:1"),
            ("https://h/proxy/v1", "https://h/proxy"),
            ("HTTPS://h", "https://h"),
            ("http://[::]:5", "http://[::1]:5"),
            ("  http://h:1  ", "http://h:1"),
        ];
        for (input, want) in cases {
            assert_eq!(r(input).unwrap(), want, "{input}");
        }
        assert!(r("  ").is_err());
        assert!(r("http://:5").is_err());
    }
}
