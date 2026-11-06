//! Copy hints, the client half: copy mode's hint sub-state.
//!
//! `keys.copy_hints` (default prefix+f), or `f` inside copy mode, freezes the
//! focused pane's visible cells, labels every copy target on them with
//! `crate::copy_hints`, and waits for a label. Typing one writes that text to
//! this client's clipboard (native, or OSC 52 under SSH), shows the usual
//! "copied to clipboard" toast, and ends copy mode as `y` does.
//!
//! ```text
//!  prefix+f / f ─► enter_copy_hints ─► enter_copy_mode, hints = Waiting ─┐
//!                                                                         ▼
//!  install_pane_surface ─┐                                   reconcile_copy_hints
//!  apply_active_snapshot ┴─► (at the end of both) ─► pane gone, unfocused, under a
//!                                                     popup or resized ─► close_copy_hints
//!                                                     Waiting, and the surface shows copy
//!                                                     mode's viewport ─► capture ─► Frozen
//!  key ─► route_copy_mode_key ─► route_copy_hints_key ─► ClientCopyHints::press
//!           Chosen(text) ─► ClipboardWrite + toast + exit_copy_mode
//!           Cancelled    ─► close_copy_hints: back to copy mode, or out of it
//!  compose ─► render_copy_hints: frozen cells, underlines, labels; reads only
//! ```
//!
//! Why frozen: a pane's `content_revision` moves on every PTY read, several
//! times a second under a spinner, and a fullscreen app redraws under scroll
//! metrics that never change. Labels tied to live cells would drift or vanish
//! while the user types. The capture is the one source the display, the labels
//! and the copied text all read, so the copy is exactly what was shown, with no
//! server round trip that could race the app. The cost is that the hinted pane
//! looks paused until a label or Esc; other panes and the chrome stay live.
//!
//! Why client cells: they are what this client drew, for local, SSH and
//! `--remote` endpoints alike, so hints need no server method and no wire
//! change. The price is that cells carry no soft-wrap flag; `row_continues`
//! guesses instead.

use super::*;
use crate::copy_hints::{HintRules, HintTarget};
use crate::protocol::{CellData, PaneSurfacePane};
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

/// Where Esc returns to, decided once when hints open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HintEntry {
    /// Opened from the pane (prefix+f): the copy session exists only for the
    /// hints, so cancelling ends it too and the user is back in the pane.
    Direct,
    /// Opened inside copy mode (`f`, or prefix+f there): cancelling returns
    /// to copy mode at the same cursor and scroll position.
    FromCopyMode,
}

/// `ClientCopyModeState::hints`: the hint session, owned by its copy session
/// so it can never outlive it. `exit_copy_mode`, pane removal and an endpoint
/// reset drop both.
///
/// While it is `Some`, hints take every key in copy mode, the prefix included
/// (`ClientCopyModeState::captures_all_keys`), and compose draws no mode bar,
/// so the pane's last row keeps its labels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClientCopyHints {
    entry: HintEntry,
    view: HintView,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HintView {
    /// The retained surface does not show copy mode's viewport yet: a
    /// `pane.scroll` from paging back is still in flight. Capturing now would
    /// freeze the page the user just left, so the capture waits for the first
    /// surface that shows the page asked for. Nothing is painted meanwhile.
    Waiting,
    Frozen(FrozenHints),
}

/// One capture. Nothing in it changes after `capture` except `typed`, which
/// only `press` changes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FrozenHints {
    /// (width, height) of the pane's inner rect at capture. A resize ends the
    /// session, because the capture no longer fits the pane.
    size: (u16, u16),
    /// Row-major `width * height` cells as drawn at capture, normalized once:
    /// wide-character spacers hold " " (a label over a wide head exposes its
    /// spacer, which must read as blank) and hyperlink ids are cleared (they
    /// index the hyperlink table of a surface later frames replace).
    cells: Vec<CellData>,
    /// In label order: bottom-most target first.
    targets: Vec<PlacedTarget>,
    /// Label keys typed so far: empty, or a proper prefix of some label.
    /// `press` consumes an exact label as a choice and resets a dead prefix.
    typed: String,
}

/// A `HintTarget` with its occurrences mapped from bytes to grid cells.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlacedTarget {
    label: String,
    text: String,
    places: Vec<HintPlace>,
}

/// One occurrence on the frozen grid.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HintPlace {
    /// The first label cell: the copied text's first cell, moved left just
    /// enough for the whole label to fit in the row, then back onto the head
    /// of a wide character if it landed on a spacer.
    label_at: GridPos,
    /// The whole match, one run per row it covers. More than one only when
    /// the match crosses a joined soft wrap.
    underline: Vec<GridRun>,
}

/// A cell of the frozen grid, relative to the pane's inner rect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridPos {
    row: u16,
    col: u16,
}

/// Columns `start..end` of one frozen row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridRun {
    row: u16,
    start: u16,
    end: u16,
}

/// What one key did to the session.
#[derive(Debug, PartialEq, Eq)]
enum HintPress {
    Consumed { repaint: bool },
    Chosen(String),
    Cancelled,
}

/// How the capture classifies a cell; used to build lines, guess soft wraps
/// and place labels, never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CellKind {
    /// A cell that starts a grapheme, `width` columns wide.
    Head { width: u16 },
    /// A column covered by the wide head before it, whatever symbol the wire
    /// carried there (`""` from the terminal, or the `" "` a buffer leaves).
    Spacer,
}

/// One logical line of the capture: a row, or several rows joined across a
/// soft wrap, with trailing blanks trimmed and the cell each grapheme of the
/// text came from.
#[derive(Default)]
struct CapturedLine {
    text: String,
    /// Ascending by `byte`.
    graphemes: Vec<CapturedGrapheme>,
}

struct CapturedGrapheme {
    /// Where the grapheme starts in `CapturedLine::text`.
    byte: usize,
    pos: GridPos,
    width: u16,
}

impl ClientShellState {
    /// The one way into hints: `keys.copy_hints` through `record_binding`,
    /// and `f` inside copy mode.
    ///
    /// It enters copy mode on the focused pane unless copy mode is already
    /// there, so a pane copy mode cannot enter gets no hints either. Then it
    /// opens the sub-state as `Waiting` and lets `reconcile_copy_hints`
    /// capture at once when the retained surface already shows copy mode's
    /// viewport, as it always does for a fresh session. Hints that are
    /// already open stay as they are.
    pub(super) fn enter_copy_hints(&mut self, outcome: &mut ClientShellInput) {
        let focused = self.focused_pane_id();
        let entry = if self
            .copy_mode
            .as_ref()
            .is_some_and(|copy_mode| focused.as_deref() == Some(copy_mode.pane_id.as_str()))
        {
            HintEntry::FromCopyMode
        } else {
            HintEntry::Direct
        };
        if !self.enter_copy_mode(outcome) {
            return;
        }
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return;
        };
        // The search prompt also takes every key, the prefix included, so no
        // key reaches here while it is open; the check keeps the two
        // sub-states from ever stacking.
        if copy_mode.search_prompt.is_some() || copy_mode.hints.is_some() {
            return;
        }
        copy_mode.hints = Some(ClientCopyHints {
            entry,
            view: HintView::Waiting,
        });
        outcome.repaint = true;
        self.reconcile_copy_hints();
    }

    /// Hints' key handling, first in `route_copy_mode_key`: ahead of the
    /// search prompt, copy-mode custom commands and the motion table, so a
    /// label key such as `c` never runs a copy-mode command bound to it.
    /// Returns false only when hints are closed.
    pub(super) fn route_copy_hints_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(hints) = self
            .copy_mode
            .as_mut()
            .and_then(|copy_mode| copy_mode.hints.as_mut())
        else {
            return false;
        };
        match hints.press(key) {
            HintPress::Consumed { repaint } => outcome.repaint |= repaint,
            HintPress::Cancelled => {
                self.close_copy_hints();
                outcome.repaint = true;
            }
            HintPress::Chosen(text) => {
                // The scanned text itself, not a server read of the range:
                // no round trip, and no race with an app that redrew since.
                outcome.repaint |= self.show_copy_feedback(std::time::Instant::now());
                outcome
                    .actions
                    .push(ClientShellAction::ClipboardWrite(text.into_bytes()));
                // A hint copy finishes with the pane, so from either entry it
                // lands on the live bottom, even if the pane was already
                // scrolled up when hints opened. Exiting also pages a
                // fullscreen app back down.
                if let Some(copy_mode) = self.copy_mode.as_mut() {
                    copy_mode.entry_offset_from_bottom = 0;
                }
                self.exit_copy_mode(false, outcome);
            }
        }
        true
    }

    /// Keeps the hint session's one lifetime rule; runs at the end of
    /// `install_pane_surface` and `apply_active_snapshot`, the seams where
    /// `reconcile_input_source` runs. Idempotent, and a single `Option` check
    /// while hints are closed, so plain Terminal and copy mode do no hint
    /// work per surface.
    ///
    /// The rule: a session lives only while its pane is in the retained
    /// surface, is the focused pane, no popup owns input, and, once frozen,
    /// the pane keeps the captured inner size. Breaking it closes the session
    /// as Esc would. A `Waiting` session captures here, on the first surface
    /// that shows copy mode's viewport; a capture with no targets closes at
    /// once with a short notice instead of leaving an unlabeled frozen pane.
    ///
    /// Deliberately not a reason to close: a new `content_revision`, new
    /// scroll metrics or an alternate-screen switch. That is the point of
    /// freezing; nothing about the live pane is needed to paint or copy.
    pub(super) fn reconcile_copy_hints(&mut self) {
        enum Step {
            Keep,
            Close,
            Capture(FrozenHints),
        }
        let step = {
            let Some(copy_mode) = self.copy_mode.as_ref() else {
                return;
            };
            let Some(hints) = copy_mode.hints.as_ref() else {
                return;
            };
            let owns_input = self.popup_terminal_id.is_none()
                && self.focused_pane_id().as_deref() == Some(copy_mode.pane_id.as_str());
            let surface = self.pane_surface.as_ref().filter(|_| owns_input);
            let pane = surface.and_then(|surface| {
                surface
                    .panes
                    .iter()
                    .find(|pane| pane.pane_id == copy_mode.pane_id)
                    .map(|pane| (surface, pane))
            });
            match (pane, &hints.view) {
                (Some((_, pane)), HintView::Frozen(frozen)) if frozen.size == inner_size(pane) => {
                    Step::Keep
                }
                (Some((surface, pane)), HintView::Waiting) => {
                    if copy_mode.shows_viewport(inner_size(pane), scroll_pair(pane)) {
                        Step::Capture(FrozenHints::capture(
                            pane,
                            &surface.frame,
                            &self.config.copy_hint_rules,
                        ))
                    } else {
                        Step::Keep
                    }
                }
                _ => Step::Close,
            }
        };
        match step {
            Step::Keep => {}
            Step::Close => {
                self.close_copy_hints();
            }
            Step::Capture(frozen) if frozen.targets.is_empty() => {
                self.close_copy_hints();
                self.show_clipboard_notice("no copy targets", std::time::Instant::now());
            }
            Step::Capture(frozen) => {
                if let Some(hints) = self
                    .copy_mode
                    .as_mut()
                    .and_then(|copy_mode| copy_mode.hints.as_mut())
                {
                    hints.view = HintView::Frozen(frozen);
                }
            }
        }
    }

    /// Ends the hint session the way Esc does, without copying. A `Direct`
    /// session drops its whole copy session and returns to Terminal mode; a
    /// `FromCopyMode` one drops only the hints. It sends nothing, so it is
    /// safe from reconcile seams that have no outcome. A `Direct` session
    /// never scrolled (hints take every key, and the mouse guard closes them
    /// before any wheel or click is handled), so it has no offset to restore
    /// and no app page to undo. Returns whether anything closed.
    pub(super) fn close_copy_hints(&mut self) -> bool {
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return false;
        };
        let Some(hints) = copy_mode.hints.take() else {
            return false;
        };
        if hints.entry == HintEntry::Direct {
            self.copy_mode = None;
            self.reset_copy_pipeline();
            if self.mode == ClientShellMode::Copy {
                self.mode = ClientShellMode::Terminal;
            }
        }
        true
    }

    /// Whether a hint session is open, waiting or frozen.
    pub(super) fn copy_hints_open(&self) -> bool {
        self.copy_mode
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.hints.is_some())
    }

    /// Paints a frozen session over its pane: the captured cells, an
    /// underline under every target whose label starts with the typed
    /// prefix, then those labels, typed keys on the accent color and the rest
    /// on yellow. Targets the prefix rules out show as plain captured text.
    ///
    /// `compose` calls it after link hover and the copy cursor and before the
    /// final `restore_mode_bar`, so toasts, popups and overlays still draw
    /// over it. It writes `frame.cells` directly, as `render_link_hover`
    /// does; a `to_ratatui_buffer` round trip would cost a full-frame pass and
    /// drop Herdr's underline-style bits. It covers the whole inner rect in
    /// `occlusion`, so kitty images in that pane hide instead of drifting over
    /// still text.
    ///
    /// Cost per frame while open: one row copy per pane row plus the active
    /// labels and underlines. It reads `hits.panes`, which compose rebuilt
    /// just before, only for the pane's screen position.
    pub(super) fn render_copy_hints(
        &self,
        frame: &mut FrameData,
        occlusion: &mut crate::kitty_graphics::surface::Occlusion,
    ) {
        let Some((pane_id, frozen)) = self.copy_mode.as_ref().and_then(|copy_mode| {
            Some((
                copy_mode.pane_id.as_str(),
                copy_mode.hints.as_ref()?.frozen()?,
            ))
        }) else {
            return;
        };
        let Some(hit) = self.hits.panes.iter().find(|hit| hit.pane_id == pane_id) else {
            return;
        };
        let inner = hit.inner_rect;
        if (inner.width, inner.height) != frozen.size {
            return;
        }
        let area = inner.intersection(Rect::new(0, 0, frame.width, frame.height));
        if area.is_empty() {
            return;
        }
        occlusion.cover(area);
        let frame_width = usize::from(frame.width);
        let width = usize::from(frozen.size.0);
        let visible = usize::from(area.width);
        for row in 0..area.height {
            let source = usize::from(area.y - inner.y + row) * width;
            let target = usize::from(area.y + row) * frame_width + usize::from(area.x);
            if let (Some(target), Some(source)) = (
                frame.cells.get_mut(target..target + visible),
                frozen.cells.get(source..source + visible),
            ) {
                target.clone_from_slice(source);
            }
        }

        let palette = &self.config.palette;
        let match_fg = crate::protocol::color_to_u32(palette.yellow);
        let label_fg = crate::protocol::color_to_u32(panel_contrast_fg(palette));
        let label_bg = crate::protocol::color_to_u32(palette.yellow);
        let typed_bg = crate::protocol::color_to_u32(palette.accent);
        let cell_index = |pos: GridPos| {
            let x = inner.x.saturating_add(pos.col);
            let y = inner.y.saturating_add(pos.row);
            (x >= area.x && x < area.right() && y >= area.y && y < area.bottom())
                .then(|| usize::from(y) * frame_width + usize::from(x))
        };
        let active = || {
            frozen
                .targets
                .iter()
                .filter(|target| target.label.starts_with(frozen.typed.as_str()))
        };
        // Underlines first, so a neighbour's underline never covers a label.
        for run in active()
            .flat_map(|target| &target.places)
            .flat_map(|place| &place.underline)
        {
            for col in run.start..run.end {
                let Some(cell) = cell_index(GridPos { row: run.row, col })
                    .and_then(|index| frame.cells.get_mut(index))
                else {
                    continue;
                };
                cell.modifier |= Modifier::UNDERLINED.bits();
                cell.fg = match_fg;
            }
        }
        for target in active() {
            for place in &target.places {
                for (offset, key) in target.label.chars().enumerate() {
                    let col = place.label_at.col.saturating_add(offset as u16);
                    if col >= frozen.size.0 {
                        break;
                    }
                    let Some(cell) = cell_index(GridPos {
                        row: place.label_at.row,
                        col,
                    })
                    .and_then(|index| frame.cells.get_mut(index)) else {
                        continue;
                    };
                    cell.symbol.clear();
                    cell.symbol.push(key);
                    cell.fg = label_fg;
                    cell.bg = if offset < frozen.typed.len() {
                        typed_bg
                    } else {
                        label_bg
                    };
                    cell.modifier = Modifier::BOLD.bits();
                    cell.skip = false;
                    cell.hyperlink = None;
                }
            }
        }
    }

    /// Test view: (label, text) of every target the typed prefix keeps, in
    /// label order. Empty while waiting.
    #[cfg(test)]
    pub(super) fn copy_hint_labels(&self) -> Vec<(String, String)> {
        self.copy_mode
            .as_ref()
            .and_then(|copy_mode| copy_mode.hints.as_ref()?.frozen())
            .map(|frozen| {
                frozen
                    .targets
                    .iter()
                    .filter(|target| target.label.starts_with(frozen.typed.as_str()))
                    .map(|target| (target.label.clone(), target.text.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl ClientCopyHints {
    fn frozen(&self) -> Option<&FrozenHints> {
        match &self.view {
            HintView::Frozen(frozen) => Some(frozen),
            HintView::Waiting => None,
        }
    }

    /// The typed-label state machine. Pure, so it is unit-tested directly.
    ///
    /// - Esc clears a partial label; with nothing typed, it cancels.
    /// - Ctrl+C cancels.
    /// - Backspace takes back the last typed key.
    /// - A character, decoded by `copy_mode_command_char` so layouts and
    ///   Shift resolve as in copy mode, is lowercased (Shift+a types `a`) and
    ///   appended. An exact label is `Chosen`, a prefix of some label narrows,
    ///   and anything else resets what was typed.
    /// - Every other key is consumed and changes nothing.
    /// - While `Waiting`, Esc and Ctrl+C cancel and every other key is
    ///   consumed.
    fn press(&mut self, key: &crate::input::TerminalKey) -> HintPress {
        // Auto-repeat never types a label: the repeat of a held `f`, the key
        // that opened hints, would otherwise pick the target labeled `f`.
        if key.kind == KeyEventKind::Repeat {
            return HintPress::Consumed { repaint: false };
        }
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        let HintView::Frozen(frozen) = &mut self.view else {
            return if ctrl_c || key.code == KeyCode::Esc {
                HintPress::Cancelled
            } else {
                HintPress::Consumed { repaint: false }
            };
        };
        if ctrl_c {
            return HintPress::Cancelled;
        }
        match key.code {
            KeyCode::Esc if frozen.typed.is_empty() => HintPress::Cancelled,
            KeyCode::Esc => {
                frozen.typed.clear();
                HintPress::Consumed { repaint: true }
            }
            KeyCode::Backspace => HintPress::Consumed {
                repaint: frozen.typed.pop().is_some(),
            },
            _ => {
                let Some(typed) = crate::copy_mode::copy_mode_command_char(key.clone()) else {
                    return HintPress::Consumed { repaint: false };
                };
                frozen.typed.push(typed.to_ascii_lowercase());
                if let Some(target) = frozen
                    .targets
                    .iter()
                    .find(|target| target.label == frozen.typed)
                {
                    return HintPress::Chosen(target.text.clone());
                }
                if !frozen
                    .targets
                    .iter()
                    .any(|target| target.label.starts_with(frozen.typed.as_str()))
                {
                    frozen.typed.clear();
                }
                HintPress::Consumed { repaint: true }
            }
        }
    }
}

impl FrozenHints {
    /// Freezes one pane's inner rect out of `frame` and labels it. Pure; runs
    /// only from `reconcile_copy_hints`, on a key or a surface install, never
    /// while composing.
    ///
    /// The cells come straight from the retained surface, what this client
    /// last drew for the pane, whatever its `content_revision`. An odd,
    /// mid-write revision is not refused as link hover refuses it, because
    /// nothing is checked against the server afterwards: the user picks from
    /// the capture and gets the capture.
    fn capture(pane: &PaneSurfacePane, frame: &FrameData, rules: &HintRules) -> Self {
        let size = inner_size(pane);
        let (width, height) = (usize::from(size.0), usize::from(size.1));
        let frame_width = usize::from(frame.width);
        let mut cells = Vec::with_capacity(width * height);
        for row in 0..height {
            let y = usize::from(pane.inner_rect.y) + row;
            for col in 0..width {
                let x = usize::from(pane.inner_rect.x) + col;
                let cell = (x < frame_width && y < usize::from(frame.height))
                    .then(|| frame.cells.get(y * frame_width + x))
                    .flatten();
                cells.push(cell.cloned().unwrap_or_else(blank_cell));
            }
        }
        let kinds = classify(&cells, size);
        let lines = logical_lines(&cells, &kinds, size);
        for (cell, kind) in cells.iter_mut().zip(&kinds) {
            if *kind == CellKind::Spacer {
                cell.symbol.clear();
                cell.symbol.push(' ');
            }
            cell.hyperlink = None;
        }
        let texts = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        let targets = rules
            .scan(&texts)
            .into_iter()
            .map(|target| place_target(target, &lines, &kinds, size))
            .collect();
        Self {
            size,
            cells,
            targets,
            typed: String::new(),
        }
    }
}

impl CapturedLine {
    /// The index of the grapheme that holds `byte`.
    fn grapheme_at(&self, byte: usize) -> Option<usize> {
        self.graphemes
            .partition_point(|grapheme| grapheme.byte <= byte)
            .checked_sub(1)
    }

    /// Trims trailing whitespace, as the server's text reads do, so a greedy
    /// pattern such as git-status's `.+` does not copy a row's padding.
    fn trim_end(&mut self) {
        let len = self.text.trim_end().len();
        self.text.truncate(len);
        let kept = self
            .graphemes
            .partition_point(|grapheme| grapheme.byte < len);
        self.graphemes.truncate(kept);
    }
}

fn blank_cell() -> CellData {
    CellData {
        symbol: " ".to_owned(),
        fg: 0,
        bg: 0,
        modifier: 0,
        skip: false,
        hyperlink: None,
    }
}

/// Classifies every cell, row by row: a cell `w` columns wide by the
/// encoder's own rule (`render_ansi::cell_width`) is a head, and the `w - 1`
/// cells after it in the row are its spacers.
fn classify(cells: &[CellData], size: (u16, u16)) -> Vec<CellKind> {
    let width = usize::from(size.0);
    let mut kinds = vec![CellKind::Head { width: 1 }; cells.len()];
    if width == 0 {
        return kinds;
    }
    for (row_cells, row_kinds) in cells.chunks(width).zip(kinds.chunks_mut(width)) {
        let mut col = 0;
        while col < row_cells.len() {
            let cell_width = crate::protocol::render_ansi::cell_width(&row_cells[col]).max(1);
            row_kinds[col] = CellKind::Head {
                width: u16::try_from(cell_width).unwrap_or(u16::MAX),
            };
            for spacer in row_kinds.iter_mut().skip(col + 1).take(cell_width - 1) {
                *spacer = CellKind::Spacer;
            }
            col += cell_width;
        }
    }
    kinds
}

/// Rows to logical lines. Each head adds its symbol, or " " for an empty
/// one, so columns stay aligned in the text; spacers add nothing. A row
/// joins the next one when `row_continues`.
fn logical_lines(cells: &[CellData], kinds: &[CellKind], size: (u16, u16)) -> Vec<CapturedLine> {
    let (width, height) = (usize::from(size.0), usize::from(size.1));
    let mut lines = Vec::new();
    if width == 0 || height == 0 {
        return lines;
    }
    let mut line = CapturedLine::default();
    for (row, (row_cells, row_kinds)) in cells.chunks(width).zip(kinds.chunks(width)).enumerate() {
        for (col, (cell, kind)) in row_cells.iter().zip(row_kinds).enumerate() {
            let CellKind::Head { width } = *kind else {
                continue;
            };
            line.graphemes.push(CapturedGrapheme {
                byte: line.text.len(),
                pos: GridPos {
                    row: row as u16,
                    col: col as u16,
                },
                width,
            });
            line.text.push_str(if cell.symbol.is_empty() {
                " "
            } else {
                &cell.symbol
            });
        }
        if row + 1 < height && row_continues(row_cells, row_kinds) {
            continue;
        }
        line.trim_end();
        lines.push(std::mem::take(&mut line));
    }
    lines
}

/// The soft-wrap guess. Client cells carry no wrap flag, so a row is taken to
/// continue on the next one when its content reaches the right edge: the last
/// column holds a non-blank head, or the spacer of a non-blank wide head.
///
/// A terminal soft wrap always fills its row, so real wraps are joined, except
/// a wrap forced by a wide character that did not fit, which leaves the last
/// column blank. A false join needs a hard line exactly as wide as the pane
/// followed by text a pattern can glue onto it, and then the user sees one
/// underline across both rows before picking it. The guess applies to both
/// screens alike, so a fullscreen app that hard-breaks a long URL at the last
/// column is joined too. Kept in this one function so a live check can retune
/// it in one edit.
fn row_continues(row: &[CellData], kinds: &[CellKind]) -> bool {
    let Some(last) = kinds.len().checked_sub(1) else {
        return false;
    };
    let head = match kinds.get(last) {
        Some(CellKind::Head { .. }) => Some(last),
        Some(CellKind::Spacer) => kinds[..last]
            .iter()
            .rposition(|kind| matches!(kind, CellKind::Head { .. })),
        None => None,
    };
    head.and_then(|head| row.get(head))
        .is_some_and(|cell| !cell.symbol.trim().is_empty())
}

/// The pane's terminal grid size, `(width, height)` of its inner rect: the
/// unit `FrozenHints::size` and `ClientCopyModeState::geometry` both use.
fn inner_size(pane: &PaneSurfacePane) -> (u16, u16) {
    (pane.inner_rect.width, pane.inner_rect.height)
}

/// A surface pane's scroll metrics as copy mode's `(offset, max offset)`,
/// saturated to `usize` the way compose builds `PaneHit::scroll`.
fn scroll_pair(pane: &PaneSurfacePane) -> Option<(usize, usize)> {
    pane.scroll.map(|scroll| {
        (
            usize::try_from(scroll.offset_from_bottom).unwrap_or(usize::MAX),
            usize::try_from(scroll.max_offset_from_bottom).unwrap_or(usize::MAX),
        )
    })
}

/// Maps one scanned target from byte ranges to the frozen grid. The graphemes
/// a match overlaps become one underline run per row; a range that starts
/// inside a cluster takes the whole cluster. The label sits on the copied
/// text's first cell, moved left to fit the row and then onto a head cell.
fn place_target(
    target: HintTarget,
    lines: &[CapturedLine],
    kinds: &[CellKind],
    size: (u16, u16),
) -> PlacedTarget {
    let label_width = u16::try_from(target.label.chars().count()).unwrap_or(u16::MAX);
    let places = target
        .occurrences
        .iter()
        .filter_map(|occurrence| {
            let line = lines.get(occurrence.line)?;
            let first = line.grapheme_at(occurrence.full.start)?;
            let last = line.grapheme_at(occurrence.full.end.checked_sub(1)?)?;
            let mut underline: Vec<GridRun> = Vec::new();
            for grapheme in line.graphemes.get(first..=last)? {
                let end = grapheme.pos.col.saturating_add(grapheme.width).min(size.0);
                match underline.last_mut() {
                    Some(run) if run.row == grapheme.pos.row => run.end = end,
                    _ => underline.push(GridRun {
                        row: grapheme.pos.row,
                        start: grapheme.pos.col,
                        end,
                    }),
                }
            }
            let anchor = line
                .graphemes
                .get(line.grapheme_at(occurrence.capture.start)?)?
                .pos;
            let mut col = anchor.col.min(size.0.saturating_sub(label_width));
            let row_start = usize::from(anchor.row) * usize::from(size.0);
            while col > 0 && kinds.get(row_start + usize::from(col)) == Some(&CellKind::Spacer) {
                col -= 1;
            }
            Some(HintPlace {
                label_at: GridPos {
                    row: anchor.row,
                    col,
                },
                underline,
            })
        })
        .collect();
    PlacedTarget {
        label: target.label,
        text: target.text,
        places,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-pane surface frame showing `rows`, each padded to `width`
    /// columns.
    fn pane_frame(rows: &[&str], width: u16) -> (PaneSurfacePane, FrameData) {
        let height = rows.len() as u16;
        let buffer = Buffer::with_lines(rows.iter().map(|row| {
            let padding = usize::from(width).saturating_sub(row.width());
            format!("{row}{}", " ".repeat(padding))
        }));
        let frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
        let rect = crate::protocol::SurfaceRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let pane = PaneSurfacePane {
            pane_id: "pane_1".into(),
            content_revision: 0,
            rect,
            inner_rect: rect,
            scrollbar_rect: None,
            scroll: None,
            focused: true,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        };
        (pane, frame)
    }

    fn rules(toml: &str) -> HintRules {
        let config: crate::config::Config = toml::from_str(toml).expect("config parses");
        let (rules, diagnostics) = HintRules::compile(&config.copy_hints);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        rules
    }

    fn capture(rows: &[&str], width: u16) -> FrozenHints {
        let (pane, frame) = pane_frame(rows, width);
        FrozenHints::capture(&pane, &frame, &HintRules::compile(&Default::default()).0)
    }

    fn only_place(frozen: &FrozenHints, text: &str) -> HintPlace {
        let target = frozen
            .targets
            .iter()
            .find(|target| target.text == text)
            .unwrap_or_else(|| panic!("no target {text:?} in {:?}", frozen.targets));
        assert_eq!(target.places.len(), 1);
        target.places[0].clone()
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> crate::input::TerminalKey {
        crate::input::TerminalKey::new(code, modifiers)
    }

    fn frozen_hints(frozen: FrozenHints) -> ClientCopyHints {
        ClientCopyHints {
            entry: HintEntry::Direct,
            view: HintView::Frozen(frozen),
        }
    }

    #[test]
    fn wide_cells_map_to_their_columns_and_spacers_become_blank() {
        // The path starts at column 9, after one narrow and three wide
        // graphemes.
        let frozen = capture(&["λ 日本語 /tmp/a"], 20);

        let place = only_place(&frozen, "/tmp/a");
        assert_eq!(place.label_at, GridPos { row: 0, col: 9 });
        assert_eq!(
            place.underline,
            [GridRun {
                row: 0,
                start: 9,
                end: 15
            }]
        );
        for spacer in [3, 5, 7] {
            assert_eq!(frozen.cells[spacer].symbol, " ", "spacer at {spacer}");
        }
    }

    #[test]
    fn halfwidth_sound_marks_count_as_two_columns() {
        // In the layout the server sends, the terminal draws the voiced mark
        // two columns wide, so the head is followed by an empty spacer cell.
        let (pane, mut frame) = pane_frame(&["xx /tmp/a"], 12);
        frame.cells[0].symbol = "ｶﾞ".into();
        frame.cells[1].symbol = String::new();

        let frozen =
            FrozenHints::capture(&pane, &frame, &HintRules::compile(&Default::default()).0);

        assert_eq!(only_place(&frozen, "/tmp/a").label_at.col, 3);
        assert_eq!(
            frozen.cells[1].symbol, " ",
            "the empty spacer reads as blank"
        );
    }

    #[test]
    fn a_url_filling_the_row_joins_the_next_row() {
        let frozen = capture(&["see https://ex.io/ab", "cd/ef done"], 20);

        let place = only_place(&frozen, "https://ex.io/abcd/ef");
        assert_eq!(place.label_at, GridPos { row: 0, col: 4 });
        assert_eq!(
            place.underline,
            [
                GridRun {
                    row: 0,
                    start: 4,
                    end: 20
                },
                GridRun {
                    row: 1,
                    start: 0,
                    end: 5
                },
            ]
        );
    }

    #[test]
    fn a_row_ending_in_a_blank_does_not_join() {
        let frozen = capture(&["see https://ex.io/a ", "cd/ef done"], 20);

        let texts = frozen
            .targets
            .iter()
            .map(|target| target.text.as_str())
            .collect::<Vec<_>>();
        assert!(texts.contains(&"https://ex.io/a"), "{texts:?}");
        assert!(texts.contains(&"cd/ef"), "{texts:?}");
    }

    #[test]
    fn trailing_padding_is_not_copied() {
        let frozen = capture(&["modified:   src/lib.rs"], 40);

        assert_eq!(
            frozen
                .targets
                .iter()
                .map(|target| target.text.as_str())
                .collect::<Vec<_>>(),
            ["src/lib.rs"]
        );
    }

    #[test]
    fn labels_at_the_right_edge_shift_left_onto_a_head_cell() {
        // With keys "ab", z gets "a" and x and y get two-key labels. A target
        // in the last column fills its row, so a blank row follows each one.
        let hint_rules = rules(
            "[copy_hints]\nalphabet = \"ab\"\nenabled_builtin_patterns = []\n\n[[copy_hints.patterns]]\nregex = '[xyz]'\n",
        );
        let (pane, frame) = pane_frame(&["  日x", "", "    y", "", "z"], 5);

        let frozen = FrozenHints::capture(&pane, &frame, &hint_rules);

        assert_eq!(
            only_place(&frozen, "x").label_at,
            GridPos { row: 0, col: 2 },
            "back past the spacer onto the wide head"
        );
        assert_eq!(
            only_place(&frozen, "y").label_at,
            GridPos { row: 2, col: 3 },
            "moved left to fit the row"
        );
        assert_eq!(
            only_place(&frozen, "z").label_at,
            GridPos { row: 4, col: 0 }
        );
    }

    #[test]
    fn hyperlink_ids_are_dropped_from_the_capture() {
        let (pane, mut frame) = pane_frame(&["link /tmp/a"], 12);
        frame.hyperlinks.push("https://herdr.dev".into());
        for cell in &mut frame.cells {
            cell.hyperlink = Some(0);
        }

        let frozen =
            FrozenHints::capture(&pane, &frame, &HintRules::compile(&Default::default()).0);

        assert!(frozen.cells.iter().all(|cell| cell.hyperlink.is_none()));
    }

    #[test]
    fn typing_narrows_backspace_pops_esc_clears_then_cancels() {
        let (pane, frame) = pane_frame(&["/x/1", "/x/2", "/x/3"], 8);
        let frozen =
            FrozenHints::capture(&pane, &frame, &rules("[copy_hints]\nalphabet = \"ab\"\n"));
        let labels = frozen
            .targets
            .iter()
            .map(|target| target.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, ["a", "ba", "bb"]);
        let mut hints = frozen_hints(frozen);

        let b = key(KeyCode::Char('b'), KeyModifiers::empty());
        assert_eq!(hints.press(&b), HintPress::Consumed { repaint: true });
        assert_eq!(hints.frozen().expect("frozen").typed, "b");
        assert_eq!(
            hints.press(&key(KeyCode::Backspace, KeyModifiers::empty())),
            HintPress::Consumed { repaint: true }
        );
        assert_eq!(hints.frozen().expect("frozen").typed, "");
        hints.press(&b);
        let esc = key(KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(hints.press(&esc), HintPress::Consumed { repaint: true });
        assert_eq!(hints.frozen().expect("frozen").typed, "");
        assert_eq!(hints.press(&esc), HintPress::Cancelled);

        hints.press(&b);
        assert_eq!(
            hints.press(&key(KeyCode::Char('a'), KeyModifiers::empty())),
            HintPress::Chosen("/x/2".into())
        );
    }

    #[test]
    fn an_unknown_prefix_resets_and_shift_types_the_plain_key() {
        let (pane, frame) = pane_frame(&["/x/1", "/x/2", "/x/3"], 8);
        let frozen =
            FrozenHints::capture(&pane, &frame, &rules("[copy_hints]\nalphabet = \"ab\"\n"));
        let mut hints = frozen_hints(frozen);

        hints.press(&key(KeyCode::Char('b'), KeyModifiers::empty()));
        assert_eq!(
            hints.press(&key(KeyCode::Char('z'), KeyModifiers::empty())),
            HintPress::Consumed { repaint: true }
        );
        assert_eq!(hints.frozen().expect("frozen").typed, "", "bz is no label");
        assert_eq!(
            hints.press(&key(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            HintPress::Chosen("/x/3".into())
        );
    }

    #[test]
    fn chords_and_repeats_type_nothing_and_ctrl_c_cancels() {
        let (pane, frame) = pane_frame(&["/x/1"], 8);
        let mut hints = frozen_hints(FrozenHints::capture(
            &pane,
            &frame,
            &HintRules::compile(&Default::default()).0,
        ));
        let repeat = key(KeyCode::Char('a'), KeyModifiers::empty()).with_kind(KeyEventKind::Repeat);

        assert_eq!(hints.press(&repeat), HintPress::Consumed { repaint: false });
        assert_eq!(
            hints.press(&key(KeyCode::Char('a'), KeyModifiers::ALT)),
            HintPress::Consumed { repaint: false }
        );
        assert_eq!(
            hints.press(&key(KeyCode::Enter, KeyModifiers::empty())),
            HintPress::Consumed { repaint: false }
        );
        assert_eq!(
            hints.press(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            HintPress::Cancelled
        );
    }

    #[test]
    fn waiting_hints_only_answer_esc_and_ctrl_c() {
        let mut hints = ClientCopyHints {
            entry: HintEntry::FromCopyMode,
            view: HintView::Waiting,
        };

        assert_eq!(
            hints.press(&key(KeyCode::Char('a'), KeyModifiers::empty())),
            HintPress::Consumed { repaint: false }
        );
        assert_eq!(
            hints.press(&key(KeyCode::Esc, KeyModifiers::empty())),
            HintPress::Cancelled
        );
        assert_eq!(
            hints.press(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            HintPress::Cancelled
        );
    }
}
