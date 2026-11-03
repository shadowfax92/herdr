//! `[ui.sidebar.agents].hidden_workspaces` is a full hide: while the Local
//! server projects no scope, the client seeds the scope from it (`crate::scope`),
//! so the workspaces leave the Spaces panel and every Agents panel layout, and
//! their keys skip them. `scope_filter.rs` covers the runtime scope itself.
use super::*;
use crate::config::AgentSidebarToken;

fn agent_in(workspace_id: &str, pane_id: &str, name: &str) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: workspace_id.into(),
        tab_id: "tab_1".into(),
        name: Some(name.into()),
        display_agent: None,
        agent: Some("pi".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: crate::api::schema::AgentStatus::Idle,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }
}

/// One focused workspace plus `ft` and a look-alike `ft-2`, each with an agent.
fn shell(hidden: &[&str]) -> ClientShellState {
    let mut config = Config::default();
    config.ui.sidebar.agents.rows = vec![vec![AgentSidebarToken::Agent]];
    config.ui.sidebar.agents.hidden_workspaces = hidden.iter().map(|s| s.to_string()).collect();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let mut snap = snapshot();
    for (number, (id, label)) in [("ws_2", "ft"), ("ws_3", "ft-2")].into_iter().enumerate() {
        let mut workspace = snap.workspaces[0].clone();
        workspace.workspace_id = id.into();
        workspace.label = label.into();
        workspace.number = number + 2;
        workspace.focused = false;
        snap.workspaces.push(workspace);
    }
    snap.agents = vec![
        agent_in("ws_1", "pane_1", "kept agent"),
        agent_in("ws_2", "pane_2", "worker agent"),
        agent_in("ws_3", "pane_3", "similar agent"),
    ];
    state.set_snapshot(Box::new(snap));
    state.set_pane_surface(surface());
    state
}

fn frame_text(state: &mut ClientShellState, width: u16, height: u16) -> String {
    let frame = state.compose(width, height).expect("shell frame");
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn agents_in_hidden_workspaces_leave_the_sidebar() {
    let mut state = shell(&["ft"]);
    let text = frame_text(&mut state, 100, 28);
    assert!(text.contains("kept agent"), "frame: {text}");
    // Labels match exactly, so a workspace that merely starts with `ft` stays.
    assert!(text.contains("similar agent"), "frame: {text}");
    assert!(!text.contains("worker agent"), "frame: {text}");
}

#[test]
fn hidden_workspaces_leave_the_spaces_panel() {
    let mut state = shell(&["ft"]);
    frame_text(&mut state, 100, 28);
    let rows = state
        .hits
        .workspaces
        .iter()
        .map(|hit| hit.workspace_id.as_str())
        .collect::<Vec<_>>();
    // Labels match exactly, so `ft-2` (ws_3) keeps its row.
    assert_eq!(rows, ["ws_1", "ws_3"]);

    let mut unfiltered = shell(&[]);
    frame_text(&mut unfiltered, 100, 28);
    assert_eq!(unfiltered.hits.workspaces.len(), 3);
}

#[test]
fn hidden_workspaces_leave_the_mobile_switcher_spaces() {
    let mut state = shell(&["ft"]);
    state.mode = ClientShellMode::Navigate;
    frame_text(&mut state, 50, 30);
    let spaces = state
        .hits
        .mobile_targets
        .iter()
        .filter_map(|(_, target)| match target {
            ClientMobileTarget::Workspace { workspace_id, .. } => Some(workspace_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(spaces, ["ws_1", "ws_3"]);
}

#[test]
fn workspace_keys_skip_hidden_workspaces() {
    let mut state = shell(&["ft"]);
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextWorkspace),
        &mut outcome,
    );
    assert!(
        matches!(
            &outcome.actions[..],
            [ClientShellAction::Endpoint { request, .. }]
                if matches!(
                    &request.method,
                    crate::api::schema::Method::WorkspaceFocus(target)
                        if target.workspace_id == "ws_3"
                )
        ),
        "{:?}",
        outcome.actions
    );
}

#[test]
fn agents_in_hidden_workspaces_leave_the_mobile_layout() {
    let mut state = shell(&["ft"]);
    // Navigate mode opens the mobile switcher, which lists agents.
    state.mode = ClientShellMode::Navigate;
    let text = frame_text(&mut state, 50, 30);
    assert!(text.contains("kept agent"), "frame: {text}");
    assert!(!text.contains("worker agent"), "frame: {text}");

    let mut unfiltered = shell(&[]);
    unfiltered.mode = ClientShellMode::Navigate;
    let text = frame_text(&mut unfiltered, 50, 30);
    assert!(text.contains("worker agent"), "frame: {text}");
}

#[test]
fn every_agent_stays_without_hidden_workspaces() {
    let mut state = shell(&[]);
    let text = frame_text(&mut state, 100, 28);
    for name in ["kept agent", "worker agent", "similar agent"] {
        assert!(text.contains(name), "{name} missing from frame: {text}");
    }
}

#[test]
fn hidden_workspaces_load_from_the_agents_sidebar_table() {
    let config: Config =
        toml::from_str("[ui.sidebar.agents]\nhidden_workspaces = [\"ft\"]\n").expect("config");
    assert_eq!(config.ui.sidebar.agents.hidden_workspaces, ["ft"]);
    assert!(Config::default()
        .ui
        .sidebar
        .agents
        .hidden_workspaces
        .is_empty());
}

#[test]
fn agent_keys_skip_hidden_workspaces() {
    let mut state = shell(&["ft"]);
    let mut outcome = ClientShellInput::default();

    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextAgent),
        &mut outcome,
    );

    // pane_2 sits in `ft` between the focused pane_1 and pane_3.
    assert!(
        matches!(
            &outcome.actions[..],
            [ClientShellAction::Endpoint { request, .. }]
                if matches!(
                    &request.method,
                    crate::api::schema::Method::PaneFocus(target) if target.pane_id == "pane_3"
                )
        ),
        "{:?}",
        outcome.actions
    );
}
