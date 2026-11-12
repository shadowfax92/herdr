//! What a pane flag draws (`super::super::pane_flag_paint`): the wash and the
//! border chip on the flagged pane, and the ⚑ markers in the tab bar, the
//! agent rows and the space rows. Frames are painted or composed through the
//! shell's real entry points, then cells are read back by symbol and color.
use super::landing_glow::tab_snapshot;
use super::*;
use crate::protocol::endpoint::{EndpointPaneFlag, EndpointPaneFlags};
use crate::protocol::CellData;
use ratatui::style::Color;
use ratatui::widgets::{Block, Widget};

pub(super) type Rgb = (u8, u8, u8);

// gruvbox, the theme the look was picked in.
pub(super) const BG: Rgb = (40, 40, 40);
pub(super) const YELLOW: Rgb = (250, 189, 47);
pub(super) const ACCENT: Rgb = (215, 153, 33);
pub(super) const OVERLAY0: Rgb = (146, 131, 116);
/// 12% of the way from the panel background to yellow: `#413A29`.
pub(super) const WASH: Rgb = (65, 58, 41);
/// The same share over gruvbox's focused-row background `#323130`.
const FOCUSED_ROW_WASH: Rgb = (74, 66, 48);
/// A cell the agent painted itself, and that cell once washed.
pub(super) const OWN_BG: Rgb = (100, 0, 0);
pub(super) const OWN_BG_WASHED: Rgb = (118, 23, 6);

pub(super) const RESET: u32 = 0;

pub(super) fn packed((r, g, b): Rgb) -> u32 {
    crate::protocol::color_to_u32(Color::Rgb(r, g, b))
}

pub(super) fn gruvbox_state() -> ClientShellState {
    let mut config = Config::default();
    config.theme.name = Some("gruvbox".into());
    let state = ClientShellState::new(ClientShellConfig::from_config(&config));
    assert_eq!(state.config.palette.yellow, Color::Rgb(250, 189, 47));
    state
}

/// The presented endpoint's flags, as the server's projection delivers them.
pub(super) fn set_flags(state: &mut ClientShellState, flags: &[(&str, Option<&str>)]) {
    state.set_endpoint_pane_flags(
        &ClientEndpointId::Local,
        1,
        EndpointPaneFlags {
            boot_id: "boot-1".into(),
            flags: flags
                .iter()
                .enumerate()
                .map(|(index, (pane, message))| EndpointPaneFlag {
                    pane_id: (*pane).into(),
                    flag: crate::api::schema::PaneFlag {
                        message: message.map(str::to_owned),
                        seq: index as u64 + 1,
                    },
                })
                .collect(),
        },
    );
}

/// Bordered panes side by side, `widths` wide and six rows tall, titled
/// ` claude ` like the server's agent labels. Each pane's first content row
/// reads `ok` and ends in one cell with the agent's own background.
pub(super) fn surface(widths: &[u16], focused: usize) -> PaneSurfaceFrame {
    let palette = crate::app::state::Palette::gruvbox();
    let total = widths.iter().sum::<u16>();
    let mut buffer = Buffer::empty(Rect::new(0, 0, total, 6));
    let mut x = 0;
    let panes = widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            let rect = Rect::new(x, 0, *width, 6);
            x += width;
            let inner = Rect::new(rect.x + 1, 1, width - 2, 4);
            let color = if index == focused {
                palette.accent
            } else {
                palette.overlay0
            };
            Block::bordered()
                .border_style(Style::default().fg(color))
                .title(" claude ")
                .render(rect, &mut buffer);
            buffer.set_string(inner.x, inner.y, "ok", Style::default());
            buffer.set_style(
                Rect::new(inner.right() - 1, inner.y, 1, 1),
                Style::default().bg(Color::Rgb(OWN_BG.0, OWN_BG.1, OWN_BG.2)),
            );
            PaneSurfacePane {
                pane_id: format!("pane_{}", index + 1),
                content_revision: 0,
                rect: rect.into(),
                inner_rect: inner.into(),
                scrollbar_rect: None,
                scroll: None,
                focused: index == focused,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width: 0,
                pixel_height: 0,
            }
        })
        .collect();
    PaneSurfaceFrame {
        boot_id: "boot-1".into(),
        projection_revision: 1,
        surface_revision: 1,
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]),
        panes,
        splits: Vec::new(),
        popup: None,
        graphics: crate::protocol::SurfaceGraphicsScene::default(),
    }
}

/// Paints `flags` over `surface` and returns the painted frame.
fn paint(
    state: &mut ClientShellState,
    surface: &PaneSurfaceFrame,
    flags: &[(&str, Option<&str>)],
) -> FrameData {
    let focused = surface
        .panes
        .iter()
        .find(|pane| pane.focused)
        .map_or("pane_1", |pane| pane.pane_id.as_str());
    state.set_snapshot(Box::new(tab_snapshot(1, focused)));
    set_flags(state, flags);
    let mut frame = surface.frame.clone();
    let marks = state.pane_flag_marks();
    state.paint_pane_flags(&mut frame, surface, Rect::default(), &marks);
    frame
}

pub(super) fn cell(frame: &FrameData, x: u16, y: u16) -> &CellData {
    &frame.cells[usize::from(y) * usize::from(frame.width) + usize::from(x)]
}

pub(super) fn row_text(frame: &FrameData, y: u16, from: u16, to: u16) -> String {
    (from..to)
        .map(|x| cell(frame, x, y).symbol.as_str())
        .collect()
}

pub(super) fn bold(cell: &CellData) -> bool {
    cell.modifier & Modifier::BOLD.bits() != 0
}

#[test]
fn flagged_pane_takes_the_wash_and_its_border_carries_the_message() {
    let mut state = gruvbox_state();
    let surface = surface(&[30, 30], 0);
    let frame = paint(&mut state, &surface, &[("pane_2", Some("review ready"))]);

    // Every default-background cell inside pane_2 takes the wash; a cell with
    // its own background is blended by the same share; text is untouched.
    for y in 1..5 {
        for x in 31..59 {
            let expected = if (x, y) == (58, 1) {
                OWN_BG_WASHED
            } else {
                WASH
            };
            assert_eq!(cell(&frame, x, y).bg, packed(expected), "({x}, {y})");
        }
    }
    assert_eq!(cell(&frame, 31, 1).symbol, "o");
    assert_eq!(cell(&frame, 31, 1).fg, surface.frame.cells[31 + 60].fg);

    // The label keeps its place, one line cell, then the filled chip.
    assert_eq!(
        row_text(&frame, 0, 30, 60),
        "┌ claude ─ ⚑ review ready ───┐"
    );
    for x in 40..56 {
        let chip = cell(&frame, x, 0);
        assert_eq!((chip.bg, chip.fg), (packed(YELLOW), packed(BG)), "x={x}");
        assert!(bold(chip));
    }
    // Border cells keep their color, the corner and the other sides included.
    assert_eq!(cell(&frame, 39, 0).bg, RESET);
    assert_eq!(cell(&frame, 56, 0).bg, RESET);
    assert_eq!(cell(&frame, 30, 3).bg, RESET);
    assert_eq!(cell(&frame, 30, 3).fg, packed(OVERLAY0));
    assert_eq!(cell(&frame, 45, 5).bg, RESET);
    // The unflagged pane is exactly as the server drew it.
    for index in 0..(6 * 60) {
        if index % 60 < 30 {
            assert_eq!(frame.cells[index], surface.frame.cells[index]);
        }
    }
}

#[test]
fn a_flag_without_a_message_is_a_bare_flag_chip() {
    let mut state = gruvbox_state();
    let surface = surface(&[30, 30], 0);
    let frame = paint(&mut state, &surface, &[("pane_2", None)]);
    assert_eq!(
        row_text(&frame, 0, 30, 60),
        format!("┌ claude ─ ⚑ {}┐", "─".repeat(16))
    );
    assert_eq!(cell(&frame, 42, 0).bg, packed(YELLOW));
    assert_eq!(cell(&frame, 44, 0).bg, RESET);
    assert_eq!(cell(&frame, 35, 2).bg, packed(WASH));
}

#[test]
fn a_flagged_focused_pane_keeps_its_focus_border_and_its_flag() {
    let mut state = gruvbox_state();
    let surface = surface(&[30, 30], 1);
    let frame = paint(&mut state, &surface, &[("pane_2", Some("review ready"))]);
    // The accent border and bold accent label are the server's focus cue.
    for (x, y) in [(30, 0), (30, 3), (59, 3), (45, 5), (57, 0)] {
        assert_eq!(cell(&frame, x, y).fg, packed(ACCENT), "({x}, {y})");
    }
    // The server's label is left exactly as drawn.
    for x in 31..39 {
        assert_eq!(cell(&frame, x, 0), &surface.frame.cells[usize::from(x)]);
    }
    // The flag still shows on top of it until the server drops the flag.
    assert_eq!(cell(&frame, 35, 2).bg, packed(WASH));
    assert_eq!(cell(&frame, 41, 0).symbol, "⚑");
    assert_eq!(cell(&frame, 41, 0).bg, packed(YELLOW));
}

#[test]
fn a_narrow_pane_drops_the_label_then_truncates_the_message_then_keeps_a_bare_flag() {
    let mut state = gruvbox_state();
    // Ten, six and two cells between the corners.
    let surface = surface(&[12, 8, 4], 0);
    let message = Some("review ready");
    let frame = paint(
        &mut state,
        &surface,
        &[
            ("pane_1", message),
            ("pane_2", message),
            ("pane_3", message),
        ],
    );
    assert_eq!(row_text(&frame, 0, 0, 12), "┌ ⚑ revie… ┐");
    assert_eq!(row_text(&frame, 0, 12, 20), "┌ ⚑ r… ┐");
    assert_eq!(row_text(&frame, 0, 20, 24), "┌⚑─┐");
    // The dropped label's cells are border line again, in the border's color.
    assert_eq!(cell(&frame, 22, 0).fg, packed(OVERLAY0));
    assert_eq!(cell(&frame, 22, 0).bg, RESET);
    // The wash still fills each pane.
    for x in [1, 13, 21] {
        assert_eq!(cell(&frame, x, 3).bg, packed(WASH), "x={x}");
    }
}

#[test]
fn a_borderless_pane_carries_the_chip_at_the_end_of_its_first_row() {
    let mut state = gruvbox_state();
    let mut surface = surface(&[30], 0);
    surface.panes[0].inner_rect = surface.panes[0].rect;
    let frame = paint(&mut state, &surface, &[("pane_1", Some("review"))]);
    assert_eq!(row_text(&frame, 0, 20, 30), " ⚑ review ");
    assert_eq!(cell(&frame, 20, 0).bg, packed(YELLOW));
    assert_eq!(cell(&frame, 5, 3).bg, packed(WASH));
}

#[test]
fn nothing_is_painted_without_flags_or_for_another_boot() {
    let mut state = gruvbox_state();
    let surface = surface(&[30, 30], 0);
    assert_eq!(paint(&mut state, &surface, &[]), surface.frame);
    assert!(!state.pane_flag_marks().tab("tab_1"));

    // A projection from an earlier server boot is not the presented one.
    state.set_endpoint_pane_flags(
        &ClientEndpointId::Local,
        1,
        EndpointPaneFlags {
            boot_id: "boot-0".into(),
            flags: vec![EndpointPaneFlag {
                pane_id: "pane_2".into(),
                flag: crate::api::schema::PaneFlag {
                    message: None,
                    seq: 9,
                },
            }],
        },
    );
    let marks = state.pane_flag_marks();
    assert!(!marks.tab("tab_1") && marks.pane("pane_2").is_none());
    let mut frame = surface.frame.clone();
    state.paint_pane_flags(&mut frame, &surface, Rect::default(), &marks);
    assert_eq!(frame, surface.frame);
}

#[test]
fn the_default_theme_takes_its_own_yellow_over_the_host_background() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let palette = state.config.palette.clone();
    let Color::Rgb(r, g, b) = palette.yellow else {
        panic!("catppuccin yellow is RGB");
    };
    state.host_background = Some(crate::terminal_theme::RgbColor {
        r: 30,
        g: 30,
        b: 46,
    });
    let surface = surface(&[30, 30], 0);
    let frame = paint(&mut state, &surface, &[("pane_2", Some("ok"))]);
    assert_eq!((r, g, b), (249, 226, 175));
    assert_eq!(cell(&frame, 35, 2).bg, packed((56, 54, 61)));
    let chip = cell(&frame, 41, 0);
    assert_eq!(chip.bg, crate::protocol::color_to_u32(palette.yellow));
    assert_eq!(chip.fg, crate::protocol::color_to_u32(palette.panel_bg));
}

/// One workspace with two tabs: the three agent panes in tab_1, which is
/// shown, and a plain shell pane in tab_2, which is not.
pub(super) fn composed_state() -> ClientShellState {
    let mut state = gruvbox_state();
    let mut snapshot = tab_snapshot(1, "pane_1");
    snapshot.agents[0].focused = true;
    snapshot.tabs.push(ClientShellTab {
        tab_id: "tab_2".into(),
        workspace_id: "ws_1".into(),
        number: 2,
        label: "review".into(),
        custom_label: true,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Idle,
    });
    snapshot.panes.push(ClientShellPane {
        pane_id: "pane_4".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_2".into(),
        label: None,
        cwd: Some("/repo".into()),
        foreground_cwd: Some("/repo".into()),
        focused: false,
        right_click_passthrough: false,
    });
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(super::landing_glow::tab_surface(
        "boot-1",
        1,
        &["pane_1", "pane_2", "pane_3"],
        "pane_1",
    ));
    state
}

fn find(frame: &FrameData, y: u16, from: u16, to: u16, symbol: &str) -> Vec<u16> {
    (from..to)
        .filter(|x| cell(frame, *x, y).symbol == symbol)
        .collect()
}

#[test]
fn the_tab_bar_marks_the_shown_tab_and_a_tab_you_are_not_looking_at() {
    let mut state = composed_state();
    let plain = state.compose(100, 16).expect("frame");
    assert!(find(&plain, 0, 0, 100, "⚑").is_empty());

    set_flags(
        &mut state,
        &[("pane_2", None), ("pane_4", Some("2 findings"))],
    );
    let frame = state.compose(100, 16).expect("frame");
    let marks = find(&frame, 0, 26, 100, "⚑");
    assert_eq!(marks.len(), 2, "tab bar: {}", row_text(&frame, 0, 26, 100));
    // The active chip: a dark flag like its label, on the accent.
    let active = cell(&frame, marks[0], 0);
    assert_eq!((active.fg, active.bg), (packed(BG), packed(ACCENT)));
    assert_eq!(cell(&frame, marks[0] + 2, 0).symbol, "1");
    // The tab not shown: the theme yellow on the muted chip.
    let other = cell(&frame, marks[1], 0);
    assert_eq!((other.fg, other.bg), (packed(YELLOW), packed((60, 56, 54))));
    assert!(bold(other));
    assert_eq!(row_text(&frame, 0, marks[1], marks[1] + 8), "⚑ review");
}

#[test]
fn a_flagged_agent_row_takes_the_wash_ends_in_a_flag_and_shows_the_message() {
    let mut state = composed_state();
    set_flags(
        &mut state,
        &[("pane_2", Some("review ready")), ("pane_1", None)],
    );
    let frame = state.compose(100, 30).expect("frame");
    let message_row = (0..30)
        .find(|y| row_text(&frame, *y, 0, 25).contains("review ready"))
        .expect("message line in the agents panel");
    let first_line = message_row - 1;
    let line = row_text(&frame, first_line, 0, 25);
    assert!(line.trim_end().ends_with('⚑'), "first line: {line:?}");
    let flag_x = find(&frame, first_line, 0, 25, "⚑")[0];
    assert_eq!(cell(&frame, flag_x, first_line).fg, packed(YELLOW));
    for x in 0..25 {
        assert_eq!(cell(&frame, x, first_line).bg, packed(WASH), "x={x}");
        assert_eq!(cell(&frame, x, message_row).bg, packed(WASH), "x={x}");
    }
    let message_x = find(&frame, message_row, 0, 25, "r")[0];
    assert_eq!(cell(&frame, message_x, message_row).fg, packed(YELLOW));

    // The focused agent's row washes over its own focused background.
    let focused_row = (0..30)
        .find(|y| row_text(&frame, *y, 0, 25).contains("pane_1"))
        .expect("focused agent row");
    assert_eq!(cell(&frame, 2, focused_row).bg, packed(FOCUSED_ROW_WASH));
}

#[test]
fn the_space_row_holding_a_flagged_pane_carries_the_flag() {
    let mut state = composed_state();
    let plain = state.compose(100, 30).expect("frame");
    let workspace_row = (0..30)
        .find(|y| row_text(&plain, *y, 0, 25).contains("client-shell"))
        .expect("space row");
    assert!(!row_text(&plain, workspace_row, 0, 25).contains('⚑'));

    // A plain shell pane in a tab that is not shown still marks its space.
    set_flags(&mut state, &[("pane_4", None)]);
    let frame = state.compose(100, 30).expect("frame");
    let line = row_text(&frame, workspace_row, 0, 25);
    assert!(line.contains('⚑'), "space row: {line:?}");
    let x = find(&frame, workspace_row, 0, 25, "⚑")[0];
    assert_eq!(cell(&frame, x, workspace_row).fg, packed(YELLOW));
}
