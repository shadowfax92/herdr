//! Focus and hide (`crate::scope`) across every client surface, with two
//! machines: Local and the SSH machine `studio`.
//!
//! ```text
//!  Local                          studio
//!   setup   herdr: alpha (idle)    main  fish: echo (idle)
//!           scratch: bravo (blocked)     outreach: golf (working)
//!   admin   fish: charlie (idle)   ops   top: hotel (idle)
//!   ft      worker: delta (idle)
//! ```
use super::*;
use crate::api::schema::{Method, PaneTarget};
use crate::client::endpoint::{ClientEndpointStatus, ProfileId, SavedSshEndpoint};
use crate::config::{AgentSidebarToken, SpaceSidebarToken, StatusIndicatorStyle};
use crate::input::{KeybindAction, KeybindMatch};
use crate::scope::{ScopeFocus, ScopeState, ScopeTarget};

const STUDIO_PROFILE: &str = "0123456789abcdef0123456789abcdef";

fn studio_id() -> ClientEndpointId {
    ClientEndpointId::Ssh(ProfileId::parse(STUDIO_PROFILE).expect("profile id"))
}

fn workspace(id: &str, number: usize, label: &str, active_tab: &str) -> ClientShellWorkspace {
    let mut workspace = snapshot().workspaces.remove(0);
    workspace.workspace_id = id.into();
    workspace.active_tab_id = active_tab.into();
    workspace.number = number;
    workspace.label = label.into();
    workspace.branch = None;
    workspace.focused = false;
    workspace
}

fn tab(workspace_id: &str, id: &str, number: usize, label: &str) -> ClientShellTab {
    ClientShellTab {
        tab_id: id.into(),
        workspace_id: workspace_id.into(),
        number,
        label: label.into(),
        custom_label: true,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Unknown,
    }
}

fn pane(workspace_id: &str, tab_id: &str, id: &str) -> ClientShellPane {
    ClientShellPane {
        pane_id: id.into(),
        workspace_id: workspace_id.into(),
        tab_id: tab_id.into(),
        label: None,
        cwd: Some("/repo".into()),
        foreground_cwd: Some("/repo".into()),
        focused: false,
        right_click_passthrough: false,
    }
}

fn agent(tab: &ClientShellTab, pane_id: &str, name: &str, status: AgentStatus) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: tab.workspace_id.clone(),
        tab_id: tab.tab_id.clone(),
        name: Some(name.into()),
        display_agent: None,
        agent: Some("pi".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }
}

/// A snapshot of `(workspace label, [(tab label, agent name, status)])`; ids
/// are `w<n>`, `w<n>:t<m>` and `w<n>:p<m>` like the server's.
/// One tab: `(tab label, agent name, agent status)`.
type TabLayout<'a> = (&'a str, &'a str, AgentStatus);

fn machine(boot_id: &str, layout: &[(&str, &[TabLayout<'_>])]) -> ClientShellSnapshot {
    let mut snap = snapshot();
    snap.boot_id = boot_id.into();
    snap.workspaces.clear();
    snap.tabs.clear();
    snap.panes.clear();
    snap.agents.clear();
    for (workspace_index, (label, tabs)) in layout.iter().enumerate() {
        let workspace_id = format!("w{}", workspace_index + 1);
        snap.workspaces.push(workspace(
            &workspace_id,
            workspace_index + 1,
            label,
            &format!("{workspace_id}:t1"),
        ));
        for (tab_index, (tab_label, name, status)) in tabs.iter().enumerate() {
            let tab_id = format!("{workspace_id}:t{}", tab_index + 1);
            let pane_id = format!("{workspace_id}:p{}", tab_index + 1);
            let mut tab = tab(&workspace_id, &tab_id, tab_index + 1, tab_label);
            tab.agent_status = *status;
            snap.panes.push(pane(&workspace_id, &tab_id, &pane_id));
            snap.agents.push(agent(&tab, &pane_id, name, *status));
            snap.tabs.push(tab);
        }
    }
    snap.workspaces[0].focused = true;
    snap.tabs[0].focused = true;
    snap.panes[0].focused = true;
    snap.agents[0].focused = true;
    snap.focused_workspace_id = Some("w1".into());
    snap.focused_tab_id = Some("w1:t1".into());
    snap.focused_pane_id = Some("w1:p1".into());
    for workspace in &mut snap.workspaces {
        workspace.agent_status = snap
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace.workspace_id)
            .map(|tab| tab.agent_status)
            .max_by_key(|status| super::super::status_priority(*status))
            .unwrap_or(AgentStatus::Unknown);
    }
    snap
}

fn local_machine() -> ClientShellSnapshot {
    machine(
        // The test pane surface's boot, so frames compose for real.
        "boot-1",
        &[
            (
                "setup",
                &[
                    ("herdr", "alpha", AgentStatus::Idle),
                    ("scratch", "bravo", AgentStatus::Blocked),
                ],
            ),
            ("admin", &[("fish", "charlie", AgentStatus::Idle)]),
            ("ft", &[("worker", "delta", AgentStatus::Idle)]),
        ],
    )
}

fn studio_machine() -> ClientShellSnapshot {
    machine(
        "studio-boot",
        &[
            (
                "main",
                &[
                    ("fish", "echo", AgentStatus::Idle),
                    ("outreach", "golf", AgentStatus::Working),
                ],
            ),
            ("ops", &[("top", "hotel", AgentStatus::Idle)]),
        ],
    )
}

fn shell_config(hidden: &[&str]) -> ClientShellConfig {
    let mut config = Config::default();
    config.ui.sidebar.agents.rows = vec![vec![AgentSidebarToken::Agent]];
    config.ui.sidebar.spaces.rows = vec![vec![
        SpaceSidebarToken::StateIcon,
        SpaceSidebarToken::Workspace,
    ]];
    config.ui.sidebar.agents.hidden_workspaces = hidden.iter().map(|s| s.to_string()).collect();
    config.ui.toast.delivery = crate::config::ToastDelivery::Herdr;
    config.ui.toast.delay_seconds = 0;
    let mut config = ClientShellConfig::from_config(&config);
    config.status_indicators = StatusIndicatorStyle::Symbols;
    config
}

/// Local alone: the single-machine sidebar.
fn local_shell(hidden: &[&str]) -> ClientShellState {
    let mut state = ClientShellState::new(shell_config(hidden));
    state.set_snapshot(Box::new(local_machine()));
    state.set_pane_surface(surface());
    state
}

/// Local plus `studio`: the machines panel and aggregate agent rows.
fn two_machines(hidden: &[&str]) -> ClientShellState {
    let mut state = ClientShellState::new(shell_config(hidden));
    state.set_endpoint_catalog(&[SavedSshEndpoint {
        id: ProfileId::parse(STUDIO_PROFILE).expect("profile id"),
        label: "studio".into(),
        target: "dev@studio.example".into(),
        session: "agents".into(),
        enabled: true,
    }]);
    state.set_endpoint_status(&studio_id(), ClientEndpointStatus::Online);
    state.set_snapshot(Box::new(local_machine()));
    state.set_pane_surface(surface());
    state.set_endpoint_snapshot(&studio_id(), Box::new(studio_machine()));
    state
}

fn target(text: &str) -> ScopeTarget {
    ScopeTarget::parse(text).expect("scope target")
}

fn focus(targets: &[&str]) -> ScopeState {
    ScopeState {
        focus: ScopeFocus {
            active: true,
            targets: targets.iter().map(|text| target(text)).collect(),
        },
        hidden: Vec::new(),
    }
}

fn hide(targets: &[&str]) -> ScopeState {
    ScopeState {
        hidden: targets.iter().map(|text| target(text)).collect(),
        ..ScopeState::default()
    }
}

/// The sidebar's columns of a composed frame, one trimmed string per row.
fn sidebar_lines(state: &mut ClientShellState, width: u16, height: u16) -> Vec<String> {
    let frame = state.compose(width, height).expect("shell frame");
    let sidebar = usize::from(state.layout(width, height).sidebar.width);
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .take(sidebar)
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
                .trim_matches(|c: char| c.is_whitespace() || c == '│')
                .to_owned()
        })
        .collect()
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

/// What the panels drew in one composed frame, read back from the hit map as
/// `machine:label` strings: machine headers, workspace rows and agent rows.
#[derive(Debug, Default)]
struct Panels {
    machines: Vec<String>,
    workspaces: Vec<String>,
    agents: Vec<String>,
}

fn label_of(state: &ClientShellState, endpoint_id: &ClientEndpointId) -> String {
    state.endpoint_label(endpoint_id).to_owned()
}

fn endpoint_snapshot<'a>(
    state: &'a ClientShellState,
    endpoint_id: &ClientEndpointId,
) -> &'a ClientShellSnapshot {
    state
        .endpoints
        .iter()
        .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
        .expect("endpoint snapshot")
}

fn workspace_name(state: &ClientShellState, endpoint_id: &ClientEndpointId, id: &str) -> String {
    let label = endpoint_snapshot(state, endpoint_id)
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == id)
        .map_or(id, |workspace| workspace.label.as_str());
    format!("{}:{label}", label_of(state, endpoint_id))
}

fn agent_name(state: &ClientShellState, endpoint_id: &ClientEndpointId, pane_id: &str) -> String {
    let name = endpoint_snapshot(state, endpoint_id)
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
        .and_then(|agent| agent.name.as_deref())
        .unwrap_or(pane_id);
    format!("{}:{name}", label_of(state, endpoint_id))
}

fn panels(state: &mut ClientShellState) -> Panels {
    state.compose(100, 40).expect("shell frame");
    let hits = &state.hits;
    Panels {
        machines: hits
            .machines
            .iter()
            .map(|hit| label_of(state, &hit.endpoint_id))
            .collect(),
        workspaces: hits
            .workspaces
            .iter()
            .map(|hit| workspace_name(state, &hit.endpoint_id, &hit.workspace_id))
            .collect(),
        agents: hits
            .agents
            .iter()
            .map(|(_, pane_id)| agent_name(state, &ClientEndpointId::Local, pane_id))
            .chain(
                hits.endpoint_agents
                    .iter()
                    .map(|(_, endpoint_id, pane_id)| agent_name(state, endpoint_id, pane_id)),
            )
            .collect(),
    }
}

/// The mobile switcher's machine, agent and workspace rows.
fn mobile_panels(state: &mut ClientShellState) -> Panels {
    state.mode = ClientShellMode::Navigate;
    state.compose(50, 60).expect("mobile frame");
    let mut panels = Panels::default();
    for (_, target) in &state.hits.mobile_targets {
        match target {
            ClientMobileTarget::Machine(endpoint_id) => {
                panels.machines.push(label_of(state, endpoint_id));
            }
            ClientMobileTarget::Workspace {
                endpoint_id,
                workspace_id,
            } => panels
                .workspaces
                .push(workspace_name(state, endpoint_id, workspace_id)),
            ClientMobileTarget::Agent {
                endpoint_id,
                pane_id,
            } => panels.agents.push(agent_name(state, endpoint_id, pane_id)),
            _ => {}
        }
    }
    panels
}

/// Where a key sent focus: `(machine, workspace or pane id)`.
fn key_target(state: &mut ClientShellState, action: KeybindAction) -> Option<(String, String)> {
    let mut outcome = ClientShellInput::default();
    state.record_binding(KeybindMatch::Action(action), &mut outcome);
    outcome.actions.iter().find_map(|action| match action {
        ClientShellAction::Endpoint {
            endpoint_id,
            request,
            ..
        } => match &request.method {
            Method::WorkspaceFocus(target) => {
                Some((label_of(state, endpoint_id), target.workspace_id.clone()))
            }
            Method::PaneFocus(PaneTarget { pane_id }) => {
                Some((label_of(state, endpoint_id), pane_id.clone()))
            }
            _ => None,
        },
        ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target:
                Some(ClientEndpointFocusTarget::Workspace(id) | ClientEndpointFocusTarget::Pane(id)),
        } => Some((label_of(state, endpoint_id), id.clone())),
        _ => None,
    })
}

fn names(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

// ---- machines panel (multi-machine sidebar) ----

#[test]
fn focusing_a_workspace_keeps_only_it_and_drops_the_machine_with_nothing_admitted() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(focus(&["setup"])));
    let drawn = panels(&mut state);
    // studio has nothing in focus, so even its header leaves.
    assert_eq!(drawn.machines, names(&["Local"]));
    assert_eq!(drawn.workspaces, names(&["Local:setup"]));
    assert_eq!(drawn.agents, names(&["Local:alpha", "Local:bravo"]));

    let text = frame_text(&mut state, 100, 40);
    assert!(text.contains("setup"), "frame: {text}");
    for gone in ["admin", "studio", "main", "charlie", "echo"] {
        assert!(!text.contains(gone), "{gone} left in frame: {text}");
    }
}

#[test]
fn focusing_one_tab_on_a_remote_machine_keeps_its_workspace_row_and_agent() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(focus(&["studio:main/outreach"])));
    let drawn = panels(&mut state);
    assert_eq!(drawn.machines, names(&["studio"]));
    assert_eq!(drawn.workspaces, names(&["studio:main"]));
    assert_eq!(drawn.agents, names(&["studio:golf"]));
}

#[test]
fn hiding_a_workspace_and_a_tab_removes_their_rows_and_agents_on_both_machines() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(hide(&["admin", "studio:main/outreach"])));
    let drawn = panels(&mut state);
    assert_eq!(drawn.machines, names(&["Local", "studio"]));
    assert_eq!(
        drawn.workspaces,
        names(&["Local:setup", "Local:ft", "studio:main", "studio:ops"])
    );
    assert_eq!(
        drawn.agents,
        names(&[
            "Local:alpha",
            "Local:bravo",
            "Local:delta",
            "studio:echo",
            "studio:hotel"
        ])
    );
    // `main` keeps its row for `fish`, but its icon no longer shows the hidden
    // tab's working agent.
    let lines = sidebar_lines(&mut state, 100, 40);
    assert!(lines.iter().any(|line| line == "○ main"), "{lines:#?}");
}

#[test]
fn the_more_specific_target_wins_and_focus_wins_ties() {
    // Focus `setup`, hide its `scratch` tab: setup shows without scratch, and
    // its row stops lighting up for scratch's blocked agent.
    let mut state = two_machines(&[]);
    let mut scope = focus(&["setup"]);
    scope.hidden.push(target("setup/scratch"));
    state.set_scope_projection(Some(scope));
    let drawn = panels(&mut state);
    assert_eq!(drawn.workspaces, names(&["Local:setup"]));
    assert_eq!(drawn.agents, names(&["Local:alpha"]));
    let lines = sidebar_lines(&mut state, 100, 40);
    assert!(lines.iter().any(|line| line == "○ setup"), "{lines:#?}");

    // Focusing `ft` overrides the config's hide of `ft` (a tie at workspace
    // specificity), on every machine the hide covers.
    let mut state = two_machines(&["ft"]);
    let mut scope = ScopeState::from_config_hidden(&["ft".into()]);
    scope.focus = focus(&["ft"]).focus;
    state.set_scope_projection(Some(scope));
    let drawn = panels(&mut state);
    assert_eq!(drawn.workspaces, names(&["Local:ft"]));
    assert_eq!(drawn.agents, names(&["Local:delta"]));
}

#[test]
fn a_machine_level_hide_drops_the_header_and_everything_under_it() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(hide(&["studio:"])));
    let drawn = panels(&mut state);
    assert_eq!(drawn.machines, names(&["Local"]));
    assert!(drawn.workspaces.iter().all(|row| row.starts_with("Local:")));
    assert!(drawn.agents.iter().all(|row| row.starts_with("Local:")));
    assert!(!state.scope_shows_machine(&studio_id()));
    assert!(state.scope_shows_machine(&ClientEndpointId::Local));
}

#[test]
fn the_collapsed_machines_panel_follows_the_scope() {
    let mut state = two_machines(&[]);
    state.sidebar_collapsed = true;
    state.set_scope_projection(Some(focus(&["admin", "studio:ops"])));
    let drawn = panels(&mut state);
    assert_eq!(drawn.machines, names(&["Local", "studio"]));
    assert_eq!(drawn.workspaces, names(&["Local:admin", "studio:ops"]));
    assert_eq!(drawn.agents, names(&["Local:charlie", "studio:hotel"]));

    state.set_scope_projection(Some(focus(&["admin"])));
    let drawn = panels(&mut state);
    assert_eq!(drawn.machines, names(&["Local"]));
    assert_eq!(drawn.workspaces, names(&["Local:admin"]));
}

// ---- single-machine sidebar ----

#[test]
fn the_single_machine_sidebar_follows_the_scope_expanded_and_collapsed() {
    let mut state = local_shell(&[]);
    state.set_scope_projection(Some(hide(&["admin", "setup/scratch"])));
    let drawn = panels(&mut state);
    assert_eq!(drawn.workspaces, names(&["Local:setup", "Local:ft"]));
    assert_eq!(drawn.agents, names(&["Local:alpha", "Local:delta"]));
    let lines = sidebar_lines(&mut state, 100, 40);
    assert!(lines.iter().any(|line| line == "○ setup"), "{lines:#?}");
    assert!(
        !lines.iter().any(|line| line.ends_with("admin")),
        "{lines:#?}"
    );

    // The collapsed rail draws the same rows, numbered in order.
    state.sidebar_collapsed = true;
    let drawn = panels(&mut state);
    assert_eq!(drawn.workspaces, names(&["Local:setup", "Local:ft"]));
    assert_eq!(drawn.agents, names(&["Local:alpha", "Local:delta"]));
    assert_eq!(
        state.hits.workspaces[1].rect.y,
        state.hits.workspaces[0].rect.y + 1,
        "rows close up over the hidden one"
    );
}

#[test]
fn a_collapsed_parent_does_not_light_up_for_an_out_of_scope_child() {
    let mut state = local_shell(&[]);
    let mut snap = local_machine();
    // `setup` is the parent checkout, `ft` its linked worktree, whose agent is
    // blocked; collapse the group so the parent rolls the child up.
    let worktree = |linked| {
        Some(crate::protocol::ClientShellWorktree {
            key: "repo".into(),
            label: "repo".into(),
            is_linked_worktree: linked,
        })
    };
    snap.workspaces[0].worktree = worktree(false);
    snap.workspaces[2].worktree = worktree(true);
    for status in [
        &mut snap.tabs[1].agent_status,
        &mut snap.agents[1].agent_status,
    ] {
        *status = AgentStatus::Idle;
    }
    snap.tabs[3].agent_status = AgentStatus::Blocked;
    snap.agents[3].agent_status = AgentStatus::Blocked;
    snap.workspaces[0].agent_status = AgentStatus::Idle;
    snap.workspaces[2].agent_status = AgentStatus::Blocked;
    state.set_snapshot(Box::new(snap));
    state.collapsed_groups.insert("repo".into());

    let lines = sidebar_lines(&mut state, 100, 40);
    assert!(
        lines.iter().any(|line| line.starts_with("× setup")),
        "{lines:#?}"
    );

    state.set_scope_projection(Some(hide(&["ft"])));
    let lines = sidebar_lines(&mut state, 100, 40);
    assert!(
        lines.iter().any(|line| line.starts_with("○ setup")),
        "{lines:#?}"
    );
    // With its only child out of scope, setup is no longer a group.
    assert!(state
        .hits
        .workspaces
        .iter()
        .all(|hit| hit.group_toggle.is_none()));
}

// ---- agents panel, mobile ----

#[test]
fn the_mobile_switcher_lists_only_in_scope_machines_agents_and_spaces() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(focus(&["setup/herdr", "studio:main"])));
    let drawn = mobile_panels(&mut state);
    assert_eq!(drawn.machines, names(&["Local", "studio"]));
    assert_eq!(
        drawn.agents,
        names(&["Local:alpha", "studio:echo", "studio:golf"])
    );
    assert_eq!(drawn.workspaces, names(&["Local:setup", "studio:main"]));

    state.set_scope_projection(Some(focus(&["studio:main"])));
    let drawn = mobile_panels(&mut state);
    assert_eq!(drawn.machines, names(&["studio"]));
}

#[test]
fn the_mobile_header_counts_only_agents_in_scope() {
    let mut state = local_shell(&[]);
    // The header's summary counts the blocked `bravo` while it is in scope.
    let text = frame_text(&mut state, 50, 30);
    assert!(text.contains("1 blocked"), "frame: {text}");

    state.set_scope_projection(Some(hide(&["setup/scratch"])));
    let text = frame_text(&mut state, 50, 30);
    assert!(!text.contains("blocked"), "frame: {text}");
    assert!(text.contains("all idle"), "frame: {text}");
}

// ---- agent keys ----

#[test]
fn agent_keys_skip_out_of_scope_agents_on_one_machine() {
    let mut state = local_shell(&[]);
    // alpha (focused) → bravo would be next; hiding its tab skips to charlie.
    state.set_scope_projection(Some(hide(&["setup/scratch"])));
    assert_eq!(
        key_target(&mut state, KeybindAction::NextAgent),
        Some(("Local".into(), "w2:p1".into()))
    );
    assert_eq!(
        key_target(&mut state, KeybindAction::FocusAgent(1)),
        Some(("Local".into(), "w2:p1".into()))
    );
    assert_eq!(
        key_target(&mut state, KeybindAction::PreviousAgent),
        Some(("Local".into(), "w3:p1".into()))
    );
}

#[test]
fn agent_keys_skip_out_of_scope_agents_across_machines() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(focus(&["setup/herdr", "studio:ops"])));
    // alpha (focused) → hotel on studio, past every unfocused agent.
    assert_eq!(
        key_target(&mut state, KeybindAction::NextAgent),
        Some(("studio".into(), "w2:p1".into()))
    );
    assert_eq!(
        key_target(&mut state, KeybindAction::FocusAgent(1)),
        Some(("studio".into(), "w2:p1".into()))
    );
    assert!(
        state.indexed_navigation_target_exists(&KeybindMatch::Action(KeybindAction::FocusAgent(1)))
    );
    assert!(!state
        .indexed_navigation_target_exists(&KeybindMatch::Action(KeybindAction::FocusAgent(2))));
}

#[test]
fn next_attention_agent_skips_out_of_scope_agents() {
    let mut state = two_machines(&[]);
    // bravo is the only blocked agent; hiding its tab leaves nothing to visit.
    assert_eq!(
        key_target(&mut state, KeybindAction::NextAttentionAgent),
        Some(("Local".into(), "w1:p2".into()))
    );
    state.set_scope_projection(Some(hide(&["setup/scratch"])));
    assert_eq!(
        key_target(&mut state, KeybindAction::NextAttentionAgent),
        None
    );
    assert!(state.visible_endpoint_notice.is_some());
}

// ---- workspace keys ----

#[test]
fn workspace_keys_skip_out_of_scope_workspaces_on_one_machine() {
    let mut state = local_shell(&[]);
    state.set_scope_projection(Some(hide(&["admin"])));
    assert_eq!(
        key_target(&mut state, KeybindAction::NextWorkspace),
        Some(("Local".into(), "w3".into()))
    );
    assert_eq!(
        key_target(&mut state, KeybindAction::SwitchWorkspace(1)),
        Some(("Local".into(), "w3".into()))
    );
    assert!(
        !state.indexed_navigation_target_exists(&KeybindMatch::Action(
            KeybindAction::SwitchWorkspace(2)
        ))
    );

    state.previous_workspace_id = Some("w2".into());
    assert_eq!(key_target(&mut state, KeybindAction::LastWorkspace), None);
    state.previous_workspace_id = Some("w3".into());
    assert_eq!(
        key_target(&mut state, KeybindAction::LastWorkspace),
        Some(("Local".into(), "w3".into()))
    );
}

#[test]
fn workspace_keys_from_an_out_of_scope_workspace_step_to_the_nearest_listed_one() {
    let mut state = local_shell(&[]);
    // The focused `setup` is out of scope, reached by an explicit jump.
    state.set_scope_projection(Some(focus(&["admin", "ft"])));
    assert_eq!(
        key_target(&mut state, KeybindAction::NextWorkspace),
        Some(("Local".into(), "w2".into()))
    );
    assert_eq!(
        key_target(&mut state, KeybindAction::PreviousWorkspace),
        Some(("Local".into(), "w3".into()))
    );
}

#[test]
fn workspace_keys_skip_out_of_scope_workspaces_across_machines() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(hide(&["admin", "ft", "studio:main"])));
    assert_eq!(
        key_target(&mut state, KeybindAction::NextWorkspace),
        Some(("studio".into(), "w2".into()))
    );
    assert_eq!(
        key_target(&mut state, KeybindAction::PreviousWorkspace),
        Some(("studio".into(), "w2".into()))
    );
}

#[test]
fn the_workspace_picker_walks_only_in_scope_workspaces() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(hide(&["admin", "studio:main"])));
    let mut visited = Vec::new();
    for _ in 0..4 {
        state.move_navigate_workspace(1);
        let target = state.navigate_workspace_id.clone().expect("picker target");
        visited.push(workspace_name(
            &state,
            &target.endpoint_id,
            &target.workspace_id,
        ));
    }
    assert_eq!(
        visited,
        names(&["Local:setup", "Local:ft", "studio:ops", "Local:setup"])
    );
}

// ---- go-to navigator ----

#[test]
fn the_navigator_lists_only_in_scope_machines_workspaces_and_panes() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(focus(&["setup/herdr", "studio:main"])));
    state.open_navigator_overlay();
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("navigator");
    };
    let rows = render::client_navigator_rows(
        &state.endpoints,
        &state.active_endpoint_id,
        navigator,
        &state.scope_view(),
    );
    let targets = rows
        .iter()
        .map(|row| match &row.target {
            ClientNavigatorTarget::Machine { endpoint_id } => label_of(&state, endpoint_id),
            ClientNavigatorTarget::Workspace {
                endpoint_id,
                workspace_id,
            } => workspace_name(&state, endpoint_id, workspace_id),
            ClientNavigatorTarget::Pane {
                endpoint_id,
                pane_id,
            } => format!("{}:{pane_id}", label_of(&state, endpoint_id)),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        names(&[
            "Local",
            "Local:setup",
            "Local:w1:p1",
            "studio",
            "studio:main",
            "studio:w1:p1",
            "studio:w1:p2",
        ])
    );
}

// ---- notifications ----

fn attention(workspace_id: &str, tab_id: &str, pane_id: &str, title: &str) -> SemanticNotification {
    SemanticNotification {
        kind: SemanticNotificationKind::NeedsAttention,
        title: title.into(),
        body: None,
        sound: Some(SemanticNotificationSound::Request),
        agent: Some("pi".into()),
        workspace_id: Some(workspace_id.into()),
        tab_id: Some(tab_id.into()),
        pane_id: Some(pane_id.into()),
        position: None,
    }
}

fn custom(title: &str) -> SemanticNotification {
    SemanticNotification {
        kind: SemanticNotificationKind::Custom,
        title: title.into(),
        body: None,
        sound: None,
        agent: None,
        workspace_id: None,
        tab_id: None,
        pane_id: None,
        position: None,
    }
}

#[test]
fn out_of_scope_notifications_are_dropped_with_their_sound() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(focus(&["admin", "studio:main/outreach"])));
    let now = std::time::Instant::now();

    // No delay: bravo's tab is outside focus, so no toast and no sound.
    let (effects, _) = state.receive_notification(
        &ClientEndpointId::Local,
        attention("w1", "w1:t2", "w1:p2", "bravo needs you"),
        now,
    );
    assert!(effects.is_empty(), "{} effects", effects.len());
    assert!(state.visible_notification.is_none());

    // The focused remote tab still notifies, sound and all.
    let (effects, _) = state.receive_notification(
        &studio_id(),
        attention("w1", "w1:t2", "w1:p2", "golf needs you"),
        now,
    );
    assert!(state
        .visible_notification
        .as_ref()
        .is_some_and(|visible| visible.event.title == "golf needs you"));
    assert!(matches!(
        effects.as_slice(),
        [ClientShellNotificationEffect::Sound { .. }]
    ));

    // A notification naming no workspace or tab is unscoped and passes.
    state.visible_notification = None;
    state.receive_notification(&ClientEndpointId::Local, custom("update ready"), now);
    assert!(state
        .visible_notification
        .as_ref()
        .is_some_and(|visible| visible.event.title == "update ready"));
}

#[test]
fn delayed_notifications_are_judged_by_the_scope_when_they_come_due() {
    let mut state = local_shell(&[]);
    state.config.toast_delay_seconds = 5;
    let now = std::time::Instant::now();
    state.receive_notification(
        &ClientEndpointId::Local,
        attention("w1", "w1:t2", "w1:p2", "bravo needs you"),
        now,
    );
    assert_eq!(state.pending_notifications.len(), 1);

    // The tab leaves the scope while the notification waits out its delay.
    state.set_scope_projection(Some(hide(&["setup/scratch"])));
    let (effects, _) = state.tick_notifications(now + std::time::Duration::from_secs(6));
    assert!(effects.is_empty(), "{} effects", effects.len());
    assert!(state.visible_notification.is_none());
    assert!(state.pending_notifications.is_empty());
}

#[test]
fn a_scope_change_purges_visible_and_queued_toasts_now_out_of_scope() {
    let mut state = two_machines(&[]);
    let now = std::time::Instant::now();
    for (endpoint_id, workspace_id, tab_id, pane_id, title) in [
        (ClientEndpointId::Local, "w1", "w1:t2", "w1:p2", "bravo"),
        (studio_id(), "w1", "w1:t2", "w1:p2", "golf"),
        (ClientEndpointId::Local, "w2", "w2:t1", "w2:p1", "charlie"),
    ] {
        state.receive_notification(
            &endpoint_id,
            SemanticNotification {
                kind: SemanticNotificationKind::Custom,
                ..attention(workspace_id, tab_id, pane_id, title)
            },
            now,
        );
    }
    state.receive_notification(&ClientEndpointId::Local, custom("update ready"), now);
    let titles = |state: &ClientShellState| {
        state
            .visible_notification
            .iter()
            .chain(&state.queued_notifications)
            .map(|toast| toast.event.title.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        titles(&state),
        names(&["bravo", "golf", "charlie", "update ready"])
    );

    // Hiding Local's `setup` drops the visible bravo toast and promotes the
    // next one still in scope; focusing studio then drops charlie.
    state.set_scope_projection(Some(hide(&["setup"])));
    assert_eq!(titles(&state), names(&["golf", "charlie", "update ready"]));
    state.set_scope_projection(Some(focus(&["studio:"])));
    assert_eq!(titles(&state), names(&["golf", "update ready"]));
}

// ---- gate helpers and the config fallback ----

#[test]
fn one_off_gates_match_the_view() {
    let mut state = two_machines(&[]);
    state.set_scope_projection(Some(focus(&["studio:main/outreach"])));
    let studio = endpoint_snapshot(&state, &studio_id()).clone();
    let (main, fish, outreach) = (&studio.workspaces[0], &studio.tabs[0], &studio.tabs[1]);
    assert!(state.scope_admits_tab(&studio_id(), main, outreach));
    assert!(!state.scope_admits_tab(&studio_id(), main, fish));
    assert!(state.scope_shows_workspace(&studio_id(), &studio, main));
    assert!(!state.scope_shows_workspace(&studio_id(), &studio, &studio.workspaces[1]));
    assert!(state.scope_shows_machine(&studio_id()));
    assert!(!state.scope_shows_machine(&ClientEndpointId::Local));

    let view = state.scope_view();
    let studio_scope = view.endpoint(&studio_id());
    assert!(studio_scope.admits_tab("w1", "w1:t2"));
    assert!(!studio_scope.admits_tab("w1", "w1:t1"));
    assert!(!view.endpoint(&ClientEndpointId::Local).shows_machine());
}

#[test]
fn without_a_projection_the_config_hides_apply_to_every_machine() {
    let mut state = two_machines(&["ft", "ops"]);
    let drawn = panels(&mut state);
    assert_eq!(
        drawn.workspaces,
        names(&["Local:setup", "Local:admin", "studio:main"])
    );
    // A projection replaces the fallback outright: un-hiding at runtime works.
    state.set_scope_projection(Some(ScopeState::default()));
    assert_eq!(panels(&mut state).workspaces.len(), 5);
    // Losing the projection (an older Local server) brings the config back.
    state.set_scope_projection(None);
    assert_eq!(panels(&mut state).workspaces.len(), 3);
}

// ---- cost ----

/// `count` workspaces of two tabs, each tab one idle agent.
fn scaled_machine(boot_id: &str, count: usize) -> ClientShellSnapshot {
    let labels = (0..count)
        .map(|index| (format!("space-{index}"), format!("tab-{index}")))
        .collect::<Vec<_>>();
    let tabs = labels
        .iter()
        .map(|(_, tab)| {
            [
                (tab.as_str(), "agent", AgentStatus::Idle),
                ("logs", "agent", AgentStatus::Idle),
            ]
        })
        .collect::<Vec<_>>();
    let layout = labels
        .iter()
        .zip(&tabs)
        .map(|((workspace, _), tabs)| (workspace.as_str(), &tabs[..]))
        .collect::<Vec<_>>();
    machine(boot_id, &layout)
}

/// Compose cost on two machines with no scope, a config hide (the fallback,
/// rebuilt every frame) and an engaged focus, at 1 and 16 workspaces per
/// machine. Run with
/// `cargo test --release --bin herdr scope_compose_profile -- --ignored --nocapture`.
#[test]
#[ignore = "manual scope composition scaling profile"]
fn scope_compose_profile() {
    for workspaces in [1, 16] {
        for (label, hidden, scope) in [
            ("none", &[][..], None),
            ("config hide", &["space-0"][..], None),
            (
                "focus",
                &[][..],
                Some(focus(&["space-0", "studio:space-1/logs"])),
            ),
        ] {
            let mut state = two_machines(hidden);
            state.set_snapshot(Box::new(scaled_machine("boot-1", workspaces)));
            state.set_endpoint_snapshot(
                &studio_id(),
                Box::new(scaled_machine("studio-boot", workspaces)),
            );
            state.set_scope_projection(scope);
            for _ in 0..50 {
                std::hint::black_box(state.compose(120, 40).expect("frame"));
            }
            let mut samples = (0..500)
                .map(|_| {
                    let start = std::time::Instant::now();
                    std::hint::black_box(state.compose(120, 40).expect("frame"));
                    start.elapsed()
                })
                .collect::<Vec<_>>();
            samples.sort_unstable();
            eprintln!(
                "scope compose: workspaces={workspaces}x2 machines scope={label:<11} median_us={} p95_us={}",
                samples[samples.len() / 2].as_micros(),
                samples[samples.len() * 95 / 100].as_micros(),
            );
        }
    }
}
