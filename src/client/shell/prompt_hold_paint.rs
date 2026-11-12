//! How a pane that holds agent prompts looks. `super::prompt_holds` owns the
//! hold itself; this module only draws it.
//!
//! While the owner has an unsent draft in an agent pane, herdr holds the
//! prompts other agents send there (`crate::app::prompt_hold`). The pane's
//! top border says so at its right end: a quiet `✎` in the border's muted
//! gray while the pane is only holding, and a teal `✎ 2 prompts waiting`
//! once prompts wait behind the draft, which is the cue to press Enter. The
//! label and the flag chip read from the left end, so the mark never sits
//! where a flag could and keeps one place on every pane. It is text on the
//! line, never a filled chip: a flag is the only filled thing on a border.
//!
//! The top border is shared by the server's label, the flag chip
//! (`super::pane_flag_paint`) and this mark. [`plan_top_row`] decides for all
//! three what a narrow border keeps, and both painters draw from that one
//! plan, measured on the server's surface, so neither overruns the other.
//!
//! ```text
//!   compose ─► paint_pane_flags()    wash, then the chip where the plan
//!          │                         puts it (room left for the mark)
//!          └► paint_prompt_holds()   the mark where the plan puts it;
//!                                    returns at once with no holds
//! ```
//!
//! Everything here is presentation for the presented endpoint. It reads no
//! clock, terminal or agent state, and nothing moves: the projection update
//! that changes a hold brings the frame that redraws it.

use unicode_width::UnicodeWidthChar;

use super::landing_glow::{is_line_glyph, with_cell};
use super::pane_flag_paint::{chip_text, redraw_as_line, PaneFlagMarks, FLAG_GLYPH};
use super::*;
use crate::protocol::{CellData, PaneSurfacePane};

/// The draft glyph: herdr sees an unsent draft here. One cell wide, not an
/// emoji, so terminals agree on its width.
pub(super) const DRAFT_GLYPH: &str = "✎";

/// One form of the mark: its runs, each bold (glyph and count) or not
/// (padding and words), and their total width in cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Mark {
    parts: Vec<(bool, String)>,
    width: u16,
}

impl Mark {
    fn new(parts: Vec<(bool, String)>) -> Self {
        let width = parts
            .iter()
            .map(|(_, text)| usize::from(super::render::display_width(text)))
            .sum::<usize>()
            .min(usize::from(u16::MAX)) as u16;
        Self { parts, width }
    }
}

/// The mark's forms for `held` waiting prompts, longest first; the last is
/// the compact form a nearly closed pane keeps.
///
/// Holding with nothing waiting: ` ✎ `, then `✎`. With prompts waiting:
/// ` ✎ 2 prompts waiting `, ` ✎ 2 waiting `, ` ✎ 2 `, then `✎2`.
fn mark_forms(held: usize) -> Vec<Mark> {
    let run = |bold: bool, text: &str| (bold, text.to_owned());
    if held == 0 {
        return vec![
            Mark::new(vec![
                run(false, " "),
                run(true, DRAFT_GLYPH),
                run(false, " "),
            ]),
            Mark::new(vec![run(true, DRAFT_GLYPH)]),
        ];
    }
    let count = held.to_string();
    let counted = |words: &str| {
        Mark::new(vec![
            run(false, " "),
            run(true, DRAFT_GLYPH),
            run(false, " "),
            run(true, &count),
            run(false, &format!("{words} ")),
        ])
    };
    let prompts = if held == 1 {
        " prompt waiting"
    } else {
        " prompts waiting"
    };
    vec![
        counted(prompts),
        counted(" waiting"),
        counted(""),
        Mark::new(vec![run(true, DRAFT_GLYPH), run(true, &count)]),
    ]
}

/// Where a pane's marks go. A bordered pane uses its top border between the
/// corners, after the server's label. A pane with no top border (a lone pane
/// under `pane_borders = "auto"`, or `"off"`) uses the right end of its first
/// row, over the agent's own output.
pub(super) struct TopRow {
    /// Surface-relative start and row, and the cells available from there.
    pub(super) x: u16,
    pub(super) y: u16,
    pub(super) room: u16,
    /// Width of the server's label (` claude `); 0 on a borderless row.
    pub(super) label: u16,
    pub(super) border: bool,
}

impl TopRow {
    /// Measures `pane` on the server's own surface. Both painters measure
    /// here, never on the composed frame: once the flag painter has dropped
    /// the label, the composed border no longer shows how wide it was.
    pub(super) fn of(server: &FrameData, pane: &PaneSurfacePane) -> Option<Self> {
        let (rect, inner) = (pane.rect, pane.inner_rect);
        if inner.y > rect.y && rect.width > 2 {
            let x = rect.x.saturating_add(1);
            let end = rect.x.saturating_add(rect.width).saturating_sub(1);
            // The server draws its label (` name `) right after the corner;
            // it runs until the first line glyph.
            let mut label_end = x;
            while label_end < end && !is_line_glyph(server, label_end, rect.y) {
                label_end += 1;
            }
            Some(Self {
                x,
                y: rect.y,
                room: end.saturating_sub(x),
                label: label_end - x,
                border: true,
            })
        } else if inner.width > 0 && inner.height > 0 {
            Some(Self {
                x: inner.x,
                y: inner.y,
                room: inner.width,
                label: 0,
                border: false,
            })
        } else {
            None
        }
    }
}

/// What one pane's top row shows once a narrow pane has given way.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TopRowPlan {
    /// The server's label is redrawn as border line.
    pub(super) drop_label: bool,
    /// The flag chip's text.
    pub(super) chip: Option<String>,
    pub(super) mark: Option<Mark>,
    /// Cells from the row's start to the chip and to the mark.
    pub(super) chip_x: u16,
    pub(super) mark_x: u16,
}

/// Fits the label, the flag chip (`flag`: the pane is flagged, with this
/// message) and the hold mark (`held`: the pane holds, with this many
/// prompts waiting) into one top row.
///
/// Prompts waiting outrank everything else on the border. As a pane narrows,
/// the mark's words go first (`prompts`, then `waiting`), then the label, then
/// the flag's message, then the padding; the narrowest pane keeps `⚑` and
/// `✎2`. The quiet mark is different: it only takes room nobody else wants,
/// so typing never moves the label or the flag. Without a hold this is exactly
/// the flag's own rule: the label stays only beside the whole chip.
pub(super) fn plan_top_row(
    row: &TopRow,
    flag: Option<Option<&str>>,
    held: Option<usize>,
) -> TopRowPlan {
    let room = usize::from(row.room);
    let label = usize::from(row.label);
    let width = |text: &str| usize::from(super::render::display_width(text));
    let full_chip = flag.and_then(|message| chip_text(message, usize::MAX));
    // Label, a one-cell gap, the chip: the left end when nothing gives way.
    let left = |label: usize, chip: Option<&str>| {
        let chip = chip.map_or(0, width);
        label + chip + usize::from(label > 0 && chip > 0)
    };
    let fits = |left: usize, mark: &Mark| {
        let mark = usize::from(mark.width);
        left + mark + usize::from(left > 0 && mark > 0) <= room
    };
    let plan = |drop_label: bool, chip: Option<String>, mark: Option<Mark>| {
        place(row, drop_label && label > 0, chip, mark)
    };

    // The flag's own rule, which the quiet mark never changes.
    let whole = left(label, full_chip.as_deref());
    let (drop_label, chip) = if whole <= room {
        (false, full_chip.clone())
    } else {
        (true, flag.and_then(|message| chip_text(message, room)))
    };
    let forms = match held {
        None => return plan(drop_label, chip, None),
        Some(held) => mark_forms(held),
    };
    if held == Some(0) {
        let used = left(if drop_label { 0 } else { label }, chip.as_deref());
        let mark = forms.into_iter().find(|mark| fits(used, mark));
        return plan(drop_label, chip, mark);
    }

    let Some((compact, padded)) = forms.split_last() else {
        return plan(drop_label, chip, None);
    };
    // 1. Nothing on the left gives way; the mark's words do.
    if let Some(mark) = padded.iter().find(|mark| fits(whole, mark)) {
        return plan(false, full_chip, Some(mark.clone()));
    }
    // 2. The label gives way, then the flag's message, beside ` ✎ 2 `.
    if let Some(shortest) = padded.last() {
        let shortest_width = usize::from(shortest.width);
        match flag {
            None if shortest_width <= room => {
                return plan(true, None, Some(shortest.clone()));
            }
            Some(message) if room > shortest_width + 1 => {
                let chip = chip_text(message, room - shortest_width - 1);
                return plan(true, chip, Some(shortest.clone()));
            }
            _ => {}
        }
    }
    // 3. The padding gives way: a bare flag and `✎2`, then `✎2` alone.
    if flag.is_some() && fits(1, compact) {
        return plan(true, Some(FLAG_GLYPH.into()), Some(compact.clone()));
    }
    let mark = (usize::from(compact.width) <= room).then(|| compact.clone());
    plan(true, None, mark)
}

/// Positions the chip and the mark in the row. On a border the label and the
/// chip read from the left and the mark sits flush against the right corner.
/// On a borderless row the chip and the mark sit together at the right end,
/// so they cover as little of the agent's output as possible.
fn place(row: &TopRow, drop_label: bool, chip: Option<String>, mark: Option<Mark>) -> TopRowPlan {
    let chip_width = chip.as_deref().map_or(0, super::render::display_width);
    let mark_x = row
        .room
        .saturating_sub(mark.as_ref().map_or(0, |mark| mark.width));
    let chip_x = if !row.border {
        mark_x.saturating_sub(chip_width)
    } else if drop_label || row.label == 0 {
        0
    } else {
        row.label + 1
    };
    TopRowPlan {
        drop_label,
        chip,
        mark,
        chip_x,
        mark_x,
    }
}

impl ClientShellState {
    /// Draws the hold mark on every holding pane in the composed frame.
    ///
    /// Composition calls this once per frame right after `paint_pane_flags`,
    /// with the same arguments, and before the mode bar, selection, popups and
    /// toasts draw over it. Without holds it returns at the first branch; with
    /// holds it writes only the holding panes' top-row cells. `offset` is
    /// where the surface sits in the frame.
    pub(super) fn paint_prompt_holds(
        &self,
        frame: &mut FrameData,
        surface: &PaneSurfaceFrame,
        offset: Rect,
        pane_flags: &PaneFlagMarks,
    ) {
        let Some(holds) = self.prompt_holds().filter(|holds| !holds.is_empty()) else {
            return;
        };
        let palette = &self.config.palette;
        let quiet = crate::protocol::color_to_u32(palette.overlay0);
        let waiting = crate::protocol::color_to_u32(palette.teal);
        for pane in &surface.panes {
            let Some(hold) = holds.get(&pane.pane_id) else {
                continue;
            };
            let Some(row) = TopRow::of(&surface.frame, pane) else {
                continue;
            };
            // The same flag the flag painter planned with, so both agree.
            let flag = pane_flags.pane(&pane.pane_id);
            let plan = plan_top_row(&row, flag, Some(hold.held));
            let x = offset.x.saturating_add(row.x);
            let y = offset.y.saturating_add(row.y);
            let end = x.saturating_add(row.room);
            // A flagged pane's label was already dropped by the flag painter,
            // and its chip now sits where the label was.
            if plan.drop_label && flag.is_none() {
                let label_end = x.saturating_add(row.label);
                // Styled like the line cell after the label, or the corner.
                let like = if label_end < end {
                    label_end
                } else {
                    x.saturating_sub(1)
                };
                redraw_as_line(frame, x, label_end, y, like);
            }
            if let Some(mark) = &plan.mark {
                let (color, bold) = if hold.held > 0 {
                    (waiting, true)
                } else {
                    (quiet, false)
                };
                put_mark(
                    frame,
                    x.saturating_add(plan.mark_x),
                    y,
                    end,
                    mark,
                    color,
                    bold,
                );
            }
        }
    }
}

/// Writes the mark's cells from `x`, never past `end`. Backgrounds stay as
/// they are: the border's, or on a borderless row the agent's own (washed
/// when the pane is flagged). With `bold`, the glyph and the count are bold.
fn put_mark(frame: &mut FrameData, x: u16, y: u16, end: u16, mark: &Mark, color: u32, bold: bool) {
    let mut x = x;
    for (strong, text) in &mark.parts {
        let modifier = if bold && *strong {
            Modifier::BOLD.bits()
        } else {
            0
        };
        for character in text.chars() {
            let width = UnicodeWidthChar::width(character).unwrap_or(0) as u16;
            if width == 0 {
                continue;
            }
            if x.saturating_add(width) > end {
                return;
            }
            for offset in 0..width {
                with_cell(frame, x + offset, y, |cell: &mut CellData| {
                    cell.symbol = if offset == 0 {
                        character.to_string()
                    } else {
                        String::new()
                    };
                    cell.fg = color;
                    cell.modifier = modifier;
                    cell.hyperlink = None;
                });
            }
            x += width;
        }
    }
}
