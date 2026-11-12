//! How a pane flag looks. `super::pane_flags` owns the flag itself: the
//! server projection, the visit rules and the clearing request.
//!
//! The owner picked "warm wash" from the Paper prototypes: in gruvbox the focus
//! border, the active tab and the palette yellow share one hue, so a flag
//! cannot win on color and wins on shape instead. Focus is a line; a flag is a
//! fill. A flagged pane's cells take a steady tint of the palette's yellow,
//! and a filled yellow chip on its top border carries `⚑ message`. The tab
//! bar, the space rows and the agent rows repeat the `⚑`, so a flag in a tab
//! or workspace the owner is not looking at can still be found.
//!
//! ```text
//!   compose ─► pane_flag_marks()        once per frame; empty and
//!                │                      allocation-free with no flags
//!                ├─► tab bar, space rows and agent rows read the marks
//!                └─► paint_pane_flags() after the server surface is
//!                                       blitted: wash and chip cells
//! ```
//!
//! Everything here is presentation for the presented endpoint. It reads no
//! clock, terminal or agent state, and nothing moves: a flag costs no repaint
//! of its own. The server's projection update brings the frame that draws or
//! clears it.

use std::collections::{HashMap, HashSet};

use ratatui::style::Color;
use unicode_width::UnicodeWidthChar;

use super::landing_glow::{luminance, mix, pack, rgb, unpack_rgb, with_cell, Rgb, RESET};
use super::prompt_hold_paint::{plan_top_row, TopRow, TopRowPlan};
use super::*;
use crate::protocol::{CellData, SurfaceRect};

/// Share of the palette's yellow blended into flagged backgrounds. 12% is the
/// level the owner picked (`#413A29` over gruvbox's `#282828`): a flagged pane
/// stands out across a 483-column screen, and the agent's text keeps its
/// contrast because only backgrounds change.
pub(super) const WASH_SHARE: f32 = 0.12;

/// The flag glyph. It is one cell wide, so it never shifts what follows it.
pub(super) const FLAG_GLYPH: &str = "⚑";

/// What the tab bar and sidebar need to know about flags, resolved once per
/// composed frame for the presented endpoint.
///
/// Built from one pass over the snapshot's panes, so a tab or workspace marker
/// is a set lookup rather than a pane scan per row. Empty collections do not
/// allocate: a frame without flags pays one emptiness check per marker.
#[derive(Default)]
pub(super) struct PaneFlagMarks {
    /// Flagged pane id → its message, for panes the snapshot still lists.
    panes: HashMap<String, Option<String>>,
    tabs: HashSet<String>,
    workspaces: HashSet<String>,
    /// The `⚑` and message color: the palette's yellow.
    flag: Color,
    /// Background of a flagged agent row, over the plain and the focused row.
    /// None when the theme's colors cannot be blended (16-color theme).
    row_wash: Option<Color>,
    focused_row_wash: Option<Color>,
}

impl PaneFlagMarks {
    pub(super) fn tab(&self, tab_id: &str) -> bool {
        !self.tabs.is_empty() && self.tabs.contains(tab_id)
    }

    pub(super) fn workspace(&self, workspace_id: &str) -> bool {
        !self.workspaces.is_empty() && self.workspaces.contains(workspace_id)
    }

    /// `Some(message)` when the pane is flagged.
    pub(super) fn pane(&self, pane_id: &str) -> Option<Option<&str>> {
        if self.panes.is_empty() {
            return None;
        }
        self.panes.get(pane_id).map(Option::as_deref)
    }
}

impl ClientShellState {
    /// Resolves the presented endpoint's flags for the chrome. Returns at
    /// once, without allocating, when nothing is flagged.
    pub(super) fn pane_flag_marks(&self) -> PaneFlagMarks {
        let (Some(flags), Some(snapshot)) = (
            self.pane_flags().filter(|flags| !flags.is_empty()),
            self.snapshot.as_deref(),
        ) else {
            return PaneFlagMarks::default();
        };
        let palette = &self.config.palette;
        let wash_over = |background: Color| {
            let base = match background {
                Color::Reset => reset_background(palette, self.host_background),
                color => rgb(color),
            }?;
            let (r, g, b) = mix(base, rgb(palette.yellow)?, WASH_SHARE);
            Some(Color::Rgb(r, g, b))
        };
        let mut marks = PaneFlagMarks {
            flag: palette.yellow,
            row_wash: wash_over(palette.sidebar_bg),
            focused_row_wash: wash_over(palette.active_row_bg),
            ..PaneFlagMarks::default()
        };
        for pane in &snapshot.panes {
            let Some(flag) = flags.get(&pane.pane_id) else {
                continue;
            };
            marks
                .panes
                .insert(pane.pane_id.clone(), flag.message.clone());
            marks.tabs.insert(pane.tab_id.clone());
            marks.workspaces.insert(pane.workspace_id.clone());
        }
        marks
    }

    /// Draws every flagged pane in `marks` into the composed frame: the wash
    /// under its content and the chip on its border.
    ///
    /// Composition calls this once per frame with the frame's marks, after
    /// the server surface is blitted and before the mode bar, selection,
    /// popups and toasts draw over it. Without flags it returns at the first
    /// branch; with flags it visits only the flagged panes' cells. `offset` is
    /// where the surface sits in the frame.
    pub(super) fn paint_pane_flags(
        &self,
        frame: &mut FrameData,
        surface: &PaneSurfaceFrame,
        offset: Rect,
        marks: &PaneFlagMarks,
    ) {
        if marks.panes.is_empty() {
            return;
        }
        let colors = FlagColors::resolve(&self.config.palette, self.host_background);
        let place = |rect: SurfaceRect| {
            Rect::new(
                offset.x.saturating_add(rect.x),
                offset.y.saturating_add(rect.y),
                rect.width,
                rect.height,
            )
        };
        // The hold mark shares the top border, so the chip is placed by the
        // joint plan (`super::prompt_hold_paint`), which leaves it room.
        let holds = self.prompt_holds().filter(|holds| !holds.is_empty());
        for pane in &surface.panes {
            let Some(message) = marks.pane(&pane.pane_id) else {
                continue;
            };
            if let Some((yellow, base)) = colors.wash {
                wash(frame, place(pane.inner_rect), yellow, base);
            }
            let Some(row) = TopRow::of(&surface.frame, pane) else {
                continue;
            };
            let held = holds
                .and_then(|holds| holds.get(&pane.pane_id))
                .map(|hold| hold.held);
            let plan = plan_top_row(&row, Some(message), held);
            paint_chip(frame, &row, &plan, offset, &colors);
        }
    }
}

/// The palette colors a frame paint needs, resolved once per paint.
struct FlagColors {
    /// The yellow to blend toward and what a `Reset` background really shows.
    /// None when the yellow is a named terminal color, which has no RGB to
    /// blend: the 16-color theme then gets the chip without the wash.
    wash: Option<(Rgb, Option<Rgb>)>,
    chip_bg: u32,
    chip_fg: u32,
}

impl FlagColors {
    fn resolve(
        palette: &Palette,
        host_background: Option<crate::terminal_theme::RgbColor>,
    ) -> Self {
        Self {
            wash: rgb(palette.yellow)
                .map(|yellow| (yellow, reset_background(palette, host_background))),
            chip_bg: crate::protocol::color_to_u32(palette.yellow),
            chip_fg: crate::protocol::color_to_u32(chip_text_color(palette)),
        }
    }
}

/// What a `Reset` background looks like: the host terminal's real background
/// when it reported one, else the theme's panel color, as `landing_glow` does.
fn reset_background(
    palette: &Palette,
    host_background: Option<crate::terminal_theme::RgbColor>,
) -> Option<Rgb> {
    host_background
        .map(|color| (color.r, color.g, color.b))
        .or_else(|| rgb(palette.panel_bg))
        .or_else(|| rgb(palette.surface_dim))
}

/// The chip's text: whichever of the panel background and the text color
/// stands out more against the yellow. Dark text on gruvbox and catppuccin,
/// and still the darker one on light themes whose yellow is mid-tone.
fn chip_text_color(palette: &Palette) -> Color {
    let Some(yellow) = rgb(palette.yellow) else {
        return Color::Black;
    };
    let contrast = |color: Rgb| {
        let (a, b) = (luminance(color), luminance(yellow));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    };
    [palette.panel_bg, palette.text]
        .into_iter()
        .filter_map(|color| rgb(color).map(|value| (color, contrast(value))))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(Color::Black, |(color, _)| color)
}

/// Blends every background in `area` toward yellow. A `Reset` background
/// blends from the real terminal background when it is known; cells with
/// their own RGB background (diffs, status lines, an agent's colored output)
/// blend by the same share, so their text stays readable. Named and indexed
/// backgrounds have no known RGB and keep their color.
fn wash(frame: &mut FrameData, area: Rect, yellow: Rgb, base: Option<Rgb>) {
    let width = usize::from(frame.width);
    let right = usize::from(area.right().min(frame.width));
    let left = usize::from(area.x).min(right);
    for y in area.y..area.bottom().min(frame.height) {
        let row = usize::from(y) * width;
        let Some(cells) = frame.cells.get_mut(row + left..row + right) else {
            continue;
        };
        for cell in cells {
            let from = match cell.bg {
                RESET => base,
                packed => unpack_rgb(packed),
            };
            if let Some(from) = from {
                cell.bg = pack(mix(from, yellow, WASH_SHARE));
            }
        }
    }
}

/// Draws the chip where `plan` puts it. On a bordered pane that drops its
/// label, the label's cells become border line first; a borderless pane (a
/// lone pane, `pane_borders = "off"`) carries the chip at the right end of its
/// first row.
fn paint_chip(
    frame: &mut FrameData,
    row: &TopRow,
    plan: &TopRowPlan,
    offset: Rect,
    colors: &FlagColors,
) {
    let x = offset.x.saturating_add(row.x);
    let y = offset.y.saturating_add(row.y);
    let end = x.saturating_add(row.room);
    if plan.drop_label {
        let label_end = x.saturating_add(row.label);
        // Styled like the line cell after the label, or the corner.
        let like = if label_end < end {
            label_end
        } else {
            x.saturating_sub(1)
        };
        redraw_as_line(frame, x, label_end, y, like);
    }
    if let Some(text) = plan.chip.as_deref() {
        put_chip(frame, x.saturating_add(plan.chip_x), y, end, text, colors);
    }
}

/// Repaints cells `from..to` on row `y` as border line, styled like the line
/// cell at `like`, where the dropped label was.
pub(super) fn redraw_as_line(frame: &mut FrameData, from: u16, to: u16, y: u16, like: u16) {
    let index = usize::from(y) * usize::from(frame.width) + usize::from(like);
    let Some(template) = frame.cells.get(index).cloned() else {
        return;
    };
    for x in from..to {
        with_cell(frame, x, y, |cell: &mut CellData| {
            *cell = CellData {
                symbol: "─".into(),
                ..template.clone()
            };
        });
    }
}

/// ` ⚑ message ` fitted to `room` cells: the whole message, then the message
/// cut with an ellipsis, then a bare ` ⚑ `, then `⚑` alone.
pub(super) fn chip_text(message: Option<&str>, room: usize) -> Option<String> {
    if let Some(message) = message.filter(|message| !message.is_empty()) {
        let full = format!(" {FLAG_GLYPH} {message} ");
        if usize::from(chip_width(&full)) <= room {
            return Some(full);
        }
        // ` ⚑ ` and a trailing space leave `room - 4` for the message and its
        // ellipsis; below two cells only the ellipsis would show.
        if room >= 6 {
            let cut = crate::ui::truncate_end(message, room - 4);
            return Some(format!(" {FLAG_GLYPH} {cut} "));
        }
    }
    match room {
        0 => None,
        1 | 2 => Some(FLAG_GLYPH.into()),
        _ => Some(format!(" {FLAG_GLYPH} ")),
    }
}

fn chip_width(text: &str) -> u16 {
    super::render::display_width(text)
}

/// Writes the chip's cells from `x`, never past `end`. A wide character's
/// second cell gets an empty symbol, as the server encodes it.
fn put_chip(frame: &mut FrameData, x: u16, y: u16, end: u16, text: &str, colors: &FlagColors) {
    let mut x = x;
    for character in text.chars() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0) as u16;
        if width == 0 {
            continue;
        }
        if x.saturating_add(width) > end {
            break;
        }
        for offset in 0..width {
            with_cell(frame, x + offset, y, |cell: &mut CellData| {
                cell.symbol = if offset == 0 {
                    character.to_string()
                } else {
                    String::new()
                };
                cell.fg = colors.chip_fg;
                cell.bg = colors.chip_bg;
                cell.modifier = Modifier::BOLD.bits();
                cell.hyperlink = None;
            });
        }
        x += width;
    }
}

/// Marks a flagged agent row: the row takes the wash, its first line ends in
/// `⚑`, and a message takes over the second line in yellow.
pub(super) fn mark_agent_row(
    buffer: &mut Buffer,
    rect: Rect,
    focused: bool,
    message: Option<&str>,
    marks: &PaneFlagMarks,
) {
    if rect.is_empty() {
        return;
    }
    if let Some(wash) = if focused {
        marks.focused_row_wash
    } else {
        marks.row_wash
    } {
        buffer.set_style(rect, Style::default().bg(wash));
    }
    end_mark(buffer, rect.x, rect.right(), rect.y, marks.flag);
    let Some(message) = message.filter(|message| !message.is_empty()) else {
        return;
    };
    if rect.height < 2 {
        return;
    }
    // The row's continuation lines are indented by three cells.
    let indent = 3.min(rect.width);
    let width = rect.width - indent;
    let y = rect.y + 1;
    super::render::put_text(
        buffer,
        rect.x + indent,
        y,
        width,
        &" ".repeat(usize::from(width)),
        Style::default(),
    );
    super::render::put_text(
        buffer,
        rect.x + indent,
        y,
        width,
        &crate::ui::truncate_end(message, usize::from(width)),
        Style::default().fg(marks.flag),
    );
}

/// Marks a space row whose workspace holds a flagged pane. A parent group's
/// toggle owns the row's last column, so the flag sits two cells before it.
pub(super) fn mark_workspace_row(
    buffer: &mut Buffer,
    rect: Rect,
    group_toggle: bool,
    marks: &PaneFlagMarks,
) {
    let right = if group_toggle {
        rect.right().saturating_sub(2)
    } else {
        rect.right()
    };
    end_mark(buffer, rect.x, right, rect.y, marks.flag);
}

/// `⚑` in the last column before `right` on row `y`. When text runs into that
/// column, the cell before it is blanked so the mark never reads as a letter.
fn end_mark(buffer: &mut Buffer, left: u16, right: u16, y: u16, color: Color) {
    if right <= left {
        return;
    }
    let x = right - 1;
    if x > left {
        if let Some(cell) = buffer.cell_mut((x - 1, y)) {
            if cell.symbol() != " " {
                cell.set_symbol(" ");
            }
        }
    }
    if let Some(cell) = buffer.cell_mut((x, y)) {
        cell.set_symbol(FLAG_GLYPH)
            .set_style(Style::default().fg(color).add_modifier(Modifier::BOLD));
    }
}

/// The tab label with the flag in front, for a tab holding a flagged pane.
pub(super) fn flagged_tab_label(label: String, flagged: bool) -> String {
    if flagged {
        format!("{FLAG_GLYPH} {label}")
    } else {
        label
    }
}

/// The flag's style on a tab chip: the theme yellow on an inactive chip, and
/// the active chip's own dark label color there, so it never fights the accent.
pub(super) fn tab_flag_style(active: bool, palette: &Palette) -> Style {
    Style::default()
        .fg(if active {
            panel_contrast_fg(palette)
        } else {
            palette.yellow
        })
        .add_modifier(Modifier::BOLD)
}
