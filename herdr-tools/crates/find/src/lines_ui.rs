//! Pure rendering of a line picker, in the pane navigator's visual language
//! (palette, query line, highlights, path elision and hints come from `ui`).
//!
//! Top to bottom: heading (title, mark count, source and item count), query,
//! rule, rows, the exact text Enter delivers, notice, key hints. Path rows dim
//! their parent directories so the leaf, the part people recognize, stands
//! out. Marks look like the navigator's kill marks (`■` in the gutter, a bold
//! name, `N marked, M hidden` in the heading), in the accent color rather than
//! red, which this UI reserves for destruction.
//!
//! A grouped popup adds the navigator's reach-strip look after the title:
//! `Paths   Dirs 477   Files 828 ……… zoxide`, the list on screen in a green
//! pill, and `ctrl-o files` in the hints. The counts move into the pills, so
//! the right side names only the shown list's source.
use crate::{
    deliver,
    lines::{LineHit, LinePicker},
    pickers::{Deliver, Kind, LineSource, Lists},
    ui::{self, Elided, ACCENT, MUTED},
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

/// What a line picker draws besides its state: the specs' static facts, one
/// source per list in the state's list order.
pub(crate) struct Chrome<'a> {
    pub lists: &'a Lists,
}

impl Chrome<'_> {
    /// The source of the list on screen.
    fn source(&self, state: &LinePicker) -> &LineSource {
        &self.lists.sources[state.active().min(self.lists.sources.len() - 1)]
    }

    /// A group's members agree on kind, enter and multi, so any source
    /// answers for the popup.
    fn shared(&self) -> &LineSource {
        &self.lists.sources[0]
    }

    fn grouped(&self) -> bool {
        self.lists.sources.len() > 1
    }

    fn noun(&self) -> &'static str {
        match self.shared().kind {
            Kind::Path => "paths",
            Kind::Text => "items",
        }
    }

    /// `paste + copy`, in the order Enter performs them.
    fn enter_label(&self) -> String {
        self.shared()
            .enter
            .iter()
            .map(|effect| match effect {
                Deliver::Paste => "paste",
                Deliver::Copy => "copy",
            })
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

pub(crate) fn render(state: &LinePicker, chrome: &Chrome<'_>, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.width < 6 || area.height < 5 {
        return;
    }
    let inner = Rect::new(area.x + 1, area.y, area.width - 2, area.height);
    let message_lines = state
        .message
        .as_deref()
        .map(|message| ui::wrap_words(message, inner.width as usize))
        .unwrap_or_default();
    let [heading, query, divider, body, details, notice, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(message_lines.len().clamp(1, 3) as u16),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(heading_line(state, chrome, heading.width)),
        heading,
    );
    let list = state.list();
    let count = (!state.query.trim().is_empty())
        .then(|| format!("{}/{}", list.hits.len(), list.items.len()));
    let placeholder = match chrome.shared().kind {
        Kind::Path => "Type part of a path…",
        Kind::Text => "Type to filter…",
    };
    ui::query_line(frame, query, &state.query, placeholder, count, ACCENT);
    frame.render_widget(
        Block::new()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(MUTED)),
        divider,
    );
    render_rows(state, chrome, frame, body);
    let chosen = state.chosen();
    if !chosen.is_empty() {
        // Exactly what Enter types (quoted if a shell needs it), or copies.
        let text = if chrome.shared().enter.contains(&Deliver::Paste) {
            deliver::typed_all(&chosen, state.kind())
        } else {
            deliver::copied(&chosen, state.kind())
        };
        let muted = Style::default().fg(MUTED);
        // One path elides its middle and keeps the leaf; several read left to
        // right in delivery order and lose their end instead.
        let line = match (state.kind(), chosen.len()) {
            (Kind::Path, 1) => Line::from(
                Elided::path(&text, &[], details.width as usize).styled_spans(&[], |_| muted),
            ),
            _ => ui::fitted(&text, &[], muted, details.width),
        };
        frame.render_widget(Paragraph::new(line), details);
    }
    if !message_lines.is_empty() {
        let lines: Vec<Line> = message_lines.into_iter().map(Line::from).collect();
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(ACCENT)),
            notice,
        );
    }
    frame.render_widget(
        Paragraph::new(hint_line(state, chrome, footer.width as usize)),
        footer,
    );
}

/// Key hints, widest first; narrower ones drop whole `key action` pairs or
/// shorten `ctrl-` to `^`. In a group `ctrl-o <next list>` sits right after
/// `enter`, as in the navigator, so it outlasts the other keys. The set is
/// chosen with the widest list name so ctrl-o never changes which one shows.
fn hint_line(state: &LinePicker, chrome: &Chrome<'_>, width: usize) -> Line<'static> {
    let enter = chrome.enter_label();
    let multi = chrome.shared().multi;
    let (tab_wide, tab) = if multi {
        ("tab mark   ", "tab mark  ")
    } else {
        ("", "")
    };
    let hints = |next: &str| {
        let (o_wide, o) = if next.is_empty() {
            (String::new(), String::new())
        } else {
            (format!("   ctrl-o {next}"), format!("  ^o {next}"))
        };
        let mut hints = vec![
            format!("↑↓ select   {tab_wide}enter {enter}{o_wide}   ctrl-r reload   esc close"),
            format!("↑↓ select  {tab}enter {enter}{o}  ^r reload  esc close"),
            format!("{tab}enter {enter}{o}  esc close"),
            format!("enter {enter}{o}  esc close"),
        ];
        if !next.is_empty() {
            hints.push(format!("enter {enter}  ^o {next}  esc"));
            hints.push(format!("enter  ^o {next}  esc"));
        }
        hints.push(
            if multi {
                "tab  enter  esc"
            } else {
                "enter  esc"
            }
            .to_owned(),
        );
        hints
    };
    let next = if chrome.grouped() {
        let sources = &chrome.lists.sources;
        sources[(state.active() + 1) % sources.len()]
            .label
            .to_lowercase()
    } else {
        String::new()
    };
    let widest = chrome
        .lists
        .sources
        .iter()
        .map(|source| source.label.width())
        .max()
        .filter(|_| chrome.grouped())
        .unwrap_or(0);
    let judged = hints(&" ".repeat(widest));
    let shown = hints(&next);
    judged
        .iter()
        .zip(shown)
        .find(|(judged, _)| judged.width() <= width)
        .map(|(_, hint)| Line::styled(hint, Style::default().fg(MUTED)))
        .unwrap_or_default()
}

/// `Paths · 2 marked, 1 hidden ……… zoxide · 477`; the item count waits for
/// the first load. Narrow popups drop the hidden count, then the origin, and
/// keep the mark count as long as it fits. A group puts its list strip after
/// the title (`Paths · 2 marked   Dirs 477   Files 828 ……… zoxide`) and sheds
/// the inactive lists last, before the marks.
fn heading_line(state: &LinePicker, chrome: &Chrome<'_>, width: u16) -> Line<'static> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(MUTED);
    let title = vec![Span::styled(chrome.lists.title.clone(), bold)];
    let marked = state.marks().len();
    let hidden = state.hidden_marks();
    // Mark-count variants, richest first.
    let mut marks: Vec<Vec<Span<'static>>> = Vec::new();
    if marked > 0 {
        let count = vec![
            Span::raw(" · "),
            Span::styled(format!("{marked} marked"), bold),
        ];
        if hidden > 0 {
            let mut extra = count.clone();
            extra.push(Span::styled(format!(", {hidden} hidden"), muted));
            marks.push(extra);
        }
        marks.push(count);
    }
    // Strip forms, richest first: every list, then the shown one alone.
    let strips: Vec<Vec<Span<'static>>> = if chrome.grouped() {
        vec![
            strip_spans(state, chrome, false),
            strip_spans(state, chrome, true),
        ]
    } else {
        vec![Vec::new()]
    };
    let source = chrome.source(state);
    let list = state.list();
    let origin = if chrome.grouped() {
        source.origin.clone()
    } else if list.loading && list.items.is_empty() {
        format!("{}…", source.origin)
    } else if list.error.is_some() {
        source.origin.clone()
    } else {
        format!("{} · {}", source.origin, list.items.len())
    };
    // Judge the origin by the widest a group can show, so ctrl-o never
    // decides whether it fits.
    let origin_room = chrome
        .lists
        .sources
        .iter()
        .map(|source| source.origin.width())
        .chain([origin.width()])
        .max()
        .unwrap_or(0);
    let width = width as usize;
    let used = |spans: &[Span]| spans.iter().map(Span::width).sum::<usize>();
    let left = |marks: &[Span<'static>], strip: &[Span<'static>]| {
        let mut spans = title.clone();
        spans.extend(marks.iter().cloned());
        if !strip.is_empty() {
            spans.push(Span::raw("  "));
            spans.extend(strip.iter().cloned());
        }
        spans
    };
    // Richest first. Within a strip form every mark form is tried with the
    // origin before any without it, as an ungrouped heading always has; then
    // the inactive lists go, then the marks, and the title with the shown
    // list's pill stays longest.
    let none: &[Span<'static>] = &[];
    let marked_forms: Vec<&[Span<'static>]> = if marks.is_empty() {
        vec![none]
    } else {
        marks.iter().map(Vec::as_slice).collect()
    };
    let mut variants = Vec::new();
    for strip in &strips {
        for with_origin in [true, false] {
            for marks in &marked_forms {
                variants.push((left(marks, strip), with_origin));
            }
        }
    }
    let last = strips.last().map_or(none, Vec::as_slice);
    variants.extend([(left(none, last), true), (left(none, last), false)]);
    let (mut spans, with_origin) = variants
        .into_iter()
        .find(|(spans, with_origin)| {
            used(spans) + if *with_origin { origin_room + 1 } else { 0 } <= width
        })
        .unwrap_or_else(|| (title.clone(), false));
    if with_origin {
        let gap = width.saturating_sub(used(&spans) + origin.width());
        spans.extend([Span::raw(" ".repeat(gap)), Span::styled(origin, muted)]);
    }
    Line::from(spans)
}

/// One padded segment per list (` Dirs 477 `), the shown one in the green
/// pill the navigator's reach strip uses. Padding every segment keeps their
/// positions fixed, so ctrl-o visibly moves the pill. A list still loading
/// counts `…`, one that failed `!`.
fn strip_spans(state: &LinePicker, chrome: &Chrome<'_>, only_active: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, (list, source)) in state.lists().iter().zip(&chrome.lists.sources).enumerate() {
        let active = index == state.active();
        if only_active && !active {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        let count = if list.loading && list.items.is_empty() {
            "…".to_owned()
        } else if list.error.is_some() {
            "!".to_owned()
        } else {
            list.items.len().to_string()
        };
        let style = if active {
            Style::default()
                .bg(Color::Green)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };
        spans.push(Span::styled(format!(" {} {count} ", source.label), style));
    }
    spans
}

/// Prose where the list would be, wrapped between words so a narrow popup
/// shows the whole sentence instead of clipping it.
fn body_text(frame: &mut Frame<'_>, area: Rect, text: &str, color: Color) {
    let lines: Vec<Line> = ui::wrap_words(text, area.width as usize)
        .into_iter()
        .map(Line::from)
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(color)),
        area,
    );
}

/// The shown list's rows, or why there are none, in that list's own words
/// (each list in a group has its own empty message and source).
fn render_rows(state: &LinePicker, chrome: &Chrome<'_>, frame: &mut Frame<'_>, area: Rect) {
    let list = state.list();
    let source = chrome.source(state);
    if let Some(error) = list.error.as_deref().filter(|_| !list.loading) {
        // The list is empty because the source failed; say why where the
        // list would be, so typing (which clears notices) never hides it.
        body_text(frame, area, error, ACCENT);
        return;
    }
    if list.hits.is_empty() {
        let message = if list.loading {
            format!("Running {}…", source.origin)
        } else if list.items.is_empty() {
            source
                .empty
                .clone()
                .unwrap_or_else(|| format!("{} printed nothing", source.origin))
        } else {
            format!("No matching {}", chrome.noun())
        };
        body_text(frame, area, &message, MUTED);
        return;
    }
    let visible = area.height.max(1) as usize;
    let offset = list.selected.saturating_sub(visible - 1);
    for (i, hit) in list.hits.iter().enumerate().skip(offset).take(visible) {
        let rect = Rect::new(area.x, area.y + (i - offset) as u16, area.width, 1);
        render_row(state, hit, i == list.selected, frame, rect);
    }
}

/// `› ~/code/side-projects/herdr-jump`: a cursor gutter, then the item. On
/// the selection bar the parent turns light gray, since dark gray on the
/// dark-gray bar would vanish. A marked row shows `■` instead (the bar still
/// shows where the cursor is) and a bold name, as in the navigator's kill mode.
fn render_row(
    state: &LinePicker,
    hit: &LineHit,
    selected: bool,
    frame: &mut Frame<'_>,
    rect: Rect,
) {
    let item = &state.list().items[hit.index];
    let marked = state.is_marked(item);
    let base = if selected {
        Style::default().bg(MUTED).fg(Color::White)
    } else {
        Style::default()
    };
    frame.render_widget(Block::new().style(base), rect);
    let gutter = if marked {
        Span::styled("■ ", base.fg(ACCENT).add_modifier(Modifier::BOLD))
    } else if selected {
        Span::styled("› ", base.fg(ACCENT))
    } else {
        Span::styled("  ", base)
    };
    let room = rect.width.saturating_sub(2) as usize;
    let parent = base.fg(if selected { Color::Gray } else { MUTED });
    let name = if marked {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    };
    let mut spans = vec![gutter];
    match state.kind() {
        Kind::Path => {
            let elided = Elided::path(&item.display, &hit.indices, room);
            spans.extend(elided.styled_spans(&hit.indices, |i| {
                if i < item.leaf {
                    parent
                } else {
                    name
                }
            }));
        }
        Kind::Text => {
            spans.extend(ui::fitted(&item.display, &hit.indices, name, room as u16).spans)
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lines::{Batch, Item};
    use crate::pickers;
    use ratatui::{
        backend::TestBackend,
        buffer::Buffer,
        crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
        Terminal,
    };
    use std::path::Path;

    /// The paths list alone, as an ungrouped picker would show it.
    fn paths_alone() -> Lists {
        let mut lists = pickers::lists("paths").unwrap();
        lists.sources.truncate(1);
        lists
    }

    fn state(lines: &[&str]) -> LinePicker {
        let mut state = LinePicker::new(Kind::Path, vec![Deliver::Paste, Deliver::Copy], true);
        let items = lines
            .iter()
            .map(|l| Item::new(l, Kind::Path, Some("/Users/me"), Path::new("/")))
            .collect();
        state.loaded(
            0,
            Ok(Batch {
                items,
                ..Batch::default()
            }),
        );
        state
    }

    fn draw(state: &LinePicker, width: u16, height: u16) -> Buffer {
        draw_with(state, &paths_alone(), width, height)
    }

    fn draw_with(state: &LinePicker, lists: &Lists, width: u16, height: u16) -> Buffer {
        let chrome = Chrome { lists };
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(state, &chrome, frame))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// Cells on row `y` whose foreground is `color`, joined.
    fn colored(buffer: &Buffer, y: u16, color: Color) -> String {
        (0..buffer.area.width)
            .filter(|&x| buffer[(x, y)].fg == color)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn paths_popup_shows_origin_rows_typed_text_and_hints() {
        let state = state(&[
            "/Users/me/code/herdr-jump",
            "/Users/me/My Drive",
            "/Users/me/.config/nvim",
        ]);
        let screen = rows(&draw(&state, 80, 12));
        assert!(screen[0].starts_with(" Paths"), "{screen:#?}");
        assert!(screen[0].ends_with("zoxide · 3"), "{screen:#?}");
        assert_eq!(screen[1], " › Type part of a path…");
        assert_eq!(screen[3], " › ~/code/herdr-jump");
        assert_eq!(screen[4], "   ~/My Drive");
        assert_eq!(screen[9], " /Users/me/code/herdr-jump");
        assert_eq!(
            screen[11],
            " ↑↓ select   tab mark   enter paste + copy   ctrl-r reload   esc close"
        );
    }

    fn tab(state: &mut LinePicker) {
        state.handle(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    }

    /// Marked rows carry `■` (also under the cursor) and a bold name; the
    /// heading counts marks and those the query hides; the line above the
    /// hints is every mark, in order, exactly as Enter types it.
    #[test]
    fn marks_show_in_the_gutter_the_heading_and_the_typed_text() {
        let mut state = state(&[
            "/Users/me/code/herdr-jump",
            "/Users/me/My Drive",
            "/Users/me/.config/nvim",
        ]);
        tab(&mut state);
        tab(&mut state);
        let buffer = draw(&state, 80, 12);
        let screen = rows(&buffer);
        assert!(screen[0].starts_with(" Paths · 2 marked"), "{screen:#?}");
        assert!(screen[0].ends_with("zoxide · 3"), "{screen:#?}");
        assert_eq!(screen[3], " ■ ~/code/herdr-jump");
        assert_eq!(screen[4], " ■ ~/My Drive");
        assert_eq!(screen[5], " › ~/.config/nvim");
        assert_eq!(buffer[(1, 3)].fg, ACCENT);
        assert!(buffer[(1, 3)].modifier.contains(Modifier::BOLD));
        // The leaf is bold, the dim parent is not.
        assert!(buffer[(10, 3)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(4, 3)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(10, 5)].modifier.contains(Modifier::BOLD));
        assert_eq!(screen[9], " /Users/me/code/herdr-jump '/Users/me/My Drive'");
        state.handle(Event::Paste("nvim".into()));
        let buffer = draw(&state, 80, 12);
        let screen = rows(&buffer);
        assert!(
            screen[0].starts_with(" Paths · 2 marked, 2 hidden"),
            "{screen:#?}"
        );
        // A marked row under the cursor keeps its mark; the bar shows the cursor.
        tab(&mut state);
        let buffer = draw(&state, 80, 12);
        assert_eq!(rows(&buffer)[3], " ■ ~/.config/nvim");
        assert_eq!(buffer[(1, 3)].bg, MUTED);
        // Narrow popups shed the hidden count, then the origin, never the marks.
        let screen = rows(&draw(&state, 30, 12));
        assert_eq!(screen[0], " Paths · 3 marked  zoxide · 3");
        assert_eq!(screen[9], " /Users/me/code/herdr-jump '…");
        assert_eq!(rows(&draw(&state, 22, 12))[0], " Paths · 3 marked");
    }

    #[test]
    fn pickers_without_multi_do_not_offer_tab() {
        let mut single = paths_alone();
        single.sources[0].multi = false;
        let state = state(&["/a"]);
        let screen = rows(&draw_with(&state, &single, 80, 12));
        assert_eq!(
            screen[11],
            " ↑↓ select   enter paste + copy   ctrl-r reload   esc close"
        );
    }

    #[test]
    fn query_highlights_matches_dims_parents_and_shows_the_quoted_text() {
        let mut state = state(&["/Users/me/code/herdr-jump", "/Users/me/My Drive"]);
        state.handle(Event::Paste("drive".into()));
        let buffer = draw(&state, 80, 12);
        let screen = rows(&buffer);
        assert!(screen[1].ends_with("1/2"), "{screen:#?}");
        assert_eq!(screen[3], " › ~/My Drive");
        assert_eq!(colored(&buffer, 3, ACCENT), "› Drive");
        assert_eq!(colored(&buffer, 3, Color::Gray), "~/");
        // What Enter types: quoted, because a shell would split the space.
        assert_eq!(screen[9], " '/Users/me/My Drive'");
        let mut state = self::state(&["/Users/me/code/herdr-jump"]);
        state.handle(Event::Paste("jump".into()));
        let buffer = draw(&state, 80, 12);
        assert_eq!(colored(&buffer, 3, Color::Gray), "~/code/");
    }

    #[test]
    fn long_paths_keep_their_match_and_leaf_at_narrow_widths() {
        let mut state = state(&[
            "/Users/me/Documents/notes-os/STARTUP/codiing-pivot/outreach-loop/ops/.wt/feat/cld-messaging-3",
        ]);
        state.handle(Event::Paste("pivot".into()));
        let screen = rows(&draw(&state, 50, 12));
        assert_eq!(
            screen[3],
            " › …/codiing-pivot/…/ops/.wt/feat/cld-messaging-3"
        );
        let screen = rows(&draw(&state, 44, 12));
        assert_eq!(screen[3], " › …/codiing-pivot/…/feat/cld-messaging-3");
        let screen = rows(&draw(&state, 40, 12));
        assert!(screen[11].starts_with(" enter paste + copy"), "{screen:#?}");
    }

    #[test]
    fn loading_empty_failed_and_unmatched_states_explain_themselves() {
        let loading = LinePicker::new(Kind::Path, vec![Deliver::Paste, Deliver::Copy], true);
        let screen = rows(&draw(&loading, 80, 12));
        assert!(screen[0].ends_with("zoxide…"));
        assert_eq!(screen[3], " Running zoxide…");
        let empty = state(&[]);
        assert_eq!(
            rows(&draw(&empty, 80, 12))[3],
            " zoxide has no directories yet. cd around and they appear here."
        );
        let narrow = rows(&draw(&empty, 40, 12));
        assert_eq!(
            narrow[3..6],
            [
                " zoxide has no directories yet. cd",
                " around and they appear here.",
                ""
            ]
        );
        let mut failed = state(&["/a"]);
        failed.loaded(
            0,
            Err("zoxide isn't installed or isn't on Herdr's PATH".into()),
        );
        failed.handle(Event::Paste("typing keeps the reason".into()));
        let screen = rows(&draw(&failed, 80, 12));
        assert!(screen[0].ends_with("zoxide"));
        assert_eq!(
            screen[3],
            " zoxide isn't installed or isn't on Herdr's PATH · ctrl-r retries"
        );
        assert_eq!(screen[10], "");
        let mut unmatched = state(&["/a"]);
        unmatched.handle(Event::Paste("zzz".into()));
        assert_eq!(rows(&draw(&unmatched, 80, 12))[3], " No matching paths");
    }

    fn items(lines: &[&str]) -> Result<Batch, String> {
        Ok(Batch {
            items: lines
                .iter()
                .map(|l| Item::new(l, Kind::Path, Some("/Users/me"), Path::new("/")))
                .collect(),
            ..Batch::default()
        })
    }

    fn ctrl_o(state: &mut LinePicker) {
        state.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        )));
    }

    /// The real alt+n popup (paths + recent-files), opened on `opened`.
    fn grouped(opened: usize) -> (LinePicker, Lists) {
        let lists = pickers::lists(["paths", "recent-files"][opened]).unwrap();
        let state = LinePicker::with_lists(
            Kind::Path,
            vec![Deliver::Paste, Deliver::Copy],
            true,
            lists.sources.len(),
            lists.opened,
        );
        (state, lists)
    }

    /// Cells on row `y` with a green background (the shown list's pill), joined.
    fn pill(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .filter(|&x| buffer[(x, y)].bg == Color::Green)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn the_heading_counts_both_lists_and_pills_the_one_on_screen() {
        let (mut state, lists) = grouped(0);
        // Before either list loads, both count `…`.
        let screen = rows(&draw_with(&state, &lists, 80, 12));
        assert!(
            screen[0].starts_with(" Paths   Dirs …   Files … "),
            "{screen:#?}"
        );
        assert!(screen[0].ends_with(" zoxide"), "{screen:#?}");
        assert_eq!(screen[3], " Running zoxide…");
        // zoxide lands first; recent-files is still running.
        state.loaded(0, items(&["/Users/me/code/app", "/Users/me/notes"]));
        let buffer = draw_with(&state, &lists, 80, 12);
        let screen = rows(&buffer);
        assert!(
            screen[0].starts_with(" Paths   Dirs 2   Files … "),
            "{screen:#?}"
        );
        assert_eq!(pill(&buffer, 0), " Dirs 2 ");
        assert_eq!(screen[3], " › ~/code/app");
        let files_at = screen[0].find("Files");
        state.loaded(
            1,
            items(&["/Users/me/code/app/main.rs", "/Users/me/a", "/Users/me/b"]),
        );
        state.handle(Event::Paste("app".into()));
        ctrl_o(&mut state);
        let buffer = draw_with(&state, &lists, 80, 12);
        let screen = rows(&buffer);
        // Same segment positions; the pill moved and the origin follows.
        assert!(
            screen[0].starts_with(" Paths   Dirs 2   Files 3 "),
            "{screen:#?}"
        );
        assert_eq!(screen[0].find("Files"), files_at);
        assert!(screen[0].ends_with(" recent-files"), "{screen:#?}");
        assert_eq!(pill(&buffer, 0), " Files 3 ");
        assert!(screen[1].starts_with(" › app") && screen[1].ends_with(" 1/3"));
        assert_eq!(screen[3], " › ~/code/app/main.rs");
        ctrl_o(&mut state);
        let buffer = draw_with(&state, &lists, 80, 12);
        assert_eq!(pill(&buffer, 0), " Dirs 2 ");
        assert_eq!(rows(&buffer)[3], " › ~/code/app");
    }

    #[test]
    fn opened_on_files_the_heading_and_hints_say_so() {
        let (mut state, lists) = grouped(1);
        state.loaded(1, items(&["/Users/me/a.rs"]));
        let buffer = draw_with(&state, &lists, 100, 12);
        let screen = rows(&buffer);
        assert!(
            screen[0].starts_with(" Paths   Dirs …   Files 1 "),
            "{screen:#?}"
        );
        assert!(screen[0].ends_with("recent-files"), "{screen:#?}");
        assert_eq!(pill(&buffer, 0), " Files 1 ");
        assert_eq!(
            screen[11],
            " ↑↓ select   tab mark   enter paste + copy   ctrl-o dirs   ctrl-r reload   esc close"
        );
        ctrl_o(&mut state);
        let screen = rows(&draw_with(&state, &lists, 100, 12));
        assert_eq!(
            screen[11],
            " ↑↓ select   tab mark   enter paste + copy   ctrl-o files   ctrl-r reload   esc close"
        );
    }

    /// ctrl-o flips the hint's word, never which hint set fits: the set is
    /// judged with the longer list name.
    #[test]
    fn narrow_hints_keep_ctrl_o_and_do_not_reflow_on_switch() {
        let (mut state, lists) = grouped(0);
        state.loaded(0, items(&["/a"]));
        state.loaded(1, items(&["/b"]));
        let hint =
            |state: &LinePicker, width| rows(&draw_with(state, &lists, width, 12))[11].clone();
        // The popup's inner width is two columns narrower.
        assert_eq!(
            hint(&state, 73),
            " ↑↓ select  tab mark  enter paste + copy  ^o files  ^r reload  esc close"
        );
        assert_eq!(
            hint(&state, 51),
            " tab mark  enter paste + copy  ^o files  esc close"
        );
        assert_eq!(hint(&state, 35), " enter paste + copy  ^o files  esc");
        assert_eq!(hint(&state, 22), " enter  ^o files  esc");
        // One column short for `files`: the next set, for `dirs` too, though
        // the wider set would fit `dirs`.
        assert_eq!(hint(&state, 50), " enter paste + copy  ^o files  esc close");
        ctrl_o(&mut state);
        assert_eq!(hint(&state, 50), " enter paste + copy  ^o dirs  esc close");
    }

    #[test]
    fn each_list_shows_its_own_empty_and_failed_state() {
        let (mut state, lists) = grouped(0);
        state.loaded(0, items(&[]));
        state.loaded(1, Err("recent-files failed (exit 3): boom".into()));
        let buffer = draw_with(&state, &lists, 80, 12);
        let screen = rows(&buffer);
        assert!(
            screen[0].starts_with(" Paths   Dirs 0   Files ! "),
            "{screen:#?}"
        );
        assert_eq!(
            screen[3],
            " zoxide has no directories yet. cd around and they appear here."
        );
        ctrl_o(&mut state);
        let screen = rows(&draw_with(&state, &lists, 80, 12));
        assert_eq!(
            screen[3],
            " recent-files failed (exit 3): boom · ctrl-r retries"
        );
        state.loaded(1, items(&[]));
        let screen = rows(&draw_with(&state, &lists, 100, 12));
        assert_eq!(
            screen[3],
            " No recent files yet. nvim and m_path add them when the recent-files CLI is installed."
        );
        state.loaded(1, items(&["/Users/me/a"]));
        state.handle(Event::Paste("zzz".into()));
        assert_eq!(
            rows(&draw_with(&state, &lists, 80, 12))[3],
            " No matching paths"
        );
    }

    /// As in an ungrouped heading, the hidden count gives way to the origin
    /// and then returns once the origin is gone; the other list goes next,
    /// then the marks, and the shown list's pill stays with the title.
    #[test]
    fn a_narrow_grouped_heading_sheds_in_order() {
        let (mut state, lists) = grouped(0);
        state.loaded(0, items(&["/Users/me/a", "/Users/me/b"]));
        state.loaded(1, items(&["/Users/me/c"]));
        state.handle(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        state.handle(Event::Paste("b".into()));
        let heading = |width| rows(&draw_with(&state, &lists, width, 12))[0].clone();
        assert!(heading(80).starts_with(" Paths · 1 marked, 1 hidden   Dirs 2   Files 1 "));
        assert!(heading(80).ends_with(" zoxide"));
        assert!(heading(60).starts_with(" Paths · 1 marked   Dirs 2   Files 1 "));
        assert!(heading(60).ends_with(" zoxide"));
        assert_eq!(
            heading(48),
            " Paths · 1 marked, 1 hidden   Dirs 2   Files 1"
        );
        assert_eq!(heading(40), " Paths · 1 marked   Dirs 2   Files 1");
        assert_eq!(heading(30), " Paths · 1 marked   Dirs 2");
        assert_eq!(heading(20), " Paths   Dirs 2");
        assert_eq!(heading(10), " Paths");
    }
}
