//! Pure rendering of a line picker, in the pane navigator's visual language
//! (palette, query line, highlights, path elision and hints come from `ui`).
//!
//! Top to bottom: heading (title, mark count, source and item count), query,
//! rule, rows, the exact text Enter delivers, notice, key hints. Path rows dim
//! their parent directories so the leaf, the part people recognize, stands
//! out. Marks look like the navigator's kill marks (`■` in the gutter, a bold
//! name, `N marked, M hidden` in the heading), in the accent color rather than
//! red, which this UI reserves for destruction.
use crate::{
    deliver,
    lines::{LineHit, LinePicker},
    pickers::{Deliver, Kind, LineSource, Picker},
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

/// What a line picker draws besides its state: the spec's static facts.
pub(crate) struct Chrome<'a> {
    pub picker: &'a Picker,
    pub source: &'a LineSource,
}

impl Chrome<'_> {
    /// The source program's name (`zoxide`), shown so the list's origin is clear.
    fn origin(&self) -> String {
        let program = self.source.command.first().map_or("", String::as_str);
        std::path::Path::new(program)
            .file_name()
            .map_or(program.into(), |name| name.to_string_lossy().into_owned())
    }

    fn noun(&self) -> &'static str {
        match self.source.kind {
            Kind::Path => "paths",
            Kind::Text => "items",
        }
    }

    /// `paste + copy`, in the order Enter performs them.
    fn enter_label(&self) -> String {
        self.source
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
    let count = (!state.query.trim().is_empty())
        .then(|| format!("{}/{}", state.hits.len(), state.items.len()));
    let placeholder = match chrome.source.kind {
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
        let text = if chrome.source.enter.contains(&Deliver::Paste) {
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
    let enter = chrome.enter_label();
    let hints = if chrome.source.multi {
        vec![
            format!("↑↓ select   tab mark   enter {enter}   ctrl-r reload   esc close"),
            format!("↑↓ select  tab mark  enter {enter}  ^r reload  esc close"),
            format!("tab mark  enter {enter}  esc close"),
            format!("enter {enter}  esc close"),
            "tab  enter  esc".to_owned(),
        ]
    } else {
        vec![
            format!("↑↓ select   enter {enter}   ctrl-r reload   esc close"),
            format!("↑↓ select  enter {enter}  ^r reload  esc close"),
            format!("enter {enter}  esc close"),
            "enter  esc".to_owned(),
        ]
    };
    frame.render_widget(
        Paragraph::new(ui::fitting_hint(&hints, footer.width as usize)),
        footer,
    );
}

/// `Paths · 2 marked, 1 hidden ……… zoxide · 477`; the item count waits for
/// the first load. Narrow popups drop the hidden count, then the origin, and
/// keep the mark count as long as it fits.
fn heading_line(state: &LinePicker, chrome: &Chrome<'_>, width: u16) -> Line<'static> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(MUTED);
    let title = vec![Span::styled(chrome.picker.title.clone(), bold)];
    let marked = state.marks().len();
    let hidden = state.hidden_marks();
    // Richest first.
    let mut counted: Vec<Vec<Span<'static>>> = Vec::new();
    if marked > 0 {
        let mut count = title.clone();
        count.extend([
            Span::raw(" · "),
            Span::styled(format!("{marked} marked"), bold),
        ]);
        if hidden > 0 {
            let mut extra = count.clone();
            extra.push(Span::styled(format!(", {hidden} hidden"), muted));
            counted.push(extra);
        }
        counted.push(count);
    }
    let origin = if state.loading && state.items.is_empty() {
        format!("{}…", chrome.origin())
    } else if state.error.is_some() {
        chrome.origin()
    } else {
        format!("{} · {}", chrome.origin(), state.items.len())
    };
    let width = width as usize;
    let used = |spans: &[Span]| spans.iter().map(Span::width).sum::<usize>();
    let (left, with_origin) = counted
        .iter()
        .map(|left| (left, true))
        .chain(counted.iter().map(|left| (left, false)))
        .chain([(&title, true)])
        .find(|(left, with_origin)| {
            used(left) + if *with_origin { origin.width() + 1 } else { 0 } <= width
        })
        .unwrap_or((&title, false));
    let mut spans = left.clone();
    if with_origin {
        let gap = width - used(left) - origin.width();
        spans.extend([Span::raw(" ".repeat(gap)), Span::styled(origin, muted)]);
    }
    Line::from(spans)
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

fn render_rows(state: &LinePicker, chrome: &Chrome<'_>, frame: &mut Frame<'_>, area: Rect) {
    if let Some(error) = state.error.as_deref().filter(|_| !state.loading) {
        // The list is empty because the source failed; say why where the
        // list would be, so typing (which clears notices) never hides it.
        body_text(frame, area, error, ACCENT);
        return;
    }
    if state.hits.is_empty() {
        let message = if state.loading {
            format!("Running {}…", chrome.origin())
        } else if state.items.is_empty() {
            chrome
                .source
                .empty
                .clone()
                .unwrap_or_else(|| format!("{} printed nothing", chrome.origin()))
        } else {
            format!("No matching {}", chrome.noun())
        };
        body_text(frame, area, &message, MUTED);
        return;
    }
    let visible = area.height.max(1) as usize;
    let offset = state.selected.saturating_sub(visible - 1);
    for (i, hit) in state.hits.iter().enumerate().skip(offset).take(visible) {
        let rect = Rect::new(area.x, area.y + (i - offset) as u16, area.width, 1);
        render_row(state, hit, i == state.selected, frame, rect);
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
    let item = &state.items[hit.index];
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

    fn paths_picker() -> Picker {
        pickers::find("paths").unwrap()
    }

    fn source(picker: &Picker) -> LineSource {
        match &picker.source {
            pickers::Source::Lines(source) => source.clone(),
            pickers::Source::Panes => unreachable!(),
        }
    }

    fn state(lines: &[&str]) -> LinePicker {
        let mut state = LinePicker::new(Kind::Path, vec![Deliver::Paste, Deliver::Copy], true);
        let items = lines
            .iter()
            .map(|l| Item::new(l, Kind::Path, Some("/Users/me"), Path::new("/")))
            .collect();
        state.loaded(Ok(Batch {
            items,
            ..Batch::default()
        }));
        state
    }

    fn draw(state: &LinePicker, width: u16, height: u16) -> Buffer {
        let picker = paths_picker();
        let source = source(&picker);
        let chrome = Chrome {
            picker: &picker,
            source: &source,
        };
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
        let picker = paths_picker();
        let mut single = source(&picker);
        single.multi = false;
        let chrome = Chrome {
            picker: &picker,
            source: &single,
        };
        let state = state(&["/a"]);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| render(&state, &chrome, frame))
            .unwrap();
        let screen = rows(terminal.backend().buffer());
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
        failed.loaded(Err("zoxide isn't installed or isn't on Herdr's PATH".into()));
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
}
