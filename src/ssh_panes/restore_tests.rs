use super::*;

#[cfg(unix)]
#[tokio::test]
async fn ssh_panes_cold_restore_reconnects_only_when_enabled_and_keeps_next_capture_argv() {
    let _table = ssh_probe::TEST_TABLE.lock().await;
    use ssh_probe::test_support::{capture_next_spawn, take_spawn};
    let mut state = crate::app::AppState::test_new();
    state.workspaces = vec![crate::workspace::Workspace::test_new("ssh-restore")];
    state.ensure_test_terminals();
    let pane = state.workspaces[0].tabs[0].root_pane;
    let terminal = state.workspaces[0].terminal_id(pane).unwrap().clone();
    let argv: Vec<String> = ["/bin/herdr", "ssh", "studio", "--cwd", "/remote"]
        .into_iter()
        .map(str::to_string)
        .collect();
    state.terminals.get_mut(&terminal).unwrap().cwd = std::env::temp_dir();
    state.terminals.get_mut(&terminal).unwrap().launch_argv = Some(argv.clone());
    let snapshot = crate::persist::capture(
        &state.workspaces,
        &state.terminals,
        &crate::terminal::TerminalRuntimeRegistry::default(),
        Some(0),
        0,
    );
    let public = crate::workspace::public_pane_id_for_number(
        &state.workspaces[0].id,
        state.workspaces[0].public_pane_number(pane).unwrap(),
    );
    for enabled in [true, false] {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        capture_next_spawn();
        let (workspaces, terminals, runtimes) = restore(
            &config::SshPanesConfig {
                enabled,
                ..Default::default()
            },
            &snapshot,
            None,
            24,
            80,
            0,
            "/bin/sh",
            crate::config::ShellModeConfig::NonLogin,
            false,
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        );
        let (guard, cmd) = take_spawn();
        assert_eq!(cmd.get_env("HERDR_PANE_ID").unwrap(), public.as_str());
        let new_pane = workspaces[0].tabs[0].root_pane;
        assert_ne!(new_pane, pane);
        let terminal = &terminals[workspaces[0].terminal_id(new_pane).unwrap()];
        if enabled {
            assert_eq!(
                cmd.get_argv(),
                &argv
                    .iter()
                    .map(std::ffi::OsString::from)
                    .collect::<Vec<_>>()
            );
            assert_eq!(cmd.get_env("HERDR_SSH_PANES_SPAWNED").unwrap(), "1");
            assert_eq!(cmd.get_cwd().unwrap(), &std::env::var_os("HOME").unwrap());
            assert_eq!(terminal.launch_argv.as_ref(), Some(&argv));
            let captured = crate::persist::capture(
                &workspaces,
                &terminals,
                &crate::terminal::TerminalRuntimeRegistry::default(),
                Some(0),
                0,
            );
            assert_eq!(
                captured.workspaces[0].tabs[0].panes[&new_pane.raw()]
                    .launch_argv
                    .as_ref(),
                Some(&argv)
            );
        } else {
            assert_eq!(cmd.get_argv()[0], "/bin/sh");
            assert!(cmd.get_env("HERDR_SSH_PANES_SPAWNED").is_none());
            assert!(terminal.launch_argv.is_none());
        }
        for (_, runtime) in runtimes {
            runtime.preserve_for_handoff();
        }
        drop(guard);
    }
    let mut unrelated = portable_pty::CommandBuilder::new("/bin/sh");
    unrelated.env("HERDR_PANE_ID", public);
    apply_restore_command(&mut unrelated).unwrap();
    assert_eq!(unrelated.get_argv()[0], "/bin/sh");
}
