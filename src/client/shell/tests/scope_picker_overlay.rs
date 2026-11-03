//! The focus/hide picker and every way to open it: rows across machines,
//! staged edits, the one `scope.set` it sends, and the jump on focus. The
//! scope comes from `set_scope_projection`, as the Local server would send it,
//! and every request is read from `scope_set_requests`.
use super::super::scope_picker::{
    scope_picker_marks, ClientScopePickerOverlay, ScopePickerControl,
};
use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
};
use crate::input::{KeybindAction, KeybindMatch, TerminalKey};
use crate::scope::{ScopeFocus, ScopePickMode, ScopeSetParams, ScopeState, ScopeTarget};

const STUDIO_KEY: &str = "ssh:0123456789abcdef0123456789abcdef";

fn workspace(id: &str, label: &str, number: usize) -> ClientShellWorkspace {
    let mut workspace = snapshot().workspaces[0].clone();
    workspace.workspace_id = id.into();
    workspace.label = label.into();
    workspace.number = number;
    workspace.focused = false;
    workspace.branch = None;
    workspace
}

fn tab(id: &str, workspace_id: &str, label: &str, number: usize) -> ClientShellTab {
    ClientShellTab {
        tab_id: id.into(),
        workspace_id: workspace_id.into(),
        number,
        label: label.into(),
        custom_label: true,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Idle,
    }
}

/// Local: `setup` (tabs herdr, nvim; displayed), `ft`, `admin`.
fn local_snapshot() -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    let mut setup = workspace("ws_1", "setup", 1);
    setup.focused = true;
    snapshot.workspaces = vec![
        setup,
        workspace("ws_2", "ft", 2),
        workspace("ws_3", "admin", 3),
    ];
    snapshot.tabs = vec![
        tab("tab_1", "ws_1", "herdr", 1),
        tab("tab_2", "ws_1", "nvim", 2),
        tab("tab_3", "ws_2", "workers", 1),
        tab("tab_4", "ws_3", "fish", 1),
    ];
    snapshot
}

/// studio: `main` (tabs fish, outreach). Its ids overlap Local's on purpose.
fn studio_snapshot() -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.boot_id = "studio-boot".into();
    snapshot.workspaces = vec![workspace("ws_1", "main", 1)];
    snapshot.tabs = vec![
        tab("tab_1", "ws_1", "fish", 1),
        tab("tab_2", "ws_1", "outreach", 2),
    ];
    snapshot.focused_workspace_id = Some("ws_1".into());
    snapshot
}

fn state_with(hidden_workspaces: &[&str]) -> (ClientShellState, ClientEndpointId) {
    let mut config = Config::default();
    config.ui.sidebar.agents.hidden_workspaces = hidden_workspaces
        .iter()
        .map(|label| (*label).to_owned())
        .collect();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let profile = SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "studio".into(),
        target: "dev@studio.example".into(),
        session: "agents".into(),
        enabled: true,
    };
    let studio = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&studio, ClientEndpointStatus::Online);
    state.set_snapshot(Box::new(local_snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_snapshot(&studio, Box::new(studio_snapshot()));
    (state, studio)
}

fn local(workspace_id: &str, label: &str) -> ScopeTarget {
    ScopeTarget {
        machine: "local".into(),
        machine_label: Some("Local".into()),
        workspace_id: Some(workspace_id.into()),
        workspace_label: Some(label.into()),
        ..ScopeTarget::default()
    }
}

fn studio(workspace_id: &str, label: &str) -> ScopeTarget {
    ScopeTarget {
        machine: STUDIO_KEY.into(),
        machine_label: Some("studio".into()),
        workspace_id: Some(workspace_id.into()),
        workspace_label: Some(label.into()),
        ..ScopeTarget::default()
    }
}

fn with_tab(target: ScopeTarget, tab_id: &str, label: &str) -> ScopeTarget {
    ScopeTarget {
        tab_id: Some(tab_id.into()),
        tab_label: Some(label.into()),
        ..target
    }
}

fn config_hide(label: &str) -> ScopeTarget {
    ScopeState::from_config_hidden(&[label.to_owned()])
        .hidden
        .remove(0)
}

fn press(state: &mut ClientShellState, code: KeyCode) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(TerminalKey::new(
        code,
        KeyModifiers::empty(),
    ))])
}

fn picker(state: &ClientShellState) -> &ClientScopePickerOverlay {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::ScopePicker(picker)) => picker,
        other => panic!("scope picker is not open: {other:?}"),
    }
}

/// Moves the selection to the row labelled `label` at `depth` on `machine`.
fn select(state: &mut ClientShellState, machine: &str, depth: u8, label: &str) {
    let mut current_machine = String::new();
    let mut key = None;
    for row in state.scope_picker_rows_now() {
        if row.depth == 0 {
            current_machine = row.label.clone();
        }
        if current_machine == machine && row.depth == depth && row.label == label {
            key = Some(row.key);
            break;
        }
    }
    let key = key.unwrap_or_else(|| panic!("no {machine}/{label} row"));
    let Some(ClientShellOverlay::ScopePicker(picker)) = state.overlay.as_mut() else {
        panic!("scope picker is not open");
    };
    picker.selected = Some(key);
}

fn record(state: &mut ClientShellState, action: KeybindAction) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.record_binding(KeybindMatch::Action(action), &mut outcome);
    outcome
}

fn activations(outcome: &ClientShellInput) -> Vec<(ClientEndpointId, ClientEndpointFocusTarget)> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target: Some(target),
            } => Some((endpoint_id.clone(), target.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn rows_list_every_machine_unfiltered_with_their_marks() {
    let (mut state, _) = state_with(&["ft"]);
    state.set_scope_projection(Some(ScopeState {
        focus: ScopeFocus {
            active: true,
            targets: vec![
                local("ws_1", "setup"),
                with_tab(studio("ws_1", "main"), "tab_2", "outreach"),
            ],
        },
        hidden: vec![config_hide("ft")],
    }));
    record(&mut state, KeybindAction::FocusPicker);

    let rows = state.scope_picker_rows_now();
    let listed = rows
        .iter()
        .map(|row| {
            let marks = scope_picker_marks(&picker(&state).staged, row);
            let mark = if marks.focus {
                "◉"
            } else if marks.hidden {
                "⊘"
            } else {
                "·"
            };
            format!(
                "{}{mark} {}{}{}",
                "  ".repeat(usize::from(row.depth)),
                row.label,
                if marks.config { " (config)" } else { "" },
                if row.shown { "" } else { " [out]" },
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            "· Local",
            "  ◉ setup",
            "    · herdr",
            "    · nvim",
            // A hidden workspace is still listed so it can be un-hidden.
            "  ⊘ ft (config) [out]",
            "    · workers [out]",
            "  · admin [out]",
            "    · fish [out]",
            "· studio",
            "  · main",
            "    · fish [out]",
            "    ◉ outreach",
        ]
    );
    let outreach = rows.last().unwrap();
    assert_eq!(
        outreach.target,
        with_tab(studio("ws_1", "main"), "tab_2", "outreach")
    );
    assert_eq!(picker(&state).mode, ScopePickMode::Focus);
    // The picker opens on the displayed workspace.
    assert_eq!(
        picker(&state)
            .selected
            .as_ref()
            .map(|key| key.workspace_id.as_deref()),
        Some(Some("ws_1"))
    );
}

#[test]
fn staged_toggles_apply_as_one_set_with_only_the_changed_fields() {
    let (mut state, _) = state_with(&[]);
    record(&mut state, KeybindAction::HidePicker);
    assert_eq!(picker(&state).mode, ScopePickMode::Hide);

    // space marks with the mode (hide); x and f reach either mark.
    select(&mut state, "Local", 1, "admin");
    press(&mut state, KeyCode::Char(' '));
    select(&mut state, "studio", 2, "fish");
    press(&mut state, KeyCode::Char('x'));
    press(&mut state, KeyCode::Char('x'));
    press(&mut state, KeyCode::Char('x'));
    select(&mut state, "Local", 1, "setup");
    press(&mut state, KeyCode::Char('f'));
    assert!(
        state.scope_set_requests.is_empty(),
        "edits stay local until enter"
    );

    let outcome = press(&mut state, KeyCode::Enter);
    assert!(state.overlay.is_none());
    assert_eq!(
        state.scope_set_requests,
        [ScopeSetParams {
            focus_targets: Some(vec![local("ws_1", "setup")]),
            // Picking a focus target switches focus on.
            focus_active: Some(true),
            hidden: Some(vec![
                local("ws_3", "admin"),
                with_tab(studio("ws_1", "main"), "tab_1", "fish"),
            ]),
        }]
    );
    // The displayed workspace is focused, so nothing moves.
    assert!(activations(&outcome).is_empty(), "{:?}", outcome.actions);
}

#[test]
fn toggling_a_marked_row_removes_the_target_that_names_it() {
    let (mut state, _) = state_with(&["ft"]);
    record(&mut state, KeybindAction::HidePicker);
    // The config hide is a label target for every machine; un-hiding ft
    // removes it.
    select(&mut state, "Local", 1, "ft");
    press(&mut state, KeyCode::Char(' '));
    press(&mut state, KeyCode::Enter);
    assert_eq!(
        state.scope_set_requests,
        [ScopeSetParams {
            hidden: Some(Vec::new()),
            ..ScopeSetParams::default()
        }]
    );
}

#[test]
fn the_picker_keeps_the_keyboard_while_a_closing_popup_is_still_known() {
    // The menu popup asks for the picker and closes; the pick can arrive
    // before the snapshot that clears the popup.
    let (mut state, _) = state_with(&[]);
    state.popup_terminal_id = Some("closing-menu-popup".into());
    record(&mut state, KeybindAction::FocusPicker);
    let before = picker(&state).selected.clone();
    let outcome = press(&mut state, KeyCode::Char('j'));
    assert_ne!(picker(&state).selected, before, "j must move the picker");
    assert!(
        outcome.requests.is_empty(),
        "no key may reach the popup or the pane: {:?}",
        outcome.requests
    );
}

#[test]
fn the_picker_survives_the_menu_popup_closing_after_it_opened() {
    // The pick reaches the client first; the surface without the popup follows.
    let (mut state, _) = state_with(&[]);
    state.set_pane_surface(super::surface_with_popup());
    state.open_scope_picker(crate::scope::ScopePickMode::Hide);
    state.set_pane_surface(surface());
    assert_eq!(picker(&state).mode, crate::scope::ScopePickMode::Hide);
    let before = picker(&state).selected.clone();
    press(&mut state, KeyCode::Char('j'));
    assert_ne!(
        picker(&state).selected,
        before,
        "j must still move the picker"
    );

    // A popup opening over the picker still closes it, as for other overlays.
    state.set_pane_surface(super::surface_with_popup());
    assert!(state.overlay.is_none());
}

#[test]
fn escape_and_an_unchanged_enter_send_nothing() {
    let (mut state, _) = state_with(&[]);
    record(&mut state, KeybindAction::FocusPicker);
    select(&mut state, "Local", 1, "admin");
    press(&mut state, KeyCode::Char(' '));
    press(&mut state, KeyCode::Esc);
    assert!(state.overlay.is_none());

    record(&mut state, KeybindAction::FocusPicker);
    press(&mut state, KeyCode::Char('x'));
    press(&mut state, KeyCode::Char('x'));
    press(&mut state, KeyCode::Enter);
    assert!(state.overlay.is_none());
    assert!(state.scope_set_requests.is_empty());
}

#[test]
fn a_flips_focus_and_c_clears_back_to_the_config_hides() {
    let (mut state, _) = state_with(&["ft"]);
    let scope = ScopeState {
        focus: ScopeFocus {
            active: true,
            targets: vec![local("ws_1", "setup")],
        },
        hidden: vec![local("ws_3", "admin")],
    };
    state.set_scope_projection(Some(scope.clone()));

    record(&mut state, KeybindAction::FocusPicker);
    press(&mut state, KeyCode::Char('a'));
    assert!(!picker(&state).staged.focus.active);
    press(&mut state, KeyCode::Enter);
    assert_eq!(
        state.scope_set_requests,
        [ScopeSetParams {
            focus_active: Some(false),
            ..ScopeSetParams::default()
        }]
    );

    state.scope_set_requests.clear();
    record(&mut state, KeybindAction::HidePicker);
    press(&mut state, KeyCode::Char('c'));
    assert_eq!(
        picker(&state).staged,
        ScopeState::from_config_hidden(&["ft".into()])
    );
    press(&mut state, KeyCode::Enter);
    assert_eq!(
        state.scope_set_requests,
        [ScopeSetParams {
            focus_targets: Some(Vec::new()),
            focus_active: Some(false),
            hidden: Some(vec![config_hide("ft")]),
        }]
    );
}

#[test]
fn apply_keeps_targets_the_picker_cannot_see() {
    let (mut state, _) = state_with(&[]);
    let offline = ScopeTarget {
        machine: "ssh:ffffffffffffffffffffffffffffffff".into(),
        machine_label: Some("laptop".into()),
        workspace_label: Some("notes".into()),
        ..ScopeTarget::default()
    };
    state.set_scope_projection(Some(ScopeState {
        focus: ScopeFocus {
            active: true,
            targets: vec![offline.clone()],
        },
        hidden: vec![offline.clone()],
    }));
    record(&mut state, KeybindAction::FocusPicker);
    let frame = state.compose(100, 32).expect("picker frame");
    assert!(frame_rows(&frame)
        .join("\n")
        .contains("2 unlisted targets are kept on apply"));
    select(&mut state, "Local", 1, "setup");
    press(&mut state, KeyCode::Char('f'));
    select(&mut state, "Local", 1, "admin");
    press(&mut state, KeyCode::Char('x'));
    press(&mut state, KeyCode::Enter);
    assert_eq!(
        state.scope_set_requests,
        [ScopeSetParams {
            focus_targets: Some(vec![offline.clone(), local("ws_1", "setup")]),
            focus_active: None,
            hidden: Some(vec![offline, local("ws_3", "admin")]),
        }]
    );
}

#[test]
fn applying_focus_jumps_to_the_first_admitted_workspace_in_panel_order() {
    let (mut state, studio_id) = state_with(&[]);
    record(&mut state, KeybindAction::FocusPicker);
    // Focus admin on Local and main on studio; the displayed setup leaves.
    select(&mut state, "studio", 1, "main");
    press(&mut state, KeyCode::Char(' '));
    select(&mut state, "Local", 1, "admin");
    press(&mut state, KeyCode::Char(' '));
    let outcome = press(&mut state, KeyCode::Enter);
    assert_eq!(state.scope_set_requests.len(), 1);
    // Local comes first in the panel, so admin wins over studio's main.
    assert_eq!(
        activations(&outcome),
        [(
            ClientEndpointId::Local,
            ClientEndpointFocusTarget::Workspace("ws_3".into())
        )]
    );

    // With only a remote workspace admitted, the jump crosses machines.
    state.scope_set_requests.clear();
    state.set_scope_projection(None);
    record(&mut state, KeybindAction::FocusPicker);
    select(&mut state, "studio", 2, "outreach");
    press(&mut state, KeyCode::Char('f'));
    let outcome = press(&mut state, KeyCode::Enter);
    assert_eq!(
        activations(&outcome),
        [(
            studio_id,
            ClientEndpointFocusTarget::Workspace("ws_1".into())
        )]
    );
}

#[test]
fn applying_focus_skips_machines_that_are_not_online() {
    let (mut state, studio_id) = state_with(&[]);
    state.set_endpoint_status(&studio_id, ClientEndpointStatus::Reconnecting);
    record(&mut state, KeybindAction::FocusPicker);
    select(&mut state, "studio", 1, "main");
    press(&mut state, KeyCode::Char('f'));
    let outcome = press(&mut state, KeyCode::Enter);
    assert_eq!(state.scope_set_requests.len(), 1);
    assert!(activations(&outcome).is_empty(), "{:?}", outcome.actions);
}

#[test]
fn toggle_focus_flips_focus_active_and_jumps_when_it_turns_on() {
    let (mut state, studio_id) = state_with(&[]);
    state.set_scope_projection(Some(ScopeState {
        focus: ScopeFocus {
            active: false,
            targets: vec![studio("ws_1", "main")],
        },
        hidden: Vec::new(),
    }));
    let outcome = record(&mut state, KeybindAction::ToggleFocus);
    assert_eq!(
        state.scope_set_requests,
        [ScopeSetParams {
            focus_active: Some(true),
            ..ScopeSetParams::default()
        }]
    );
    assert_eq!(
        activations(&outcome),
        [(
            studio_id,
            ClientEndpointFocusTarget::Workspace("ws_1".into())
        )]
    );

    state.scope_set_requests.clear();
    state.set_scope_projection(Some(ScopeState {
        focus: ScopeFocus {
            active: true,
            targets: vec![studio("ws_1", "main")],
        },
        hidden: Vec::new(),
    }));
    let outcome = record(&mut state, KeybindAction::ToggleFocus);
    assert_eq!(
        state.scope_set_requests,
        [ScopeSetParams {
            focus_active: Some(false),
            ..ScopeSetParams::default()
        }]
    );
    assert!(activations(&outcome).is_empty());
}

#[test]
fn clicking_rows_marks_with_the_mode_and_controls_apply() {
    let (mut state, _) = state_with(&[]);
    record(&mut state, KeybindAction::HidePicker);
    let frame = state.compose(100, 32).expect("picker frame");
    let text = frame_rows(&frame).join("\n");
    assert!(text.contains(" Hide "), "{text}");
    assert!(text.contains("focus off"), "{text}");
    assert!(text.contains("· admin"), "{text}");
    assert!(text.contains("↵ apply"), "{text}");

    let admin_key = state
        .scope_picker_rows_now()
        .into_iter()
        .find(|row| row.label == "admin")
        .unwrap()
        .key;
    let admin = state
        .hits
        .scope_picker_rows
        .iter()
        .find(|(_, key)| *key == admin_key)
        .map(|(rect, _)| *rect)
        .expect("admin row hit");
    let click = |rect: Rect| {
        RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 2,
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        })
    };
    state.handle_raw_events(vec![click(admin)]);
    let frame = state.compose(100, 32).expect("picker frame");
    assert!(frame_rows(&frame).join("\n").contains("⊘ admin"));

    let apply = state
        .hits
        .scope_picker_controls
        .iter()
        .find(|(_, control)| *control == ScopePickerControl::Apply)
        .map(|(rect, _)| *rect)
        .expect("apply button");
    state.handle_raw_events(vec![click(apply)]);
    assert!(state.overlay.is_none());
    assert_eq!(
        state.scope_set_requests,
        [ScopeSetParams {
            hidden: Some(vec![local("ws_3", "admin")]),
            ..ScopeSetParams::default()
        }]
    );
}

#[test]
fn a_plugin_pick_while_open_switches_mode_and_keeps_edits() {
    let (mut state, _) = state_with(&[]);
    state.open_scope_picker(ScopePickMode::Focus);
    select(&mut state, "Local", 1, "admin");
    press(&mut state, KeyCode::Char(' '));
    state.open_scope_picker(ScopePickMode::Hide);
    assert_eq!(picker(&state).mode, ScopePickMode::Hide);
    assert_eq!(
        picker(&state).staged.focus.targets,
        [local("ws_3", "admin")]
    );
}

#[test]
fn the_sidebar_menu_opens_the_pickers_and_names_the_focus_flip() {
    let (mut state, _) = state_with(&[]);
    let labels = |focus_active| {
        super::super::global_menu::global_menu_items(&local_snapshot(), focus_active)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        labels(false),
        [
            "settings",
            "keybinds",
            "reload config",
            "focus…",
            "hide…",
            "focus on",
            "detach"
        ]
    );
    assert_eq!(labels(true)[5], "focus off");

    state.toggle_global_menu();
    state.activate_global_menu_item(4, &mut ClientShellInput::default());
    assert_eq!(picker(&state).mode, ScopePickMode::Hide);

    state.overlay = None;
    state.toggle_global_menu();
    let frame = state.compose(100, 32).expect("menu frame");
    assert!(frame_rows(&frame).join("\n").contains("focus on"));
    state.activate_global_menu_item(5, &mut ClientShellInput::default());
    assert!(state.overlay.is_none());
    assert_eq!(
        state.scope_set_requests,
        [ScopeSetParams {
            focus_active: Some(true),
            ..ScopeSetParams::default()
        }]
    );

    state.set_scope_projection(Some(ScopeState {
        focus: ScopeFocus {
            active: true,
            targets: Vec::new(),
        },
        hidden: Vec::new(),
    }));
    state.toggle_global_menu();
    let frame = state.compose(100, 32).expect("menu frame");
    assert!(frame_rows(&frame).join("\n").contains("focus off"));
}

#[test]
fn workspace_menu_focus_and_hide_add_the_workspace() {
    let (mut state, studio_id) = state_with(&["ft"]);
    state.set_scope_projection(Some(ScopeState {
        focus: ScopeFocus {
            active: false,
            targets: vec![studio("ws_1", "main")],
        },
        hidden: vec![config_hide("ft")],
    }));
    let activate = |state: &mut ClientShellState, workspace_id: &str, action| {
        state.open_workspace_context_menu(workspace_id.into(), 1, 1);
        let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
            panic!("workspace menu");
        };
        let items = menu.items();
        let index = items
            .iter()
            .position(|item| item.action == action)
            .expect("scope item");
        assert_eq!(
            items[items.len() - 2..]
                .iter()
                .map(|item| item.label)
                .collect::<Vec<_>>(),
            ["Focus", "Hide"]
        );
        let mut outcome = ClientShellInput::default();
        state.activate_context_menu_item(index, &mut outcome);
        outcome
    };

    activate(&mut state, "ws_3", ClientContextMenuAction::HideScope);
    activate(&mut state, "ws_3", ClientContextMenuAction::FocusScope);
    assert_eq!(
        state.scope_set_requests,
        [
            ScopeSetParams {
                hidden: Some(vec![config_hide("ft"), local("ws_3", "admin")]),
                ..ScopeSetParams::default()
            },
            ScopeSetParams {
                focus_targets: Some(vec![studio("ws_1", "main"), local("ws_3", "admin")]),
                focus_active: Some(true),
                hidden: None,
            },
        ]
    );

    // On a remote machine the target carries that machine's storage key.
    state.scope_set_requests.clear();
    assert!(state.activate_endpoint_projection(&studio_id));
    activate(&mut state, "ws_1", ClientContextMenuAction::FocusScope);
    assert_eq!(
        state.scope_set_requests,
        [ScopeSetParams {
            // main is already focused, so it is not added twice.
            focus_targets: Some(vec![studio("ws_1", "main")]),
            focus_active: Some(true),
            hidden: None,
        }]
    );
}

#[test]
fn every_workspace_menu_variant_ends_with_focus_and_hide() {
    for (is_git, is_linked_worktree, has_worktree_children) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let menu = ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Workspace {
                workspace_id: "ws_1".into(),
                is_git,
                is_linked_worktree,
                has_worktree_children,
                close_group: false,
                collapsed: false,
            },
            x: 0,
            y: 0,
            highlighted: 0,
        };
        let actions = menu
            .items()
            .iter()
            .map(|item| item.action)
            .collect::<Vec<_>>();
        assert_eq!(
            actions[actions.len() - 2..],
            [
                ClientContextMenuAction::FocusScope,
                ClientContextMenuAction::HideScope
            ]
        );
    }
    let tab_menu = ClientContextMenuOverlay {
        target: ClientContextMenuTarget::Tab {
            tab_id: "tab_1".into(),
            workspace_id: "ws_1".into(),
        },
        x: 0,
        y: 0,
        highlighted: 0,
    };
    assert!(!tab_menu
        .items()
        .iter()
        .any(|item| item.action == ClientContextMenuAction::FocusScope));
}
