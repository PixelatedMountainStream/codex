//! `codex-local`: seeds an isolated home and delegates to upstream's CLI (hook H1).
//!
//! Upstream `codex-rs/cli/src/main.rs` is included as this crate's root so its private
//! `cli_main(argv, ..)` is callable with codex-local's own argv; `build.rs` sets
//! `cfg(codex_local)`, which disables upstream's stock `fn main`.

include!("../../../cli/src/main.rs");

/// codex-local registers its resume/model hooks in-process, so the TUI must run
/// in-process too; upstream's app-server daemon would drop them and needs a package
/// codex-local does not ship. Insert `--no-daemon` for plain TUI, `resume` and `fork`
/// unless the user chose `--remote` or already passed the flag. Anything that does
/// not parse (help, errors) is passed through unchanged.
fn inject_no_daemon(argv: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let Ok(cli) = MultitoolCli::try_parse_from(argv.clone()) else {
        return argv;
    };
    let (eligible, remote, sub_no_daemon) = match &cli.subcommand {
        None => (true, false, false),
        Some(Subcommand::Resume(c)) => (
            true,
            c.remote.remote.is_some(),
            c.config_overrides.0.no_daemon,
        ),
        Some(Subcommand::Fork(c)) => (
            true,
            c.remote.remote.is_some(),
            c.config_overrides.0.no_daemon,
        ),
        Some(_) => (false, false, false),
    };
    if !eligible
        || remote
        || cli.remote.remote.is_some()
        || cli.interactive.no_daemon
        || sub_no_daemon
        || argv.is_empty()
    {
        return argv;
    }
    let mut out = argv;
    out.insert(1, "--no-daemon".into());
    out
}

fn main() -> anyhow::Result<()> {
    codex_build_info::initialize!();
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let action = codex_local_cli::prepare(
        args,
        &codex_local_cli::process_env,
        codex_local_cli::user_home_dir(),
    )?;
    let (argv, codex_home, preflight_root, probe_model) = match action {
        codex_local_cli::Action::Version(v) => {
            println!("{v}");
            return Ok(());
        }
        codex_local_cli::Action::SyncConfig => return Ok(()),
        codex_local_cli::Action::Run {
            argv,
            codex_home,
            preflight_root,
            probe_model,
        } => (argv, codex_home, preflight_root, probe_model),
    };
    if let Some(root) = &preflight_root
        && let Err(e) = codex_local_cli::preflight_check(root)
    {
        eprintln!("{}", codex_local_cli::preflight_failure_message(root, &e));
        std::process::exit(1);
    }
    if let (Some(root), Some(model)) = (&preflight_root, &probe_model)
        && let Some(hint) = codex_local_cli::engine_hint(root, model)
    {
        eprintln!("{hint}");
    }
    // SAFETY: still single-threaded; no runtime or worker threads exist yet.
    unsafe { std::env::set_var("CODEX_HOME", &codex_home) };
    codex_local_models::register();
    codex_local_resume::register();
    let remote_control_disabled = codex_app_server::take_remote_control_disabled_env();
    arg0_dispatch_or_else(move |arg0_paths: Arg0DispatchPaths| async move {
        Box::pin(cli_main(
            inject_no_daemon(argv),
            arg0_paths,
            remote_control_disabled,
        ))
        .await?;
        Ok(())
    })
}

#[cfg(test)]
mod no_daemon_tests {
    use super::inject_no_daemon;

    #[test]
    fn injection_decision_table() {
        let cases: &[(&[&str], bool)] = &[
            (&["x"], true),
            (&["x", "fix the bug"], true),
            (&["x", "resume", "0192-abc"], true),
            (&["x", "resume", "--last"], true),
            (&["x", "fork", "--last"], true),
            (&["x", "--no-daemon"], false),
            (&["x", "resume", "--last", "--no-daemon"], false),
            (&["x", "--remote", "ws://h:1"], false),
            (&["x", "resume", "--last", "--remote", "ws://h:1"], false),
            (&["x", "exec", "hi"], false),
            (&["x", "app-server"], false),
            (&["x", "app-server", "daemon", "start"], false),
            (&["x", "--help"], false),
            (&["x", "--bogus"], false),
            (&["x", "--", "exec"], true),
        ];
        for (args, injected) in cases {
            let argv: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
            let out = inject_no_daemon(argv.clone());
            if *injected {
                let mut want = argv;
                want.insert(1, "--no-daemon".into());
                assert_eq!(out, want, "{args:?}");
            } else {
                assert_eq!(out, argv, "{args:?}");
            }
        }
    }
}
