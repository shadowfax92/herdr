use super::*;
use crate::{
    api::schema::{Method, Request},
    config::Config,
    detect::{Agent, AgentState},
    layout::PaneId,
    ssh_panes::api::{AttachParams, RemoteForeground, ReportParams},
};

fn app_with_pane() -> (App, PaneId, TerminalId) {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        crate::app::AppPolicy::TEST,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.state = crate::app::AppState::test_new();
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("ssh-core")];
    app.state.active = Some(0);
    app.state.ensure_test_terminals();
    let pane = app.state.workspaces[0].tabs[0].root_pane;
    let terminal = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
    app.state.terminals.get_mut(&terminal).unwrap().cwd = "/local".into();
    app.apply_ssh_panes_config(&SshPanesConfig {
        enabled: true,
        ..Default::default()
    });
    (app, pane, terminal)
}
fn attach(app: &mut App, pane: PaneId) -> serde_json::Value {
    let pane_id = app.public_pane_id(0, pane).unwrap();
    let response = app.handle_ssh_panes(
        "attach".into(),
        SshPanesRequest::Attach(AttachParams {
            pane_id,
            link_id: "one".into(),
            host: "studio".into(),
            connector_pid: 42,
            connector_pgid: 42,
            cwd_hint: Some("/hint".into()),
            protocol: 1,
        }),
    );
    serde_json::from_str(&response).unwrap()
}
fn report(seq: u64) -> ReportParams {
    ReportParams {
        link_id: "one".into(),
        seq,
        shell_pid: 80,
        shell_name: "fish".into(),
        foreground: RemoteForeground {
            process_group_id: Some(90),
            foreground_is_pane_shell: false,
            agent: Some("claude".into()),
            process_name: Some("claude".into()),
        },
        cwd: Some("/does-not-exist-locally".into()),
        foreground_cwd: Some("/remote/job".into()),
    }
}
fn send(app: &mut App, request: SshPanesRequest) -> serde_json::Value {
    serde_json::from_str(&app.handle_ssh_panes("id".into(), request)).unwrap()
}

#[test]
fn ssh_panes_api_projects_remote_cwd_without_mutating_local_state_and_heartbeats_are_quiet() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
    let (mut app, pane, terminal) = app_with_pane();
    let stock = serde_json::to_vec(&app.pane_metadata(0, pane).unwrap()).unwrap();
    let response = attach(&mut app, pane);
    assert_eq!(response["result"]["ssh_panes"]["lease_ms"], 15000);
    assert_eq!(
        response["result"]["ssh_panes"]["link"]["last_report_ms_ago"],
        serde_json::Value::Null
    );
    let info = app.pane_metadata(0, pane).unwrap();
    assert_eq!(info.cwd.as_deref(), Some("/hint"));
    assert_eq!(info.ssh.unwrap().local_cwd.as_deref(), Some("/local"));
    let sequence = app.event_hub.current_sequence();
    attach(&mut app, pane);
    assert_eq!(app.event_hub.current_sequence(), sequence);
    send(&mut app, SshPanesRequest::Report(report(1)));
    assert_eq!(app.event_hub.current_sequence(), sequence + 1);
    let info = app.pane_metadata(0, pane).unwrap();
    assert_eq!(info.cwd.as_deref(), Some("/does-not-exist-locally"));
    assert_eq!(info.foreground_cwd.as_deref(), Some("/remote/job"));
    assert_eq!(info.ssh.unwrap().host, "studio");
    assert_eq!(app.state.terminals[&terminal].cwd.to_str(), Some("/local"));
    send(&mut app, SshPanesRequest::Report(report(2)));
    send(&mut app, SshPanesRequest::Report(report(1)));
    attach(&mut app, pane);
    assert_eq!(app.event_hub.current_sequence(), sequence + 1);
    let mut changed = report(3);
    changed.foreground.agent = Some("codex".into());
    send(&mut app, SshPanesRequest::Report(changed));
    assert_eq!(app.event_hub.current_sequence(), sequence + 2);
    app.apply_ssh_panes_config(&SshPanesConfig::default());
    let restored = app.pane_metadata(0, pane).unwrap();
    assert_eq!(serde_json::to_vec(&restored).unwrap(), stock);
    assert!(restored.ssh.is_none());
    assert_eq!(restored.cwd.as_deref(), Some("/local"));
    assert_eq!(
        send(&mut app, SshPanesRequest::Report(report(4)))["error"]["code"],
        "ssh_panes_disabled"
    );
}

#[test]
fn ssh_panes_api_rejects_missing_pane_and_filters_list_by_public_identity() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
    let (mut app, pane, _) = app_with_pane();
    let missing = SshPanesRequest::Attach(AttachParams {
        pane_id: "wZZ:pZZ".into(),
        link_id: "bad".into(),
        host: "studio".into(),
        connector_pid: 42,
        connector_pgid: 42,
        cwd_hint: None,
        protocol: 1,
    });
    assert_eq!(send(&mut app, missing)["error"]["code"], "pane_not_found");
    attach(&mut app, pane);
    let pane_id = app.public_pane_id(0, pane).unwrap();
    let response = send(
        &mut app,
        SshPanesRequest::List(crate::ssh_panes::api::ListParams {
            pane_id: Some(pane_id.clone()),
        }),
    );
    assert_eq!(
        response["result"]["ssh_panes"]["links"][0]["pane_id"],
        pane_id
    );
    send(
        &mut app,
        SshPanesRequest::Detach(crate::ssh_panes::api::DetachParams {
            link_id: "one".into(),
            exit_code: Some(0),
        }),
    );
    assert!(app.pane_metadata(0, pane).unwrap().ssh.is_none());
}

#[test]
fn ssh_panes_snapshot_scrubs_remote_resumes_but_keeps_connector_reconnect_argv() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
    let (mut app, pane, _) = app_with_pane();
    attach(&mut app, pane);
    let mut snapshot = crate::persist::capture(
        &app.state.workspaces,
        &app.state.terminals,
        &app.terminal_runtimes,
        app.state.active,
        app.state.selected,
    );
    let saved = snapshot.workspaces[0].tabs[0]
        .panes
        .get_mut(&pane.raw())
        .unwrap();
    saved.agent_session = Some(serde_json::from_value(serde_json::json!({"source":"herdr:claude","agent":"claude","kind":"id","value":"remote-session"})).unwrap());
    saved.agent_resume = Some(serde_json::from_value(serde_json::json!({"source":"remote","agent":"claude","argv":["claude","--resume","remote-session"]})).unwrap());
    saved.managed_agent_kind = Some("claude".into());
    let argv: Vec<String> = ["/bin/herdr", "ssh", "studio", "--cwd", "/remote"]
        .into_iter()
        .map(str::to_string)
        .collect();
    saved.launch_argv = Some(argv.clone());
    app.scrub_ssh_pane_snapshot(&mut snapshot);
    let saved = snapshot.workspaces[0].tabs[0]
        .panes
        .get_mut(&pane.raw())
        .unwrap();
    assert!(
        saved.agent_session.is_none()
            && saved.agent_resume.is_none()
            && saved.managed_agent_kind.is_none()
    );
    assert_eq!(saved.cwd.to_str(), Some("/local"));
    assert_eq!(saved.launch_argv.as_ref(), Some(&argv));
    saved.launch_argv = Some(vec![
        "claude".into(),
        "--resume".into(),
        "remote-session".into(),
    ]);
    app.scrub_ssh_pane_snapshot(&mut snapshot);
    assert!(snapshot.workspaces[0].tabs[0].panes[&pane.raw()]
        .launch_argv
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn ssh_panes_prompt_uses_remote_foreground_even_with_a_local_connector_process() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.lock().await;
    let (mut app, pane, terminal) = app_with_pane();
    let (runtime, _events, guard) =
        crate::pane::ssh_probe::test_support::normal_terminal_runtime(pane);
    assert!(runtime.child_pid().is_some()); // avoids runtime_hosts_agent's channel-only test shortcut
    let key = runtime.ssh_probe_key();
    app.terminal_runtimes.insert(terminal.clone(), runtime);
    attach(&mut app, pane);
    crate::pane::ssh_probe::test_support::foreground(key, Some(42));
    send(&mut app, SshPanesRequest::Report(report(1)));
    let state = app.state.terminals.get_mut(&terminal).unwrap();
    state.detected_agent = Some(Agent::Claude);
    state.state = AgentState::Idle;
    let info = app.agent_info(0, pane).unwrap();
    assert_eq!(info.ssh.unwrap().host, "studio");
    assert_eq!(info.cwd.as_deref(), Some("/does-not-exist-locally"));
    let target = app.public_pane_id(0, pane).unwrap();
    let request = || Request {
        id: "prompt".into(),
        method: Method::AgentPrompt(
            serde_json::from_value(serde_json::json!({"target":target,"text":"say ok"})).unwrap(),
        ),
    };
    let response: serde_json::Value = {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(request(), tx));
        serde_json::from_str(&rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap()).unwrap()
    };
    assert!(response.get("error").is_none(), "{response}");
    let mut shell = report(2);
    shell.foreground.agent = None;
    shell.foreground.foreground_is_pane_shell = true;
    send(&mut app, SshPanesRequest::Report(shell));
    let response: serde_json::Value = {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(request(), tx));
        serde_json::from_str(&rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap()).unwrap()
    };
    assert_eq!(response["error"]["code"], "agent_not_ready");
    app.terminal_runtimes
        .remove(&terminal)
        .unwrap()
        .preserve_for_handoff();
    drop(guard);
}

#[test]
fn ssh_panes_explicit_tab_cwd_and_inheritance_config_delegate_to_stock() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
    let (mut app, pane, _) = app_with_pane();
    attach(&mut app, pane);
    let env = std::collections::HashMap::new();
    assert!(app
        .ssh_panes_tab_create("id", 0, Some("/local"), &env, true, None)
        .is_none());
    app.apply_ssh_panes_config(&SshPanesConfig {
        enabled: true,
        inherit_new_tab: false,
        ..Default::default()
    });
    assert!(app
        .ssh_panes_tab_create("id", 0, None, &env, true, None)
        .is_none());
    app.apply_ssh_panes_config(&SshPanesConfig::default());
    let params = serde_json::from_value(serde_json::json!({"direction":"right"})).unwrap();
    assert!(app
        .ssh_panes_split("id", 0, pane, &params, 24, 80)
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn ssh_panes_split_and_tab_use_workspace_creation_with_remote_argv_and_local_home() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.lock().await;
    use crate::pane::ssh_probe::test_support::{capture_next_spawn, take_spawn};
    let (mut app, pane, _) = app_with_pane();
    attach(&mut app, pane);
    send(&mut app, SshPanesRequest::Report(report(1)));
    let params = serde_json::from_value(serde_json::json!({"direction":"right", "ratio":0.3, "focus":false, "cwd":"/explicit remote", "env":{"KEEP":"value"}, "right_click":"pane"})).unwrap();
    capture_next_spawn();
    let response: serde_json::Value = serde_json::from_str(
        &app.ssh_panes_split("split", 0, pane, &params, 24, 80)
            .unwrap(),
    )
    .unwrap();
    let (split_guard, cmd) = take_spawn();
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        &cmd.get_argv()[1..],
        &["ssh", "studio", "--cwd", "/explicit remote"]
    );
    assert_eq!(cmd.get_cwd().unwrap(), &std::env::var_os("HOME").unwrap());
    assert_eq!(cmd.get_env("KEEP").unwrap(), "value");
    assert_eq!(cmd.get_env("HERDR_SSH_PANES_SPAWNED").unwrap(), "1");
    assert!(cmd.get_env("HERDR_SSH_PANES_INHERIT").is_none());
    assert_eq!(
        cmd.get_env("HERDR_PANE_ID").unwrap().to_str().unwrap(),
        response["result"]["pane"]["pane_id"].as_str().unwrap()
    );
    assert_eq!(app.state.workspaces[0].focused_pane_id(), Some(pane));
    match &app.state.workspaces[0].tabs[0].layout.root() {
        crate::layout::Node::Split { ratio, .. } => assert_eq!(*ratio, 0.3),
        _ => panic!("split should preserve the requested ratio"),
    }
    let remote_terminal = app
        .state
        .terminals
        .values()
        .find(|t| t.launch_argv.is_some())
        .unwrap();
    assert_eq!(
        &remote_terminal.launch_argv.as_ref().unwrap()[1..],
        &["ssh", "studio", "--cwd", "/explicit remote"]
    );
    assert_eq!(
        remote_terminal.cwd,
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
    );
    capture_next_spawn();
    let response: serde_json::Value = serde_json::from_str(
        &app.ssh_panes_tab_create(
            "tab",
            0,
            None,
            &std::collections::HashMap::new(),
            false,
            Some("remote tab"),
        )
        .unwrap(),
    )
    .unwrap();
    let (tab_guard, cmd) = take_spawn();
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        &cmd.get_argv()[1..],
        &["ssh", "studio", "--cwd", "/remote/job"]
    );
    assert_eq!(app.state.workspaces[0].active_tab, 0);
    assert_eq!(
        app.state.workspaces[0].tabs[1].custom_name.as_deref(),
        Some("remote tab")
    );
    let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
    for (_, runtime) in runtimes {
        runtime.preserve_for_handoff();
    }
    drop(split_guard);
    drop(tab_guard);
}

#[cfg(unix)]
#[tokio::test]
async fn ssh_panes_opt_out_spawns_stock_local_shell_and_strips_control_flag() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.lock().await;
    use crate::pane::ssh_probe::test_support::{capture_next_spawn, take_spawn};
    let (mut app, pane, _) = app_with_pane();
    attach(&mut app, pane);
    // Use an existing local directory: the explicit cwd is local after opt-out.
    let local = std::env::temp_dir();
    let params = serde_json::from_value(serde_json::json!({"direction":"down", "focus":false, "cwd":local, "env":{"HERDR_SSH_PANES_INHERIT":"0","KEEP":"value"}})).unwrap();
    capture_next_spawn();
    let response: serde_json::Value = serde_json::from_str(
        &app.ssh_panes_split("split", 0, pane, &params, 24, 80)
            .unwrap(),
    )
    .unwrap();
    let (guard, cmd) = take_spawn();
    assert!(response.get("error").is_none(), "{response}");
    assert!(cmd.get_env("HERDR_SSH_PANES_INHERIT").is_none());
    assert!(cmd.get_env("HERDR_SSH_PANES_SPAWNED").is_none());
    assert_eq!(cmd.get_env("KEEP").unwrap(), "value");
    assert!(app
        .state
        .terminals
        .values()
        .all(|t| t.launch_argv.is_none()));
    for (_, runtime) in app.terminal_runtimes.drain() {
        runtime.preserve_for_handoff();
    }
    drop(guard);
}

#[test]
fn ssh_panes_expired_link_does_not_scrub_a_later_local_agent_session() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
    let (mut app, pane, terminal) = app_with_pane();
    let expired = Instant::now() - crate::ssh_panes::HARD_LEASE;
    app.ssh_panes
        .attach(
            AttachInput {
                params: AttachParams {
                    pane_id: app.public_pane_id(0, pane).unwrap(),
                    link_id: "old".into(),
                    host: "studio".into(),
                    connector_pid: 42,
                    connector_pgid: 42,
                    cwd_hint: None,
                    protocol: 1,
                },
                pane_id: pane,
                terminal_id: terminal,
                local_cwd: Some("/local".into()),
            },
            expired,
        )
        .unwrap();
    assert!(app.pane_metadata(0, pane).unwrap().ssh.is_none());
    let mut snapshot = crate::persist::capture(
        &app.state.workspaces,
        &app.state.terminals,
        &app.terminal_runtimes,
        Some(0),
        0,
    );
    let saved = snapshot.workspaces[0].tabs[0]
        .panes
        .get_mut(&pane.raw())
        .unwrap();
    saved.agent_session = Some(serde_json::from_value(serde_json::json!({"source":"herdr:claude","agent":"claude","kind":"id","value":"new-local-session"})).unwrap());
    saved.agent_resume = Some(serde_json::from_value(serde_json::json!({"source":"local","agent":"claude","argv":["claude","--resume","new-local-session"]})).unwrap());
    let before = serde_json::to_vec(&snapshot).unwrap();
    app.scrub_ssh_pane_snapshot(&mut snapshot);
    assert_eq!(serde_json::to_vec(&snapshot).unwrap(), before);
}
