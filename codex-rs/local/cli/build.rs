use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(codex_local)");
    // Switches off the stock `main` in the upstream file this crate includes (H1).
    println!("cargo:rustc-cfg=codex_local");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-ObjC");
    }
    println!("cargo:rerun-if-env-changed=CODEX_LOCAL_GIT_SHA");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    if let Some(head) = git(&manifest, &["rev-parse", "--git-path", "HEAD"]) {
        let head = Path::new(&manifest).join(head);
        println!("cargo:rerun-if-changed={}", head.display());
    }
    let sha = std::env::var("CODEX_LOCAL_GIT_SHA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| git(&manifest, &["rev-parse", "--short=7", "HEAD"]))
        .map(|s| s.trim().chars().take(7).collect::<String>())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CODEX_LOCAL_SHA7={sha}");
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}
