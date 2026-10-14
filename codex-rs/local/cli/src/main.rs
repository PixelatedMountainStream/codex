//! `codex-local`: seeds an isolated home and delegates to upstream's CLI (hook H1).
//!
//! Upstream `codex-rs/cli/src/main.rs` is included as this crate's root so its private
//! `cli_main(argv, ..)` is callable with codex-local's own argv; `build.rs` sets
//! `cfg(codex_local)`, which disables upstream's stock `fn main`.

include!("../../../cli/src/main.rs");

fn main() -> anyhow::Result<()> {
    codex_build_info::initialize!();
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let action = codex_local_cli::prepare(args, &codex_local_cli::process_env, codex_local_cli::user_home_dir())?;
    let (argv, codex_home) = match action {
        codex_local_cli::Action::Version(v) => {
            println!("{v}");
            return Ok(());
        }
        codex_local_cli::Action::SyncConfig => return Ok(()),
        codex_local_cli::Action::Run { argv, codex_home } => (argv, codex_home),
    };
    // SAFETY: still single-threaded; no runtime or worker threads exist yet.
    unsafe { std::env::set_var("CODEX_HOME", &codex_home) };
    let remote_control_disabled = codex_app_server::take_remote_control_disabled_env();
    arg0_dispatch_or_else(move |arg0_paths: Arg0DispatchPaths| async move {
        Box::pin(cli_main(argv, arg0_paths, remote_control_disabled)).await?;
        Ok(())
    })
}
