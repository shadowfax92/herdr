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
            ServerMessage::ServerShutdown { .. } => {
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
