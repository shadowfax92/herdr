use super::*;
use crate::api::schema::{AgentPromptParams, AgentPromptWaitOptions, Method, Request};
use crate::protocol::{ClientKeyCode, ClientKeyKind, ClientPaneInputEvent};

fn prompt_test_server() -> (
    HeadlessServer,
    tokio::sync::mpsc::Receiver<Bytes>,
    String,
    crate::terminal::TerminalId,
) {
    let mut server = test_headless_server();
    let rx = install_focused_test_runtime(&mut server, b"");
    server.app.state.ensure_test_terminals();
    let _ = connect_matching_test_shell(&mut server, 1);
    let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
    let terminal_id = server.app.state.workspaces[0]
        .terminal_id(pane_id)
        .unwrap()
        .clone();
    let terminal = server.app.state.terminals.get_mut(&terminal_id).unwrap();
    terminal.set_detected_state(
        Some(crate::detect::Agent::OpenCode),
        crate::detect::AgentState::Idle,
    );
    terminal.set_agent_name("reviewer".to_owned());
    let target = server.app.public_pane_id(0, pane_id).unwrap();
    (server, rx, target, terminal_id)
}

fn human(server: &mut HeadlessServer, target: &str, event: ClientPaneInputEvent) {
    server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
        client_id: 1,
        pane_id: target.to_owned(),
        events: vec![event],
    });
}

fn enter() -> ClientPaneInputEvent {
    ClientPaneInputEvent::Key {
        code: ClientKeyCode::Enter,
        modifiers: 0,
        kind: ClientKeyKind::Press,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: false,
        physical_key_id: None,
        windows_record: None,
    }
}

fn prompt(
    server: &mut HeadlessServer,
    target: &str,
    text: &str,
    wait: Option<AgentPromptWaitOptions>,
) -> std::sync::mpsc::Receiver<String> {
    let (respond_to, rx) = std::sync::mpsc::channel();
    assert!(server.app.handle_deferred_agent_api_request(
        Request {
            id: text.to_owned(),
            method: Method::AgentPrompt(AgentPromptParams {
                target: target.to_owned(),
                text: text.to_owned(),
                wait
            }),
        },
        respond_to
    ));
    rx
}

#[tokio::test]
async fn prompt_hold_draft_timer_rearms_earlier_and_stale_wake_changes_nothing() {
    use crate::terminal::prompt_gate::{HumanInput, SETTLE};
    let (mut server, _input, target, terminal) = prompt_test_server();
    let now = Instant::now();
    server.app.state.agent_prompt_config.abandoned_draft_secs = 10;
    assert_eq!(server.app.next_prompt_gate_deadline(), None);
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], now);
    let first = server.app.prompt_holds_projection("test-boot");
    assert_eq!(first.holds[&target].held, 0);
    assert_eq!(
        server.app.next_prompt_gate_deadline(),
        Some(now + Duration::from_secs(10))
    );
    server.app.note_prompt_human_input(
        &terminal,
        &[HumanInput::Edit],
        now + Duration::from_secs(2),
    );
    assert_eq!(server.app.prompt_holds_projection("test-boot"), first);
    assert_eq!(
        server.app.next_prompt_gate_deadline(),
        Some(now + Duration::from_secs(10))
    );
    server
        .app
        .poll_due_prompt_gates(now + Duration::from_secs(10));
    assert_eq!(server.app.prompt_holds_projection("test-boot"), first);
    assert_eq!(
        server.app.next_prompt_gate_deadline(),
        Some(now + Duration::from_secs(12))
    );
    // Enter shortens the cached abandonment deadline immediately.
    let entered = now + Duration::from_secs(10);
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Clear], entered);
    assert_eq!(
        server.app.next_prompt_gate_deadline(),
        Some(entered + SETTLE)
    );
    server
        .app
        .poll_due_prompt_gates(entered + SETTLE - Duration::from_millis(1));
    assert_eq!(server.app.prompt_holds_projection("test-boot"), first);
    server.app.poll_due_prompt_gates(entered + SETTLE);
    assert!(server
        .app
        .prompt_holds_projection("test-boot")
        .holds
        .is_empty());
    assert_eq!(server.app.next_prompt_gate_deadline(), None);
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], entered + SETTLE);
    server
        .app
        .poll_due_prompt_gates(entered + SETTLE + Duration::from_secs(10));
    assert!(server
        .app
        .prompt_holds_projection("test-boot")
        .holds
        .is_empty());
    assert_eq!(server.app.next_prompt_gate_deadline(), None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_projection_obeys_agent_config_and_reset_lifetimes() {
    use crate::terminal::prompt_gate::HumanInput;
    let (mut server, _input, _target, terminal) = prompt_test_server();
    let now = Instant::now();
    server.app.state.terminals.get_mut(&terminal).unwrap().state =
        crate::detect::AgentState::Blocked;
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], now);
    assert!(server
        .app
        .prompt_holds_projection("test-boot")
        .holds
        .is_empty());
    assert_eq!(server.app.next_prompt_gate_deadline(), None);
    server.app.state.terminals.get_mut(&terminal).unwrap().state = crate::detect::AgentState::Idle;
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], now);
    assert_eq!(
        server.app.prompt_holds_projection("test-boot").holds.len(),
        1
    );
    server.app.state.agent_prompt_config.hold_while_drafting = false;
    server.app.configure_prompt_gate(&terminal);
    server.app.poll_prompt_gate(&terminal, now);
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], now);
    assert!(server
        .app
        .prompt_holds_projection("test-boot")
        .holds
        .is_empty());
    assert_eq!(server.app.next_prompt_gate_deadline(), None);
    server.app.state.agent_prompt_config.hold_while_drafting = true;
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], now);
    server
        .app
        .state
        .terminals
        .get_mut(&terminal)
        .unwrap()
        .set_detected_state(None, crate::detect::AgentState::Idle);
    server.app.poll_prompt_gate(&terminal, now);
    assert!(server
        .app
        .prompt_holds_projection("test-boot")
        .holds
        .is_empty());
    // Plain shells never acquire draft timers or appear in the projection.
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], now);
    assert_eq!(server.app.next_prompt_gate_deadline(), None);
    server
        .app
        .state
        .terminals
        .get_mut(&terminal)
        .unwrap()
        .set_detected_state(
            Some(crate::detect::Agent::OpenCode),
            crate::detect::AgentState::Idle,
        );
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], now);
    server.app.discard_all_prompt_gates("shutdown");
    assert!(server
        .app
        .prompt_holds_projection("test-boot")
        .holds
        .is_empty());
    assert_eq!(server.app.next_prompt_gate_deadline(), None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_count_expiry_keeps_draft_and_exposes_api_holding() {
    use crate::terminal::prompt_gate::{HumanInput, SETTLE};
    let (mut server, _input, target, terminal) = prompt_test_server();
    let now = Instant::now();
    server.app.state.agent_prompt_config.abandoned_draft_secs = 0;
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], now);
    assert_eq!(
        server.app.next_prompt_gate_deadline(),
        None,
        "indefinite draft needs no timer"
    );
    let one = prompt(&mut server, &target, "one", None);
    let response: serde_json::Value = serde_json::from_str(&one.try_recv().unwrap()).unwrap();
    assert_eq!(response["result"]["agent"]["holding"], true);
    let two = prompt(
        &mut server,
        &target,
        "two",
        Some(AgentPromptWaitOptions {
            submission_deadline: Some(now + Duration::from_secs(2)),
            until: Vec::new(),
            timeout_ms: None,
            submission_baseline: None,
        }),
    );
    assert_eq!(
        server.app.prompt_holds_projection("test-boot").holds[&target].held,
        2
    );
    server
        .app
        .poll_due_prompt_gates(now + Duration::from_secs(2));
    assert!(two.try_recv().unwrap().contains("timeout"));
    assert_eq!(
        server.app.prompt_holds_projection("test-boot").holds[&target].held,
        1
    );
    server.app.note_prompt_human_input(
        &terminal,
        &[HumanInput::Clear],
        now + Duration::from_secs(3),
    );
    assert_eq!(
        server.app.next_prompt_gate_deadline(),
        Some(now + Duration::from_secs(3) + SETTLE)
    );
    server
        .app
        .poll_due_prompt_gates(now + Duration::from_secs(3) + SETTLE);
    assert!(server
        .app
        .prompt_holds_projection("test-boot")
        .holds
        .is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_key_iterations_do_not_walk_held_fifos() {
    use crate::terminal::prompt_gate::HumanInput;
    let (mut server, _input, target, terminal) = prompt_test_server();
    let now = Instant::now();
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], now);
    for index in 0..32 {
        let response = prompt(&mut server, &target, &format!("queued-{index}"), None);
        assert!(response.try_recv().unwrap().contains("held"));
    }
    let scans = server.app.state.terminals[&terminal]
        .prompt_gate
        .expiry_scan_count();
    for index in 1..=128 {
        let at = now + Duration::from_millis(index);
        server
            .app
            .note_prompt_human_input(&terminal, &[HumanInput::Edit], at);
        server.app.poll_due_prompt_gates(at);
        assert!(server.app.next_prompt_gate_deadline().is_some());
    }
    assert_eq!(
        server.app.state.terminals[&terminal]
            .prompt_gate
            .expiry_scan_count(),
        scans,
        "the full key iteration must not inspect waiting requests"
    );
    assert_eq!(
        server.app.prompt_holds_projection("test-boot").holds[&target].held,
        32
    );
    shutdown_test_runtimes(&mut server);
}

/// Opt-in is independent of surface activity, just like the real JSON hello.
fn attach_hold_shell(
    server: &mut HeadlessServer,
    id: u64,
    active: bool,
    enabled: bool,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (writer, control, render) = test_client_writer();
    server.handle_server_event(ServerEvent::ClientShellConnected {
        client_id: id,
        surface_cols: 80,
        surface_rows: 23,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        direct_graphics: false,
        endpoint_keybindings: false,
        mouse_capture: false,
        surface_active: active,
        surface_reuse: false,
        surface_delta: false,
        surface_scroll: false,
        prompt_holds: enabled,
        writer,
    });
    (control, render)
}

fn read_holds(
    rx: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Vec<protocol::endpoint::EndpointPromptHolds> {
    let mut holds = Vec::new();
    while let Ok(bytes) = rx.recv_timeout(Duration::from_millis(100)) {
        if let ServerMessage::EndpointControl { kind, data } = read_server_message(bytes) {
            if kind == protocol::endpoint::PROMPT_HOLDS_KIND {
                holds.push(serde_json::from_str(&data).unwrap());
            }
        }
    }
    holds
}

#[tokio::test]
async fn prompt_hold_projection_follows_terminal_move_and_close() {
    use crate::terminal::prompt_gate::HumanInput;
    let (mut server, _input, old_id, terminal) = prompt_test_server();
    server
        .app
        .state
        .workspaces
        .push(crate::workspace::Workspace::test_new("target"));
    server.app.state.ensure_test_terminals();
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], Instant::now());
    let shell = attach_hold_shell(&mut server, 901, false, true);
    assert!(read_holds(&shell.0)[0].holds.contains_key(&old_id));
    let pane = server.app.state.workspaces[0].focused_pane_id().unwrap();
    let taken = server.app.state.workspaces[0]
        .take_pane_for_move(pane)
        .unwrap();
    let target = server.app.state.workspaces[1].focused_pane_id().unwrap();
    let pane = server.app.state.workspaces[1]
        .insert_moved_pane_into_tab(
            0,
            target,
            taken.moved,
            ratatui::layout::Direction::Horizontal,
            0.5,
            true,
        )
        .ok()
        .unwrap();
    let new_id = server.app.public_pane_id(1, pane).unwrap();
    assert_ne!(old_id, new_id);
    server.stream_prompt_holds(true);
    let projected = read_holds(&shell.0);
    assert_eq!(projected.len(), 1);
    assert!(projected[0].holds.contains_key(&new_id));
    assert!(!projected[0].holds.contains_key(&old_id));
    server.app.discard_prompt_gate(&terminal, "terminal closed");
    server.stream_prompt_holds(false);
    assert!(read_holds(&shell.0)[0].holds.is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_controls_attach_change_counts_and_quiet_expiry_for_all_opted_in_shells() {
    use crate::terminal::prompt_gate::HumanInput;
    let (mut server, _input, target, terminal) = prompt_test_server();
    let now = Instant::now();
    server
        .app
        .note_prompt_human_input(&terminal, &[HumanInput::Edit], now);
    let active = attach_hold_shell(&mut server, 901, true, true);
    let inactive = attach_hold_shell(&mut server, 902, false, true);
    let old = attach_hold_shell(&mut server, 903, false, false);
    for shell in [&active, &inactive] {
        let holds = read_holds(&shell.0);
        assert_eq!(holds.len(), 1);
        assert_eq!(holds[0].boot_id, "test-boot");
        assert_eq!(holds[0].holds[&target].held, 0);
    }
    assert!(read_holds(&old.0).is_empty());
    // Neither repeated input nor its stale timer wake sends a replacement.
    server.app.note_prompt_human_input(
        &terminal,
        &[HumanInput::Edit],
        now + Duration::from_secs(1),
    );
    server
        .app
        .poll_due_prompt_gates(now + Duration::from_secs(600));
    server.stream_prompt_holds(false);
    for shell in [&active, &inactive] {
        assert!(read_holds(&shell.0).is_empty());
    }
    let response = prompt(&mut server, &target, "queued", None);
    assert!(response.try_recv().unwrap().contains("held"));
    server.stream_prompt_holds(false);
    for shell in [&active, &inactive] {
        assert_eq!(read_holds(&shell.0)[0].holds[&target].held, 1);
    }
    server.app.note_prompt_human_input(
        &terminal,
        &[HumanInput::Clear],
        now + Duration::from_secs(600),
    );
    server
        .app
        .poll_due_prompt_gates(server.app.next_prompt_gate_deadline().unwrap());
    server.stream_prompt_holds(false); // No render, PTY output or additional key.
    for shell in [&active, &inactive] {
        assert!(read_holds(&shell.0)[0].holds.is_empty());
    }
    assert!(read_holds(&old.0).is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_preserves_human_submission_before_agent_text_and_enter() {
    let (mut server, mut input, target, terminal_id) = prompt_test_server();
    human(
        &mut server,
        &target,
        ClientPaneInputEvent::TextCommit("abc".to_owned()),
    );
    assert_eq!(input.try_recv().unwrap().as_ref(), b"abc");
    let response = prompt(&mut server, &target, "held-message", None);
    let held: serde_json::Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(held["result"]["delivery"], "held");
    assert_eq!(held["result"]["agent"]["held_prompts"], 1);
    assert_eq!(server.app.prompt_delivery.notifications.len(), 1);
    assert!(input.try_recv().is_err());
    human(&mut server, &target, enter());
    assert_eq!(input.try_recv().unwrap().as_ref(), b"\r");
    assert!(
        input.try_recv().is_err(),
        "settle must keep agent text out of the human submit"
    );
    let deadline = server.app.next_prompt_gate_deadline().unwrap();
    server.app.poll_due_prompt_gates(deadline);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), input.recv())
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        b"held-message"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), input.recv())
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        b"\r"
    );
    assert_eq!(
        server.app.state.terminals[&terminal_id].prompt_gate.held(),
        0
    );
    assert_eq!(server.app.next_prompt_gate_deadline(), None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_wait_timeout_withdraws_without_typing() {
    let (mut server, mut input, target, _) = prompt_test_server();
    human(
        &mut server,
        &target,
        ClientPaneInputEvent::Paste("abc".to_owned()),
    );
    assert_eq!(input.try_recv().unwrap().as_ref(), b"abc");
    let deadline = Instant::now() + Duration::from_secs(1);
    let response = prompt(
        &mut server,
        &target,
        "must-not-type",
        Some(AgentPromptWaitOptions {
            until: Vec::new(),
            timeout_ms: Some(1000),
            submission_deadline: Some(deadline),
            submission_baseline: None,
        }),
    );
    assert!(response.try_recv().is_err());
    assert!(input.try_recv().is_err());
    server.app.poll_due_prompt_gates(deadline);
    let error: crate::api::schema::ErrorResponse =
        serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(error.error.code, "timeout");
    assert!(error.error.message.contains("nothing was typed"));
    human(&mut server, &target, enter());
    assert_eq!(input.try_recv().unwrap().as_ref(), b"\r");
    server
        .app
        .poll_due_prompt_gates(deadline + Duration::from_secs(2));
    assert!(input.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_replacement_and_terminal_removal_surface_owned_text() {
    let (mut server, mut input, target, terminal_id) = prompt_test_server();
    human(
        &mut server,
        &target,
        ClientPaneInputEvent::TextCommit("abc".to_owned()),
    );
    input.try_recv().unwrap();
    let response = prompt(&mut server, &target, "do-not-lose", None);
    response.try_recv().unwrap();
    server.app.state.terminals.remove(&terminal_id);
    server
        .app
        .discard_prompt_gate(&terminal_id, "terminal closed");
    assert!(input.try_recv().is_err());
    assert!(server
        .app
        .prompt_delivery
        .notifications
        .iter()
        .any(|notice| notice
            .body
            .as_ref()
            .is_some_and(|body| body.contains("do-not-lose"))));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_ctrl_c_cannot_release_a_draft_consumed_by_an_overlay() {
    let (mut server, mut input, target, terminal_id) = prompt_test_server();
    human(
        &mut server,
        &target,
        ClientPaneInputEvent::TextCommit("abc".to_owned()),
    );
    input.try_recv().unwrap();
    prompt(&mut server, &target, "still-held", None)
        .try_recv()
        .unwrap();
    let mut cancel = enter();
    if let ClientPaneInputEvent::Key {
        code, modifiers, ..
    } = &mut cancel
    {
        *code = ClientKeyCode::Char('c');
        *modifiers = crossterm::event::KeyModifiers::CONTROL.bits();
    }
    human(&mut server, &target, cancel);
    assert_eq!(input.try_recv().unwrap().as_ref(), b"\x03");
    server
        .app
        .poll_due_prompt_gates(Instant::now() + Duration::from_secs(2));
    assert_eq!(
        server.app.state.terminals[&terminal_id].prompt_gate.held(),
        1
    );
    assert!(input.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_graceful_shutdown_reports_acknowledged_text_before_disconnect() {
    let (mut server, mut input, target, terminal_id) = prompt_test_server();
    human(
        &mut server,
        &target,
        ClientPaneInputEvent::TextCommit("abc".to_owned()),
    );
    input.try_recv().unwrap();
    let response = prompt(&mut server, &target, "must-not-disappear-on-stop", None);
    let held: serde_json::Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
    assert_eq!(held["result"]["delivery"], "held");
    let (control, _render) = connect_matching_test_shell(&mut server, 2);
    server.initiate_shutdown();
    let mut loss_notice = false;
    loop {
        match read_server_message(
            control
                .recv_timeout(Duration::from_secs(1))
                .expect("shutdown reaches client"),
        ) {
            ServerMessage::SemanticNotification(notice)
                if notice.title.starts_with("Prompt undeliverable") =>
            {
                let body = notice.body.as_deref().unwrap();
                assert!(body.contains("must-not-disappear-on-stop"));
                assert!(body.contains("server shutdown"));
                loss_notice = true;
            }
            ServerMessage::ServerShutdown { reason } => {
                let reason = reason.unwrap();
                assert!(reason.contains("must-not-disappear-on-stop"));
                assert!(reason.contains("1 held prompt(s) undelivered"));
                assert!(loss_notice, "held text must be surfaced before disconnect");
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        server.app.state.terminals[&terminal_id].prompt_gate.held(),
        0
    );
    assert!(input.try_recv().is_err(), "shutdown never types held text");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_owned_submission_failure_returns_to_notification_lane() {
    let (mut server, mut input, target, terminal_id) = prompt_test_server();
    human(
        &mut server,
        &target,
        ClientPaneInputEvent::TextCommit("abc".to_owned()),
    );
    input.try_recv().unwrap();
    prompt(&mut server, &target, "owned-text-write-failed", None)
        .try_recv()
        .unwrap();
    drop(input);
    server.app.state.agent_prompt_config.hold_while_drafting = false;
    server.app.configure_prompt_gate(&terminal_id);
    server.app.poll_prompt_gate(&terminal_id, Instant::now());
    let event = tokio::time::timeout(Duration::from_secs(1), server.app.event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&event, crate::events::AppEvent::HeldPromptDeliveryFailed { text, .. } if text == "owned-text-write-failed")
    );
    server.app.handle_internal_event_with_render_impact(event);
    assert!(server
        .app
        .prompt_delivery
        .notifications
        .iter()
        .any(|notice| notice.title.starts_with("Prompt undeliverable")
            && notice
                .body
                .as_ref()
                .is_some_and(|text| text.contains("owned-text-write-failed"))));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn prompt_hold_wait_publishes_its_baseline_only_at_release() {
    let (mut server, mut input, target, _) = prompt_test_server();
    human(
        &mut server,
        &target,
        ClientPaneInputEvent::TextCommit("abc".to_owned()),
    );
    input.try_recv().unwrap();
    let baseline = std::sync::Arc::new(std::sync::OnceLock::new());
    let response = prompt(
        &mut server,
        &target,
        "wait-for-new-activity",
        Some(AgentPromptWaitOptions {
            until: Vec::new(),
            timeout_ms: None,
            submission_deadline: None,
            submission_baseline: Some(std::sync::Arc::clone(&baseline)),
        }),
    );
    assert!(baseline.get().is_none(), "holding is not submission");
    for status in [
        crate::api::schema::AgentStatus::Working,
        crate::api::schema::AgentStatus::Idle,
    ] {
        server
            .app
            .event_hub
            .push(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::PaneAgentStatusChanged,
                data: crate::api::schema::EventData::PaneAgentStatusChanged {
                    pane_id: target.clone(),
                    workspace_id: "w1".into(),
                    agent_status: status,
                    agent: Some("opencode".into()),
                    title: None,
                    display_agent: None,
                    state_labels: Default::default(),
                },
            });
    }
    human(&mut server, &target, enter());
    input.try_recv().unwrap();
    let release_sequence = server.app.event_hub.current_sequence();
    server
        .app
        .poll_due_prompt_gates(server.app.next_prompt_gate_deadline().unwrap());
    let baseline = baseline
        .get()
        .expect("release publishes correlation before enqueue");
    assert_eq!(baseline.event_sequence, release_sequence);
    assert!(!baseline.started_working);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), input.recv())
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        b"wait-for-new-activity"
    );
    tokio::time::timeout(Duration::from_secs(1), input.recv())
        .await
        .unwrap()
        .unwrap();
    response.recv_timeout(Duration::from_secs(1)).unwrap();
    shutdown_test_runtimes(&mut server);
}

#[test]
fn prompt_hold_shutdown_reason_is_bounded_and_terminal_safe() {
    let notice = crate::api::schema::NotificationShowParams {
        title: "Prompt undeliverable".into(),
        body: Some("\x1b[31msecret\ntext".repeat(100)),
        position: None,
        sound: Default::default(),
    };
    let reason = crate::server::headless::notifications::prompt_shutdown_reason(&vec![notice; 100]);
    assert!(reason.contains("100 held prompt(s) undelivered"));
    assert!(reason.contains("see server log"));
    assert!(!reason.contains('\x1b') && !reason.contains('\n'));
    assert!(reason.chars().count() < 4300);
    assert_eq!(
        crate::server::headless::notifications::prompt_shutdown_reason(&[]),
        "server is shutting down"
    );
}
