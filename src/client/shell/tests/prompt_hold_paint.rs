//! What the prompt-hold mark draws (`super::super::prompt_hold_paint`): the
//! quiet `✎` and the teal waiting count on a pane's top border, how they share
//! that border with the label and the flag chip, and the borderless fallback.
//! Frames are painted the way composition paints them (flags, then holds), or
//! composed through the shell's real entry point, then read back by cell.
use super::landing_glow::tab_snapshot;
use super::pane_flag_paint::{
    bold, cell, composed_state, gruvbox_state, packed, row_text, set_flags, surface, Rgb, ACCENT,
    OVERLAY0, OWN_BG, OWN_BG_WASHED, RESET, WASH, YELLOW,
};
use super::prompt_holds::holds as projection;
use super::*;

/// gruvbox's teal, the waiting mark's color.
const TEAL: Rgb = (142, 192, 124);

/// Paints `flags` and `holds` over `surface` in composition's order and
/// returns the painted frame. The snapshot lists every surface pane, so a
/// flag on any of them counts.
fn paint(
    state: &mut ClientShellState,
    surface: &PaneSurfaceFrame,
    flags: &[(&str, Option<&str>)],
    holds: &[(&str, usize)],
) -> FrameData {
    let focused = surface
        .panes
        .iter()
        .find(|pane| pane.focused)
        .map_or("pane_1", |pane| pane.pane_id.as_str());
    let mut snapshot = tab_snapshot(1, focused);
    for pane in &surface.panes {
        if !snapshot
            .panes
            .iter()
            .any(|known| known.pane_id == pane.pane_id)
        {
            let mut extra = snapshot.panes[0].clone();
            extra.pane_id = pane.pane_id.clone();
            extra.focused = false;
            snapshot.panes.push(extra);
        }
    }
    state.set_snapshot(Box::new(snapshot));
    set_flags(state, flags);
    set_holds(state, holds);
    let mut frame = surface.frame.clone();
    let marks = state.pane_flag_marks();
    state.paint_pane_flags(&mut frame, surface, Rect::default(), &marks);
    state.paint_prompt_holds(&mut frame, surface, Rect::default(), &marks);
    frame
}

/// The presented endpoint's holds, as the server's projection delivers them.
fn set_holds(state: &mut ClientShellState, holds: &[(&str, usize)]) {
    state.set_endpoint_prompt_holds(&ClientEndpointId::Local, 1, projection("boot-1", holds));
}

#[test]
fn holding_with_nothing_waiting_is_a_quiet_pencil_at_the_right_end() {
    let mut state = gruvbox_state();
    let surface = surface(&[30, 30], 0);
    let frame = paint(&mut state, &surface, &[], &[("pane_2", 0)]);

    assert_eq!(
        row_text(&frame, 0, 30, 60),
        format!("┌ claude {} ✎ ┐", "─".repeat(17))
    );
    // The border's own muted gray, not bold, on the border's background.
    let pencil = cell(&frame, 57, 0);
    assert_eq!((pencil.fg, pencil.bg), (packed(OVERLAY0), RESET));
    assert!(!bold(pencil));
    // Nothing else changes: the label, the rest of the border, the content.
    for index in 0..(6 * 60) {
        if !(56..59).contains(&index) {
            assert_eq!(frame.cells[index], surface.frame.cells[index], "{index}");
        }
    }
}

#[test]
fn prompts_waiting_show_a_teal_count_with_words() {
    let mut state = gruvbox_state();
    let surface = surface(&[40, 40], 0);
    let frame = paint(&mut state, &surface, &[], &[("pane_1", 1), ("pane_2", 3)]);

    assert_eq!(
        row_text(&frame, 0, 0, 40),
        format!("┌ claude {} ✎ 1 prompt waiting ┐", "─".repeat(10))
    );
    assert_eq!(
        row_text(&frame, 0, 40, 80),
        format!("┌ claude {} ✎ 3 prompts waiting ┐", "─".repeat(9))
    );
    // The glyph and the count are bold teal; the words are teal, not bold.
    let (glyph, count, word) = (
        cell(&frame, 59, 0),
        cell(&frame, 61, 0),
        cell(&frame, 63, 0),
    );
    for mark in [glyph, count, word] {
        assert_eq!((mark.fg, mark.bg), (packed(TEAL), RESET));
    }
    assert!(bold(glyph) && bold(count) && !bold(word));
    assert_eq!((glyph.symbol.as_str(), count.symbol.as_str()), ("✎", "3"));
    // The line before the mark is still the server's line.
    assert_eq!(cell(&frame, 57, 0), &surface.frame.cells[57]);
}

#[test]
fn a_focused_flagged_holding_pane_keeps_focus_label_chip_and_mark() {
    let mut state = gruvbox_state();
    let surface = surface(&[60, 60], 1);
    let frame = paint(
        &mut state,
        &surface,
        &[("pane_2", Some("review ready"))],
        &[("pane_2", 3)],
    );

    assert_eq!(
        row_text(&frame, 0, 60, 120),
        format!(
            "┌ claude ─ ⚑ review ready {} ✎ 3 prompts waiting ┐",
            "─".repeat(12)
        )
    );
    // The server's bold accent label and accent line are untouched.
    for x in 61..69 {
        assert_eq!(cell(&frame, x, 0), &surface.frame.cells[usize::from(x)]);
    }
    for x in [60, 86, 97, 119] {
        assert_eq!(cell(&frame, x, 0).fg, packed(ACCENT), "x={x}");
    }
    // The chip is the flag's filled yellow; the mark is teal text, unfilled.
    assert_eq!(cell(&frame, 71, 0).bg, packed(YELLOW));
    assert_eq!(cell(&frame, 99, 0).symbol, "✎");
    assert_eq!(
        (cell(&frame, 99, 0).fg, cell(&frame, 99, 0).bg),
        (packed(TEAL), RESET)
    );
    // The wash still fills the pane.
    assert_eq!(cell(&frame, 70, 3).bg, packed(WASH));
}

#[test]
fn a_narrowing_pane_drops_words_then_label_then_message_then_padding() {
    let mut state = gruvbox_state();
    let widths = [52, 40, 30, 18, 12, 9, 6, 4];
    let surface = surface(&widths, 0);
    let ids: Vec<String> = (1..=widths.len()).map(|n| format!("pane_{n}")).collect();
    let flags: Vec<_> = ids
        .iter()
        .map(|id| (id.as_str(), Some("review ready")))
        .collect();
    let holds: Vec<_> = ids.iter().map(|id| (id.as_str(), 2)).collect();
    let frame = paint(&mut state, &surface, &flags, &holds);

    let mut x = 0;
    let rows: Vec<String> = widths
        .iter()
        .map(|width| {
            let row = row_text(&frame, 0, x, x + width);
            x += width;
            row
        })
        .collect();
    assert_eq!(
        rows,
        [
            "┌ claude ─ ⚑ review ready ──── ✎ 2 prompts waiting ┐",
            "┌ claude ─ ⚑ review ready ──────── ✎ 2 ┐",
            "┌ ⚑ review ready ─────── ✎ 2 ┐",
            "┌ ⚑ revie… ─ ✎ 2 ┐",
            "┌ ⚑ ── ✎ 2 ┐",
            "┌⚑─ ✎ 2 ┐",
            "┌⚑─✎2┐",
            "┌✎2┐",
        ]
    );
    // A dropped label's cells are border line again, in the border's color:
    // the 9-wide pane's label ran under its `⚑─`.
    let dropped = cell(&frame, 52 + 40 + 30 + 18 + 12 + 2, 0);
    assert_eq!(
        (dropped.symbol.as_str(), dropped.fg, dropped.bg),
        ("─", packed(OVERLAY0), RESET)
    );
}

#[test]
fn an_unflagged_pane_too_narrow_for_its_label_and_count_drops_the_label() {
    let mut state = gruvbox_state();
    let surface = surface(&[14, 30], 1);
    let frame = paint(&mut state, &surface, &[], &[("pane_1", 2)]);
    assert_eq!(row_text(&frame, 0, 0, 14), "┌─────── ✎ 2 ┐");
    let line = cell(&frame, 3, 0);
    assert_eq!(
        (line.symbol.as_str(), line.fg, line.bg),
        ("─", packed(OVERLAY0), RESET)
    );
}

#[test]
fn the_quiet_mark_never_moves_the_label_or_the_flag() {
    let mut state = gruvbox_state();
    // 18 wide: the flag's chip already fills the border, so no room is left.
    let surface = surface(&[18, 30], 0);
    let flags = [("pane_1", Some("review ready")), ("pane_2", Some("ok"))];
    let without = paint(&mut state, &surface, &flags, &[]);
    let quiet = paint(
        &mut state,
        &surface,
        &flags,
        &[("pane_1", 0), ("pane_2", 0)],
    );

    assert_eq!(row_text(&quiet, 0, 0, 18), row_text(&without, 0, 0, 18));
    assert_eq!(row_text(&quiet, 0, 0, 18), "┌ ⚑ review ready ┐");
    // With room to spare, the pencil takes only cells the line had.
    assert_eq!(
        row_text(&quiet, 0, 18, 48),
        "┌ claude ─ ⚑ ok ────────── ✎ ┐"
    );
    for x in 18..44 {
        assert_eq!(cell(&quiet, x, 0), cell(&without, x, 0), "x={x}");
    }
}

#[test]
fn a_borderless_pane_carries_chip_and_mark_at_the_end_of_its_first_row() {
    let mut state = gruvbox_state();
    let mut surface = surface(&[40], 0);
    surface.panes[0].inner_rect = surface.panes[0].rect;
    // A cell the agent painted itself, under where the mark will go.
    surface.frame.cells[38].bg = packed(OWN_BG);
    let frame = paint(
        &mut state,
        &surface,
        &[("pane_1", Some("review"))],
        &[("pane_1", 2)],
    );
    assert_eq!(
        row_text(&frame, 0, 9, 40),
        " ⚑ review  ✎ 2 prompts waiting "
    );
    assert_eq!(cell(&frame, 9, 0).bg, packed(YELLOW));
    // The mark keeps the agent's backgrounds: washed, and its own colored
    // cell at the row's end washed by the same share.
    assert_eq!(
        (cell(&frame, 20, 0).fg, cell(&frame, 20, 0).bg),
        (packed(TEAL), packed(WASH))
    );
    assert_eq!(cell(&frame, 38, 0).bg, packed(OWN_BG_WASHED));

    // Without a flag the mark alone takes the row's end.
    let frame = paint(&mut state, &surface, &[], &[("pane_1", 0)]);
    assert_eq!(row_text(&frame, 0, 37, 40), " ✎ ");
    assert_eq!(cell(&frame, 38, 0).fg, packed(OVERLAY0));
}

#[test]
fn nothing_is_painted_without_holds_or_for_panes_not_shown() {
    let mut state = gruvbox_state();
    let surface = surface(&[30, 30], 0);
    assert_eq!(paint(&mut state, &surface, &[], &[]), surface.frame);
    assert_eq!(
        paint(&mut state, &surface, &[], &[("pane_9", 4)]),
        surface.frame
    );
    // A flag without a hold is exactly the flag's own look.
    let flags = [("pane_2", Some("review ready"))];
    let flag_only = paint(&mut state, &surface, &flags, &[]);
    assert!(row_text(&flag_only, 0, 30, 60).starts_with("┌ claude ─ ⚑ review ready ─"));
    assert!(!row_text(&flag_only, 0, 0, 60).contains('✎'));
}

#[test]
fn the_default_theme_takes_its_own_teal_and_gray() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let palette = state.config.palette.clone();
    let surface = surface(&[40, 40], 0);
    let frame = paint(&mut state, &surface, &[], &[("pane_1", 0), ("pane_2", 2)]);
    assert_eq!(palette.teal, ratatui::style::Color::Rgb(148, 226, 213));
    let quiet = cell(&frame, 37, 0);
    assert_eq!(quiet.symbol, "✎");
    assert_eq!(quiet.fg, crate::protocol::color_to_u32(palette.overlay0));
    let waiting = row_text(&frame, 0, 40, 80);
    let x = 40 + waiting.chars().position(|c| c == '✎').expect("mark") as u16;
    assert_eq!(
        cell(&frame, x, 0).fg,
        crate::protocol::color_to_u32(palette.teal)
    );
}

#[test]
fn the_composed_frame_draws_the_mark_and_drops_it_with_the_hold() {
    let mut state = composed_state();
    let plain = state.compose(100, 16).expect("frame");
    set_holds(&mut state, &[("pane_2", 2)]);
    let held = state.compose(100, 16).expect("frame");
    let pencils: Vec<(u16, u16)> = (0..16)
        .flat_map(|y| (0..100).map(move |x| (x, y)))
        .filter(|(x, y)| cell(&held, *x, *y).symbol == "✎")
        .collect();
    assert_eq!(pencils.len(), 1, "{pencils:?}");
    let (x, y) = pencils[0];
    assert_eq!(cell(&held, x + 2, y).symbol, "2");
    assert_eq!(cell(&held, x, y).fg, packed(TEAL));
    // Pressing Enter releases the prompts: the server drops the hold, and the
    // next frame is exactly the frame without it.
    set_holds(&mut state, &[]);
    assert_eq!(state.compose(100, 16).expect("frame").frame, plain.frame);
}
