//! Pure rendering of local popup state. No host reads or state transitions are
//! hidden inside drawing, including preview sizing and Unicode cursor placement.
//!
//! Top to bottom: heading (mode title + view strip), query, rule, results or
//! the kill review, selected-pane details, notice, key hints. Every width
//! decision (strip form, row layout, hint set) drops whole elements, so a
//! narrow popup never clips a word or a key hint mid-way.
//!
//! Every result shows why it matched: yellow letters in the title, workspace,
//! tab, or pane name, or, when the query matched a field the row doesn't show
//! (the pane's path, or its name in a popup too narrow for the Name column), a
//! dim reason chip after the title.
//!
//! The palette, the query line, match highlighting, path elision and notice
//! wrapping are `pub(crate)` because every picker draws with them: `lines_ui`
//! renders line pickers (e.g. zoxide paths) in this same visual language.
use std::ops::Range;

use crate::{
    age,
    app::{App, Mode, View},
    catalog::{AgentState, Candidate, Hit, Reason, ReasonKind},
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(crate) const ACCENT: Color = Color::Yellow;
pub(crate) const MUTED: Color = Color::DarkGray;
/// Reserved for destruction: the kill title, rule, prompt, and marks.
const DANGER: Color = Color::Red;
const TITLE: &str = "Jump to session";
/// Between a title and its reason chips, and between the details' facets. The
/// detail line already separates a pane's facets this way, and the dot keeps
/// the title's end findable where color can't (inert rows, text captures).
const FACET: &str = " · ";
/// One-line `Workspace › Tab › Task` rows need this much width once the state
/// and age columns are added; narrower areas use two-line rows instead.
const COLUMN_MIN_WIDTH: u16 = 80;
/// The Name column is as wide as the widest pane name, but names past
/// `NAME_MAX` cells end in `…`. It narrows so Task keeps `TASK_MIN` cells; when
/// that would leave it under `NAME_MIN`, too clipped to recognize a name, it
/// gives way and name matches go back to being chips.
const NAME_MAX: usize = 20;
const NAME_MIN: usize = 12;
const TASK_MIN: usize = 32;
/// The ` › ` between one-line columns.
const SEPARATOR: u16 = 3;

/// Key hints per mode, widest first. Narrower variants drop whole `key action`
/// pairs or shorten `ctrl-` to `^`, and the new keys sit right after `enter`
/// so they survive every narrowing.
const BROWSE_HINTS: [&str; 7] = [
    "↑↓ select   enter jump   ctrl-e filter   ctrl-x kill   ctrl-space preview   ctrl-r refresh   esc close",
    "↑↓ select  enter jump  ctrl-e filter  ctrl-x kill  ctrl-space preview  esc close",
    "↑↓ select  enter jump  ^e filter  ^x kill  ^space preview  esc close",
    "↑↓ select  enter jump  ^e filter  ^x kill  esc close",
    "↑↓  enter jump  ^e filter  ^x kill  esc close",
    "enter  ^e filter  ^x kill  esc",
    "enter  esc",
];
const KILL_HINTS: [&str; 6] = [
    "↑↓ move   tab/space mark   enter review   ctrl-e filter   ctrl-space preview   esc cancel",
    "↑↓ move  tab/space mark  enter review  ctrl-e filter  esc cancel",
    "↑↓ move  tab mark  enter review  ^e filter  esc cancel",
    "tab mark  enter review  ^e filter  esc cancel",
    "tab mark  enter review  esc cancel",
    "tab  enter  esc",
];

/// herdr's `symbols` status indicators in its theme's hues (red, teal, yellow,
/// green, overlay gray map to the terminal's ANSI slots). Every state has its
/// own shape, so state never depends on color alone.
fn state_glyph(state: AgentState) -> (&'static str, Color) {
    match state {
        AgentState::Blocked => ("×", Color::Red),
        AgentState::Done => ("✓", Color::Cyan),
        AgentState::Working => ("◐", Color::Yellow),
        AgentState::Idle => ("○", Color::Green),
        AgentState::Neutral => ("·", MUTED),
    }
}

pub(crate) fn render(app: &App, frame: &mut Frame<'_>) {
    render_at(app, frame, age::now_ms());
}

/// Ages are the only clock-dependent pixels; taking `now_ms` keeps rendering a
/// pure function of state for tests. The picker redraws each second.
fn render_at(app: &App, frame: &mut Frame<'_>, now_ms: i64) {
    let area = frame.area();
    if area.width < 6 || area.height < 5 {
        return;
    }
    let inner = Rect::new(area.x + 1, area.y, area.width - 2, area.height);
    let message_lines = app
        .message
        .as_deref()
        .map(|message| wrap_words(message, inner.width as usize))
        .unwrap_or_default();
    let notice_height = message_lines.len().clamp(1, 3) as u16;
    let [heading, query, divider, body, details, notice, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(if inner.height >= 12 { 2 } else { 1 }),
        Constraint::Length(notice_height),
        Constraint::Length(1),
    ])
    .areas(inner);
    let reviewing = app.mode() == Mode::Confirm;
    frame.render_widget(
        Paragraph::new(heading_line(app, heading.width as usize)),
        heading,
    );
    render_query(app, frame, query);
    frame.render_widget(
        Block::new()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(if app.mode() == Mode::Browse {
                MUTED
            } else {
                DANGER
            })),
        divider,
    );
    // The results' layout is decided once: the details line below shows the
    // pane name only when the rows can't show it whole.
    let mut name_cell = 0;
    if reviewing {
        render_review(app, frame, body, now_ms);
    } else {
        let mut results = body;
        if app.preview_visible && body.height >= 5 {
            let areas: [Rect; 2] = if body.width >= 100 {
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .areas(body)
            } else {
                Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .areas(body)
            };
            results = areas[0];
            render_preview(app, frame, areas[1]);
        }
        let rows: Vec<_> = app.candidates.iter().collect();
        let columns = Columns::fit(&rows, results, now_ms);
        name_cell = columns.as_ref().map_or(0, |columns| columns.name);
        render_results(app, frame, results, columns.as_ref(), now_ms);
    }
    if let Some(selected) = app.selected().filter(|_| !reviewing) {
        let mut lines = vec![Line::from(identity_line(
            selected,
            details.width as usize,
            name_cell,
        ))];
        if details.height >= 2 {
            // Dense result rows spend their space on hierarchy and task names;
            // keep the selected pane's project/agent status in the detail area.
            lines.insert(0, Line::from(selected.detail.as_str()));
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(MUTED)),
            details,
        );
    }
    if app.message.is_some() {
        frame.render_widget(
            Paragraph::new(
                message_lines
                    .into_iter()
                    .map(Line::from)
                    .collect::<Vec<_>>(),
            )
            .style(Style::default().fg(ACCENT)),
            notice,
        );
    }
    frame.render_widget(
        Paragraph::new(hint_line(app, footer.width as usize)),
        footer,
    );
}

/// Title variants for the current mode, richest first; all keep the mark count.
fn titles(app: &App) -> Vec<Vec<Span<'static>>> {
    let danger = Style::default().fg(DANGER).add_modifier(Modifier::BOLD);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    match app.mode() {
        Mode::Browse => vec![
            vec![Span::styled(TITLE, bold)],
            vec![Span::styled("Jump", bold)],
        ],
        Mode::Kill => {
            let marked = app.marked_count();
            // Marks survive view and query changes, and the review includes
            // them, so say up front how many are not on screen.
            let visible = app
                .hits
                .iter()
                .filter(|hit| app.is_marked(&app.candidates[hit.index]))
                .count();
            let hidden = marked.saturating_sub(visible);
            let count = Span::styled(format!("{marked} marked"), bold);
            let extra = (hidden > 0)
                .then(|| Span::styled(format!(", {hidden} hidden"), Style::default().fg(MUTED)));
            let sep = || Span::raw(" · ");
            vec![
                [Span::styled("Kill mode", danger), sep(), count.clone()]
                    .into_iter()
                    .chain(extra.clone())
                    .collect(),
                [Span::styled("Kill", danger), sep(), count.clone()]
                    .into_iter()
                    .chain(extra)
                    .collect(),
                vec![Span::styled("Kill", danger), sep(), count],
                vec![
                    Span::styled("Kill", danger),
                    Span::styled(format!(" {marked}"), bold),
                ],
            ]
        }
        Mode::Confirm => {
            let count = app.confirmation().map_or(0, <[Candidate]>::len);
            vec![vec![Span::styled(
                format!("Kill {count} {}?", panes(count)),
                danger,
            )]]
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Strip {
    /// `All 54   In progress 6   Unread/Blocked 9   Idle 22`
    Full,
    /// Inactive filtered views shrink to their glyphs: `All 54   ◐ 6   ×✓ 9   Idle 22`.
    Compact,
    /// Only the active view.
    Active,
}

/// One padded segment per view. Padding every segment (not just the pill)
/// keeps segment positions fixed, so ctrl-e visibly slides the pill along.
fn strip_spans(app: &App, form: Strip, active: View) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for view in View::CYCLE {
        if form == Strip::Active && view != active {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        let count = app.view_count(view);
        if view == active {
            let pill = Style::default()
                .bg(ACCENT)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD);
            spans.push(Span::styled(format!(" {} {count} ", view.label()), pill));
            continue;
        }
        let muted = Style::default().fg(MUTED);
        let glyphs: &[AgentState] = match view {
            View::InProgress => &[AgentState::Working],
            View::Attention => &[AgentState::Blocked, AgentState::Done],
            View::Idle => &[AgentState::Idle],
            View::All => &[],
        };
        if form == Strip::Compact && !glyphs.is_empty() {
            spans.push(Span::raw(" "));
            for state in glyphs {
                let (glyph, color) = state_glyph(*state);
                spans.push(Span::styled(glyph, Style::default().fg(color)));
            }
            spans.push(Span::styled(format!(" {count} "), muted));
        } else {
            spans.push(Span::styled(format!(" {} {count} ", view.label()), muted));
        }
    }
    spans
}

pub(crate) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// The strip form depends on width alone — its widest variant over every
/// active view must fit beside the browse title — so cycling views or toggling
/// kill mode never reflows the header. Titles shorten before the strip does.
fn heading_line(app: &App, width: usize) -> Line<'static> {
    let titles = titles(app);
    let first_fitting = |room: usize| {
        titles
            .iter()
            .find(|title| spans_width(title) + room <= width)
            .cloned()
    };
    let strip_hidden = app.mode() == Mode::Confirm || (app.loading && app.candidates.is_empty());
    if !strip_hidden {
        let shortest = titles.last().map_or(0, |title| spans_width(title));
        for (form, reference) in [
            (Strip::Full, TITLE.width()),
            (Strip::Compact, TITLE.width()),
            (Strip::Active, TITLE.width()),
            (Strip::Active, shortest),
        ] {
            let widest = View::CYCLE
                .iter()
                .map(|view| spans_width(&strip_spans(app, form, *view)))
                .max()
                .unwrap_or(0);
            if reference + 2 + widest > width {
                continue;
            }
            if let Some(mut spans) = first_fitting(2 + widest) {
                let strip = strip_spans(app, form, app.view());
                let gap = width.saturating_sub(spans_width(&spans) + spans_width(&strip));
                spans.push(Span::raw(" ".repeat(gap)));
                spans.extend(strip);
                return Line::from(spans);
            }
        }
    }
    Line::from(first_fitting(0).unwrap_or_else(|| titles[titles.len() - 1].clone()))
}

fn render_query(app: &App, frame: &mut Frame<'_>, area: Rect) {
    if app.mode() == Mode::Confirm {
        // The review accepts no text, so it shows no cursor; the query slot
        // states the consequence instead.
        frame.render_widget(
            Paragraph::new("Each pane closes and its running process ends.")
                .style(Style::default().fg(MUTED)),
            area,
        );
        return;
    }
    // fzf-style match count, only while typing: with an empty query it would
    // repeat the active view's count from the strip.
    let count = (!app.query.trim().is_empty())
        .then(|| format!("{}/{}", app.hits.len(), app.view_count(app.view())));
    let prompt = if app.mode() == Mode::Kill {
        DANGER
    } else {
        ACCENT
    };
    query_line(
        frame,
        area,
        &app.query,
        "Type a session, name, path, or tab…",
        count,
        prompt,
    );
}

/// The `› query` line every picker shares: the tail of a long query stays
/// visible beside the cursor, the placeholder shows while it is empty, and a
/// right-aligned `count` appears when there is room for it.
pub(crate) fn query_line(
    frame: &mut Frame<'_>,
    area: Rect,
    query: &str,
    placeholder: &str,
    count: Option<String>,
    prompt: Color,
) {
    let count = count.filter(|count| count.width() + 12 <= area.width as usize);
    let [area, count_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(count.as_ref().map_or(0, |count| count.width() as u16 + 1)),
    ])
    .areas(area);
    if let Some(count) = count {
        frame.render_widget(
            Paragraph::new(count)
                .right_aligned()
                .style(Style::default().fg(MUTED)),
            count_area,
        );
    }
    let max = area.width.saturating_sub(3) as usize;
    let mut width = 0;
    let mut tail = Vec::new();
    for grapheme in query.graphemes(true).rev() {
        let size = UnicodeWidthStr::width(grapheme);
        if width + size > max {
            break;
        }
        width += size;
        tail.push(grapheme);
    }
    tail.reverse();
    let prompt = Span::styled("› ", Style::default().fg(prompt));
    let text = if query.is_empty() {
        Line::from(vec![
            prompt,
            Span::styled(placeholder.to_owned(), Style::default().fg(MUTED)),
        ])
    } else {
        Line::from(vec![prompt, Span::raw(tail.concat())])
    };
    frame.render_widget(Paragraph::new(text), area);
    frame.set_cursor_position((area.x + 2 + width as u16, area.y));
}

/// One pane as a list row. Results and the kill review share this so a
/// reviewed target looks exactly like the row that was marked.
struct RowView<'a> {
    candidate: &'a Candidate,
    hit: Option<&'a Hit>,
    selected: bool,
    marked: bool,
    /// Kill mode only: the source pane cannot be marked, so it reads muted.
    inert: bool,
}

impl RowView<'_> {
    fn base(&self) -> Style {
        if self.selected {
            Style::default().bg(MUTED).fg(Color::White)
        } else {
            Style::default()
        }
    }

    /// `›` is the cursor, `■` a kill mark (fzf-style: unmarked rows stay
    /// blank). A mark wins on the cursor row so it never disappears under the
    /// selection bar, which still shows where the cursor is. An inert row never
    /// gets the `›`: it should not look like something Tab can act on.
    fn gutter(&self) -> Span<'static> {
        if self.marked {
            let color = if self.selected { Color::White } else { DANGER };
            Span::styled("■", self.base().fg(color).add_modifier(Modifier::BOLD))
        } else if self.selected && !self.inert {
            Span::styled("›", self.base().fg(ACCENT))
        } else {
            Span::styled(" ", self.base())
        }
    }

    /// The selection bar is `DarkGray`, on which state colors drop to ~2:1
    /// contrast; like herdr's navigator, the selected row keeps the glyph's
    /// shape in the bar's foreground and the detail line names the state.
    fn glyph(&self) -> Span<'static> {
        let (glyph, color) = state_glyph(self.candidate.state);
        let style = match (self.selected, self.inert) {
            (true, true) => self.context(),
            (true, false) => self.base(),
            (false, true) => Style::default().fg(MUTED),
            (false, false) => Style::default().fg(color),
        };
        Span::styled(glyph, style)
    }

    fn context(&self) -> Style {
        self.base()
            .fg(if self.selected { Color::Gray } else { MUTED })
    }

    /// Red cannot survive the selection bar (~1.5:1), so a marked title under
    /// the cursor keeps its weight and the `■` instead of its color.
    fn title(&self) -> Style {
        let weight = |style: Style| {
            if self.marked {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            }
        };
        match (self.selected, self.inert) {
            (true, true) => self.context(),
            (true, false) => weight(self.base()),
            (false, true) => Style::default().fg(MUTED),
            (false, false) if self.marked => weight(Style::default().fg(DANGER)),
            (false, false) => Style::default(),
        }
    }

    /// Match highlights invite action, so inert rows render without them.
    fn indices(&self, pick: fn(&Hit) -> &[u32]) -> &[u32] {
        match self.hit {
            Some(hit) if !self.inert => pick(hit),
            _ => &[],
        }
    }

    /// The title, then one dim chip per hidden field that explains the match
    /// (Name then Path), in the row's context style so it sits at the weight
    /// of `Workspace › Tab` and age. With a Name column on screen the pane
    /// name is not hidden, so its matches highlight there instead of in a
    /// chip. The title is guaranteed half the cell (all of it when shorter);
    /// chips may use the rest and give back what they don't use. Without
    /// reasons, as with an empty query or in the review, this is exactly the
    /// plain title.
    fn title_cell(&self, width: u16, name_column: bool) -> Line<'_> {
        let title = &self.candidate.title;
        let indices = self.indices(|h| &h.title_indices);
        let reasons = match self.hit {
            Some(hit) if name_column => hit.reasons_beside_name.as_slice(),
            Some(hit) => hit.reasons.as_slice(),
            None => &[],
        };
        if reasons.is_empty() {
            return fitted(title, indices, self.title(), width);
        }
        let width = usize::from(width);
        let guaranteed = UnicodeWidthStr::width(title.as_str()).min(width.div_ceil(2));
        let chips = fit_chips(reasons, width - guaranteed);
        let used: usize = chips.iter().map(|chip| FACET.width() + chip.width()).sum();
        let mut line = fitted(
            title,
            indices,
            self.title(),
            width.saturating_sub(used) as u16,
        );
        let context = self.context();
        for (reason, chip) in reasons.iter().zip(&chips) {
            line.spans.push(Span::styled(FACET, context));
            // Inert rows keep the chip's layout but, like their titles,
            // drop the highlights that would invite action.
            let indices = if self.inert { &[][..] } else { &reason.indices };
            line.spans.extend(chip.spans(indices, context));
        }
        line
    }

    fn render(&self, frame: &mut Frame<'_>, rect: Rect, columns: Option<&Columns>, age: &str) {
        frame.render_widget(Block::new().style(self.base()), rect);
        let line = Rect::new(rect.x, rect.y, rect.width, 1);
        let cells = match columns {
            Some(columns) => columns.cells(line),
            None => {
                // Two-line rows: title and age on the first line, the muted
                // `detail` (which also names the state) indented beneath.
                let [gutter, glyph, title, _, age] =
                    Layout::horizontal(stacked_constraints(age.width() as u16)).areas(line);
                Cells {
                    gutter,
                    glyph,
                    title,
                    age,
                    ..Cells::default()
                }
            }
        };
        frame.render_widget(Paragraph::new(self.gutter()), cells.gutter);
        frame.render_widget(Paragraph::new(self.glyph()), cells.glyph);
        let row = self.candidate;
        if columns.is_some() {
            // The pane's place in herdr, as one muted breadcrumb. Its name is
            // the last step, so it shares the breadcrumb's weight and leaves
            // the bright title the row's one focal point.
            // A label wider than its cell keeps its matched letters, as a chip
            // does: cutting it from the end could hide the row's only reason
            // to match.
            let context = self.context();
            for (text, indices, cell) in [
                (
                    row.workspace.as_str(),
                    self.indices(|h| &h.workspace_indices),
                    cells.workspace,
                ),
                (
                    row.tab.as_str(),
                    self.indices(|h| &h.tab_indices),
                    cells.tab,
                ),
                (
                    row.name.as_str(),
                    self.indices(|h| &h.name_indices),
                    cells.name,
                ),
            ] {
                let label = Elided::label(text, indices, usize::from(cell.width));
                frame.render_widget(
                    Paragraph::new(Line::from(label.spans(indices, context))),
                    cell,
                );
            }
            let [first, before_name, before_title] = cells.separators;
            // An unnamed pane leaves the Name cell blank rather than pointing
            // a `›` at nothing; the title stays in line with every other row.
            let before_name = if row.name.is_empty() {
                Rect::default()
            } else {
                before_name
            };
            for separator in [first, before_name, before_title] {
                frame.render_widget(Paragraph::new(" › ").style(context), separator);
            }
        }
        let name_column = columns.is_some_and(Columns::shows_name);
        frame.render_widget(
            Paragraph::new(self.title_cell(cells.title.width, name_column)),
            cells.title,
        );
        frame.render_widget(
            Paragraph::new(age.to_owned())
                .right_aligned()
                .style(self.context()),
            cells.age,
        );
        if columns.is_none() && rect.height >= 2 {
            let detail = Rect::new(rect.x + 4, rect.y + 1, rect.width.saturating_sub(4), 1);
            frame.render_widget(
                Paragraph::new(fitted(
                    &row.detail,
                    self.indices(|h| &h.detail_indices),
                    self.context(),
                    detail.width,
                )),
                detail,
            );
        }
    }
}

fn stacked_constraints(age_width: u16) -> [Constraint; 5] {
    [
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(u16::from(age_width > 0)),
        Constraint::Length(age_width),
    ]
}

/// Column widths for one-line rows (`Workspace › Tab › Name › Task … Age`),
/// sized from the whole catalog rather than the current hits so typing or
/// cycling views cannot make columns jump.
struct Columns {
    workspace: u16,
    tab: u16,
    /// Zero, with no column at all, when no pane has a name or Task would be
    /// left too narrow (see `NAME_MAX`).
    name: u16,
    age: u16,
}

/// Where one row's cells sit. Two-line rows have no workspace, tab or name.
#[derive(Default)]
struct Cells {
    gutter: Rect,
    glyph: Rect,
    workspace: Rect,
    tab: Rect,
    name: Rect,
    title: Rect,
    age: Rect,
    /// Before the tab, the name, and the title.
    separators: [Rect; 3],
}

impl Columns {
    /// The one-line layout for `rows` in `area`, or `None` when the area is
    /// too narrow or short and rows stack on two lines instead.
    fn fit(rows: &[&Candidate], area: Rect, now_ms: i64) -> Option<Self> {
        (area.width >= COLUMN_MIN_WIDTH && area.height >= 2).then(|| {
            let age = age_width(rows.iter().copied(), now_ms);
            Self::new(rows, area.width, age)
        })
    }

    fn new(rows: &[&Candidate], width: u16, age: u16) -> Self {
        let widest = |text: fn(&Candidate) -> &str| {
            rows.iter()
                .map(|row| UnicodeWidthStr::width(text(row)))
                .max()
                .unwrap_or(0)
        };
        let width = usize::from(width);
        // Reserve most of each row for the pane's recognizable task.
        let workspace = widest(|row| &row.workspace).clamp(9, (width / 6).clamp(9, 16));
        let tab = widest(|row| &row.tab).clamp(3, (width / 4).clamp(3, 24));
        let names = widest(|row| &row.name);
        let wanted = if names == 0 {
            0
        } else {
            names.clamp("Name".width(), NAME_MAX)
        };
        let separator = usize::from(SEPARATOR);
        let taken =
            4 + workspace + separator + tab + separator + usize::from(age > 0) + age as usize;
        let room = width.saturating_sub(taken + separator + TASK_MIN);
        let name = wanted.min(room);
        let name = if name > 0 && name >= wanted.min(NAME_MIN) {
            name
        } else {
            0
        };
        Self {
            workspace: workspace as u16,
            tab: tab as u16,
            name: name as u16,
            age,
        }
    }

    fn shows_name(&self) -> bool {
        self.name > 0
    }

    fn cells(&self, area: Rect) -> Cells {
        let named = |width: u16| if self.shows_name() { width } else { 0 };
        let [gutter, glyph, workspace, first, tab, before_name, name, before_title, title, _, age] =
            Layout::horizontal([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(self.workspace),
                Constraint::Length(SEPARATOR),
                Constraint::Length(self.tab),
                Constraint::Length(named(SEPARATOR)),
                Constraint::Length(self.name),
                Constraint::Length(SEPARATOR),
                Constraint::Min(1),
                Constraint::Length(u16::from(self.age > 0)),
                Constraint::Length(self.age),
            ])
            .areas(area);
        Cells {
            gutter,
            glyph,
            workspace,
            tab,
            name,
            title,
            age,
            separators: [first, before_name, before_title],
        }
    }

    fn render_heading(&self, frame: &mut Frame<'_>, area: Rect) {
        let cells = self.cells(area);
        let [first, before_name, before_title] = cells.separators;
        for (text, rect) in [
            ("Workspace", cells.workspace),
            (" › ", first),
            ("Tab", cells.tab),
            (" › ", before_name),
            ("Name", cells.name),
            (" › ", before_title),
            ("Task", cells.title),
        ] {
            frame.render_widget(Paragraph::new(text).style(Style::default().fg(MUTED)), rect);
        }
        if self.age > 0 {
            frame.render_widget(
                Paragraph::new("Age")
                    .right_aligned()
                    .style(Style::default().fg(MUTED)),
                cells.age,
            );
        }
    }
}

/// Session age (herdr-agents' meaning), blank when the start is unknown.
fn age_label(row: &Candidate, now_ms: i64) -> String {
    row.started_at
        .map(|at| age::label(at, now_ms))
        .unwrap_or_default()
}

/// The age column is as wide as the widest label in `rows` (at least 3, so the
/// `Age` heading fits), or absent when no pane has a known start.
fn age_width<'a>(rows: impl IntoIterator<Item = &'a Candidate>, now_ms: i64) -> u16 {
    rows.into_iter()
        .filter(|row| row.started_at.is_some())
        .map(|row| age_label(row, now_ms).width().max(3) as u16)
        .max()
        .unwrap_or(0)
}

fn render_results(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    columns: Option<&Columns>,
    now_ms: i64,
) {
    if app.hits.is_empty() {
        frame.render_widget(
            Paragraph::new(empty_message(app)).style(Style::default().fg(MUTED)),
            area,
        );
        return;
    }
    // Rank and select panes only. Repeating the parents on every row preserves
    // context when fuzzy ranking interleaves workspaces or scrolling hides an
    // earlier sibling; headings never become navigation targets.
    let area = if let Some(columns) = columns {
        columns.render_heading(frame, Rect::new(area.x, area.y, area.width, 1));
        Rect::new(area.x, area.y + 1, area.width, area.height - 1)
    } else {
        area
    };
    let row_height = if columns.is_none() && area.height >= 4 {
        2
    } else {
        1
    };
    let visible = (area.height / row_height).max(1) as usize;
    let offset = app.selected.saturating_sub(visible - 1);
    let killing = app.mode() == Mode::Kill;
    for (i, hit) in app.hits.iter().enumerate().skip(offset).take(visible) {
        let candidate = &app.candidates[hit.index];
        let row = RowView {
            candidate,
            hit: Some(hit),
            selected: i == app.selected,
            marked: killing && app.is_marked(candidate),
            inert: killing && !app.is_killable(candidate),
        };
        let rect = Rect::new(
            area.x,
            area.y + (i - offset) as u16 * row_height,
            area.width,
            row_height,
        );
        row.render(frame, rect, columns, &age_label(candidate, now_ms));
    }
}

fn empty_message(app: &App) -> String {
    if app.loading {
        return "Loading panes…".into();
    }
    let view = app.view();
    let message = if !app.query.trim().is_empty() {
        if view == View::All {
            return "No matching panes".into();
        }
        "No matching panes in this view"
    } else {
        match view {
            View::All => return "No panes".into(),
            View::InProgress => "No agents are working",
            View::Attention => "No blocked or unread agents",
            View::Idle => "No idle agents",
        }
    };
    format!("{message} · ctrl-e next filter")
}

/// The kill review lists exactly `app.confirmation()`, in kill order, including
/// marks the current view or query hides. Any key answers it, so it cannot
/// scroll: overflow becomes `+N more` while the heading keeps the exact count.
fn render_review(app: &App, frame: &mut Frame<'_>, area: Rect, now_ms: i64) {
    let targets = app.confirmation().unwrap_or_default();
    let rows: Vec<_> = app.candidates.iter().chain(targets).collect();
    let columns = Columns::fit(&rows, area, now_ms);
    let area = if let Some(columns) = &columns {
        columns.render_heading(frame, Rect::new(area.x, area.y, area.width, 1));
        Rect::new(area.x, area.y + 1, area.width, area.height - 1)
    } else {
        area
    };
    let row_height = if columns.is_none() && area.height >= 4 {
        2
    } else {
        1
    };
    let shown = if targets.len() * row_height as usize <= area.height as usize {
        targets.len()
    } else {
        (area.height.saturating_sub(1) / row_height) as usize
    };
    for (i, candidate) in targets.iter().take(shown).enumerate() {
        let row = RowView {
            candidate,
            hit: None,
            selected: false,
            marked: true,
            inert: false,
        };
        let rect = Rect::new(
            area.x,
            area.y + i as u16 * row_height,
            area.width,
            row_height,
        );
        row.render(frame, rect, columns.as_ref(), &age_label(candidate, now_ms));
    }
    if shown < targets.len() {
        frame.render_widget(
            Paragraph::new(format!("+{} more", targets.len() - shown))
                .style(Style::default().fg(MUTED)),
            Rect::new(area.x, area.y + shown as u16 * row_height, area.width, 1),
        );
    }
}

/// The widest hint variant that fits, so narrowing drops whole `key action`
/// pairs instead of clipping one.
pub(crate) fn fitting_hint<S: AsRef<str>>(hints: &[S], width: usize) -> Line<'static> {
    hints
        .iter()
        .map(AsRef::as_ref)
        .find(|hint| hint.width() <= width)
        .map(|hint| Line::styled(hint.to_owned(), Style::default().fg(MUTED)))
        .unwrap_or_default()
}

fn hint_line(app: &App, width: usize) -> Line<'static> {
    let muted = Style::default().fg(MUTED);
    match app.mode() {
        Mode::Browse => fitting_hint(&BROWSE_HINTS, width),
        Mode::Kill => fitting_hint(&KILL_HINTS, width),
        Mode::Confirm => {
            // The review's only actions; `y` is the one key that kills.
            let count = app.confirmation().map_or(0, <[Candidate]>::len);
            let key = Span::styled(
                "y",
                Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
            );
            [
                vec![
                    key.clone(),
                    Span::raw(format!(" kill {count} {}", panes(count))),
                    Span::styled("   any other key cancels", muted),
                ],
                vec![
                    key.clone(),
                    Span::raw(" kill"),
                    Span::styled("   other keys cancel", muted),
                ],
                vec![key, Span::raw(" kill")],
            ]
            .into_iter()
            .find(|spans| spans_width(spans) <= width)
            .map(Line::from)
            .unwrap_or_default()
        }
    }
}

fn panes(count: usize) -> &'static str {
    if count == 1 {
        "pane"
    } else {
        "panes"
    }
}

pub(crate) fn fitted<'a>(text: &'a str, indices: &[u32], style: Style, width: u16) -> Line<'a> {
    // Clip on grapheme boundaries and reserve a cell for the ellipsis. Letting
    // the terminal clip silently would hide that two long parent names differ.
    let truncated = UnicodeWidthStr::width(text) > usize::from(width);
    let available = width.saturating_sub(u16::from(truncated)) as usize;
    let mut used = 0;
    let mut spans = Vec::new();
    for span in highlighted(text, indices, style) {
        let size = UnicodeWidthStr::width(span.content.as_ref());
        if used + size > available {
            break;
        }
        used += size;
        spans.push(span);
    }
    if truncated && width > 0 {
        spans.push(Span::styled("…", style));
    }
    Line::from(spans)
}

fn highlighted<'a>(text: &'a str, indices: &[u32], base: Style) -> Vec<Span<'a>> {
    // Nucleo indices address graphemes, matching Ratatui's cell rendering rather
    // than UTF-8 byte offsets. Keep combining marks attached to their base.
    text.graphemes(true)
        .enumerate()
        .map(|(i, text)| {
            let style = if indices.binary_search(&(i as u32)).is_ok() {
                base.fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                base
            };
            Span::styled(text, style)
        })
        .collect()
}

/// Sizes each reason's chip within `room` cells, separators included. Paths
/// shorten well around their match, so they give way first and a name stays
/// whole when it can. Returned in `reasons` order.
fn fit_chips(reasons: &[Reason], room: usize) -> Vec<Elided<'_>> {
    let separator = FACET.width();
    let names: usize = reasons
        .iter()
        .filter(|reason| reason.kind == ReasonKind::Name)
        .map(|reason| separator + reason.text.width())
        .sum();
    let mut left = room;
    let mut chips: Vec<Option<Elided>> = reasons.iter().map(|_| None).collect();
    for (kind, reserve) in [(ReasonKind::Path, names), (ReasonKind::Name, 0)] {
        for (i, reason) in reasons.iter().enumerate() {
            if reason.kind != kind {
                continue;
            }
            let chip = Elided::reason(reason, left.saturating_sub(reserve + separator));
            left = left.saturating_sub(separator + chip.width());
            chips[i] = Some(chip);
        }
    }
    chips.into_iter().flatten().collect()
}

/// A shown run of graphemes (`start..end` of the full text), or the `…`
/// standing in for an elided one.
#[derive(Clone, Copy)]
enum Piece {
    Text(usize, usize),
    Ellipsis,
}

/// A visible window onto a chip's or path's text. Grapheme positions stay
/// those of the full text, so `Reason.indices` highlight the shown letters
/// without remapping.
pub(crate) struct Elided<'a> {
    graphemes: Vec<&'a str>,
    pieces: Vec<Piece>,
}

impl<'a> Elided<'a> {
    /// The engine's indices mark the letters the chip exists to show; elision
    /// never drops them, even when that overflows `room` (the title then gives
    /// up the difference).
    fn reason(reason: &'a Reason, room: usize) -> Self {
        match reason.kind {
            ReasonKind::Path => Self::path(&reason.text, &reason.indices, room),
            ReasonKind::Name => Self::label(&reason.text, &reason.indices, room),
        }
    }

    /// A short label (pane name, tab, workspace) shortened to `room` cells
    /// around its `matched` graphemes (see `elide_name`), for a Name chip or a
    /// breadcrumb column. Labels are a few dozen cells at most, so one
    /// protected span covering every matched letter is cheap.
    fn label(text: &'a str, matched: &[u32], room: usize) -> Self {
        let graphemes: Vec<&str> = text.graphemes(true).collect();
        let matched: Vec<usize> = matched
            .iter()
            .map(|&i| i as usize)
            .filter(|&i| i < graphemes.len())
            .collect();
        let keep = matched
            .first()
            .zip(matched.last())
            .map_or(0..0, |(lo, hi)| *lo..hi + 1);
        let pieces = elide_name(&graphemes, keep, room);
        Self { graphemes, pieces }
    }

    /// A path shortened to `room` cells, keeping the `matched` graphemes and
    /// the leaf (see `elide_path`).
    pub(crate) fn path(text: &'a str, matched: &[u32], room: usize) -> Self {
        let graphemes: Vec<&str> = text.graphemes(true).collect();
        let matched: Vec<usize> = matched
            .iter()
            .map(|&i| i as usize)
            .filter(|&i| i < graphemes.len())
            .collect();
        let pieces = elide_path(&graphemes, &matched, room);
        Self { graphemes, pieces }
    }

    pub(crate) fn width(&self) -> usize {
        pieces_width(&self.graphemes, &self.pieces)
    }

    fn spans(&self, indices: &[u32], base: Style) -> Vec<Span<'a>> {
        self.styled_spans(indices, |_| base)
    }

    /// Like `spans`, with the base style chosen per full-text grapheme index
    /// (line pickers dim a path's parent directories but not its leaf). An
    /// elision `…` takes the style of the grapheme it replaces.
    pub(crate) fn styled_spans(
        &self,
        indices: &[u32],
        base: impl Fn(usize) -> Style,
    ) -> Vec<Span<'a>> {
        let mut spans = Vec::new();
        for (n, piece) in self.pieces.iter().enumerate() {
            match *piece {
                Piece::Ellipsis => {
                    let at = match self.pieces.get(n + 1) {
                        Some(Piece::Text(start, _)) => start.saturating_sub(1),
                        _ => self.graphemes.len().saturating_sub(1),
                    };
                    spans.push(Span::styled("…", base(at)));
                }
                Piece::Text(start, end) => {
                    for i in start..end {
                        let style = if indices.binary_search(&(i as u32)).is_ok() {
                            base(i).fg(ACCENT).add_modifier(Modifier::BOLD)
                        } else {
                            base(i)
                        };
                        spans.push(Span::styled(self.graphemes[i], style));
                    }
                }
            }
        }
        spans
    }

    fn text(&self) -> String {
        self.spans(&[], Style::default())
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }
}

/// The `…` that stands in for an elided run is one cell wide.
const ELLIPSIS: usize = 1;

/// Terminal cells, measured per grapheme as Ratatui draws them.
fn cells(graphemes: &[&str]) -> usize {
    graphemes.iter().map(|g| g.width()).sum()
}

/// Paths keep their matched directories and their leaf, the most
/// identifying part, the way fish and VS Code shorten paths: drop leading
/// directories, then unmatched ones between the matches and the leaf, then
/// the tail. Cuts land on `/`, so no fragment like `…fig/` shows. With no
/// `matched` letters (the details line) only the leaf is protected.
fn elide_path(g: &[&str], matched: &[usize], room: usize) -> Vec<Piece> {
    use Piece::{Ellipsis, Text};
    let n = g.len();
    if cells(g) <= room {
        return vec![Text(0, n)];
    }
    // Each directory owns its leading `/`, so a match across one
    // (`skl/library`) keeps both sides.
    let mut starts = vec![0];
    starts.extend((1..n).filter(|&i| g[i] == "/"));
    let dirs: Vec<(usize, usize)> = starts
        .iter()
        .zip(starts.iter().skip(1).chain([&n]))
        .map(|(&start, &end)| (start, end))
        .collect();
    let leaf = dirs.len() - 1;
    let fits = |kept: &dyn Fn(usize) -> bool| {
        let pieces = join_dirs(&dirs, kept);
        (pieces_width(g, &pieces) <= room).then_some(pieces)
    };
    let holds = |dir: usize| {
        let (start, end) = dirs[dir];
        matched.iter().any(|i| (start..end).contains(i))
    };
    let Some(first) = (0..dirs.len()).find(|&dir| holds(dir)) else {
        if let Some(pieces) = (1..=leaf).find_map(|from| fits(&|dir| dir >= from)) {
            return pieces;
        }
        let start = (0..n)
            .find(|&i| ELLIPSIS + cells(&g[i..]) <= room)
            .unwrap_or(n);
        return vec![Ellipsis, Text(start, n)];
    };
    let last = (0..dirs.len()).rfind(|&dir| holds(dir)).unwrap_or(first);
    (1..=first)
        .find_map(|from| fits(&|dir| dir >= from))
        .or_else(|| (last + 1..=leaf).find_map(|tail| fits(&|dir| holds(dir) || dir >= tail)))
        .or_else(|| fits(&holds))
        .unwrap_or_else(|| {
            if first == last {
                around(g, matched[0]..matched[matched.len() - 1] + 1, room)
            } else {
                // Separate matched directories that can't fit even alone
                // overflow; the title gives up the difference.
                join_dirs(&dirs, &holds)
            }
        })
}

/// Joins the kept directories, replacing each dropped run with `…`: a leading
/// or trailing run is a bare `…` (`…/skl/library`, `…/codiing-pivot…`); a
/// run between kept directories keeps its opening slash (`/…/ops`).
fn join_dirs(dirs: &[(usize, usize)], kept: &dyn Fn(usize) -> bool) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut dir = 0;
    while dir < dirs.len() {
        if kept(dir) {
            pieces.push(Piece::Text(dirs[dir].0, dirs[dir].1));
            dir += 1;
            continue;
        }
        let run = dir;
        while dir < dirs.len() && !kept(dir) {
            dir += 1;
        }
        if run > 0 && dir < dirs.len() {
            pieces.push(Piece::Text(dirs[run].0, dirs[run].0 + 1));
        }
        pieces.push(Piece::Ellipsis);
    }
    pieces
}

fn pieces_width(g: &[&str], pieces: &[Piece]) -> usize {
    pieces
        .iter()
        .map(|piece| match *piece {
            Piece::Text(start, end) => cells(&g[start..end]),
            Piece::Ellipsis => ELLIPSIS,
        })
        .sum()
}

/// Names are short slugs recognized by how they begin: keep the start, else
/// the end, else the matched letters.
fn elide_name(g: &[&str], keep: Range<usize>, room: usize) -> Vec<Piece> {
    use Piece::{Ellipsis, Text};
    let n = g.len();
    if cells(g) <= room {
        return vec![Text(0, n)];
    }
    if cells(&g[..keep.end]) + ELLIPSIS <= room {
        let end = (keep.end..n)
            .take_while(|&end| cells(&g[..=end]) + ELLIPSIS <= room)
            .last()
            .map_or(keep.end, |end| end + 1);
        return vec![Text(0, end), Ellipsis];
    }
    if ELLIPSIS + cells(&g[keep.start..]) <= room {
        let start = (0..=keep.start)
            .find(|&start| ELLIPSIS + cells(&g[start..]) <= room)
            .unwrap_or(keep.start);
        return vec![Ellipsis, Text(start, n)];
    }
    around(g, keep, room)
}

/// The last resort: the matched letters, extended right while they fit.
fn around(g: &[&str], keep: Range<usize>, room: usize) -> Vec<Piece> {
    use Piece::{Ellipsis, Text};
    let n = g.len();
    let lead = usize::from(keep.start > 0);
    let mut end = keep.end;
    while end < n && lead + cells(&g[keep.start..=end]) + usize::from(end + 1 < n) <= room {
        end += 1;
    }
    (keep.start > 0)
        .then_some(Ellipsis)
        .into_iter()
        .chain([Text(keep.start, end)])
        .chain((end < n).then_some(Ellipsis))
        .collect()
}

/// The selected pane's own facets: its `~` path (the same form a path chip
/// uses) and its pane id, led by its name (what the owner calls it) unless
/// its row already shows the whole name in a `name_cell`-wide Name column (0
/// without one). Workspace and tab open the detail line above. A long path
/// drops leading directories so the name, leaf, and id stay visible.
fn identity_line(row: &Candidate, width: usize, name_cell: u16) -> String {
    let shown_whole = row.name.width() <= usize::from(name_cell);
    let name = if shown_whole { "" } else { row.name.as_str() };
    let fixed: usize = [name, row.pane_id.as_str()]
        .iter()
        .filter(|facet| !facet.is_empty())
        .map(|facet| facet.width() + FACET.width())
        .sum();
    // A bare `…` would only push the id off the line, so a path with no room
    // for a single letter is left out.
    let room = width.saturating_sub(fixed);
    let path = if room > ELLIPSIS {
        Elided::path(&row.path, &[], room).text()
    } else {
        String::new()
    };
    [name, path.as_str(), row.pane_id.as_str()]
        .into_iter()
        .filter(|facet| !facet.is_empty())
        .collect::<Vec<_>>()
        .join(FACET)
}

fn render_preview(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let block = Block::bordered()
        .title(" Preview ")
        .border_style(Style::default().fg(MUTED));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = app
        .preview
        .as_deref()
        .unwrap_or(if app.selected().is_some() {
            "Loading preview…"
        } else {
            "No pane selected"
        });
    let lines = wrap_preview(text, inner.width as usize);
    let skip = lines.len().saturating_sub(inner.height as usize);
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(skip)
                .map(Line::from)
                .collect::<Vec<_>>(),
        ),
        inner,
    );
}

/// Notices (kill results, jump errors) are prose that must stay readable in a
/// 1–3 line slot: break between words, and split by grapheme only a single
/// word wider than the line. Control characters never reach the terminal.
pub(crate) fn wrap_words(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut current = String::new();
        let mut width = 0;
        for word in paragraph.split_whitespace() {
            let word: String = word
                .graphemes(true)
                .filter(|grapheme| !grapheme.chars().any(char::is_control))
                .collect();
            // Ratatui draws one grapheme per cell run, so size words the same
            // way: a whole-string width can be narrower (Arabic `لا` is one
            // cell as a string, two as graphemes) and the line would clip.
            let size: usize = word.graphemes(true).map(UnicodeWidthStr::width).sum();
            if size == 0 {
                continue;
            }
            if !current.is_empty() && width + 1 + size <= max {
                current.push(' ');
                current.push_str(&word);
                width += 1 + size;
                continue;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                width = 0;
            }
            if size <= max {
                current = word;
                width = size;
                continue;
            }
            for grapheme in word.graphemes(true) {
                let size = grapheme.width();
                if width + size > max && !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                    width = 0;
                }
                if size <= max {
                    current.push_str(grapheme);
                    width += size;
                }
            }
        }
        lines.push(current);
    }
    lines
}

fn wrap_preview(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut current = String::new();
        let mut width = 0;
        for grapheme in line.graphemes(true) {
            let size = UnicodeWidthStr::width(grapheme);
            if width + size > max && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                width = 0;
            }
            if size <= max {
                current.push_str(grapheme);
                width += size;
            }
        }
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::Event;
    use ratatui::{backend::TestBackend, Terminal};

    /// A catalog with every state, known and unknown ages, and a source pane.
    /// `NOW` pins `render_at`, so ages are exact literals.
    const NOW: i64 = 1_800_000_000_000;
    const SOURCE: &str = "w1:p9";

    fn fixture_rows() -> Vec<Candidate> {
        use serde_json::json;
        // (pane, workspace, tab, title, agent, status, age in seconds)
        let panes = [
            (
                "w2:p1",
                "w2",
                "w2:t1",
                "Debug BrowserOS Neo crashes",
                Some("claude"),
                "blocked",
                Some(240),
            ),
            (
                "w1:p2",
                "w1",
                "w1:t1",
                "Review RTR sessions",
                Some("codex"),
                "done",
                Some(720),
            ),
            (
                "w2:p3",
                "w2",
                "w2:t1",
                "Trigger release build",
                Some("codex"),
                "working",
                Some(45),
            ),
            (
                "w3:p4",
                "w3",
                "w3:t2",
                "Optimize Sentry tracing costs",
                Some("claude"),
                "idle",
                Some(86_400),
            ),
            (
                "w3:p5",
                "w3",
                "w3:t3",
                "Drop 30 day filter",
                Some("codex"),
                "idle",
                Some(12 * 86_400),
            ),
            ("w1:p6", "w1", "w1:t2", "nvim", None, "unknown", None),
            (
                "w1:p7",
                "w1",
                "w1:t1",
                "Fresh agent without transcript",
                Some("claude"),
                "working",
                None,
            ),
            (
                "w1:p9",
                "w1",
                "w1:t1",
                "Update Claude aliases",
                Some("claude"),
                "idle",
                Some(7_200),
            ),
        ];
        let panes: Vec<_> = panes
            .iter()
            .map(|(id, workspace, tab, title, agent, status, age)| {
                json!({"pane_id": id, "terminal_id": format!("t-{id}"), "tab_id": tab,
                    "workspace_id": workspace, "title": title, "agent": agent,
                    "agent_status": status, "cwd": "/code/rtr",
                    "started_at": age.map(|age: i64| NOW - age * 1000)})
            })
            .collect();
        crate::catalog::catalog(
            json!({
                "workspaces": [{"workspace_id":"w1","label":"SETUP","number":1},
                               {"workspace_id":"w2","label":"MAIN","number":2},
                               {"workspace_id":"w3","label":"MISC","number":3}],
                "tabs": [{"tab_id":"w1:t1","workspace_id":"w1","label":"clis","number":1},
                         {"tab_id":"w1:t2","workspace_id":"w1","label":"nvim","number":2},
                         {"tab_id":"w2:t1","workspace_id":"w2","label":"bos","number":1},
                         {"tab_id":"w3:t2","workspace_id":"w3","label":"posthog","number":2},
                         {"tab_id":"w3:t3","workspace_id":"w3","label":"coding-pivot-reachouts","number":3}],
                "panes": panes
            }),
            SOURCE,
        )
        .unwrap()
    }

    fn fixture() -> App {
        let mut app = App::with_context(SOURCE.into(), String::new());
        app.loaded(fixture_rows());
        app
    }

    fn ctrl(ch: char) -> Event {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL))
    }

    fn press(code: ratatui::crossterm::event::KeyCode) -> Event {
        use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Renders at a pinned clock and returns the buffer as rows of cells.
    fn draw(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render_at(app, frame, NOW)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        let width = buffer.area.width as usize;
        buffer
            .content
            .chunks(width)
            .map(|cells| cells.iter().map(|c| c.symbol()).collect())
            .collect()
    }

    /// Character column of `needle` in a rendered line. Fixture text is all
    /// single-width, so characters and terminal cells coincide.
    fn col(line: &str, needle: &str) -> usize {
        let byte = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {line:?}"));
        line[..byte].chars().count()
    }

    fn row(lines: &[String], needle: &str) -> usize {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", lines.join("\n")))
    }

    fn mark_two(app: &mut App) {
        use ratatui::crossterm::event::KeyCode;
        app.handle(ctrl('x'));
        app.handle(press(KeyCode::Tab)); // marks the blocked row, cursor moves on
        app.handle(press(KeyCode::Down));
        app.handle(press(KeyCode::Tab)); // marks the 45s working row
    }

    /// Rows from the live snapshot the search spec was drawn from, with the
    /// same workspaces and tabs, so column widths match the spec's mocks
    /// (`1008-jump-search/ui-search-spec.md`). Paths are already `~` paths, so
    /// the catalog's HOME collapsing leaves them alone on any machine.
    const CHIP_SOURCE: &str = "w1B:p1D";
    const LONG_PATH: &str =
        "~/Documents/notes-os/STARTUP/codiing-pivot/outreach-loop/ops/.wt/feat/cld-messaging-3";

    fn search_fixture() -> App {
        use serde_json::json;
        // (pane, tab, title, name, path, status, age in seconds)
        let panes = [
            (
                "w1C:pS",
                "w3:t1",
                "Logseq fork with vim navigation",
                "",
                "~/code/oss/logseq",
                "idle",
                Some(12 * 86_400),
            ),
            (
                "w1B:p1D",
                "w1:t1",
                "CLAUDE.md task label rules",
                "vim-html-skill",
                "~/.config/skl/library/skills",
                "idle",
                Some(4 * 3_600),
            ),
            (
                "w1B:p1V",
                "w1:t1",
                "CLAUDE.md task label rules",
                "agent-alias-defaults",
                "~/.config/skl/library/skills",
                "idle",
                Some(2 * 86_400),
            ),
            (
                "w1B:p1E",
                "w1:t2",
                "Rill diff colors contrast",
                "diag-sidekick-send",
                "~/.config/nvim",
                "idle",
                None,
            ),
            (
                "w1D:p8X",
                "w4:t1",
                "Vh-skeleton brief run",
                "vh-skeleton",
                "~/.config/skl/library/.wt/feat/vh-skeleton",
                "working",
                Some(18 * 60),
            ),
            (
                "w1D:p8Y",
                "w4:t2",
                "Vh-themes brief run",
                "vh-themes",
                "~/.config/skl/library/.wt/feat/vh-themes",
                "working",
                Some(17 * 60),
            ),
            (
                "w1B:pA",
                "w1:t1",
                "Herdr-factory-go @focus, @mac-notify, @hog fixes",
                "clis-fixes-fleet",
                "~/code/side-projects/clis",
                "idle",
                Some(86_400),
            ),
            (
                "w1D:p8K",
                "w4:t3",
                "Outreach-loop cld-messaging-3 run",
                "cld-messaging-3",
                LONG_PATH,
                "done",
                Some(2 * 3_600),
            ),
            (
                "w1C:p1D",
                "w3:t2",
                "Drop 30 day filter",
                "linkedin-outreach-orchestrator",
                "~/Documents/notes-os/STARTUP/codiing-pivot/outreach-loop",
                "idle",
                Some(12 * 86_400),
            ),
        ];
        let panes: Vec<_> = panes
            .iter()
            .map(|(id, tab, title, name, path, status, age)| {
                json!({"pane_id": id, "terminal_id": format!("t-{id}"), "tab_id": tab,
                    "workspace_id": &tab[..2], "terminal_title_stripped": title,
                    "label": name, "agent": "claude", "agent_status": status, "cwd": path,
                    "started_at": age.map(|age: i64| NOW - age * 1000)})
            })
            .collect();
        let rows = crate::catalog::catalog(
            json!({
                "workspaces": [{"workspace_id":"w1","label":"SETUP","number":1},
                               {"workspace_id":"w3","label":"MISC","number":3},
                               {"workspace_id":"w4","label":"ft","number":4}],
                "tabs": [{"tab_id":"w1:t1","workspace_id":"w1","label":"skills","number":1},
                         {"tab_id":"w1:t2","workspace_id":"w1","label":"nvim","number":2},
                         {"tab_id":"w3:t1","workspace_id":"w3","label":"logseq","number":1},
                         {"tab_id":"w3:t2","workspace_id":"w3","label":"coding-pivot-reachouts","number":2},
                         {"tab_id":"w4:t1","workspace_id":"w4","label":"vh-skeleton","number":1},
                         {"tab_id":"w4:t2","workspace_id":"w4","label":"vh-themes","number":2},
                         {"tab_id":"w4:t3","workspace_id":"w4","label":"cld-messaging-3","number":3}],
                "panes": panes
            }),
            CHIP_SOURCE,
        )
        .unwrap();
        let mut app = App::with_context(CHIP_SOURCE.into(), String::new());
        app.loaded(rows);
        app
    }

    /// Grapheme indices of `needle`'s first occurrence in `text`.
    fn at(text: &str, needle: &str) -> Vec<u32> {
        let byte = text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {text:?}"));
        let start = text[..byte].graphemes(true).count() as u32;
        (start..start + needle.graphemes(true).count() as u32).collect()
    }

    fn reason(kind: ReasonKind, text: &str, needle: &str) -> Reason {
        Reason {
            kind,
            text: text.into(),
            indices: at(text, needle),
        }
    }

    fn hit(app: &App, pane: &str) -> Hit {
        Hit {
            index: app
                .candidates
                .iter()
                .position(|row| row.pane_id == pane)
                .unwrap(),
            title_indices: Vec::new(),
            workspace_indices: Vec::new(),
            tab_indices: Vec::new(),
            name_indices: Vec::new(),
            detail_indices: Vec::new(),
            reasons: Vec::new(),
            reasons_beside_name: Vec::new(),
        }
    }

    /// A hit the pane name alone explains: highlighted in the Name column, or
    /// a Name chip where the popup has no Name column.
    fn named(app: &App, pane: &str, needle: &str) -> Hit {
        let name = app.candidates[hit(app, pane).index].name.clone();
        Hit {
            name_indices: at(&name, needle),
            reasons: vec![reason(ReasonKind::Name, &name, needle)],
            ..hit(app, pane)
        }
    }

    /// A hit its path alone explains: a Path chip in every layout.
    fn pathed(app: &App, pane: &str, needle: &str) -> Hit {
        let path = app.candidates[hit(app, pane).index].path.clone();
        let reasons = vec![reason(ReasonKind::Path, &path, needle)];
        Hit {
            reasons_beside_name: reasons.clone(),
            reasons,
            ..hit(app, pane)
        }
    }

    /// A scripted search: the query plus the hits the engine would return.
    type Search = fn(&mut App);

    /// The state the engine leaves after typing `query`: these hits in this
    /// order, cursor on the first. Built by hand so these render tests never
    /// depend on the engine's matching or ranking.
    fn searched(app: &mut App, query: &str, hits: Vec<Hit>) {
        app.query = query.into();
        app.hits = hits;
        app.selected = 0;
    }

    /// `vim`: a title word, a pane name, and the `nvim` tab (spec order).
    fn vim(app: &mut App) {
        let detail = app.candidates[hit(app, "w1B:p1E").index].detail.clone();
        let hits = vec![
            Hit {
                title_indices: at("Logseq fork with vim navigation", "vim"),
                ..hit(app, "w1C:pS")
            },
            named(app, "w1B:p1D", "vim"),
            Hit {
                tab_indices: at("nvim", "vim"),
                detail_indices: at(&detail, "vim"),
                ..hit(app, "w1B:p1E")
            },
        ];
        searched(app, "vim", hits);
    }

    /// `skl/library`: four path-only matches, working agents first.
    fn skl_library(app: &mut App) {
        let hits = ["w1D:p8X", "w1D:p8Y", "w1B:p1D", "w1B:p1V"]
            .into_iter()
            .map(|pane| pathed(app, pane, "skl/library"))
            .collect();
        searched(app, "skl/library", hits);
    }

    /// `clis-fixes`: one pane whose name, not its long title, matched.
    fn clis_fixes(app: &mut App) {
        let hits = vec![named(app, "w1B:pA", "clis-fixes")];
        searched(app, "clis-fixes", hits);
    }

    /// `codiing`: the owner's typo'd directory, deep inside an 86-column path.
    fn codiing(app: &mut App) {
        let hits = ["w1D:p8K", "w1C:p1D"]
            .into_iter()
            .map(|pane| pathed(app, pane, "codiing"))
            .collect();
        searched(app, "codiing", hits);
    }

    /// Cells of `needle` in rendered row `y`, found by text.
    fn cells_of(buffer: &ratatui::buffer::Buffer, y: usize, needle: &str) -> Vec<(u16, u16)> {
        let x = col(&text(buffer)[y], needle);
        (x..x + needle.chars().count())
            .map(|x| (x as u16, y as u16))
            .collect()
    }

    fn is_match(cell: &ratatui::buffer::Cell) -> bool {
        cell.fg == Color::Yellow && cell.modifier.contains(Modifier::BOLD)
    }

    /// Asserts a chip's look on a plain row: separator and text in the
    /// context gray, exactly `matched` in the highlight.
    fn assert_chip(buffer: &ratatui::buffer::Buffer, y: usize, chip: &str, matched: &str) {
        let line = &text(buffer)[y];
        let chip_cells = cells_of(buffer, y, &format!(" · {chip}"));
        let first = col(line, &format!(" · {chip}")) + 3 + col(chip, matched);
        for (x, y) in chip_cells {
            let cell = &buffer[(x, y)];
            let in_match = (first..first + matched.chars().count()).contains(&(x as usize));
            if in_match {
                assert!(
                    is_match(cell),
                    "{chip}: {x} should be highlighted in {line}"
                );
            } else {
                assert_eq!(cell.fg, Color::DarkGray, "{chip}: {x} in {line}");
                assert!(!cell.modifier.contains(Modifier::BOLD), "{chip}: {x}");
            }
        }
    }

    #[test]
    fn tests_that_names_match_in_their_column_and_paths_chip_after_the_title() {
        // The spec's mock rows, with the Name column the popup gained since,
        // for each width. At 70 there is no Name column and the name is a chip.
        let cases: [(Search, u16, &[&str]); 6] = [
            (vim, 108, &["○ SETUP     › skills                 › vim-html-skill       › CLAUDE.md task label rules              4h"]),
            (vim, 140, &["○ SETUP     › skills                 › vim-html-skill       › CLAUDE.md task label rules                                              4h"]),
            (vim, 70, &["○ CLAUDE.md task label rules · vim-html-skill                   4h"]),
            (skl_library, 108, &[
                "› ◐ ft        › vh-skeleton            › vh-skeleton          › Vh-skeleton brief run · …/skl/library… 18m",
                "◐ ft        › vh-themes              › vh-themes            › Vh-themes brief run · …/skl/library…   17m",
                "○ SETUP     › skills                 › vim-html-skill       › CLAUDE.md task label… · …/skl/library…  4h",
            ]),
            (skl_library, 140, &[
                "› ◐ ft        › vh-skeleton            › vh-skeleton          › Vh-skeleton brief run · ~/.config/skl/library/.wt/feat/vh-skeleton     18m",
                "○ SETUP     › skills                 › agent-alias-defaults › CLAUDE.md task label rules · ~/.config/skl/library/skills               2d",
            ]),
            (skl_library, 70, &[
                "› ◐ Vh-skeleton brief run · …/skl/library/.wt/feat/vh-skeleton   18m",
                "○ CLAUDE.md task label rules · ~/.config/skl/library/skills     4h",
            ]),
        ];
        for (query, width, rows) in cases {
            let mut app = search_fixture();
            query(&mut app);
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            for expected in rows {
                assert!(
                    lines.iter().any(|line| line.trim() == *expected),
                    "{expected:?} at {width}:\n{}",
                    lines.join("\n")
                );
            }
        }
        // Styles on unselected rows: yellow letters in a gray name, whether
        // in its column or in a chip; plain title; gray path chip.
        for width in [108, 140, 70] {
            let mut app = search_fixture();
            vim(&mut app);
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            let y = row(&lines, "vim-html-skill");
            if width == 70 {
                assert_chip(&buffer, y, "vim-html-skill", "vim");
            } else {
                assert!(!lines[y].contains(" · "), "{}", lines[y]);
                let name = cells_of(&buffer, y, "vim-html-skill");
                for (i, cell) in name.into_iter().enumerate() {
                    let cell = &buffer[cell];
                    assert_eq!(is_match(cell), i < 3, "{width}: letter {i}");
                    if i >= 3 {
                        assert_eq!(cell.fg, Color::DarkGray);
                    }
                }
            }
            let (x, y) = cells_of(&buffer, y, "CLAUDE.md")[0];
            assert_eq!(buffer[(x, y)].fg, Color::Reset, "title keeps its color");

            let mut app = search_fixture();
            skl_library(&mut app);
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            let y = row(&lines, "Vh-themes brief run");
            let chip = lines[y].split(" · ").nth(1).unwrap();
            let chip = chip.split("  ").next().unwrap().trim_end();
            assert_chip(&buffer, y, chip, "skl/library");
        }
    }

    #[test]
    fn tests_that_rows_explained_by_what_they_show_get_no_chip() {
        for width in [108, 140, 70] {
            let mut app = search_fixture();
            vim(&mut app);
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            for title in [
                "Logseq fork with vim navigation",
                "Rill diff colors contrast",
            ] {
                let y = row(&lines, title);
                assert!(
                    !lines[y].contains(" · "),
                    "{title} at {width}: {}",
                    lines[y]
                );
            }
            let y = row(&lines, "Logseq fork");
            assert!(cells_of(&buffer, y, "vim")
                .into_iter()
                .all(|cell| is_match(&buffer[cell])));
            // The `nvim` tab explains its row: in the tab column, or in the
            // detail line that carries the tab on two-line rows.
            let y = if width >= 108 {
                row(&lines, "Rill diff")
            } else {
                row(&lines, "Rill diff") + 1
            };
            assert!(cells_of(&buffer, y, "vim")
                .into_iter()
                .all(|cell| is_match(&buffer[cell])));
        }
        // The empty query has no reasons, so it renders no chips.
        let app = search_fixture();
        let screen = text(&draw(&app, 108, 30));
        assert!(screen[3..screen.len() - 4]
            .iter()
            .all(|line| !line.contains(" · ")));
    }

    #[test]
    fn tests_that_a_long_path_keeps_its_match_and_leaf() {
        for (width, chip, shorter) in [
            (108, "…/codiing-pivot…", "…/codiing-pivot…"),
            (
                140,
                "…/codiing-pivot/…/cld-messaging-3",
                "…/notes-os/STARTUP/codiing-pivot/outreach-loop",
            ),
            (
                70,
                "…/codiing-pivot…",
                "…/STARTUP/codiing-pivot/outreach-loop",
            ),
        ] {
            let mut app = search_fixture();
            codiing(&mut app);
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            let y = row(&lines, "Outreach-loop");
            assert!(
                lines[y].contains(&format!(" · {chip} ")),
                "{width}: {}",
                lines[y]
            );
            assert!(
                cells_of(&buffer, y, "codiing")
                    .into_iter()
                    .all(|cell| is_match(&buffer[cell])),
                "{width}"
            );
            // The shorter path drops leading directories but keeps its leaf
            // where there is room for one.
            let y = row(&lines, "Drop 30 day filter");
            assert!(lines[y].contains(&format!(" · {shorter} ")), "{}", lines[y]);
            assert_chip(&buffer, y, shorter, "codiing");
        }
    }

    #[test]
    fn tests_that_a_long_title_gives_way_to_a_whole_name() {
        // As a chip, the name stays whole and the title is cut instead.
        let mut app = search_fixture();
        clis_fixes(&mut app);
        let lines = text(&draw(&app, 70, 30));
        let y = row(&lines, "Herdr-factory-go");
        assert!(
            lines[y].contains("Herdr-factory-go @focus, @mac-notify, @ho… · clis-fixes-fleet 1d"),
            "{}",
            lines[y]
        );
        // In its column, the name leaves the whole Task cell to the title.
        for (width, expected) in [
            (
                108,
                "› clis-fixes-fleet     › Herdr-factory-go @focus, @mac-notify,…  1d",
            ),
            (
                140,
                "› clis-fixes-fleet     › Herdr-factory-go @focus, @mac-notify, @hog fixes ",
            ),
        ] {
            let lines = text(&draw(&app, width, 30));
            let y = row(&lines, "Herdr-factory-go");
            assert!(lines[y].contains(expected), "{width}: {}", lines[y]);
        }
    }

    #[test]
    fn tests_that_two_reasons_show_name_then_path_and_paths_give_way_first() {
        let mut app = search_fixture();
        let path = reason(ReasonKind::Path, "~/.config/skl/library/skills", "library");
        let hits = vec![Hit {
            reasons: vec![
                reason(ReasonKind::Name, "vim-html-skill", "vim"),
                path.clone(),
            ],
            reasons_beside_name: vec![path],
            ..named(&app, "w1B:p1D", "vim")
        }];
        searched(&mut app, "vim library", hits);
        // Without a Name column both are chips, the name first and whole.
        // With one, the name is highlighted in its column and only the path
        // is a chip.
        for (width, expected) in [
            (
                70,
                "CLAUDE.md task label rules · vim-html-skill · …/library… ",
            ),
            (
                140,
                "vim-html-skill       › CLAUDE.md task label rules · ~/.config/skl/library/skills ",
            ),
            (
                108,
                "vim-html-skill       › CLAUDE.md task lab… · …/library/skills ",
            ),
        ] {
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            let y = row(&lines, "CLAUDE.md");
            assert!(lines[y].contains(expected), "{width}: {}", lines[y]);
            for matched in ["vim", "library"] {
                let x = col(&lines[y], expected) + col(expected, matched);
                assert!(
                    is_match(&buffer[(x as u16, y as u16)]),
                    "{matched} at {width}"
                );
            }
        }
    }

    #[test]
    fn tests_that_separate_matches_in_one_path_drop_the_directories_between() {
        let path = format!("~/alpha/{}omega", "unrelated-directory/".repeat(4));
        let mut app = search_fixture();
        let index = hit(&app, "w1C:pS").index;
        app.candidates[index].path = path.clone();
        let mut indices = at(&path, "alpha");
        indices.extend(at(&path, "omega"));
        let reasons = vec![Reason {
            kind: ReasonKind::Path,
            text: path,
            indices,
        }];
        let hits = vec![Hit {
            reasons_beside_name: reasons.clone(),
            reasons,
            ..hit(&app, "w1C:pS")
        }];
        searched(&mut app, "alpha omega", hits);
        for width in [140, 70] {
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            let y = row(&lines, "Logseq fork");
            // The whole title, both matched directories, and the age all fit.
            assert!(
                lines[y].contains("Logseq fork with vim navigation · …/alpha/…/omega ")
                    && lines[y].trim_end().ends_with("12d"),
                "{width}: {}",
                lines[y]
            );
            for word in ["alpha", "omega"] {
                assert!(cells_of(&buffer, y, word)
                    .into_iter()
                    .all(|cell| is_match(&buffer[cell])));
            }
        }
    }

    #[test]
    fn tests_that_the_details_drop_a_path_with_no_room_rather_than_the_id() {
        let mut app = search_fixture();
        vim(&mut app);
        // 56 + ` · ` + `w1C:pS` + ` · ` fills all 68 cells, leaving the path none.
        let index = app.hits[0].index;
        app.candidates[index].name = "n".repeat(56);
        let lines = text(&draw(&app, 70, 30));
        let expected = format!("{} · w1C:pS", "n".repeat(56));
        assert!(
            lines.iter().any(|line| line.trim() == expected),
            "{}",
            lines.join("\n")
        );
    }

    #[test]
    fn tests_that_selected_marked_and_inert_rows_keep_their_chip() {
        use ratatui::crossterm::event::KeyCode;
        // Jump was opened from the vim-html-skill pane, so its row is inert.
        let mut app = search_fixture();
        app.handle(ctrl('x'));
        skl_library(&mut app);
        app.handle(press(KeyCode::Down));
        app.handle(press(KeyCode::Tab)); // marks vh-themes; the cursor moves to the source
        app.handle(press(KeyCode::Up));
        app.handle(press(KeyCode::Up)); // back to vh-skeleton
        for width in [108, 140, 70] {
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            // Selected: the chip turns bar-gray, its letters stay yellow.
            let y = row(&lines, "Vh-skeleton brief run");
            for cell in cells_of(&buffer, y, "skl/library") {
                assert!(is_match(&buffer[cell]), "{width}");
                assert_eq!(buffer[cell].bg, Color::DarkGray);
            }
            let (x, y) = cells_of(&buffer, y, " · ")[1];
            assert_eq!(
                (buffer[(x, y)].fg, buffer[(x, y)].bg),
                (Color::Gray, Color::DarkGray)
            );
            // Marked: red title, chip unchanged.
            let y = row(&lines, "Vh-themes brief run");
            assert_eq!(buffer[(1, y as u16)].symbol(), "■");
            assert_eq!(buffer[cells_of(&buffer, y, "Vh-themes")[0]].fg, Color::Red);
            for cell in cells_of(&buffer, y, "skl/library") {
                assert!(is_match(&buffer[cell]), "{width}");
            }
            // Inert source: the chip stays (same layout), all muted, no yellow.
            let y = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.contains("CLAUDE.md task label"))
                .map(|(y, _)| y)
                .find(|&y| buffer[cells_of(&buffer, y, "CLAUDE.md")[0]].fg == Color::DarkGray)
                .expect("the source row renders muted");
            assert!(
                lines[y].contains(" · ") && lines[y].contains("skl/library"),
                "{}",
                lines[y]
            );
            let x = col(&lines[y], " · ");
            for x in x..lines[y].trim_end().chars().count() {
                let cell = &buffer[(x as u16, y as u16)];
                assert!(!is_match(cell), "inert chip at {x}, width {width}");
            }
        }
    }

    #[test]
    fn tests_that_the_details_name_the_pane_only_where_its_row_does_not() {
        let mut app = search_fixture();
        vim(&mut app);
        app.selected = 1;
        for (width, expected) in [
            (108, "~/.config/skl/library/skills · w1B:p1D"),
            (140, "~/.config/skl/library/skills · w1B:p1D"),
            (
                70,
                "vim-html-skill · ~/.config/skl/library/skills · w1B:p1D",
            ),
        ] {
            let lines = text(&draw(&app, width, 30));
            let detail = lines
                .iter()
                .position(|line| line.trim() == expected)
                .unwrap_or_else(|| panic!("{expected:?} at {width}:\n{}", lines.join("\n")));
            assert_eq!(
                lines[detail - 1].trim(),
                "SETUP › skills · skills · claude idle"
            );
        }
        // The preview halves the results, so rows stack and the name returns.
        app.preview_visible = true;
        let lines = text(&draw(&app, 108, 30));
        assert!(lines
            .iter()
            .any(|line| line.trim() == "vim-html-skill · ~/.config/skl/library/skills · w1B:p1D"));
        app.preview_visible = false;
        // An unnamed pane shows its path and id in every layout.
        app.selected = 0;
        for width in [108, 70] {
            let lines = text(&draw(&app, width, 30));
            assert!(lines
                .iter()
                .any(|line| line.trim() == "~/code/oss/logseq · w1C:pS"));
        }
        // A long path drops leading directories; name, leaf, and id stay whole.
        codiing(&mut app);
        for (width, expected) in [
            (108, "~/Documents/notes-os/STARTUP/codiing-pivot/outreach-loop/ops/.wt/feat/cld-messaging-3 · w1D:p8K"),
            (70, "cld-messaging-3 · …/ops/.wt/feat/cld-messaging-3 · w1D:p8K"),
        ] {
            let lines = text(&draw(&app, width, 30));
            assert!(
                lines.iter().any(|line| line.trim() == expected),
                "{width}:\n{}",
                lines.join("\n")
            );
        }
    }

    #[test]
    fn tests_that_names_take_the_breadcrumb_style_in_every_row_state() {
        use ratatui::crossterm::event::KeyCode;
        let mut app = search_fixture();
        vim(&mut app);
        app.selected = 1;
        let buffer = draw(&app, 108, 30);
        let y = row(&text(&buffer), "vim-html-skill");
        // Selected: the bar's context gray, the matched letters still yellow.
        for (i, cell) in cells_of(&buffer, y, "vim-html-skill")
            .into_iter()
            .enumerate()
        {
            let cell = &buffer[cell];
            assert_eq!(cell.bg, Color::DarkGray, "{i}");
            assert_eq!(is_match(cell), i < 3, "{i}");
            if i >= 3 {
                assert_eq!(cell.fg, Color::Gray, "{i}");
            }
        }
        // Inert (Jump was opened from this pane): muted, and no highlight
        // invites an action, though the name matched.
        app.handle(ctrl('x'));
        vim(&mut app);
        app.handle(press(KeyCode::Down));
        let buffer = draw(&app, 108, 30);
        let y = row(&text(&buffer), "vim-html-skill");
        for cell in cells_of(&buffer, y, "vim-html-skill") {
            assert!(!is_match(&buffer[cell]));
            assert_eq!(buffer[cell].fg, Color::Gray, "inert under the cursor");
        }
        // Unnamed: a blank Name cell and no `›` pointing at it; the title
        // starts in the same column as every named row's.
        let lines = text(&buffer);
        let unnamed = &lines[row(&lines, "Logseq fork")];
        let named = &lines[row(&lines, "Rill diff")];
        assert_eq!(unnamed.matches('›').count(), named.matches('›').count() - 1);
        assert_eq!(col(unnamed, "Logseq fork"), col(named, "Rill diff"));
        assert_eq!(col(unnamed, "› Logseq"), col(named, "› Rill"));
    }

    #[test]
    fn tests_that_the_name_column_fits_its_names_and_gives_way_to_the_task() {
        let mut app = search_fixture();
        let task = |columns: &Columns, width: usize| {
            let name = usize::from(columns.name);
            width
                - 4
                - usize::from(columns.workspace)
                - 3
                - usize::from(columns.tab)
                - 3
                - if name > 0 { name + 3 } else { 0 }
                - 1
                - usize::from(columns.age)
        };
        // The widest name (30 cells) is capped; below the cap the column
        // narrows to keep Task readable, and below the floor it goes away.
        for (width, name) in [(106, 20), (100, 20), (98, 18), (92, 12), (91, 0), (78, 0)] {
            let rows: Vec<_> = app.candidates.iter().collect();
            let columns = Columns::new(&rows, width, 3);
            assert_eq!(columns.name, name, "{width}");
            assert!(task(&columns, width as usize) >= TASK_MIN, "{width}");
        }
        // Short names take only their own width, and at least the heading's.
        for row in &mut app.candidates {
            row.name = row.name.chars().take(2).collect();
        }
        let rows: Vec<_> = app.candidates.iter().collect();
        assert_eq!(Columns::new(&rows, 106, 3).name, 4);
        // Without names there is no column to fill.
        for row in &mut app.candidates {
            row.name.clear();
        }
        let rows: Vec<_> = app.candidates.iter().collect();
        assert_eq!(Columns::new(&rows, 106, 3).name, 0);
    }

    #[test]
    fn tests_that_a_name_column_too_narrow_to_show_gives_its_matches_back_to_chips() {
        let mut app = search_fixture();
        vim(&mut app);
        app.selected = 1;
        // 94 leaves the column its 12-cell floor; 93 does not.
        let buffer = draw(&app, 94, 30);
        let lines = text(&buffer);
        assert!(lines[row(&lines, "Workspace")].contains("› Name"));
        let y = row(&lines, "vim-html-sk…");
        assert!(!lines[y].contains(" · "), "{}", lines[y]);
        assert!(cells_of(&buffer, y, "vim")
            .into_iter()
            .all(|cell| is_match(&buffer[cell])));
        let lines = text(&draw(&app, 93, 30));
        let heading = &lines[row(&lines, "Workspace")];
        assert!(heading.contains("Tab") && heading.contains("› Task") && !heading.contains("Name"));
        let y = row(&lines, "CLAUDE.md");
        assert!(
            lines[y].contains("CLAUDE.md task label rules · vim-html-skill"),
            "{}",
            lines[y]
        );
        assert!(lines
            .iter()
            .any(|line| line.trim() == "vim-html-skill · ~/.config/skl/library/skills · w1B:p1D"));
    }

    #[test]
    fn tests_that_a_name_too_long_for_its_column_keeps_its_match() {
        // `vim` ends a 24-cell name: cut from the end at the 20-cell cap (or
        // the 12-cell floor at 94), the row would show no reason to match.
        let mut app = search_fixture();
        let pane = "w1C:p1D";
        let index = hit(&app, pane).index;
        app.candidates[index].name = "review-browseros-neo-vim".into();
        let hits = vec![named(&app, pane, "vim")];
        searched(&mut app, "vim", hits);
        for (width, cell) in [(108, "…w-browseros-neo-vim"), (94, "…ros-neo-vim")] {
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            let y = row(&lines, "Drop 30 day filter");
            assert!(
                lines[y].contains(&format!("› {cell} › Drop")),
                "{width}: {}",
                lines[y]
            );
            assert!(!lines[y].contains(" · "), "{width}: {}", lines[y]);
            let x = col(&lines[y], cell) + cell.chars().count() - 3;
            for x in x..x + 3 {
                assert!(is_match(&buffer[(x as u16, y as u16)]), "{width}: {x}");
            }
            // The column shortened it, so the details show it whole.
            assert!(lines
                .iter()
                .any(|line| line.trim().starts_with("review-browseros-neo-vim · ")));
        }
        // The same holds for a tab past the Tab column's 24-cell cap.
        let tab = "coding-pivot-reachouts-old-vim";
        app.candidates[index].tab = tab.into();
        let hits = vec![Hit {
            tab_indices: at(tab, "vim"),
            ..hit(&app, pane)
        }];
        searched(&mut app, "vim", hits);
        let buffer = draw(&app, 108, 30);
        let lines = text(&buffer);
        let y = row(&lines, "Drop 30 day filter");
        assert!(
            lines[y].contains("› …pivot-reachouts-old-vim ›"),
            "{}",
            lines[y]
        );
        let x = col(&lines[y], "-old-vim") + 5;
        for x in x..x + 3 {
            assert!(is_match(&buffer[(x as u16, y as u16)]), "{x}");
        }
    }

    #[test]
    fn tests_that_elision_measures_cells_and_never_drops_matched_letters() {
        let path = "~/項目/日本語のディレクトリ/skl/library/ノート";
        let deep = reason(ReasonKind::Path, path, "skl");
        for room in 0..=path.width() + 2 {
            let chip = Elided::reason(&deep, room);
            let shown = chip.text();
            assert!(shown.contains("skl"), "{room}: {shown}");
            // Only the matched letters may overflow the room.
            assert!(
                chip.width() <= room.max("…skl…".width()),
                "{room}: {shown} is {}",
                chip.width()
            );
            assert_eq!(chip.width(), shown.width(), "{shown}");
        }
        let name = reason(
            ReasonKind::Name,
            "linkedin-outreach-orchestrator",
            "orchestrator",
        );
        for (room, expected) in [
            (30, "linkedin-outreach-orchestrator"),
            (24, "…n-outreach-orchestrator"),
            (22, "…outreach-orchestrator"),
            (13, "…orchestrator"),
            (8, "…orchestrator"),
        ] {
            assert_eq!(Elided::reason(&name, room).text(), expected, "{room}");
        }
        let name = reason(
            ReasonKind::Name,
            "linkedin-outreach-orchestrator",
            "linkedin",
        );
        assert_eq!(Elided::reason(&name, 12).text(), "linkedin-ou…");
    }

    /// Writes the spec's three queries (and the long path) at 70, 108, and 140
    /// as text dumps plus a map of highlighted cells, for review evidence:
    /// `JUMP_RENDER_DIR=/abs/dir cargo test -- --ignored renders`.
    #[test]
    #[ignore = "writes review evidence to $JUMP_RENDER_DIR"]
    fn renders() {
        let dir = std::path::PathBuf::from(std::env::var("JUMP_RENDER_DIR").unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        let queries: [(&str, Search); 4] = [
            ("vim", vim),
            ("skl-library", skl_library),
            ("clis-fixes", clis_fixes),
            ("codiing", codiing),
        ];
        for (name, query) in queries {
            for width in [70, 108, 140] {
                let mut app = search_fixture();
                query(&mut app);
                let buffer = draw(&app, width, 24);
                let lines = text(&buffer);
                let marks: Vec<String> = lines
                    .iter()
                    .enumerate()
                    .map(|(y, line)| {
                        (0..line.chars().count())
                            .map(|x| {
                                let cell = &buffer[(x as u16, y as u16)];
                                if is_match(cell) {
                                    '^'
                                } else if cell.fg == Color::DarkGray && cell.symbol() != " " {
                                    '.'
                                } else {
                                    ' '
                                }
                            })
                            .collect::<String>()
                            .trim_end()
                            .to_owned()
                    })
                    .collect();
                let dump = lines
                    .iter()
                    .zip(&marks)
                    .map(|(line, marks)| format!("{}\n{marks}", line.trim_end()))
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(dir.join(format!("{name}-{width}.txt")), lines.join("\n")).unwrap();
                std::fs::write(dir.join(format!("{name}-{width}.styled.txt")), dump).unwrap();
            }
        }
    }

    #[test]
    fn tests_that_each_state_shows_herdrs_glyph_in_herdrs_color() {
        let app = fixture();
        for width in [70, 110, 140] {
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            for (title, glyph, color) in [
                ("Review RTR sessions", "✓", Color::Cyan),
                ("Trigger release build", "◐", Color::Yellow),
                ("Drop 30 day filter", "○", Color::Green),
                ("nvim", "·", Color::DarkGray),
            ] {
                let y = row(&lines, title) as u16;
                let cell = &buffer[(3, y)];
                assert_eq!(
                    (cell.symbol(), cell.fg),
                    (glyph, color),
                    "{title} at {width}"
                );
            }
            // The selected (blocked) row keeps the shape in the selection fg.
            let y = row(&lines, "Debug BrowserOS") as u16;
            assert_eq!(buffer[(1, y)].symbol(), "›");
            assert_eq!(
                (buffer[(3, y)].symbol(), buffer[(3, y)].fg),
                ("×", Color::White)
            );
        }
    }

    #[test]
    fn tests_that_ages_share_one_dim_right_aligned_column() {
        let app = fixture();
        for width in [70, 110, 140] {
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            let right_edge = width as usize - 1;
            for (title, age) in [
                ("Debug BrowserOS", "4m"),
                ("Review RTR", "12m"),
                ("Trigger release", "45s"),
                ("Update Claude", "2h"),
                ("Optimize Sentry", "1d"),
                ("Drop 30 day", "12d"),
            ] {
                let line = lines[row(&lines, title)].trim_end();
                assert!(line.ends_with(age), "{title}: {line:?}");
                assert_eq!(line.chars().count(), right_edge, "{title} at {width}");
                if title != "Debug BrowserOS" {
                    let y = row(&lines, title) as u16;
                    assert_eq!(buffer[(right_edge as u16 - 1, y)].fg, Color::DarkGray);
                }
            }
            // Unknown and neutral starts stay blank rather than inventing an age.
            for title in ["Fresh agent without transcript", "nvim"] {
                let line = lines[row(&lines, title)].trim_end();
                assert!(
                    line.ends_with(title),
                    "{title} has no age at {width}: {line:?}"
                );
            }
            // One-line columns label the age column; two-line rows have no header row.
            if width >= 110 {
                let heading = lines[row(&lines, "Workspace")].trim_end();
                assert!(heading.ends_with("Age"));
                assert_eq!(heading.chars().count(), right_edge);
            } else {
                assert!(!lines.iter().any(|line| line.contains("Workspace")));
            }
        }
    }

    #[test]
    fn tests_that_header_names_every_view_with_counts_and_pills_the_active_one() {
        for width in [70, 110, 140] {
            let mut app = fixture();
            let mut strips = Vec::new();
            for active in [
                " All 8 ",
                " In progress 2 ",
                " Unread/Blocked 2 ",
                " Idle 3 ",
            ] {
                let buffer = draw(&app, width, 30);
                let header = text(&buffer).remove(0);
                assert!(header.contains("Jump to session"), "{header}");
                for segment in ["All 8", "In progress 2", "Unread/Blocked 2", "Idle 3"] {
                    let x = col(&header, segment) as u16;
                    let pill = buffer[(x, 0)].bg == Color::Yellow;
                    assert_eq!(pill, active.trim() == segment, "{segment} at {width}");
                }
                let x = col(&header, active);
                for i in 0..active.chars().count() {
                    assert_eq!(buffer[((x + i) as u16, 0)].bg, Color::Yellow);
                }
                strips.push(header);
                app.handle(ctrl('e'));
            }
            // Only the pill moves while ctrl-e cycles; the text never reflows.
            assert!(
                strips.windows(2).all(|pair| pair[0] == pair[1]),
                "{strips:#?}"
            );
        }
        // Too narrow for every label: inactive views become glyph counts.
        let mut app = fixture();
        app.handle(ctrl('e'));
        let header = text(&draw(&app, 60, 30)).remove(0);
        for part in ["Jump to session", "All 8", " In progress 2 ", "×✓ 2", "○ 3"] {
            assert!(header.contains(part), "{part:?} in {header:?}");
        }
    }

    #[test]
    fn tests_that_cycling_views_never_changes_the_header_form() {
        // How many views the header names (full label or glyph count) while
        // `active` is selected. The form may only depend on the width.
        let named = |header: &str| {
            [
                &["All 8"][..],
                &["In progress 2", "◐ 2"],
                &["Unread/Blocked 2", "×✓ 2"],
                &["Idle 3", "○ 3"],
            ]
            .iter()
            .filter(|forms| forms.iter().any(|form| header.contains(form)))
            .count()
        };
        for width in 30..=90u16 {
            let mut app = fixture();
            let mut forms = Vec::new();
            for _ in View::CYCLE {
                let header = text(&draw(&app, width, 30)).remove(0);
                forms.push((named(&header), header.contains(TITLE)));
                app.handle(ctrl('e'));
            }
            assert!(
                forms.windows(2).all(|pair| pair[0] == pair[1]),
                "width {width}: {forms:?}"
            );
            assert!(
                forms[0].0 >= 1,
                "the active view is always named at width {width}"
            );
        }
    }

    #[test]
    fn tests_that_marks_are_visible_as_text_and_the_source_cannot_look_markable() {
        let mut app = fixture();
        mark_two(&mut app);
        for width in [70, 110, 140] {
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            assert!(lines[0].contains("2 marked"), "{}", lines[0]);
            assert_eq!(buffer[(1, 2)].fg, Color::Red, "kill rule");
            for title in ["Debug BrowserOS", "Trigger release"] {
                let y = row(&lines, title) as u16;
                let x = col(&lines[y as usize], title) as u16;
                let (mark, name) = (&buffer[(1, y)], &buffer[(x, y)]);
                assert_eq!((mark.symbol(), mark.fg), ("■", Color::Red));
                assert_eq!(name.fg, Color::Red);
                assert!(name.modifier.contains(Modifier::BOLD));
            }
            for title in ["Review RTR", "Drop 30 day", "Update Claude"] {
                let y = row(&lines, title);
                assert_eq!(&buffer[(1, y as u16)].symbol(), &" ", "{title}");
            }
            // The cursor moved past both marks; it stays the plain `›`.
            assert_eq!(buffer[(1, row(&lines, "Fresh agent") as u16)].symbol(), "›");
            let y = row(&lines, "Update Claude aliases");
            let x = col(&lines[y], "Update Claude aliases") as u16;
            assert_eq!(
                buffer[(x, y as u16)].fg,
                Color::DarkGray,
                "source pane is inert"
            );
            assert_eq!(buffer[(3, y as u16)].fg, Color::DarkGray);
        }
        // Marks outside the current view are announced before review.
        app.handle(ctrl('e'));
        assert!(text(&draw(&app, 110, 30))[0].contains("Kill mode · 2 marked, 1 hidden"));
    }

    #[test]
    fn tests_that_the_cursor_neither_dresses_up_the_source_nor_hides_a_mark() {
        use ratatui::crossterm::event::KeyCode;
        for width in [70, 110, 140] {
            // The source under the cursor keeps the bar (so the cursor is
            // findable) but no `›`, no bright text, and no match highlights.
            let mut app = fixture();
            app.handle(ctrl('x'));
            app.handle(Event::Paste("update claude".into()));
            assert_eq!(app.selected().unwrap().pane_id, SOURCE);
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            let y = row(&lines, "Update Claude aliases");
            let x = col(&lines[y], "Update Claude aliases");
            let y = y as u16;
            assert_eq!(buffer[(1, y)].symbol(), " ", "no cursor mark on the source");
            assert_eq!(buffer[(3, y)].fg, Color::Gray);
            for i in 0.."Update Claude".len() {
                let cell = &buffer[((x + i) as u16, y)];
                assert_eq!((cell.fg, cell.bg), (Color::Gray, Color::DarkGray), "{i}");
            }

            // Revisiting a mark keeps both the square and the bold title.
            let mut app = fixture();
            app.handle(ctrl('x'));
            app.handle(press(KeyCode::Tab));
            app.handle(press(KeyCode::Up));
            let buffer = draw(&app, width, 30);
            let lines = text(&buffer);
            let y = row(&lines, "Debug BrowserOS");
            let x = col(&lines[y], "Debug BrowserOS") as u16;
            let y = y as u16;
            assert_eq!(buffer[(1, y)].symbol(), "■");
            assert!(buffer[(1, y)].modifier.contains(Modifier::BOLD));
            assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
            assert_eq!(buffer[(x, y)].bg, Color::DarkGray, "still the cursor row");
        }
    }

    #[test]
    fn tests_that_review_lists_exactly_the_frozen_targets_in_kill_order() {
        use ratatui::crossterm::event::KeyCode;
        let mut app = fixture();
        app.handle(ctrl('x'));
        app.handle(press(KeyCode::Tab)); // blocked row, All view
        app.handle(ctrl('e')); // In progress view hides that mark
        app.handle(press(KeyCode::Tab)); // youngest working row
        app.handle(press(KeyCode::Enter));
        assert_eq!(app.mode(), Mode::Confirm);
        let others = [
            "Review RTR",
            "Fresh agent",
            "Update Claude",
            "Optimize Sentry",
            "Drop 30 day",
        ];
        for width in [70, 110, 140] {
            let lines = text(&draw(&app, width, 24));
            let screen = lines.join("\n");
            assert_eq!(lines[0].trim(), "Kill 2 panes?");
            let first = row(&lines, "Debug BrowserOS Neo crashes");
            let second = row(&lines, "Trigger release build");
            assert!(first < second);
            assert!(lines[first].contains('■') && lines[second].contains('■'));
            for other in others {
                assert!(!screen.contains(other), "{other} must not be listed");
            }
            assert_eq!(
                lines.last().unwrap().trim(),
                "y kill 2 panes   any other key cancels"
            );
        }
    }

    #[test]
    fn tests_that_review_overflow_keeps_the_exact_count() {
        use ratatui::crossterm::event::KeyCode;
        let mut app = fixture();
        app.handle(ctrl('x'));
        for _ in 0..4 {
            app.handle(press(KeyCode::Tab));
        }
        app.handle(press(KeyCode::Enter));
        let lines = text(&draw(&app, 110, 10));
        assert_eq!(lines[0].trim(), "Kill 4 panes?");
        assert_eq!(lines.iter().filter(|line| line.contains('■')).count(), 2);
        assert!(lines.iter().any(|line| line.trim() == "+2 more"));
    }

    #[test]
    fn tests_that_hint_lines_fit_every_width_as_whole_hints() {
        use ratatui::crossterm::event::KeyCode;
        // The spec's hint table: each mode shows the widest variant that fits.
        let browse = [
            "↑↓ select   enter jump   ctrl-e filter   ctrl-x kill   ctrl-space preview   ctrl-r refresh   esc close",
            "↑↓ select  enter jump  ctrl-e filter  ctrl-x kill  ctrl-space preview  esc close",
            "↑↓ select  enter jump  ^e filter  ^x kill  ^space preview  esc close",
            "↑↓ select  enter jump  ^e filter  ^x kill  esc close",
            "↑↓  enter jump  ^e filter  ^x kill  esc close",
            "enter  ^e filter  ^x kill  esc",
            "enter  esc",
        ];
        let kill = [
            "↑↓ move   tab/space mark   enter review   ctrl-e filter   ctrl-space preview   esc cancel",
            "↑↓ move  tab/space mark  enter review  ctrl-e filter  esc cancel",
            "↑↓ move  tab mark  enter review  ^e filter  esc cancel",
            "tab mark  enter review  ^e filter  esc cancel",
            "tab mark  enter review  esc cancel",
            "tab  enter  esc",
        ];
        let review = [
            "y kill 2 panes   any other key cancels",
            "y kill   other keys cancel",
            "y kill",
        ];
        let mut killing = fixture();
        mark_two(&mut killing);
        let mut reviewing = fixture();
        mark_two(&mut reviewing);
        reviewing.handle(press(KeyCode::Enter));
        for (app, variants) in [
            (&fixture(), &browse[..]),
            (&killing, &kill[..]),
            (&reviewing, &review[..]),
        ] {
            for width in 6..=160u16 {
                let footer = text(&draw(app, width, 24)).pop().unwrap();
                let footer = footer.trim();
                let fits: Vec<_> = variants
                    .iter()
                    .filter(|hint| hint.width() + 2 <= width as usize)
                    .collect();
                assert_eq!(
                    fits.first().map_or("", |hint| **hint),
                    footer,
                    "width {width}"
                );
            }
        }
        // The new keys are taught at every width that fits more than basics.
        for width in [47, 70, 110, 140] {
            let footer = text(&draw(&fixture(), width, 24)).pop().unwrap();
            assert!(
                footer.contains("e filter") && footer.contains("x kill"),
                "{footer}"
            );
        }
    }

    #[test]
    fn tests_that_empty_views_name_the_state_and_point_at_the_next_filter() {
        let mut app = App::with_context(SOURCE.into(), String::new());
        app.loaded(
            fixture_rows()
                .into_iter()
                .filter(|row| row.state != AgentState::Working)
                .collect(),
        );
        app.handle(ctrl('e'));
        let screen = text(&draw(&app, 110, 24)).join("\n");
        assert!(screen.contains("In progress 0"));
        assert!(screen.contains("No agents are working · ctrl-e next filter"));
        app.handle(ctrl('e'));
        app.handle(Event::Paste("zzzz".into()));
        let screen = text(&draw(&app, 110, 24)).join("\n");
        assert!(screen.contains("No matching panes in this view · ctrl-e next filter"));
        app.handle(ctrl('e'));
        app.handle(ctrl('e'));
        let screen = text(&draw(&app, 110, 24)).join("\n");
        assert!(screen.contains("No matching panes"));
        assert!(!screen.contains("in this view"));
    }

    #[test]
    fn tests_that_typing_counts_hits_within_the_active_view() {
        let mut app = fixture();
        app.handle(Event::Paste("trigger".into()));
        let query = text(&draw(&app, 110, 24)).remove(1);
        assert!(query.trim_end().ends_with("1/8"), "{query}");
        app.handle(ctrl('e'));
        let query = text(&draw(&app, 110, 24)).remove(1);
        assert!(query.trim_end().ends_with("1/2"), "{query}");
        app.handle(ctrl('u'));
        let query = text(&draw(&app, 110, 24)).remove(1);
        assert!(!query.contains('/'), "{query}");
    }

    #[test]
    fn tests_that_ranked_and_scrolled_rows_keep_their_workspace_and_tab() {
        use serde_json::json;
        let panes: Vec<_> = (0..20)
            .map(|i| {
                json!({"pane_id":format!("w1:p{i}"),"terminal_id":format!("t{i}"),
                "tab_id":"w1:t1","workspace_id":"w1","title":format!("Task {i:02}")})
            })
            .collect();
        let mut app = App::new();
        app.loaded(
            crate::catalog::catalog(
                json!({
                    "workspaces":[{"workspace_id":"w1","label":"SETUP"}],
                    "tabs":[{"tab_id":"w1:t1","workspace_id":"w1","label":"herdr"}],
                    "panes":panes
                }),
                "w1:p0",
            )
            .unwrap(),
        );
        app.handle(Event::Paste("task".into()));
        app.selected = 19;
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let lines: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(100)
            .map(|cells| cells.iter().map(|c| c.symbol()).collect())
            .collect();
        // No pane has a name, so there is no empty Name column.
        assert!(lines.iter().any(|line| line.contains("Workspace › Tab")
            && line.contains("› Task")
            && !line.contains("Name")));
        assert!(lines.iter().any(|line| line.contains("SETUP")
            && line.contains("herdr")
            && line.contains("Task 19")));
    }

    /// The notice's rendered lines: below the rule, the only rows drawn in
    /// the accent color on a popup with no catalog (body and hints are muted).
    fn notice_lines(app: &App, width: u16) -> Vec<String> {
        let buffer = draw(app, width, 16);
        text(&buffer)
            .into_iter()
            .enumerate()
            .skip(3)
            .filter(|(y, line)| {
                line.chars()
                    .enumerate()
                    .any(|(x, ch)| ch != ' ' && buffer[(x as u16, *y as u16)].fg == Color::Yellow)
            })
            .map(|(_, line)| line.trim().to_owned())
            .collect()
    }

    #[test]
    fn tests_that_notices_wrap_between_words() {
        for message in [
            "Killed 1 of 3 · 1 already closed · refused “Explain RNN weight sharing”: changed since it was marked",
            "pane.get (pane_not_found): pane w1:p2 not found · Edit or move selection to retry",
            // `لا` measures one cell as a string but renders as two graphemes;
            // sizing words by whole-string width would clip the last `x`.
            &format!("pane.get (pane_not_found): لا{}", "x".repeat(20)),
            &format!("pane.get (pane_not_found): لا{}", "x".repeat(40)),
        ] {
            for width in [50, 70] {
                let mut app = App::new();
                app.message = Some(message.into());
                let lines = notice_lines(&app, width);
                assert!((1..=3).contains(&lines.len()), "{lines:?}");
                // Every word arrives whole and in order: no word was split.
                let words: Vec<_> = lines.iter().flat_map(|line| line.split(' ')).collect();
                assert_eq!(words, message.split(' ').collect::<Vec<_>>(), "width {width}");
                assert!(lines.iter().all(|line| line.width() <= width as usize - 2));
            }
        }
    }

    #[test]
    fn tests_that_an_overlong_notice_word_still_breaks_and_stays_clean() {
        let mut app = App::new();
        app.message = Some(format!("Refused {}\u{1b}[31m done", "w1:p".repeat(30)));
        let lines = notice_lines(&app, 50);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "Refused");
        assert!(lines[1..].iter().all(|line| line.width() == 48));
        assert!(lines.concat().chars().all(|ch| !ch.is_control()));
    }

    #[test]
    fn tests_that_narrow_failure_keeps_recovery_instructions_visible() {
        let mut terminal = Terminal::new(TestBackend::new(50, 16)).unwrap();
        let mut app = App::new();
        app.message =
            Some("Selected pane is no longer available. Edit or move selection to retry".into());
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("retry"));
    }

    #[test]
    fn tests_that_empty_popup_prompts_for_immediate_typing() {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let app = App::new();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Type a session"));
    }

    #[test]
    fn tests_that_unicode_query_cursor_fits_tiny_and_ultrawide_views() {
        for (width, height) in [(1, 1), (24, 8), (60, 20), (100, 28), (240, 50)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut app = App::new();
            app.handle(Event::Paste(
                "日本語 café really long query 日本語".repeat(10),
            ));
            app.preview_visible = true;
            terminal.draw(|frame| render(&app, frame)).unwrap();
            if width >= 24 {
                let cursor = terminal.get_cursor_position().unwrap();
                assert!(cursor.x > 0 && cursor.x < width && cursor.y < height);
            }
        }
    }
}
