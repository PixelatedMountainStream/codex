use std::path::Path;

use anyhow::Result;
use predicates::str::contains;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

#[test]
fn debug_prompt_input_rejects_local_provider_without_oss() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "--local-provider",
        "ollama",
        "debug",
        "prompt-input",
        "hello",
    ])
    .assert()
    .failure()
    .stderr(contains("--local-provider requires --oss"));

    Ok(())
}
