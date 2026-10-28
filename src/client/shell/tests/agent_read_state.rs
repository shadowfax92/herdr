//! Reading an agent takes focusing its own pane: agents sharing its tab, or
//! hidden behind it while zoomed, keep their unread mark.
use super::*;
use crate::api::schema::AgentStatus;

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

fn status_of(state: &ClientShellState, pane_id: &str) -> Option<AgentStatus> {
    state
        .snapshot
        .as_deref()?
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
        .map(|agent| agent.agent_status)
}

#[test]
fn focusing_reads_one_agent_and_leaves_its_tab_mates_unread() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut working = snapshot();
    working.agents = vec![
        agent("pane_1", "ws_1", AgentStatus::Working, 1),
        agent("pane_2", "ws_1", AgentStatus::Working, 2),
    ];
    state.set_snapshot(Box::new(working));
    let mut finished = snapshot();
    finished.revision = 2;
    finished.agents = vec![
        agent("pane_1", "ws_1", AgentStatus::Idle, 3),
        agent("pane_2", "ws_1", AgentStatus::Idle, 4),
    ];
    state.set_snapshot(Box::new(finished));

    // Both panes are on screen; only pane_1 has focus.
    let mut shown = surface();
    shown.projection_revision = 2;
    let mut peer = shown.panes[0].clone();
    peer.pane_id = "pane_2".into();
    peer.focused = false;
    shown.panes.push(peer);
    state.set_pane_surface(shown);

    assert_eq!(status_of(&state, "pane_1"), Some(AgentStatus::Idle));
    assert_eq!(status_of(&state, "pane_2"), Some(AgentStatus::Done));
}

#[test]
fn a_fresh_client_keeps_agents_the_server_still_reports_done() {
    use super::super::endpoint_agent_state::EndpointAgentPresentation;

    let mut presentation = EndpointAgentPresentation::default();
    let mut fresh = snapshot();
    fresh.agents = vec![
        agent("pane_1", "ws_1", AgentStatus::Done, 5),
        agent("pane_2", "ws_1", AgentStatus::Idle, 6),
    ];
    presentation.receive_completions(
        None,
        crate::protocol::endpoint::EndpointAgentCompletions {
            boot_id: fresh.boot_id.clone(),
            revision: fresh.revision,
            completions: [("pane_1".to_owned(), 5), ("pane_2".to_owned(), 6)]
                .into_iter()
                .collect(),
        },
    );

    presentation.project_snapshot(&mut fresh);

    // The server's done survives; an agent it already marked read stays read.
    assert_eq!(fresh.agents[0].agent_status, AgentStatus::Done);
    assert_eq!(fresh.agents[1].agent_status, AgentStatus::Idle);
}
