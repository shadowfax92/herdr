//! The hold store is read through the presented snapshot, and the placeholder
//! is checked against bordered surfaces using the real client paint seam.
use super::landing_glow::{tab_snapshot, tab_surface};
use super::*;
use crate::protocol::endpoint::{EndpointPromptHold, EndpointPromptHolds};

fn holds(boot: &str, values: &[(&str, usize)]) -> EndpointPromptHolds {
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

#[test]
fn prompt_holds_placeholder_marks_only_holding_pane_and_empty_paints_nothing() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(tab_snapshot(1, "pane_1")));
    let surface = tab_surface("boot-1", 1, &["pane_1", "pane_2", "pane_3"], "pane_1");
    let original = surface.frame.clone();
    let mut frame = original.clone();
    state.paint_prompt_holds(&mut frame, &surface, Rect::default());
    assert_eq!(frame, original);
    state.set_endpoint_prompt_holds(
        &ClientEndpointId::Local,
        1,
        holds("boot-1", &[("pane_2", 2)]),
    );
    state.paint_prompt_holds(&mut frame, &surface, Rect::default());
    let row: String = frame.cells[..frame.width as usize]
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect();
    assert_eq!(&row.chars().skip(20).take(3).collect::<String>(), "⏸ 2");
    assert_eq!(&frame.cells[..12], &original.cells[..12]);
    assert_eq!(&frame.cells[24..], &original.cells[24..]);
    assert_eq!(
        frame.cells[20].fg,
        crate::protocol::color_to_u32(state.config.palette.yellow)
    );
    state.set_endpoint_prompt_holds(
        &ClientEndpointId::Local,
        1,
        holds("boot-1", &[("pane_2", 0)]),
    );
    let mut frame = original.clone();
    state.paint_prompt_holds(&mut frame, &surface, Rect::default());
    assert_eq!(frame.cells[22].symbol, "⏸");
    state.set_endpoint_prompt_holds(&ClientEndpointId::Local, 1, holds("boot-1", &[]));
    let mut frame = original.clone();
    state.paint_prompt_holds(&mut frame, &surface, Rect::default());
    assert_eq!(frame, original);
}
