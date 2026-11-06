//! `next_attention_agent` walks the Agents panel across machines to agents
//! whose status is in `[ui.sidebar.agents].attention`.
use super::*;
use crate::api::schema::AgentStatus;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
};

fn agent(pane_id: &str, workspace_id: &str, status: AgentStatus, seq: u64) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: workspace_id.into(),
        tab_id: "tab_1".into(),
        name: Some(pane_id.into()),
        display_agent: None,
        agent: Some("pi".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: seq,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }
}

fn state_with(configure: impl FnOnce(&mut Config)) -> ClientShellState {
    let mut config = Config::default();
    config.ui.agent_panel_sort = crate::config::AgentPanelSortConfig::Priority;
    configure(&mut config);
    ClientShellState::new(ClientShellConfig::from_config(&config))
}

fn add_remote(state: &mut ClientShellState) -> ClientEndpointId {
    let profile = SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
    };
    let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    endpoint_id
}

fn press(state: &mut ClientShellState) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextAttentionAgent),
        &mut outcome,
    );
    outcome
}

fn pane_focus(outcome: &ClientShellInput) -> Option<&str> {
    match outcome.actions.as_slice() {
        [ClientShellAction::Endpoint { request, .. }] => match &request.method {
            crate::api::schema::Method::PaneFocus(target) => Some(target.pane_id.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn activation(outcome: &ClientShellInput) -> Option<(&ClientEndpointId, &str)> {
    match outcome.actions.as_slice() {
        [ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }] => Some((endpoint_id, pane_id.as_str())),
        _ => None,
    }
}

#[test]
fn walks_blocked_then_unread_agents_across_machines() {
    let mut state = state_with(|_| {});
    let remote_id = add_remote(&mut state);
    let mut local = snapshot();
    local.agents = vec![
        agent("pane_1", "ws_1", AgentStatus::Idle, 1),
        agent("pane_2", "ws_1", AgentStatus::Blocked, 2),
    ];
    state.set_snapshot(Box::new(local.clone()));
    state.set_pane_surface(surface());
    // The remote agent finishes a turn this client watched it start, so the
    // panel marks it unread.
    let mut remote = snapshot();
    remote.boot_id = "remote-boot".into();
    remote.agents = vec![agent("pane_1", "ws_1", AgentStatus::Working, 3)];
    state.set_endpoint_snapshot(&remote_id, Box::new(remote.clone()));
    remote.revision = 2;
    remote.agents = vec![agent("pane_1", "ws_1", AgentStatus::Idle, 4)];
    state.set_endpoint_snapshot(&remote_id, Box::new(remote));

    let first = press(&mut state);
    assert_eq!(
        activation(&first),
        Some((&ClientEndpointId::Local, "pane_2")),
        "blocked comes first: {:?}",
        first.actions
    );

    local.revision = 2;
    local.focused_pane_id = Some("pane_2".into());
    state.set_snapshot(Box::new(local));
    let second = press(&mut state);
    assert_eq!(
        activation(&second),
        Some((&remote_id, "pane_1")),
        "the unread remote agent follows: {:?}",
        second.actions
    );
}

#[test]
fn skips_hidden_workspaces_and_says_when_nothing_is_left() {
    let mut state = state_with(|config| {
        config.ui.sidebar.agents.hidden_workspaces = vec!["ft".into()];
    });
    let mut local = snapshot();
    let mut ft = local.workspaces[0].clone();
    ft.workspace_id = "ws_2".into();
    ft.label = "ft".into();
    ft.number = 2;
    ft.focused = false;
    local.workspaces.push(ft);
    local.agents = vec![
        agent("pane_1", "ws_1", AgentStatus::Idle, 1),
        agent("pane_3", "ws_2", AgentStatus::Blocked, 2),
    ];
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());

    let outcome = press(&mut state);

    assert!(outcome.actions.is_empty(), "{:?}", outcome.actions);
    assert_eq!(
        state
            .visible_endpoint_notice
            .as_ref()
            .map(|notice| notice.title.as_str()),
        Some("No other blocked or unread agents")
    );
}

#[test]
fn attention_setting_chooses_the_statuses_to_visit() {
    let mut state = state_with(|config| {
        config.ui.sidebar.agents.attention = vec!["working".into()];
    });
    let mut local = snapshot();
    local.agents = vec![
        agent("pane_1", "ws_1", AgentStatus::Idle, 1),
        agent("pane_2", "ws_1", AgentStatus::Blocked, 2),
        agent("pane_3", "ws_1", AgentStatus::Working, 3),
    ];
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());

    let outcome = press(&mut state);

    assert_eq!(
        pane_focus(&outcome),
        Some("pane_3"),
        "{:?}",
        outcome.actions
    );
}

#[test]
fn attention_rejects_unknown_statuses() {
    let error = toml::from_str::<Config>("[ui.sidebar.agents]\nattention = [\"unread\"]\n")
        .expect_err("unknown status");
    assert!(
        error.to_string().contains("unknown agent status `unread`"),
        "{error}"
    );
    let config: Config =
        toml::from_str("[ui.sidebar.agents]\nattention = [\"blocked\", \"working\"]\n")
            .expect("config");
    assert_eq!(config.ui.sidebar.agents.attention, ["blocked", "working"]);
    assert_eq!(
        Config::default().ui.sidebar.agents.attention,
        ["blocked", "done"]
    );
}

/// Finishes `unread` turns this client watched start, so the panel marks each
/// agent unread, and returns the snapshot. Every other agent keeps its status.
fn with_unread(
    state: &mut ClientShellState,
    agents: Vec<ClientShellAgent>,
    unread: &[&str],
) -> ClientShellSnapshot {
    let mut working = snapshot();
    working.agents = agents
        .iter()
        .cloned()
        .map(|mut agent| {
            if unread.contains(&agent.pane_id.as_str()) {
                agent.agent_status = AgentStatus::Working;
                agent.state_change_seq -= 1;
            }
            agent
        })
        .collect();
    state.set_snapshot(Box::new(working));
    state.set_pane_surface(surface());
    let mut finished = snapshot();
    finished.revision = 2;
    finished.agents = agents;
    state.set_snapshot(Box::new(finished.clone()));
    let mut drawn = surface();
    drawn.projection_revision = 2;
    state.set_pane_surface(drawn);
    finished
}

fn focus(state: &mut ClientShellState, mut snapshot: ClientShellSnapshot, pane_id: &str) {
    snapshot.revision += 1;
    snapshot.focused_pane_id = Some(pane_id.into());
    state.set_snapshot(Box::new(snapshot));
}

fn panel_order(state: &ClientShellState) -> Vec<String> {
    aggregate_navigation::aggregate_agent_rows(
        &state.endpoints,
        &state.active_endpoint_id,
        crate::config::AgentPanelSortConfig::Priority,
        &state.scope_view(),
    )
    .into_iter()
    .map(|row| row.agent.pane_id.clone())
    .collect()
}

#[test]
fn visits_statuses_in_attention_order_not_panel_order() {
    let mut state = state_with(|config| {
        config.ui.sidebar.agents.attention =
            vec!["blocked".into(), "done".into(), "working".into()];
    });
    // The user just sent pane_1 a prompt, so it is working and focused, with
    // another working agent below it and an unread one above.
    let finished = with_unread(
        &mut state,
        vec![
            agent("pane_1", "ws_1", AgentStatus::Working, 6),
            agent("pane_2", "ws_1", AgentStatus::Working, 5),
            agent("pane_3", "ws_1", AgentStatus::Idle, 8),
        ],
        &["pane_3"],
    );
    assert_eq!(panel_order(&state), ["pane_3", "pane_1", "pane_2"]);

    let outcome = press(&mut state);
    assert_eq!(
        pane_focus(&outcome),
        Some("pane_3"),
        "unread comes before the working agent below the cursor: {:?}",
        outcome.actions
    );

    // The list is an order: naming working first flips it.
    state.config.agents.attention = vec!["working".into(), "done".into()];
    let outcome = press(&mut state);
    assert_eq!(
        pane_focus(&outcome),
        Some("pane_2"),
        "{:?}",
        outcome.actions
    );

    // Once the unread agent is read, nothing unread is left and working
    // agents follow.
    state.config.agents.attention = vec!["blocked".into(), "done".into(), "working".into()];
    focus(&mut state, finished, "pane_3");
    let mut read = surface();
    read.projection_revision = 3;
    read.panes[0].pane_id = "pane_3".into();
    state.set_pane_surface(read);
    let outcome = press(&mut state);
    assert_eq!(
        pane_focus(&outcome),
        Some("pane_1"),
        "{:?}",
        outcome.actions
    );
}

#[test]
fn walks_one_status_from_the_focused_row_and_wraps() {
    let mut state = state_with(|config| {
        config.ui.sidebar.agents.attention = vec!["blocked".into(), "done".into()];
    });
    let finished = with_unread(
        &mut state,
        vec![
            agent("pane_1", "ws_1", AgentStatus::Blocked, 4),
            agent("pane_2", "ws_1", AgentStatus::Blocked, 3),
            agent("pane_3", "ws_1", AgentStatus::Blocked, 2),
            agent("pane_4", "ws_1", AgentStatus::Idle, 6),
        ],
        &["pane_4"],
    );
    assert_eq!(
        panel_order(&state),
        ["pane_1", "pane_2", "pane_3", "pane_4"]
    );

    // Blocked agents come before the unread one, each press moving one row on
    // from the focused agent and wrapping at the end of its status.
    let mut visited = Vec::new();
    let mut current = finished;
    for _ in 0..3 {
        let outcome = press(&mut state);
        let target = pane_focus(&outcome)
            .unwrap_or_else(|| panic!("{:?}", outcome.actions))
            .to_owned();
        visited.push(target.clone());
        current.revision += 1;
        current.focused_pane_id = Some(target);
        state.set_snapshot(Box::new(current.clone()));
    }
    assert_eq!(visited, ["pane_2", "pane_3", "pane_1"]);

    // The focused agent is never its own target: once it is the only blocked
    // one, the key moves on to the unread agent.
    current.revision += 1;
    for agent in &mut current.agents[1..3] {
        agent.agent_status = AgentStatus::Working;
        agent.state_change_seq += 10;
    }
    state.set_snapshot(Box::new(current));
    let outcome = press(&mut state);
    assert_eq!(
        pane_focus(&outcome),
        Some("pane_4"),
        "{:?}",
        outcome.actions
    );
}
