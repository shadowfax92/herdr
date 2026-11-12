//! The hold store is read through the presented snapshot. What a hold draws
//! is pinned in `super::prompt_hold_paint`.
use super::landing_glow::tab_snapshot;
use super::*;
use crate::protocol::endpoint::{EndpointPromptHold, EndpointPromptHolds};

pub(super) fn holds(boot: &str, values: &[(&str, usize)]) -> EndpointPromptHolds {
    EndpointPromptHolds {
        boot_id: boot.into(),
        holds: values
            .iter()
            .map(|(pane, held)| ((*pane).into(), EndpointPromptHold { held: *held }))
            .collect(),
    }
}

#[test]
fn prompt_holds_match_presented_endpoint_boot_and_connection() {
    use crate::client::endpoint::ProfileId;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
    state.cache_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        1,
        Box::new(tab_snapshot(1, "pane_1")),
    );
    assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
    state.set_endpoint_prompt_holds(
        &ClientEndpointId::Local,
        1,
        holds("boot-1", &[("pane_2", 2)]),
    );
    assert_eq!(state.prompt_holds().unwrap()["pane_2"].held, 2);
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
    state.set_endpoint_prompt_holds(&remote, 2, holds("boot-1", &[("pane_3", 0)]));
    assert_eq!(state.prompt_holds().unwrap()["pane_2"].held, 2);
    assert!(state.activate_endpoint_projection(&remote));
    assert!(state.prompt_holds().unwrap().contains_key("pane_3"));
    assert!(!state.prompt_holds().unwrap().contains_key("pane_2"));
    state.set_endpoint_prompt_holds(&remote, 1, holds("boot-1", &[]));
    assert!(state.prompt_holds().unwrap().contains_key("pane_3"));
    state.cache_endpoint_snapshot_for_generation(&remote, 3, Box::new(tab_snapshot(2, "pane_1")));
    assert!(state.activate_endpoint_projection(&remote));
    assert!(
        state.prompt_holds().is_none(),
        "a new connection cannot inherit old holds"
    );
    state.set_endpoint_prompt_holds(&remote, 3, holds("different-boot", &[("pane_2", 1)]));
    assert!(
        state.prompt_holds().is_none(),
        "ids from another boot must never paint"
    );
    state.set_endpoint_prompt_holds(&remote, 3, holds("boot-1", &[]));
    assert!(state.prompt_holds().unwrap().is_empty());
}
