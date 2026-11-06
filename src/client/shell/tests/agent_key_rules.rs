//! Agent key rules driven through `ClientShellState`: real snapshots with
//! agent entries, keys through the input parser, and what an observer sees,
//! which is the pane input events, the notice and the mode. No PTY and no
//! server.

use super::*;
use crate::protocol::{ClientKeyCode, ClientKeyKind};

/// The owner's rules: Claude Code exits on ctrl+d and cannot unbind it.
const OWNER_RULES: &str = r#"
[agent_key_rules.claude]
"ctrl+d" = "drop"
"ctrl+t" = "drop"
"#;

const CTRL_D: &[u8] = b"\x04";
const CTRL_T: &[u8] = b"\x14";
const CTRL_U: &[u8] = b"\x15";
const CTRL: u8 = KeyModifiers::CONTROL.bits();

fn config(toml: &str) -> Config {
    toml::from_str(toml).expect("config parses")
}

fn agent_entry(pane_id: &str, label: Option<&str>) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: None,
        display_agent: None,
        agent: label.map(Into::into),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Idle,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
    }
}

fn snapshot_with(revision: u64, agents: Vec<ClientShellAgent>) -> Box<ClientShellSnapshot> {
    let mut snapshot = snapshot();
    snapshot.revision = revision;
    snapshot.agents = agents;
    Box::new(snapshot)
}

/// A client on `shell_config` whose focused pane_1 runs `label`, or a plain
/// shell when `label` is `None`.
fn client_on(shell_config: ClientShellConfig, label: Option<&str>) -> ClientShellState {
    let mut state = ClientShellState::new(shell_config);
    let agents = label
        .map(|label| agent_entry("pane_1", Some(label)))
        .into_iter()
        .collect();
    state.set_snapshot(snapshot_with(1, agents));
    state
}

fn client(toml: &str, label: Option<&str>) -> ClientShellState {
    client_on(ClientShellConfig::from_config(&config(toml)), label)
}

/// Every key event `outcome` sends to `pane_id`, in order.
fn key_events(outcome: &ClientShellInput, pane_id: &str) -> Vec<ClientPaneInputEvent> {
    outcome
        .requests
        .iter()
        .flat_map(|request| match request {
            ClientMessage::ClientShellPaneInput {
                pane_id: target,
                events,
            } if target == pane_id => events
                .iter()
                .filter(|event| matches!(event, ClientPaneInputEvent::Key { .. }))
                .cloned()
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// (code, modifiers, kind) of every key `outcome` sends to `pane_id`.
fn keys_to(outcome: &ClientShellInput, pane_id: &str) -> Vec<(ClientKeyCode, u8, ClientKeyKind)> {
    key_events(outcome, pane_id)
        .into_iter()
        .filter_map(|event| match event {
            ClientPaneInputEvent::Key {
                code,
                modifiers,
                kind,
                ..
            } => Some((code, modifiers, kind)),
            _ => None,
        })
        .collect()
}

fn keys_to_pane(outcome: &ClientShellInput) -> Vec<(ClientKeyCode, u8, ClientKeyKind)> {
    keys_to(outcome, "pane_1")
}

fn ctrl_press(ch: char) -> Vec<(ClientKeyCode, u8, ClientKeyKind)> {
    vec![(ClientKeyCode::Char(ch), CTRL, ClientKeyKind::Press)]
}

fn stroke(code: ClientKeyCode) -> Vec<(ClientKeyCode, u8, ClientKeyKind)> {
    vec![
        (code.clone(), 0, ClientKeyKind::Press),
        (code, 0, ClientKeyKind::Release),
    ]
}

fn notice(state: &ClientShellState) -> Option<&str> {
    state
        .copy_feedback
        .as_ref()
        .map(|feedback| feedback.message.as_str())
}

/// Lets the current notice expire through the real timer path.
fn expire_notice(state: &mut ClientShellState) {
    state.tick_copy_feedback(std::time::Instant::now() + std::time::Duration::from_secs(3));
    assert_eq!(notice(state), None);
}

#[test]
fn dropped_keys_never_reach_the_agent() {
    let mut state = client(OWNER_RULES, Some("claude"));

    let ctrl_d = state.handle_input_bytes(CTRL_D);
    assert_eq!(keys_to_pane(&ctrl_d), Vec::new());
    assert_eq!(notice(&state), Some("ctrl+d is disabled for claude"));
    assert!(ctrl_d.repaint, "the notice is drawn");

    let ctrl_t = state.handle_input_bytes(CTRL_T);
    assert_eq!(keys_to_pane(&ctrl_t), Vec::new());
    assert_eq!(notice(&state), Some("ctrl+t is disabled for claude"));

    // A key without a rule still reaches the same agent.
    let ctrl_u = state.handle_input_bytes(CTRL_U);
    assert_eq!(keys_to_pane(&ctrl_u), ctrl_press('u'));
}

#[test]
fn press_repeat_and_release_all_stay_with_herdr() {
    let mut state = client(OWNER_RULES, Some("claude"));

    let press = state.handle_input_bytes(b"\x1b[100;5u");
    assert_eq!(keys_to_pane(&press), Vec::new());
    assert_eq!(notice(&state), Some("ctrl+d is disabled for claude"));
    expire_notice(&mut state);
    let repeat = state.handle_input_bytes(b"\x1b[100;5:2u");
    let release = state.handle_input_bytes(b"\x1b[100;5:3u");
    assert_eq!(keys_to_pane(&repeat), Vec::new());
    assert_eq!(keys_to_pane(&release), Vec::new());
    assert_eq!(notice(&state), None, "a held key shows the notice once");

    // The same report for a key without a rule reaches the pane whole.
    let forwarded: Vec<_> = [
        b"\x1b[117;5u".as_slice(),
        b"\x1b[117;5:2u",
        b"\x1b[117;5:3u",
    ]
    .into_iter()
    .flat_map(|bytes| keys_to_pane(&state.handle_input_bytes(bytes)))
    .collect();
    assert_eq!(
        forwarded,
        vec![
            (ClientKeyCode::Char('u'), CTRL, ClientKeyKind::Press),
            (ClientKeyCode::Char('u'), CTRL, ClientKeyKind::Repeat),
            (ClientKeyCode::Char('u'), CTRL, ClientKeyKind::Release),
        ]
    );
}

#[test]
fn a_plain_shell_keeps_ctrl_d() {
    let mut state = client(OWNER_RULES, None);

    let ctrl_d = state.handle_input_bytes(CTRL_D);

    assert_eq!(keys_to_pane(&ctrl_d), ctrl_press('d'));
    assert_eq!(notice(&state), None);
}

#[test]
fn rules_follow_detection_both_ways() {
    let mut state = client(OWNER_RULES, None);
    assert_eq!(
        keys_to_pane(&state.handle_input_bytes(CTRL_D)),
        ctrl_press('d'),
        "before the pane has an agent entry"
    );

    // A managed launch is listed before detection names its agent.
    state.set_snapshot(snapshot_with(2, vec![agent_entry("pane_1", None)]));
    assert_eq!(
        keys_to_pane(&state.handle_input_bytes(CTRL_D)),
        ctrl_press('d'),
        "before detection names the agent"
    );

    state.set_snapshot(snapshot_with(
        3,
        vec![agent_entry("pane_1", Some("claude"))],
    ));
    assert_eq!(keys_to_pane(&state.handle_input_bytes(CTRL_D)), Vec::new());

    // Claude exited: the pane is a shell again, and ctrl+d is EOF there.
    state.set_snapshot(snapshot_with(4, Vec::new()));
    assert_eq!(
        keys_to_pane(&state.handle_input_bytes(CTRL_D)),
        ctrl_press('d'),
        "after the agent exits"
    );
}

#[test]
fn rules_match_only_their_agent() {
    let mut state = client(OWNER_RULES, Some("codex"));
    assert_eq!(
        keys_to_pane(&state.handle_input_bytes(CTRL_D)),
        ctrl_press('d')
    );

    // The sidebar's name and display label are not the agent.
    let mut entry = agent_entry("pane_1", Some("codex"));
    entry.name = Some("claude".into());
    entry.display_agent = Some("claude".into());
    state.set_snapshot(snapshot_with(2, vec![entry]));
    assert_eq!(
        keys_to_pane(&state.handle_input_bytes(CTRL_D)),
        ctrl_press('d')
    );
    assert_eq!(notice(&state), None);
}

#[test]
fn the_focused_pane_decides_which_rules_apply() {
    let mut state = client(OWNER_RULES, None);
    let mut two_panes = snapshot_with(
        2,
        vec![
            agent_entry("pane_1", Some("claude")),
            agent_entry("pane_2", None),
        ],
    );
    let mut shell_pane = two_panes.panes[0].clone();
    shell_pane.pane_id = "pane_2".into();
    two_panes.panes[0].focused = false;
    two_panes.panes.push(shell_pane);
    two_panes.focused_pane_id = Some("pane_2".into());
    state.set_snapshot(two_panes.clone());

    let shell = state.handle_input_bytes(CTRL_D);
    assert_eq!(keys_to(&shell, "pane_2"), ctrl_press('d'));

    two_panes.revision = 3;
    two_panes.focused_pane_id = Some("pane_1".into());
    state.set_snapshot(two_panes);
    let claude = state.handle_input_bytes(CTRL_D);
    assert_eq!(keys_to(&claude, "pane_1"), Vec::new());
    assert_eq!(keys_to(&claude, "pane_2"), Vec::new());
    assert_eq!(notice(&state), Some("ctrl+d is disabled for claude"));
}

#[test]
fn labels_fold_like_detection() {
    for label in ["Claude", "claude-code"] {
        let mut state = client(OWNER_RULES, Some(label));
        assert_eq!(
            keys_to_pane(&state.handle_input_bytes(CTRL_D)),
            Vec::new(),
            "{label}"
        );
        assert_eq!(notice(&state), Some("ctrl+d is disabled for claude"));
    }

    let mut state = client(
        "[agent_key_rules.claude-code]\n\"ctrl+d\" = \"drop\"\n",
        Some("claude"),
    );
    assert_eq!(keys_to_pane(&state.handle_input_bytes(CTRL_D)), Vec::new());
}

const REMAP_RULES: &str = r#"
[agent_key_rules.codex]
"ctrl+p" = { remap = "up" }
"#;

#[test]
fn a_remap_sends_one_whole_stroke_per_press_and_repeat() {
    let mut state = client(REMAP_RULES, Some("codex"));

    let press = state.handle_input_bytes(b"\x1b[112;5u");
    let up = |kind| ClientPaneInputEvent::Key {
        code: ClientKeyCode::Up,
        modifiers: 0,
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: true,
        physical_key_id: None,
        windows_record: None,
    };
    // A clean synthesized Up: nothing of the user's ctrl+p rides along.
    assert_eq!(
        key_events(&press, "pane_1"),
        vec![up(ClientKeyKind::Press), up(ClientKeyKind::Release)]
    );
    assert_eq!(notice(&state), None, "a remap shows no notice");

    let repeat = state.handle_input_bytes(b"\x1b[112;5:2u");
    assert_eq!(keys_to_pane(&repeat), stroke(ClientKeyCode::Up));
    let release = state.handle_input_bytes(b"\x1b[112;5:3u");
    assert_eq!(keys_to_pane(&release), Vec::new());

    let legacy = state.handle_input_bytes(b"\x10");
    assert_eq!(keys_to_pane(&legacy), stroke(ClientKeyCode::Up));
}

#[test]
fn a_remap_honours_a_multi_count_press() {
    let mut state = client(REMAP_RULES, Some("codex"));

    let press = state.handle_raw_events(vec![RawInputEvent::Key(
        crate::input::TerminalKey::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
            .with_repeat_count(3),
    )]);

    assert_eq!(
        keys_to_pane(&press),
        [
            stroke(ClientKeyCode::Up),
            stroke(ClientKeyCode::Up),
            stroke(ClientKeyCode::Up)
        ]
        .concat()
    );
}

#[test]
fn herdr_bindings_and_the_prefix_win() {
    let rules = r#"
[keys]
workspace_picker = ["prefix+w", "ctrl+g"]

[agent_key_rules.claude]
"ctrl+b" = "drop"
"ctrl+g" = "drop"
"#;

    let mut state = client(rules, Some("claude"));
    let binding = state.handle_input_bytes(b"\x07");
    assert_eq!(state.mode, ClientShellMode::Navigate, "the binding ran");
    assert_eq!(keys_to_pane(&binding), Vec::new());
    assert_eq!(notice(&state), None);

    let mut state = client(rules, Some("claude"));
    let prefix = state.handle_input_bytes(b"\x02");
    assert_eq!(state.mode, ClientShellMode::Prefix);
    assert_eq!(keys_to_pane(&prefix), Vec::new());
    assert_eq!(notice(&state), None);
}

#[test]
fn the_literal_prefix_reaches_the_pane_despite_a_rule() {
    let mut state = client(
        "[agent_key_rules.claude]\n\"ctrl+b\" = \"drop\"\n",
        Some("claude"),
    );

    let literal = state.handle_input_bytes(b"\x02\x02");

    assert_eq!(keys_to_pane(&literal), ctrl_press('b'));
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert_eq!(notice(&state), None);
}

/// pane_1 in copy mode with claude as its agent, over herdr's scrollback or
/// over a fullscreen app on the alternate screen.
fn claude_in_copy_mode(alternate_screen: bool) -> ClientShellState {
    let mut state = client(OWNER_RULES, Some("claude"));
    let mut pane_surface = surface();
    pane_surface.panes[0].alternate_screen_active = alternate_screen;
    pane_surface.panes[0].scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: if alternate_screen { 0 } else { 20 },
        viewport_rows: 2,
    });
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    state.handle_input_bytes(b"\x02[");
    assert_eq!(state.mode, ClientShellMode::Copy);
    state
}

fn copy_cursor_row(state: &ClientShellState) -> Option<u32> {
    state
        .copy_mode
        .as_ref()
        .map(|copy_mode| copy_mode.cursor.row)
}

#[test]
fn copy_mode_keeps_its_ctrl_d() {
    // Over herdr's scrollback, ctrl+d moves copy mode down half a page.
    let mut state = claude_in_copy_mode(false);
    let bottom = copy_cursor_row(&state).expect("copy cursor");
    let up = state.handle_input_bytes(CTRL_U);
    assert_eq!(copy_cursor_row(&state), Some(bottom - 1));
    let down = state.handle_input_bytes(CTRL_D);
    assert_eq!(copy_cursor_row(&state), Some(bottom));
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert_eq!(keys_to_pane(&up), Vec::new());
    assert_eq!(keys_to_pane(&down), Vec::new());
    assert_eq!(notice(&state), None);

    // Over a fullscreen app, copy mode pages the app itself with PageDown.
    let mut state = claude_in_copy_mode(true);
    let page = state.handle_input_bytes(CTRL_D);
    assert_eq!(keys_to_pane(&page), stroke(ClientKeyCode::PageDown));
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert_eq!(notice(&state), None);
}

#[test]
fn other_owners_keep_their_keys() {
    // A popup takes every key, ctrl+d included.
    let mut state = client(OWNER_RULES, Some("claude"));
    state.set_pane_surface(surface_with_popup());
    state.compose(106, 20).expect("popup frame");
    let popup = state.handle_input_bytes(CTRL_D);
    assert!(matches!(
        &popup.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, events }]
            if terminal_id == "terminal-popup"
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Key { code: ClientKeyCode::Char('d'), modifiers, .. }]
                        if *modifiers == CTRL
                )
    ));
    assert_eq!(notice(&state), None);

    // A prompt, the sidebar and Resize mode consume it as they always have.
    let mut state = client(OWNER_RULES, Some("claude"));
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::RenamePane),
        &mut ClientShellInput::default(),
    );
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Rename(_))));
    let prompt = state.handle_input_bytes(CTRL_D);
    assert_eq!(keys_to_pane(&prompt), Vec::new());
    assert_eq!(notice(&state), None);

    for (enter, mode) in [
        (b"\x02w".as_slice(), ClientShellMode::Navigate),
        (b"\x02r", ClientShellMode::Resize),
    ] {
        let mut state = client(OWNER_RULES, Some("claude"));
        state.handle_input_bytes(enter);
        assert_eq!(state.mode, mode);
        let owned = state.handle_input_bytes(CTRL_D);
        assert_eq!(keys_to_pane(&owned), Vec::new(), "{mode:?}");
        assert_eq!(notice(&state), None, "{mode:?}");
    }
}

#[test]
fn a_bad_entry_costs_only_its_own_rule() {
    let mut state = client(
        r#"
[agent_key_rules.claude]
"ctrl+d" = "drop"
"ctrl+t" = "block"
q = "drop"
"#,
        Some("claude"),
    );

    assert_eq!(keys_to_pane(&state.handle_input_bytes(CTRL_D)), Vec::new());
    assert_eq!(
        keys_to_pane(&state.handle_input_bytes(CTRL_T)),
        ctrl_press('t')
    );
    assert_eq!(
        keys_to_pane(&state.handle_input_bytes(b"q")),
        vec![(ClientKeyCode::Char('q'), 0, ClientKeyKind::Press)]
    );
}

#[test]
fn live_reload_swaps_rules_in_every_keybinding_mode() {
    for source in [
        ClientShellKeybindingSource::Local,
        ClientShellKeybindingSource::RemoteLocal,
        ClientShellKeybindingSource::Endpoint,
    ] {
        let shell =
            ClientShellConfig::from_config(&config(OWNER_RULES)).with_keybinding_source(source);
        let mut state = client_on(shell, Some("claude"));
        assert_eq!(
            keys_to_pane(&state.handle_input_bytes(CTRL_D)),
            Vec::new(),
            "{source:?} at startup"
        );

        state.config.apply_live_config(&Config::default(), &[], &[]);
        assert_eq!(
            keys_to_pane(&state.handle_input_bytes(CTRL_D)),
            ctrl_press('d'),
            "{source:?} after rules were removed"
        );

        state
            .config
            .apply_live_config(&config(OWNER_RULES), &[], &[]);
        assert_eq!(
            keys_to_pane(&state.handle_input_bytes(CTRL_D)),
            Vec::new(),
            "{source:?} after rules came back"
        );

        // A section the live loader could not read keeps the current rules.
        state
            .config
            .apply_live_config(&Config::default(), &[], &["agent_key_rules".to_owned()]);
        assert_eq!(
            keys_to_pane(&state.handle_input_bytes(CTRL_D)),
            Vec::new(),
            "{source:?} after an unreadable section"
        );
    }
}

#[test]
fn the_notice_follows_the_clipboard_toast_setting() {
    let mut state = client(
        &format!("{OWNER_RULES}\n[ui.toast.clipboard]\nenabled = false\n"),
        Some("claude"),
    );

    let ctrl_d = state.handle_input_bytes(CTRL_D);

    assert_eq!(keys_to_pane(&ctrl_d), Vec::new(), "still dropped");
    assert_eq!(notice(&state), None);
}
