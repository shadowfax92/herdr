//! Visit rules are exercised through the shell's snapshot, surface, input and
//! timer entry points. No clock sleeps or standalone state-machine mocks.
use super::landing_glow::{tab_snapshot, tab_surface};
use super::*;
use crate::protocol::endpoint::{EndpointPaneFlag, EndpointPaneFlags};
use std::time::{Duration, Instant};

fn focus(state: &mut ClientShellState, pane: &str, revision: u64, at: Instant) {
    state.pane_flags.clock = Some(at);
    let mut snapshot = tab_snapshot(revision, pane);
    snapshot.agents.clear(); // Flags must work on ordinary shells.
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(tab_surface(
        "boot-1",
        revision,
        &["pane_1", "pane_2", "pane_3"],
        pane,
    ));
}

fn flags(state: &mut ClientShellState, values: &[(&str, u64)], at: Instant) {
    state.pane_flags.clock = Some(at);
    state.set_endpoint_pane_flags(
        &ClientEndpointId::Local,
        1,
        EndpointPaneFlags {
            boot_id: "boot-1".into(),
            flags: values
                .iter()
                .map(|(pane, seq)| EndpointPaneFlag {
                    pane_id: (*pane).into(),
                    flag: crate::api::schema::PaneFlag {
                        message: Some("review".into()),
                        seq: *seq,
                    },
                })
                .collect(),
        },
    );
}

fn shell(at: Instant) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    focus(&mut state, "pane_1", 1, at);
    flags(&mut state, &[], at);
    state.set_endpoint_methods(Some(vec!["pane.unflag".into()]));
    state
}

fn clears(outcome: ClientShellInput) -> Vec<String> {
    outcome
        .actions
        .into_iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => match request.method {
                crate::api::schema::Method::PaneUnflag(target) => {
                    assert!(target.expected_seq.is_some());
                    Some(target.pane_id)
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn tick(state: &mut ClientShellState, at: Instant) -> Vec<String> {
    state.pane_flags.clock = Some(at);
    let mut outcome = ClientShellInput::default();
    state.tick_pane_flags(at, &mut outcome);
    clears(outcome)
}

#[test]
fn pane_flags_pass_through_survives_then_arrival_dwell_clears_only_focused_pane() {
    let t = Instant::now();
    let mut state = shell(t);
    flags(&mut state, &[("pane_2", 1), ("pane_3", 2)], t);
    assert!(state.tab_has_pane_flags("tab_1"));
    focus(&mut state, "pane_2", 2, t);
    assert!(tick(&mut state, t + Duration::from_millis(999)).is_empty());
    focus(&mut state, "pane_1", 3, t + Duration::from_millis(999));
    assert!(tick(&mut state, t + Duration::from_secs(2)).is_empty());
    focus(&mut state, "pane_2", 4, t + Duration::from_secs(2));
    assert_eq!(tick(&mut state, t + Duration::from_secs(3)), ["pane_2"]);
    assert!(tick(&mut state, t + Duration::from_secs(4)).is_empty());
}

#[test]
fn pane_flags_set_while_focused_and_reflags_do_not_start_or_inherit_dwell() {
    let t = Instant::now();
    let mut state = shell(t);
    flags(&mut state, &[("pane_1", 1)], t);
    assert!(tick(&mut state, t + Duration::from_secs(9)).is_empty());
    focus(&mut state, "pane_2", 2, t + Duration::from_secs(9));
    focus(&mut state, "pane_1", 3, t + Duration::from_secs(10));
    flags(
        &mut state,
        &[("pane_1", 2)],
        t + Duration::from_millis(10500),
    );
    assert!(tick(&mut state, t + Duration::from_secs(15)).is_empty());
    assert_eq!(clears(state.handle_input_bytes(b"x")), ["pane_1"]);
}

#[test]
fn pane_flags_host_focus_pauses_dwell_and_default_focus_counts() {
    let t = Instant::now();
    let mut state = shell(t);
    flags(&mut state, &[("pane_2", 1)], t);
    focus(&mut state, "pane_2", 2, t);
    state.pane_flags.clock = Some(t + Duration::from_millis(400));
    state.handle_raw_events(vec![RawInputEvent::OuterFocusLost]);
    assert!(tick(&mut state, t + Duration::from_secs(30)).is_empty());
    state.handle_raw_events(vec![RawInputEvent::OuterFocusGained]);
    assert!(tick(&mut state, t + Duration::from_millis(30599)).is_empty());
    assert_eq!(
        tick(&mut state, t + Duration::from_millis(30600)),
        ["pane_2"]
    );
}

#[test]
fn pane_flags_paste_and_forwarded_click_clear_but_navigation_and_hover_do_not() {
    for input in [b"x".as_slice(), b"\x1b[200~hello\x1b[201~"] {
        let t = Instant::now();
        let mut state = shell(t);
        flags(&mut state, &[("pane_1", 1)], t);
        assert_eq!(clears(state.handle_input_bytes(input)), ["pane_1"]);
    }
    let t = Instant::now();
    let mut state = shell(t);
    flags(&mut state, &[("pane_1", 1)], t);
    let mouse = |kind| ClientMessage::ClientShellPaneInput {
        pane_id: "pane_1".into(),
        events: vec![ClientPaneInputEvent::Mouse {
            kind,
            position: crate::protocol::ClientMousePosition::Cell { column: 1, row: 1 },
            geometry: None,
            modifiers: 0,
            lines: 0,
        }],
    };
    let mut outcome = ClientShellInput {
        requests: vec![mouse(crate::protocol::ClientMouseKind::Moved)],
        ..Default::default()
    };
    state.clear_pane_flag_on_input(&mut outcome);
    assert!(clears(outcome).is_empty());
    let mut outcome = ClientShellInput {
        requests: vec![mouse(crate::protocol::ClientMouseKind::Down(
            crate::protocol::ClientMouseButton::Left,
        ))],
        ..Default::default()
    };
    state.clear_pane_flag_on_input(&mut outcome);
    assert_eq!(clears(outcome), ["pane_1"]);
}

#[test]
fn pane_flags_old_servers_receive_no_unflag_and_keep_the_flag() {
    for methods in [None, Some(Vec::new())] {
        let t = Instant::now();
        let mut state = shell(t);
        state.set_endpoint_methods(methods);
        flags(&mut state, &[("pane_2", 1)], t);
        focus(&mut state, "pane_2", 2, t);
        assert!(tick(&mut state, t + Duration::from_secs(1)).is_empty());
        let input = state.handle_input_bytes(b"x");
        assert!(clears(input).is_empty());
        assert!(state.pane_flags().unwrap().contains_key("pane_2"));
        assert!(state.visible_endpoint_notice.is_none());
    }
}

#[test]
fn pane_flags_tab_and_workspace_arrivals_start_dwell_for_only_the_shown_pane() {
    let t = Instant::now();
    let mut state = shell(t);
    flags(&mut state, &[("pane_2", 1), ("pane_3", 2)], t);
    let mut next = tab_snapshot(2, "pane_2");
    next.focused_tab_id = Some("tab_2".into());
    next.focused_workspace_id = Some("ws_2".into());
    next.panes[1].tab_id = "tab_2".into();
    next.panes[1].workspace_id = "ws_2".into();
    state.pane_flags.clock = Some(t);
    state.set_snapshot(Box::new(next));
    state.set_pane_surface(tab_surface("boot-1", 2, &["pane_2", "pane_3"], "pane_2"));
    assert_eq!(tick(&mut state, t + Duration::from_secs(1)), ["pane_2"]);
}

#[test]
fn pane_flags_an_unrelated_snapshot_does_not_manufacture_arrival() {
    let t = Instant::now();
    let mut state = shell(t);
    flags(&mut state, &[("pane_1", 1)], t);
    focus(&mut state, "pane_1", 2, t + Duration::from_secs(1));
    assert!(tick(&mut state, t + Duration::from_secs(5)).is_empty());
    state.set_endpoint_pane_flags(
        &ClientEndpointId::Local,
        1,
        EndpointPaneFlags {
            boot_id: "retired-boot".into(),
            flags: Vec::new(),
        },
    );
    assert!(state.pane_flags().is_none());
}

#[test]
fn pane_flags_are_isolated_by_endpoint_and_connection_generation() {
    use crate::client::endpoint::{ProfileId, SavedSshEndpoint};
    let t = Instant::now();
    let mut state = shell(t);
    flags(&mut state, &[("pane_2", 1)], t);
    let profile = SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Remote".into(),
        target: "example.test".into(),
        session: "test".into(),
        enabled: true,
    };
    let remote = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
    state.cache_endpoint_snapshot_for_generation(&remote, 2, Box::new(tab_snapshot(1, "pane_1")));
    state.set_endpoint_pane_flags(
        &remote,
        2,
        EndpointPaneFlags {
            boot_id: "boot-1".into(),
            flags: vec![EndpointPaneFlag {
                pane_id: "pane_3".into(),
                flag: crate::api::schema::PaneFlag {
                    message: None,
                    seq: 7,
                },
            }],
        },
    );
    assert!(state.pane_flags().unwrap().contains_key("pane_2"));
    assert!(!state.pane_flags().unwrap().contains_key("pane_3"));
    assert!(state.activate_endpoint_projection(&remote));
    assert!(state.pane_flags().unwrap().contains_key("pane_3"));
    assert!(!state.pane_flags().unwrap().contains_key("pane_2"));
    state.set_endpoint_pane_flags(
        &remote,
        1,
        EndpointPaneFlags {
            boot_id: "boot-1".into(),
            flags: Vec::new(),
        },
    );
    assert!(state.pane_flags().unwrap().contains_key("pane_3"));
}

#[test]
fn pane_flags_failed_timed_out_and_cancelled_clears_are_silent_and_retryable() {
    for code in [Some("pane_not_found"), Some("endpoint_timeout"), None] {
        let t = Instant::now();
        let mut state = shell(t);
        flags(&mut state, &[("pane_1", 1)], t);
        let first = state.handle_input_bytes(b"x");
        let ClientShellAction::Endpoint { request, .. } = &first.actions[0] else {
            panic!("unflag request");
        };
        let id = request.id.clone();
        assert!(
            clears(state.handle_input_bytes(b"y")).is_empty(),
            "one request in flight"
        );
        let repaint = match code {
            Some(code) => {
                state
                    .handle_endpoint_result(
                        "boot-1",
                        &id,
                        Err(ClientShellEndpointError {
                            code: Some(code.into()),
                            message: "clear failed".into(),
                        }),
                    )
                    .0
            }
            None => state.cancel_endpoint_request(&id),
        };
        assert!(
            !repaint,
            "automatic clear {code:?} must not request a notice repaint"
        );
        assert!(
            state.visible_endpoint_notice.is_none(),
            "automatic clear {code:?} must be silent"
        );
        assert_eq!(clears(state.handle_input_bytes(b"z")), ["pane_1"]);
        assert!(state.visible_endpoint_notice.is_none());
    }
}

#[test]
fn pane_flags_automatic_clear_results_preserve_existing_notices() {
    for code in [
        Some("pane_not_found"),
        Some("endpoint_timeout"),
        Some("endpoint_cancelled"),
        None,
    ] {
        let t = Instant::now();
        let mut state = shell(t);
        flags(&mut state, &[("pane_1", 1)], t);
        state.push_endpoint_notice(
            ClientEndpointNoticeKind::Timeout,
            "pane.unflag",
            "Existing notice",
            "Keep this notice",
        );
        let notice = state.visible_endpoint_notice.as_ref().unwrap();
        let before = (
            notice.key.clone(),
            notice.title.clone(),
            notice.body.clone(),
            notice.deadline,
        );
        let seen = state.endpoint_notice_seen.clone();
        let input = state.handle_input_bytes(b"x");
        let ClientShellAction::Endpoint { request, .. } = &input.actions[0] else {
            panic!("unflag request");
        };
        let result = match code {
            Some(code) => Err(ClientShellEndpointError {
                code: Some(code.into()),
                message: "clear failed".into(),
            }),
            None => Ok(crate::api::schema::ResponseResult::PaneUnflag { changed: true }),
        };
        let (repaint, actions) = state.handle_endpoint_result("boot-1", &request.id, result);
        assert!(!repaint);
        assert!(actions.is_empty());
        let notice = state
            .visible_endpoint_notice
            .as_ref()
            .expect("existing notice preserved");
        assert_eq!(
            (
                notice.key.clone(),
                notice.title.clone(),
                notice.body.clone(),
                notice.deadline
            ),
            before
        );
        assert_eq!(state.endpoint_notice_seen, seen);
    }
}

#[test]
fn pane_flags_keep_elapsed_visit_while_endpoint_input_is_fenced() {
    let t = Instant::now();
    let mut state = shell(t);
    flags(&mut state, &[("pane_2", 9)], t);
    focus(&mut state, "pane_2", 2, t);
    state.set_pane_flag_input_available(false);
    assert!(tick(&mut state, t + Duration::from_secs(2)).is_empty());
    assert_eq!(state.pane_flags.next_deadline(), None);
    state.set_pane_flag_input_available(true);
    assert_eq!(tick(&mut state, t + Duration::from_secs(2)), ["pane_2"]);
}

#[test]
fn pane_flags_reflag_cancels_old_dwell_then_a_fresh_visit_clears_new_seq() {
    let t = Instant::now();
    let mut state = shell(t);
    flags(&mut state, &[("pane_2", 1)], t);
    focus(&mut state, "pane_2", 2, t);
    flags(&mut state, &[("pane_2", 2)], t + Duration::from_millis(500));
    assert!(tick(&mut state, t + Duration::from_secs(2)).is_empty());
    focus(&mut state, "pane_1", 3, t + Duration::from_secs(2));
    focus(&mut state, "pane_2", 4, t + Duration::from_secs(3));
    let mut outcome = ClientShellInput::default();
    state.tick_pane_flags(t + Duration::from_secs(4), &mut outcome);
    let ClientShellAction::Endpoint { request, .. } = &outcome.actions[0] else {
        panic!("unflag action");
    };
    let crate::api::schema::Method::PaneUnflag(params) = &request.method else {
        panic!("unflag method");
    };
    assert_eq!(params.expected_seq, Some(2));
}

#[test]
fn pane_flags_absent_projection_does_not_create_a_visit_or_block_later_flags() {
    let t = Instant::now();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_endpoint_methods(Some(vec!["pane.unflag".into()]));
    for revision in 1..=3 {
        focus(&mut state, "pane_1", revision, t);
        assert!(tick(&mut state, t + Duration::from_secs(2)).is_empty());
        assert!(state.pane_flags().is_none());
    }
    // An empty initial projection acknowledges the current focus before a
    // later flag arrives. That later flag must still wait for actual input.
    flags(&mut state, &[], t);
    flags(&mut state, &[("pane_1", 1)], t);
    assert!(tick(&mut state, t + Duration::from_secs(2)).is_empty());
    assert_eq!(clears(state.handle_input_bytes(b"x")), ["pane_1"]);
}
