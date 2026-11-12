//! Zoom ownership under pane-tree edits, without terminal runtimes or PTYs.

use super::*;
use crate::workspace::Workspace;

fn zoomed_workspace(width: ZoomWidth) -> (Workspace, PaneId, PaneId) {
    let mut ws = Workspace::test_new("zoom-keep");
    let owner = ws.tabs[0].root_pane;
    let other = ws.test_split(Direction::Horizontal);
    ws.test_split(Direction::Vertical);
    ws.layout.focus_pane(owner);
    ws.zoomed = true;
    ws.zoom_width = width;
    (ws, owner, other)
}

fn assert_zoom(ws: &Workspace, owner: PaneId, width: ZoomWidth) {
    assert!(ws.zoomed, "{width:?}");
    assert_eq!(ws.zoom_width, width);
    assert_eq!(ws.layout.focused(), owner);
    ws.assert_invariants_for_test();
}

#[test]
fn closing_another_pane_keeps_the_owners_zoom() {
    for width in [ZoomWidth::Fitted, ZoomWidth::Full] {
        let (mut ws, owner, other) = zoomed_workspace(width);
        ws.remove_pane(other);
        assert!(!ws.tabs[0].panes.contains_key(&other));
        assert_zoom(&ws, owner, width);
    }
}

/// Exercise the split's state commit after substituting for a successful spawn.
/// The server test covers the real runtime path; these cases need no PTYs.
fn split(ws: &mut Workspace, target: PaneId, focus: bool) -> PaneId {
    let tab = &mut ws.tabs[0];
    let new_id = tab
        .layout
        .split_pane(target, Direction::Horizontal, 0.5)
        .unwrap();
    tab.complete_pane_split(new_id, PaneState::new(TerminalId::alloc()), focus);
    ws.register_new_pane(new_id);
    new_id
}

#[test]
fn splitting_any_pane_without_focus_keeps_the_owners_zoom() {
    for width in [ZoomWidth::Fitted, ZoomWidth::Full] {
        for split_owner in [true, false] {
            let (mut ws, owner, other) = zoomed_workspace(width);
            split(&mut ws, if split_owner { owner } else { other }, false);
            assert_zoom(&ws, owner, width);
        }
    }
}

#[test]
fn moving_a_pane_in_without_focus_keeps_the_owners_zoom() {
    for width in [ZoomWidth::Fitted, ZoomWidth::Full] {
        let (mut ws, owner, _) = zoomed_workspace(width);
        let mut source = Workspace::test_new("source");
        let moved = source.take_pane_for_move(source.root_pane).unwrap().moved;
        assert!(ws
            .insert_moved_pane_into_tab(0, owner, moved, Direction::Horizontal, 0.5, false)
            .is_ok());
        assert_zoom(&ws, owner, width);
    }
}

#[test]
fn moving_another_pane_out_keeps_the_owners_zoom() {
    for width in [ZoomWidth::Fitted, ZoomWidth::Full] {
        let (mut ws, owner, other) = zoomed_workspace(width);
        ws.take_pane_for_move(other).unwrap();
        ws.unregister_moved_pane(other);
        assert_zoom(&ws, owner, width);
    }
}

#[test]
fn swapping_and_resizing_other_panes_keeps_the_owners_zoom() {
    for width in [ZoomWidth::Fitted, ZoomWidth::Full] {
        let (mut ws, owner, other) = zoomed_workspace(width);
        let third = ws
            .layout
            .pane_ids()
            .into_iter()
            .find(|id| *id != owner && *id != other)
            .unwrap();
        assert!(ws.layout.swap_panes(other, third));
        assert!(ws.layout.resize_pane(
            other,
            crate::layout::NavDirection::Right,
            0.05,
            ratatui::layout::Rect::new(0, 0, 200, 50)
        ));
        assert_zoom(&ws, owner, width);
    }
}

#[test]
fn closing_or_moving_the_owner_ends_zoom() {
    for width in [ZoomWidth::Fitted, ZoomWidth::Full] {
        for moving in [false, true] {
            let (mut ws, owner, _) = zoomed_workspace(width);
            if moving {
                ws.take_pane_for_move(owner).unwrap();
                ws.unregister_moved_pane(owner);
            } else {
                ws.remove_pane(owner);
            }
            assert!(!ws.zoomed);
            assert_ne!(ws.layout.focused(), owner);
            ws.assert_invariants_for_test();
        }
    }
}

#[test]
fn a_split_that_takes_focus_ends_zoom() {
    for width in [ZoomWidth::Fitted, ZoomWidth::Full] {
        let (mut ws, owner, _) = zoomed_workspace(width);
        let new_id = split(&mut ws, owner, true);
        assert!(!ws.zoomed);
        assert_eq!(ws.layout.focused(), new_id);
        ws.assert_invariants_for_test();
    }
}

#[test]
fn a_state_level_move_that_takes_focus_ends_zoom() {
    for width in [ZoomWidth::Fitted, ZoomWidth::Full] {
        let (mut ws, owner, _) = zoomed_workspace(width);
        let mut source = Workspace::test_new("source");
        let moved = source.take_pane_for_move(source.root_pane).unwrap().moved;
        let moved_id = moved.pane_id;
        assert!(ws
            .insert_moved_pane_into_tab(0, owner, moved, Direction::Horizontal, 0.5, true)
            .is_ok());
        assert!(!ws.zoomed);
        assert_eq!(ws.layout.focused(), moved_id);
        ws.assert_invariants_for_test();
    }
}

fn assert_last_sibling_removal_preserves_zoom(moving: bool) {
    for width in [ZoomWidth::Fitted, ZoomWidth::Full] {
        let mut ws = Workspace::test_new("single");
        let owner = ws.root_pane;
        let other = ws.test_split(Direction::Horizontal);
        ws.layout.focus_pane(owner);
        ws.zoomed = true;
        ws.zoom_width = width;
        if moving {
            ws.take_pane_for_move(other).unwrap();
            ws.unregister_moved_pane(other);
        } else {
            ws.remove_pane(other);
        }
        assert_eq!(ws.layout.pane_count(), 1);
        assert_zoom(&ws, owner, width);

        let mut state = crate::app::AppState::test_new();
        state.workspaces = vec![ws];
        state.active = Some(0);
        state.ensure_test_terminals();
        state.assert_invariants_for_test();
        split(&mut state.workspaces[0], owner, false);
        state.ensure_test_terminals();
        assert_zoom(&state.workspaces[0], owner, width);
        state.assert_invariants_for_test();

        let outcome = state
            .apply_pane_zoom(
                0,
                owner,
                crate::app::actions::PaneZoomCommand::Toggle(ZoomWidth::Fitted),
            )
            .unwrap();
        assert!(outcome.changed && !outcome.zoomed);
        assert!(!state.workspaces[0].zoomed);
        state.assert_invariants_for_test();
    }
}

#[test]
fn closing_last_sibling_keeps_zoom_and_the_following_unfocused_split() {
    assert_last_sibling_removal_preserves_zoom(false);
}

#[test]
fn moving_last_sibling_out_keeps_zoom_and_the_following_unfocused_split() {
    assert_last_sibling_removal_preserves_zoom(true);
}
