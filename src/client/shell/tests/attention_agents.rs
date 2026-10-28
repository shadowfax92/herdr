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
