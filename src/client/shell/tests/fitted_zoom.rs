//! A fitted zoom as the client sees it: a surface whose one pane sits between
//! blank margins. The server owns the geometry, so the client must take the
//! pane's rects from the surface and nothing in a margin may reach the pane.

use super::*;

/// prefix+f with the default prefix, ctrl+b.
const PREFIX_F: &[u8] = b"\x02f";
const SURFACE_WIDTH: u16 = 100;
const MARGIN: u16 = 20;

/// A surface whose one pane is zoomed between two `MARGIN`-column margins and
/// shows `rows`. Its program asked for mouse reports, so any click that
/// reached it would be passed through.
fn fitted_surface(rows: &[&str]) -> PaneSurfaceFrame {
    let pane_width = SURFACE_WIDTH - 2 * MARGIN;
    let blank = " ".repeat(usize::from(MARGIN));
    let buffer = Buffer::with_lines(rows.iter().map(|row| {
        format!(
            "{blank}{row:<width$}{blank}",
            width = usize::from(pane_width)
        )
    }));
    let mut surface = surface();
    surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    let rect = SurfaceRect {
        x: MARGIN,
        y: 0,
        width: pane_width,
        height: rows.len() as u16,
    };
    surface.panes[0].rect = rect;
    surface.panes[0].inner_rect = rect;
    surface.panes[0].mouse_reporting = true;
    surface.panes[0].scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: rows.len() as u64,
    });
    surface
}

fn client(surface: &PaneSurfaceFrame) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface.clone());
    state
        .compose(SURFACE_WIDTH + 26, 20)
        .expect("composed frame");
    state
}

/// The column the pane surface starts at in a composed client frame, taken
/// from a surface whose pane starts at its origin.
fn surface_origin_x() -> u16 {
    client(&surface()).hits.panes[0].rect.x
}

fn mouse(state: &mut ClientShellState, kind: MouseEventKind, column: u16) -> ClientShellInput {
    let row = state.hits.panes[0].inner_rect.y;
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })])
}

#[test]
fn mouse_events_in_a_fitted_zoom_margin_reach_no_pane() {
    let mut state = client(&fitted_surface(&["agent output", ""]));
    let origin = surface_origin_x();
    let pane_start = origin + MARGIN;
    let right_margin = origin + SURFACE_WIDTH - MARGIN;
    assert_eq!(state.hits.panes[0].inner_rect.x, pane_start);

    for column in [
        origin,
        pane_start - 1,
        right_margin,
        origin + SURFACE_WIDTH - 1,
    ] {
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::ScrollUp,
            MouseEventKind::Moved,
        ] {
            let outcome = mouse(&mut state, kind, column);
            assert!(
                outcome.requests.is_empty() && outcome.actions.is_empty(),
                "{kind:?} at column {column} reached the pane: {:?} {:?}",
                outcome.requests,
                outcome.actions
            );
        }
    }

    let inside = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        pane_start,
    );
    assert!(
        matches!(
            &inside.requests[..],
            [ClientMessage::ClientShellPaneInput { pane_id, .. }] if pane_id == "pane_1"
        ),
        "a click on the pane's first column is passed through"
    );
}

#[test]
fn hints_on_a_fitted_zoom_label_the_cells_inside_its_margins() {
    let mut state = client(&fitted_surface(&["see /tmp/a.txt", ""]));
    state.handle_input_bytes(PREFIX_F);
    assert_eq!(
        state.copy_hint_labels(),
        vec![("a".to_owned(), "/tmp/a.txt".to_owned())]
    );

    let frame = state.compose(SURFACE_WIDTH + 26, 20).expect("hint frame");
    let row: String = frame_rows(&frame)[usize::from(state.hits.panes[0].inner_rect.y)]
        .chars()
        .skip(usize::from(surface_origin_x()))
        .take(usize::from(SURFACE_WIDTH))
        .collect();
    let path = usize::from(MARGIN) + "see ".len();
    assert_eq!(
        &row[path..path + 10],
        "atmp/a.txt",
        "label a covers the path's first cell"
    );
    assert!(
        row[..usize::from(MARGIN)].trim().is_empty(),
        "nothing is drawn in the left margin"
    );

    let chosen = state.handle_input_bytes(b"a");
    assert!(chosen.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::ClipboardWrite(bytes) if bytes == b"/tmp/a.txt"
    )));
}
