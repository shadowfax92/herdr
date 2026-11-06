//! A just-read agent keeps its row in the priority-sorted Agents panel for
//! `READ_SETTLE`, while its status, and everything that reads it, turns read at
//! once (`super::super::read_settle`).
use super::*;
use crate::api::schema::AgentStatus;
use crate::config::AgentPanelSortConfig;

use super::super::read_settle::READ_SETTLE;

fn agent(pane_id: &str, status: AgentStatus, seq: u64) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
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

fn priority_state() -> ClientShellState {
    let mut config = Config::default();
    config.ui.agent_panel_sort = AgentPanelSortConfig::Priority;
    ClientShellState::new(ClientShellConfig::from_config(&config))
}

/// The aggregate panel's order, which agent keys and the multi-machine panel
/// walk.
fn aggregate_order(state: &ClientShellState) -> Vec<String> {
    aggregate_navigation::aggregate_agent_rows(
        &state.endpoints,
        &state.active_endpoint_id,
        AgentPanelSortConfig::Priority,
        &state.scope_view(),
    )
    .into_iter()
    .map(|row| row.agent.pane_id.clone())
    .collect()
}

/// The single-machine panel's rows: order and the status each row draws.
fn panel_rows(state: &ClientShellState) -> Vec<(String, AgentStatus)> {
    let snapshot = state.snapshot.as_deref().expect("snapshot");
    agent_sidebar::agent_rows(
        snapshot,
        &state.config,
        None,
        &state.active_endpoint_scope(snapshot),
        state.active_read_settle(),
    )
    .into_iter()
    .map(|row| (row.pane_id, row.status))
    .collect()
}

fn panel_order(state: &ClientShellState) -> Vec<String> {
    panel_rows(state)
        .into_iter()
        .map(|(pane, _)| pane)
        .collect()
}

/// pane_4 and pane_1 finish turns this client watched (pane_1 last), pane_2
/// keeps working and pane_3 was idle all along. Then pane_1 is read.
fn read_pane_1(state: &mut ClientShellState) -> ClientShellSnapshot {
    let mut working = snapshot();
    working.focused_pane_id = Some("pane_2".into());
    working.agents = vec![
        agent("pane_1", AgentStatus::Working, 4),
        agent("pane_2", AgentStatus::Working, 3),
        agent("pane_3", AgentStatus::Idle, 1),
        agent("pane_4", AgentStatus::Working, 2),
    ];
    state.set_snapshot(Box::new(working));
    let mut finished = snapshot();
    finished.revision = 2;
    finished.focused_pane_id = Some("pane_2".into());
    finished.agents = vec![
        agent("pane_1", AgentStatus::Idle, 6),
        agent("pane_2", AgentStatus::Working, 3),
        agent("pane_3", AgentStatus::Idle, 1),
        agent("pane_4", AgentStatus::Idle, 5),
    ];
    state.set_snapshot(Box::new(finished.clone()));
    // Drawn with pane_2 focused, so both finished agents show unread.
    let mut drawn = surface();
    drawn.projection_revision = 2;
    drawn.panes[0].pane_id = "pane_2".into();
    state.set_pane_surface(drawn);
    assert_eq!(
        panel_rows(state),
        [
            ("pane_1".to_owned(), AgentStatus::Done),
            ("pane_4".to_owned(), AgentStatus::Done),
            ("pane_2".to_owned(), AgentStatus::Working),
            ("pane_3".to_owned(), AgentStatus::Idle),
        ]
    );

    finished.revision = 3;
    finished.focused_pane_id = Some("pane_1".into());
    state.set_snapshot(Box::new(finished.clone()));
    let mut shown = surface();
    shown.projection_revision = 3;
    state.set_pane_surface(shown);
    finished
}

#[test]
fn a_read_agent_holds_its_row_for_the_settle_then_moves() {
    let mut state = priority_state();
    read_pane_1(&mut state);

    // Read at once: the row draws read and the rollup counts it read...
    assert_eq!(
        panel_rows(&state),
        [
            ("pane_1".to_owned(), AgentStatus::Idle),
            ("pane_4".to_owned(), AgentStatus::Done),
            ("pane_2".to_owned(), AgentStatus::Working),
            ("pane_3".to_owned(), AgentStatus::Idle),
        ],
        "the read row keeps its place among the unread"
    );
    assert_eq!(aggregate_order(&state), panel_order(&state));
    let snapshot = state.snapshot.as_deref().expect("snapshot");
    assert_eq!(snapshot.tabs[0].agent_status, AgentStatus::Done, "pane_4");

    // ...and the client loop is due to wake when the hold ends, not before.
    let deadline = state.next_read_settle_deadline().expect("held");
    assert!(deadline <= std::time::Instant::now() + READ_SETTLE);
    assert!(deadline > std::time::Instant::now() + READ_SETTLE - std::time::Duration::from_secs(1));
    assert_eq!(state.timer_delay(deadline), std::time::Duration::ZERO);
    assert!(!state.tick_read_settle(std::time::Instant::now()));
    assert_eq!(
        panel_order(&state),
        ["pane_1", "pane_4", "pane_2", "pane_3"]
    );

    assert!(state.tick_read_settle(deadline), "the expiry repaints");
    assert_eq!(
        panel_order(&state),
        ["pane_4", "pane_2", "pane_1", "pane_3"]
    );
    assert_eq!(aggregate_order(&state), panel_order(&state));
    assert_eq!(state.next_read_settle_deadline(), None);
    assert!(!state.tick_read_settle(deadline + READ_SETTLE));
}

#[test]
fn a_status_change_during_the_settle_moves_the_row_at_once() {
    let mut state = priority_state();
    let mut current = read_pane_1(&mut state);
    assert_eq!(
        panel_order(&state),
        ["pane_1", "pane_4", "pane_2", "pane_3"]
    );

    // The user prompts pane_1 within the settle: it is working again, which is
    // news, so it takes its working place in the same snapshot.
    current.revision = 4;
    current.agents[0].agent_status = AgentStatus::Working;
    current.agents[0].state_change_seq = 7;
    state.set_snapshot(Box::new(current));

    assert_eq!(
        panel_order(&state),
        ["pane_4", "pane_1", "pane_2", "pane_3"]
    );
    assert_eq!(aggregate_order(&state), panel_order(&state));
    assert_eq!(state.next_read_settle_deadline(), None);
    assert!(!state.tick_read_settle(std::time::Instant::now() + READ_SETTLE));
}

#[test]
fn keys_treat_a_held_agent_as_read() {
    let mut state = priority_state();
    read_pane_1(&mut state);

    // pane_1 still sits at the top, but alt+u only sees pane_4 as unread.
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextAttentionAgent),
        &mut outcome,
    );
    let target = match outcome.actions.as_slice() {
        [ClientShellAction::Endpoint { request, .. }] => match &request.method {
            crate::api::schema::Method::PaneFocus(target) => Some(target.pane_id.as_str()),
            _ => None,
        },
        _ => None,
    };
    assert_eq!(target, Some("pane_4"), "{:?}", outcome.actions);
}

#[test]
fn an_agent_that_finishes_while_watched_is_not_held() {
    // Either arrival order: the snapshot in which the watched pane_1 finishes
    // projects it unread, but compose draws nothing until its surface pairs
    // with it, and that pair reads pane_1. It never showed unread, so it drops
    // straight from working to its read place.
    for surface_first in [false, true] {
        let mut state = priority_state();
        let mut working = snapshot();
        working.agents = vec![
            agent("pane_1", AgentStatus::Working, 2),
            agent("pane_2", AgentStatus::Working, 1),
        ];
        state.set_snapshot(Box::new(working));
        state.set_pane_surface(surface());
        let mut finished = snapshot();
        finished.revision = 2;
        finished.agents = vec![
            agent("pane_1", AgentStatus::Idle, 3),
            agent("pane_2", AgentStatus::Working, 1),
        ];
        let mut next_surface = surface();
        next_surface.projection_revision = 2;
        if surface_first {
            state.set_pane_surface(next_surface);
            state.set_snapshot(Box::new(finished));
        } else {
            state.set_snapshot(Box::new(finished));
            state.set_pane_surface(next_surface);
        }

        assert_eq!(
            panel_rows(&state),
            [
                ("pane_2".to_owned(), AgentStatus::Working),
                ("pane_1".to_owned(), AgentStatus::Idle),
            ],
            "surface first: {surface_first}"
        );
        assert_eq!(state.next_read_settle_deadline(), None);
    }
}
