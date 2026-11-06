//! The popup's local state machine. It never performs host I/O, so query editing
//! and selection can be tested independently of terminal and worker timing.
//!
//! Two independent membership filters decide which catalog rows are searched:
//! the [`Reach`] (Herdr's focus/hide scope, or every pane; ctrl-o) and the
//! status [`View`] (ctrl-e). Counts in the header follow the reach, so a view's
//! count is always what it would list.
use crate::catalog::{self, AgentState, Candidate, Hit, Listing};
use crate::prompt;
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

/// Whether the list honors Herdr's focus/hide scope. Each popup opens in
/// `Scope`, as Find always behaved; `Everything` is a peek outside the scope
/// that changes nothing in Herdr, since a jump never touches the scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    Scope,
    Everything,
}
impl Reach {
    pub const BOTH: [Reach; 2] = [Self::Scope, Self::Everything];
    fn other(self) -> Self {
        match self {
            Self::Scope => Self::Everything,
            Self::Everything => Self::Scope,
        }
    }
}

/// A catalog-wide membership filter, independent of the current fuzzy query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    All,
    InProgress,
    Attention,
    Idle,
}
impl View {
    pub const CYCLE: [View; 4] = [Self::All, Self::InProgress, Self::Attention, Self::Idle];
    pub fn next(self) -> Self {
        Self::CYCLE[(Self::CYCLE.iter().position(|v| *v == self).unwrap() + 1) % Self::CYCLE.len()]
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::InProgress => "In progress",
            Self::Attention => "Unread/Blocked",
            Self::Idle => "Idle",
        }
    }
    pub fn includes(self, state: AgentState) -> bool {
        match self {
            Self::All => true,
            Self::InProgress => state == AgentState::Working,
            Self::Attention => matches!(state, AgentState::Blocked | AgentState::Done),
            Self::Idle => state == AgentState::Idle,
        }
    }
}

/// Confirmation is a separate input state so ordinary typing cannot authorize
/// destructive work or silently change the frozen targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Browse,
    Kill,
    Confirm,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    None,
    Cancel,
    Jump,
    Refresh,
    Kill(Vec<Candidate>),
}

/// Owns selection and query revisions, including the retry latch that prevents
/// buffered Enter from selecting a different target after a host failure.
pub(crate) struct App {
    pub query: String,
    pub candidates: Vec<Candidate>,
    pub hits: Vec<Hit>,
    pub selected: usize,
    pub preview_visible: bool,
    pub generation: u64,
    pub preview: Option<String>,
    pub message: Option<String>,
    pub loading: bool,
    catalog_revision: u64,
    jump_armed: bool,
    view: View,
    reach: Reach,
    /// From the last catalog read: whether focus (rather than hide alone)
    /// produced the rows' scope verdicts, which only names the scoped reach.
    focus: bool,
    /// Rows Herdr's scope leaves out, cached per catalog read. Zero means the
    /// reach toggle has nothing to switch, so it is hidden and inert.
    out_of_scope: usize,
    mode: Mode,
    source: String,
    own: String,
    // Insertion order is confirmation/execution order. Keep the marked identity
    // frozen, even if a refresh observes the same terminal moved to another tab.
    marks: Vec<Candidate>,
    confirmation: Option<Vec<Candidate>>,
    confirmation_armed: bool,
}

impl App {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_context(String::new(), String::new())
    }

    pub fn with_context(source: String, own: String) -> Self {
        Self {
            query: String::new(),
            candidates: Vec::new(),
            hits: Vec::new(),
            selected: 0,
            preview_visible: false,
            generation: 0,
            preview: None,
            message: None,
            loading: true,
            catalog_revision: 0,
            jump_armed: true,
            view: View::All,
            reach: Reach::Scope,
            focus: false,
            out_of_scope: 0,
            mode: Mode::Browse,
            source,
            own,
            marks: Vec::new(),
            confirmation: None,
            confirmation_armed: false,
        }
    }

    pub fn handle(&mut self, event: Event) -> Action {
        // Confirmation freezes both membership and identities. Paste, resize,
        // focus, and release events cannot confirm or edit the underlying query.
        if self.mode == Mode::Confirm {
            if let Event::Key(key) = event {
                if key.kind == KeyEventKind::Release {
                    return Action::None;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Action::Cancel;
                }
                // Only the foreground picker can acknowledge a rendered and
                // flushed prompt after discarding input typed ahead of it.
                if !self.confirmation_armed {
                    return Action::None;
                }
                self.confirmation_armed = false;
                self.mode = Mode::Kill;
                self.message = None;
                let confirmed = self.confirmation.take().unwrap_or_default();
                if key.kind == KeyEventKind::Press
                    && matches!(key.code, KeyCode::Char('y' | 'Y'))
                    && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                {
                    return Action::Kill(confirmed);
                }
            }
            return Action::None;
        }
        match &event {
            Event::Paste(_) => {
                if prompt::edit(&mut self.query, &event) {
                    self.edited();
                }
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let key = *key;
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc if self.mode == Mode::Kill => self.leave_kill(),
                    KeyCode::Esc => return Action::Cancel,
                    // Ctrl-k stays unbound so global navigation shortcuts cannot toggle killing.
                    KeyCode::Char('x') if ctrl => {
                        if self.mode == Mode::Browse {
                            self.mode = Mode::Kill;
                            self.message = None;
                        } else {
                            self.leave_kill();
                        }
                    }
                    KeyCode::Char('e') if ctrl => {
                        let selected = self.selected().cloned();
                        self.view = self.view.next();
                        self.rebuild(selected.as_ref());
                        self.message = None;
                        self.jump_armed = true;
                    }
                    // Herdr's own focus picker switches focus with Ctrl-o, and
                    // Herdr binds no direct ctrl+o, so it reaches this popup.
                    KeyCode::Char('o') if ctrl => {
                        // Pressed ahead of the first catalog, it still counts
                        // and applies once rows arrive, as a typed query does.
                        if self.reach_toggles() || (self.loading && self.candidates.is_empty()) {
                            let selected = self.selected().cloned();
                            self.reach = self.reach.other();
                            self.rebuild(selected.as_ref());
                            self.message = None;
                            self.jump_armed = true;
                        } else {
                            self.message = Some("Every pane is already listed".into());
                        }
                    }
                    KeyCode::Enter if self.mode == Mode::Kill => {
                        if key.kind == KeyEventKind::Press {
                            if self.marks.is_empty() {
                                self.message =
                                    Some("Mark panes with Tab or Space before killing".into());
                            } else {
                                self.confirmation = Some(self.marks.clone());
                                self.confirmation_armed = false;
                                self.mode = Mode::Confirm;
                            }
                        }
                    }
                    KeyCode::Tab | KeyCode::BackTab | KeyCode::Char(' ')
                        if self.mode == Mode::Kill
                            && !ctrl
                            && key.modifiers.difference(KeyModifiers::SHIFT).is_empty() =>
                    {
                        if key.kind == KeyEventKind::Press {
                            self.toggle_mark(if key.code == KeyCode::BackTab { -1 } else { 1 });
                        }
                    }
                    KeyCode::Char('c') if ctrl => return Action::Cancel,
                    KeyCode::Enter
                        if key.kind == KeyEventKind::Press
                            && self.jump_armed
                            && self.selected().is_some()
                            && !self.loading =>
                    {
                        // Unix repeats commonly arrive as Press, not Repeat.
                        // Re-arm only through deliberate editing/navigation so
                        // a failed jump cannot submit a newly refreshed target.
                        self.jump_armed = false;
                        return Action::Jump;
                    }
                    KeyCode::Down | KeyCode::Char('n') if key.code == KeyCode::Down || ctrl => {
                        self.move_selection(1)
                    }
                    KeyCode::Up | KeyCode::Char('p') if key.code == KeyCode::Up || ctrl => {
                        self.move_selection(-1)
                    }
                    KeyCode::Char('r') if ctrl => {
                        self.loading = true;
                        return Action::Refresh;
                    }
                    KeyCode::Char(' ') | KeyCode::Char('@') if ctrl => self.toggle_preview(),
                    KeyCode::Null => self.toggle_preview(),
                    // Typing, Backspace and Ctrl-u edit the query the same way in
                    // every picker; kill mode's Space was claimed above.
                    _ => {
                        if prompt::edit(&mut self.query, &event) {
                            self.edited();
                        }
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    /// The foreground owner advances this revision before every catalog read.
    /// A coalescing worker may still finish an older in-flight read, so loading
    /// and confirmation must remain gated until this exact revision completes.
    pub fn request_catalog(&mut self) -> u64 {
        self.catalog_revision = self.catalog_revision.wrapping_add(1);
        self.loading = true;
        self.confirmation_armed = false;
        self.catalog_revision
    }

    pub fn catalog_finished(&mut self, revision: u64, result: Result<Listing, String>) -> bool {
        if revision != self.catalog_revision {
            return false;
        }
        match result {
            Ok(listing) => {
                self.focus = listing.focus;
                self.loaded(listing.rows);
            }
            Err(error) => {
                self.loading = false;
                self.message = Some(format!("{} · Ctrl-r retries", catalog::clean(&error)));
            }
        }
        true
    }

    pub fn loaded(&mut self, rows: Vec<Candidate>) {
        let selected = self.selected().cloned();
        self.candidates = rows;
        // A refresh can change the scope too. The reach is kept either way:
        // with nothing out of scope both reaches list the same rows.
        self.out_of_scope = self.candidates.iter().filter(|c| !c.in_scope).count();
        self.marks
            .retain(|marked| self.candidates.iter().any(|row| same_identity(marked, row)));
        // Confirmation identities stay frozen, but a refresh may drop a closed
        // or replaced target while an earlier one finishes. Never leave such a pane in
        // either the review or the `y` payload. A smaller review needs a fresh
        // foreground render acknowledgement before it can be confirmed.
        if let Some(targets) = &mut self.confirmation {
            self.confirmation_armed = false;
            targets.retain(|target| self.candidates.iter().any(|row| same_identity(target, row)));
            if targets.is_empty() {
                self.confirmation = None;
                self.mode = Mode::Kill;
            }
        }
        self.rebuild(selected.as_ref());
        self.loading = false;
    }

    fn rebuild(&mut self, selected: Option<&Candidate>) {
        let (reach, view) = (self.reach, self.view);
        self.hits = catalog::search_view(&self.candidates, &self.query, view == View::All, |row| {
            reaches(reach, row) && view.includes(row.state)
        });
        self.selected = selected
            .and_then(|selected| {
                self.hits
                    .iter()
                    .position(|h| same_identity(selected, &self.candidates[h.index]))
            })
            .unwrap_or_default();
        self.invalidate_preview();
    }

    pub fn view(&self) -> View {
        self.view
    }
    /// Rows `view` lists within the current reach.
    pub fn view_count(&self, view: View) -> usize {
        self.view_count_in(view, self.reach)
    }
    /// Rows `view` would list within `reach`; the header measures its strip
    /// with both reaches so ctrl-o cannot change the strip's form.
    pub fn view_count_in(&self, view: View, reach: Reach) -> usize {
        self.candidates
            .iter()
            .filter(|c| reaches(reach, c) && view.includes(c.state))
            .count()
    }
    pub fn reach(&self) -> Reach {
        self.reach
    }
    /// Whether ctrl-o switches anything: some row is out of Herdr's scope.
    /// False with focus and hide off, on a Herdr without scope, or when the
    /// scope read failed open, so the header and hints leave the toggle out.
    pub fn reach_toggles(&self) -> bool {
        self.out_of_scope > 0
    }
    /// Rows `reach` covers, across every status view.
    pub fn reach_count(&self, reach: Reach) -> usize {
        match reach {
            Reach::Scope => self.candidates.len() - self.out_of_scope,
            Reach::Everything => self.candidates.len(),
        }
    }
    /// The scoped reach's name: the panes focus keeps, or with focus off,
    /// the ones hide leaves visible. Never "All", which is a status view.
    pub fn scope_label(&self) -> &'static str {
        if self.focus {
            "In focus"
        } else {
            "Visible"
        }
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn is_marked(&self, candidate: &Candidate) -> bool {
        self.marks.iter().any(|c| same_identity(c, candidate))
    }
    pub fn marked_count(&self) -> usize {
        self.marks.len()
    }
    pub fn is_killable(&self, candidate: &Candidate) -> bool {
        candidate.pane_id != self.source && candidate.pane_id != self.own
    }
    pub fn confirmation(&self) -> Option<&[Candidate]> {
        self.confirmation.as_deref()
    }

    /// The picker calls this only after drawing/flushing this confirmation and
    /// draining already-queued input. Merely entering Confirm never arms `y`.
    pub fn confirmation_shown(&mut self) {
        if self.mode == Mode::Confirm && !self.loading {
            self.confirmation_armed = true;
        }
    }

    fn toggle_mark(&mut self, direction: isize) {
        if let Some(row) = self.selected().cloned() {
            if !self.is_killable(&row) {
                self.message = Some("The source and picker panes cannot be killed".into());
                return;
            }
            if self.is_marked(&row) {
                self.marks.retain(|c| !same_identity(c, &row));
            } else {
                self.marks.push(row);
            }
            self.move_selection(direction);
        }
    }

    fn leave_kill(&mut self) {
        self.mode = Mode::Browse;
        self.marks.clear();
        self.confirmation = None;
        self.confirmation_armed = false;
        self.message = None;
    }

    pub fn killed(&mut self, summary: String) {
        self.leave_kill();
        self.message = Some(summary);
        self.loading = true;
        self.jump_armed = false;
    }

    pub fn selected(&self) -> Option<&Candidate> {
        self.hits
            .get(self.selected)
            .and_then(|hit| self.candidates.get(hit.index))
    }

    pub fn apply_preview(&mut self, generation: u64, text: String) {
        if self.preview_visible && generation == self.generation {
            self.preview = Some(text);
        }
    }

    fn edited(&mut self) {
        self.rebuild(None);
        self.message = None;
        self.jump_armed = true;
    }

    fn move_selection(&mut self, direction: isize) {
        if self.hits.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(direction)
            .min(self.hits.len() - 1);
        self.message = None;
        self.jump_armed = true;
        self.invalidate_preview();
    }

    fn toggle_preview(&mut self) {
        self.preview_visible = !self.preview_visible;
        self.invalidate_preview();
    }

    fn invalidate_preview(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.preview = None;
    }
}

fn same_identity(a: &Candidate, b: &Candidate) -> bool {
    a.pane_id == b.pane_id && a.terminal_id == b.terminal_id
}

fn reaches(reach: Reach, row: &Candidate) -> bool {
    reach == Reach::Everything || row.in_scope
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    pub(super) fn rows() -> Vec<Candidate> {
        crate::catalog::catalog(json!({"workspaces":[{"workspace_id":"w1","label":"SETUP"}],"tabs":[{"tab_id":"w1:t1","workspace_id":"w1","label":"tools"}],"panes":[
            {"pane_id":"w1:p1","terminal_id":"t1","tab_id":"w1:t1","workspace_id":"w1","label":"jump rust"},
            {"pane_id":"w1:p2","terminal_id":"t2","tab_id":"w1:t1","workspace_id":"w1","label":"jump shell"}
        ]}), "w1:p1").unwrap()
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn listing(rows: Vec<Candidate>) -> Listing {
        Listing { rows, focus: false }
    }

    /// `rows()`'s two SETUP panes in scope, plus two panes in an `ft`
    /// workspace that Herdr's scope leaves out, the way `Host::candidates`
    /// marks them. `ft`'s blocked worker is the only agent.
    fn scoped_rows() -> Vec<Candidate> {
        let mut rows = crate::catalog::catalog(json!({
            "workspaces":[{"workspace_id":"w1","label":"SETUP","number":1},{"workspace_id":"w2","label":"ft","number":2}],
            "tabs":[{"tab_id":"w1:t1","workspace_id":"w1","label":"tools"},{"tab_id":"w2:t1","workspace_id":"w2","label":"agents"}],
            "panes":[
                {"pane_id":"w1:p1","terminal_id":"t1","tab_id":"w1:t1","workspace_id":"w1","label":"jump rust"},
                {"pane_id":"w1:p2","terminal_id":"t2","tab_id":"w1:t1","workspace_id":"w1","label":"jump shell"},
                {"pane_id":"w2:p1","terminal_id":"t3","tab_id":"w2:t1","workspace_id":"w2","label":"ft worker","agent":"codex","agent_status":"blocked"},
                {"pane_id":"w2:p2","terminal_id":"t4","tab_id":"w2:t1","workspace_id":"w2","label":"ft shell"}
            ]}), "w1:p1").unwrap();
        for row in &mut rows {
            row.in_scope = row.workspace_id == "w1";
        }
        rows
    }

    fn scoped_app(focus: bool) -> App {
        let mut app = App::with_context("w1:p1".into(), "own".into());
        let revision = app.request_catalog();
        assert!(app.catalog_finished(
            revision,
            Ok(Listing {
                rows: scoped_rows(),
                focus
            })
        ));
        app
    }

    fn listed(app: &App) -> Vec<&str> {
        app.hits
            .iter()
            .map(|hit| app.candidates[hit.index].pane_id.as_str())
            .collect()
    }

    #[test]
    fn each_open_lists_only_panes_in_scope() {
        let app = scoped_app(true);
        assert_eq!(app.reach(), Reach::Scope);
        assert!(app.reach_toggles());
        assert_eq!(listed(&app), ["w1:p1", "w1:p2"]);
        assert_eq!(app.candidates.len(), 4, "the catalog keeps every pane");
    }

    #[test]
    fn ctrl_o_switches_to_every_pane_and_back_keeping_query_view_and_selection() {
        let mut app = scoped_app(true);
        app.handle(Event::Paste("shell".into()));
        assert_eq!(listed(&app), ["w1:p2"]);
        assert_eq!(app.handle(ctrl('o')), Action::None);
        assert_eq!(app.reach(), Reach::Everything);
        assert_eq!(app.query, "shell");
        assert_eq!(listed(&app), ["w1:p2", "w2:p2"]);
        // The selection follows its pane across the switch.
        assert_eq!(app.selected().unwrap().pane_id, "w1:p2");
        app.handle(key(KeyCode::Down));
        assert_eq!(app.selected().unwrap().pane_id, "w2:p2");
        app.handle(ctrl('o'));
        assert_eq!(app.reach(), Reach::Scope);
        assert_eq!(app.query, "shell");
        // An out-of-scope selection has nowhere to stay, so the top row is selected.
        assert_eq!(app.selected().unwrap().pane_id, "w1:p2");

        // The status view survives both ways, and keeps filtering inside the reach.
        app.handle(ctrl('u'));
        app.handle(ctrl('e'));
        app.handle(ctrl('e'));
        assert_eq!(app.view(), View::Attention);
        assert!(app.hits.is_empty());
        app.handle(ctrl('o'));
        assert_eq!(app.view(), View::Attention);
        assert_eq!(listed(&app), ["w2:p1"]);
        app.handle(ctrl('o'));
        assert_eq!(app.view(), View::Attention);
        assert!(app.hits.is_empty());
    }

    #[test]
    fn counts_follow_the_reach() {
        let mut app = scoped_app(true);
        assert_eq!(
            (
                app.reach_count(Reach::Scope),
                app.reach_count(Reach::Everything)
            ),
            (2, 4)
        );
        // Status counts are what each view would list in the current reach.
        assert_eq!(View::CYCLE.map(|view| app.view_count(view)), [2, 0, 0, 0]);
        app.handle(ctrl('o'));
        assert_eq!(View::CYCLE.map(|view| app.view_count(view)), [4, 0, 1, 0]);
        // The reach counts do not depend on the reach or the status view.
        app.handle(ctrl('e'));
        assert_eq!(
            (
                app.reach_count(Reach::Scope),
                app.reach_count(Reach::Everything)
            ),
            (2, 4)
        );
    }

    #[test]
    fn the_scoped_reach_is_named_by_what_narrows_it() {
        assert_eq!(scoped_app(true).scope_label(), "In focus");
        assert_eq!(scoped_app(false).scope_label(), "Visible");
    }

    #[test]
    fn with_nothing_out_of_scope_ctrl_o_is_inert() {
        // Focus and hide off, a Herdr without scope, or a scope read that
        // failed open: every row is in scope, so there is nothing to switch.
        let mut app = App::new();
        app.loaded(rows());
        assert!(!app.reach_toggles());
        app.handle(Event::Paste("jump".into()));
        app.handle(key(KeyCode::Down));
        assert_eq!(app.handle(ctrl('o')), Action::None);
        assert_eq!(app.reach(), Reach::Scope);
        assert_eq!(app.message.as_deref(), Some("Every pane is already listed"));
        assert_eq!(app.query, "jump");
        assert_eq!(app.selected().unwrap().pane_id, "w1:p2");
        assert_eq!(
            (
                app.reach_count(Reach::Scope),
                app.reach_count(Reach::Everything)
            ),
            (2, 2)
        );
    }

    #[test]
    fn enter_on_an_out_of_scope_row_jumps_to_it() {
        let mut app = scoped_app(true);
        app.handle(ctrl('o'));
        app.handle(Event::Paste("ft worker".into()));
        let selected = app.selected().unwrap();
        assert_eq!(
            (selected.pane_id.as_str(), selected.in_scope),
            ("w2:p1", false)
        );
        assert_eq!(app.handle(key(KeyCode::Enter)), Action::Jump);
    }

    #[test]
    fn ctrl_o_typed_ahead_of_the_first_catalog_applies_when_it_arrives() {
        let mut app = App::with_context("w1:p1".into(), "own".into());
        let revision = app.request_catalog();
        app.handle(ctrl('o'));
        assert!(app.message.is_none());
        assert!(app.catalog_finished(revision, Ok(listing(scoped_rows()))));
        assert_eq!(app.reach(), Reach::Everything);
        assert_eq!(app.hits.len(), 4);
    }

    #[test]
    fn a_refresh_keeps_the_reach() {
        let mut app = scoped_app(true);
        app.handle(ctrl('o'));
        assert_eq!(app.handle(ctrl('r')), Action::Refresh);
        let revision = app.request_catalog();
        assert!(app.catalog_finished(revision, Ok(listing(scoped_rows()))));
        assert_eq!(app.reach(), Reach::Everything);
        assert_eq!(app.hits.len(), 4);
        assert_eq!(
            app.scope_label(),
            "Visible",
            "the new read's verdicts name the reach"
        );
    }

    #[test]
    fn marks_survive_ctrl_o_and_the_review_keeps_out_of_scope_targets() {
        let mut app = scoped_app(true);
        app.handle(ctrl('x'));
        app.handle(ctrl('o'));
        app.handle(Event::Paste("ft shell".into()));
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.marked_count(), 1);
        app.handle(ctrl('o'));
        assert_eq!(app.mode(), Mode::Kill);
        assert_eq!(app.marked_count(), 1);
        assert!(app.hits.is_empty());
        app.handle(key(KeyCode::Enter));
        app.confirmation_shown();
        let Action::Kill(targets) = app.handle(key(KeyCode::Char('y'))) else {
            panic!("expected kill")
        };
        assert_eq!(targets[0].pane_id, "w2:p2");
    }

    #[test]
    fn tests_that_typing_starts_immediately_and_selects_best_match() {
        let mut app = App::new();
        for ch in "jump rust".chars() {
            app.handle(key(KeyCode::Char(ch)));
        }
        app.loaded(rows());
        assert_eq!(app.query, "jump rust");
        assert_eq!(app.selected().unwrap().pane_id, "w1:p1");
        assert_eq!(app.handle(key(KeyCode::Enter)), Action::Jump);
    }

    #[test]
    fn tests_that_navigation_keeps_typing_focus_and_escape_cancels() {
        let mut app = App::new();
        app.loaded(rows());
        app.handle(key(KeyCode::Down));
        assert_eq!(app.selected().unwrap().pane_id, "w1:p2");
        app.handle(key(KeyCode::Char('j')));
        assert_eq!(app.query, "j");
        assert_eq!(app.handle(key(KeyCode::Esc)), Action::Cancel);
    }

    #[test]
    fn tests_that_no_results_cannot_jump() {
        let mut app = App::new();
        app.loaded(rows());
        app.handle(Event::Paste("missing zz".into()));
        assert!(app.selected().is_none());
        assert_eq!(app.handle(key(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn tests_that_stale_preview_cannot_replace_new_selection() {
        let mut app = App::new();
        app.loaded(rows());
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::CONTROL,
        )));
        let old = app.generation;
        app.handle(key(KeyCode::Down));
        app.apply_preview(old, "wrong pane".into());
        assert!(app.preview.is_none());
        app.apply_preview(app.generation, "right pane".into());
        assert_eq!(app.preview.as_deref(), Some("right pane"));
    }

    #[test]
    fn tests_that_failed_jump_cannot_be_retried_by_held_enter() {
        let mut app = App::new();
        app.loaded(rows());
        assert_eq!(app.handle(key(KeyCode::Enter)), Action::Jump);
        app.message = Some("Target changed".into());
        app.loaded(rows());
        assert_eq!(app.handle(key(KeyCode::Enter)), Action::None);
        app.handle(key(KeyCode::Down));
        assert_eq!(app.handle(key(KeyCode::Enter)), Action::Jump);
    }

    #[test]
    fn tests_that_backspace_removes_a_whole_grapheme() {
        let mut app = App::new();
        app.handle(Event::Paste("cafe\u{301}".into()));
        app.handle(key(KeyCode::Backspace));
        assert_eq!(app.query, "caf");
    }
    fn ctrl(ch: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL))
    }

    #[test]
    fn ctrl_x_toggles_kill_mode_and_clears_marks() {
        let mut app = App::new();
        app.loaded(rows());
        app.handle(Event::Paste("jump".into()));
        app.handle(key(KeyCode::Down));
        assert_eq!(app.handle(ctrl('x')), Action::None);
        assert_eq!(app.mode(), Mode::Kill);
        assert_eq!(app.query, "jump");
        assert_eq!(app.selected().unwrap().pane_id, "w1:p2");
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.marked_count(), 1);

        assert_eq!(app.handle(ctrl('x')), Action::None);
        assert_eq!(app.mode(), Mode::Browse);
        assert_eq!(app.marked_count(), 0);
        assert_eq!(app.query, "jump");
        assert_eq!(app.selected().unwrap().pane_id, "w1:p2");
        app.handle(ctrl('x'));
        assert_eq!(app.mode(), Mode::Kill);
        assert_eq!(app.marked_count(), 0);
    }

    #[test]
    fn ctrl_k_is_inert_in_browse() {
        let mut app = App::new();
        app.loaded(rows());
        app.handle(Event::Paste("jump".into()));
        app.handle(key(KeyCode::Down));
        assert_eq!(app.handle(ctrl('k')), Action::None);
        assert_eq!(app.mode(), Mode::Browse);
        assert_eq!(app.query, "jump");
        assert_eq!(app.selected().unwrap().pane_id, "w1:p2");
        assert_eq!(app.marked_count(), 0);
    }

    #[test]
    fn ctrl_k_is_inert_in_kill_and_preserves_marks() {
        let mut app = kill_app();
        app.handle(Event::Paste("jump".into()));
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.handle(ctrl('k')), Action::None);
        assert_eq!(app.mode(), Mode::Kill);
        assert_eq!(app.marked_count(), 1);
        assert!(app.is_marked(&rows()[0]));
        assert_eq!(app.query, "jump");
        assert_eq!(app.selected().unwrap().pane_id, "w1:p2");
    }

    fn kill_app() -> App {
        let mut app = App::with_context("source".into(), "own".into());
        app.loaded(rows());
        app.handle(ctrl('x'));
        app
    }

    #[test]
    fn invariant_1_kill_requires_explicit_mark_then_confirmation() {
        let mut app = kill_app();
        assert_eq!(app.handle(key(KeyCode::Char('y'))), Action::None);
        app.handle(ctrl('u'));
        assert_eq!(app.marked_count(), 0);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.marked_count(), 1);
        assert_eq!(app.handle(key(KeyCode::Enter)), Action::None);
        assert_eq!(app.mode(), Mode::Confirm);
        assert_eq!(app.confirmation().unwrap()[0].pane_id, "w1:p1");
        app.confirmation_shown();
        let Action::Kill(targets) = app.handle(key(KeyCode::Char('y'))) else {
            panic!("expected kill")
        };
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].pane_id, "w1:p1");
        assert_eq!(app.handle(key(KeyCode::Char('y'))), Action::None);
    }

    #[test]
    fn invariant_2_highlighted_but_unmarked_panes_never_kill() {
        let mut app = kill_app();
        assert_eq!(app.handle(key(KeyCode::Enter)), Action::None);
        assert_eq!(app.mode(), Mode::Kill);
        assert!(app.message.as_deref().unwrap().contains("Mark panes"));
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.selected().unwrap().pane_id, "w1:p2");
        app.handle(key(KeyCode::Enter));
        app.confirmation_shown();
        let Action::Kill(targets) = app.handle(key(KeyCode::Char('y'))) else {
            panic!()
        };
        assert_eq!(
            targets
                .iter()
                .map(|c| c.pane_id.as_str())
                .collect::<Vec<_>>(),
            ["w1:p1"]
        );
    }

    #[test]
    fn invariant_3_only_plain_y_confirms_other_keys_cancel_without_losing_marks() {
        for event in [
            key(KeyCode::Enter),
            key(KeyCode::Esc),
            key(KeyCode::Char('n')),
            ctrl('e'),
            ctrl('k'),
            ctrl('x'),
            ctrl('y'),
            ctrl('o'),
            Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::ALT)),
            key(KeyCode::Down),
        ] {
            let mut app = kill_app();
            app.handle(key(KeyCode::Tab));
            app.handle(key(KeyCode::Enter));
            app.confirmation_shown();
            assert_eq!(app.handle(event), Action::None);
            assert_eq!(app.mode(), Mode::Kill);
            assert_eq!(app.marked_count(), 1);
            assert!(app.confirmation().is_none());
        }
        for event in [
            key(KeyCode::Char('y')),
            Event::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT)),
        ] {
            let mut app = kill_app();
            app.handle(key(KeyCode::Tab));
            app.handle(key(KeyCode::Enter));
            app.confirmation_shown();
            assert!(matches!(app.handle(event), Action::Kill(_)));
        }
        let mut app = kill_app();
        app.handle(key(KeyCode::Tab));
        app.handle(key(KeyCode::Enter));
        app.confirmation_shown();
        assert_eq!(app.handle(ctrl('c')), Action::Cancel);
    }

    #[test]
    fn invariant_4_source_and_picker_cannot_be_marked() {
        let mut app = App::with_context("w1:p1".into(), "w1:p2".into());
        app.loaded(rows());
        app.handle(ctrl('x'));
        for code in [KeyCode::Tab, KeyCode::Char(' '), KeyCode::BackTab] {
            app.handle(key(code));
            app.handle(key(KeyCode::Down));
            app.handle(key(code));
            assert_eq!(app.marked_count(), 0);
        }
    }

    #[test]
    fn invariant_6_marks_follow_pane_terminal_identity_across_views_query_and_refresh() {
        let mut app = kill_app();
        app.handle(key(KeyCode::Tab));
        app.handle(Event::Paste("shell".into()));
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.marked_count(), 2);
        app.handle(ctrl('e'));
        assert!(app.hits.is_empty());
        app.loaded(rows());
        assert_eq!(app.marked_count(), 2);
        app.handle(key(KeyCode::Enter));
        app.confirmation_shown();
        assert_eq!(
            app.confirmation()
                .unwrap()
                .iter()
                .map(|c| c.pane_id.as_str())
                .collect::<Vec<_>>(),
            ["w1:p1", "w1:p2"]
        );
        // Async refresh cannot broaden or relabel a confirmation already shown;
        // a disappeared/replaced target is revoked rather than kept in review.
        let mut replacement = rows();
        replacement[0].terminal_id = "new".into();
        app.loaded(replacement);
        assert_eq!(app.marked_count(), 1);
        assert_eq!(app.confirmation().unwrap().len(), 1);
        assert_eq!(app.confirmation().unwrap()[0].terminal_id, "t2");
        app.confirmation_shown();
        app.handle(key(KeyCode::Char('n')));
        app.loaded(Vec::new());
        assert_eq!(app.marked_count(), 0);
    }

    #[test]
    fn marking_keys_toggle_move_and_leave_kill_clears_all_marks() {
        for exit in [key(KeyCode::Esc), ctrl('x')] {
            let mut app = kill_app();
            app.handle(key(KeyCode::Char(' ')));
            assert_eq!(app.query, "");
            assert_eq!(app.selected, 1);
            app.handle(key(KeyCode::BackTab));
            assert_eq!(app.selected, 0);
            assert_eq!(app.marked_count(), 2);
            app.handle(key(KeyCode::Tab));
            assert_eq!(app.marked_count(), 1);
            assert_eq!(app.handle(exit), Action::None);
            assert_eq!(app.mode(), Mode::Browse);
            assert_eq!(app.marked_count(), 0);
        }
        let mut app = kill_app();
        app.handle(key(KeyCode::Tab));
        app.killed("Closed 1".into());
        assert_eq!(app.mode(), Mode::Browse);
        assert_eq!(app.marked_count(), 0);
        app.loaded(rows());
        assert_eq!(
            app.handle(key(KeyCode::Enter)),
            Action::None,
            "buffered Enter cannot jump after killing"
        );
    }

    #[test]
    fn confirmation_ignores_releases_paste_resize_and_focus() {
        let mut app = kill_app();
        app.handle(key(KeyCode::Tab));
        app.handle(key(KeyCode::Enter));
        app.confirmation_shown();
        let mut release = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        for event in [
            Event::Paste("y".into()),
            Event::Resize(100, 40),
            Event::FocusLost,
            Event::FocusGained,
            Event::Key(release),
        ] {
            assert_eq!(app.handle(event), Action::None);
            assert_eq!(app.mode(), Mode::Confirm);
        }
    }

    #[test]
    fn view_cycle_membership_counts_and_selection_are_query_independent() {
        let mut all = rows();
        for (i, state) in [
            AgentState::Blocked,
            AgentState::Done,
            AgentState::Working,
            AgentState::Idle,
        ]
        .iter()
        .enumerate()
        {
            let mut row = all[0].clone();
            row.pane_id = format!("state{i}");
            row.terminal_id = format!("term{i}");
            row.state = *state;
            all.push(row);
        }
        let mut app = App::new();
        app.loaded(all);
        assert_eq!(app.view(), View::All);
        for (view, count) in [
            (View::All, 6),
            (View::InProgress, 1),
            (View::Attention, 2),
            (View::Idle, 1),
        ] {
            assert_eq!(app.view_count(view), count);
        }
        app.handle(Event::Paste("missing".into()));
        assert!(app.hits.is_empty());
        assert_eq!(app.view_count(View::All), 6);
        app.handle(ctrl('u'));
        app.selected = app
            .hits
            .iter()
            .position(|h| app.candidates[h.index].state == AgentState::Working)
            .unwrap();
        app.handle(ctrl('e'));
        assert_eq!(app.selected().unwrap().pane_id, "state2");
        for (view, count) in [(View::Attention, 2), (View::Idle, 1), (View::All, 6)] {
            app.handle(ctrl('e'));
            assert_eq!(app.view(), view);
            assert_eq!(app.hits.len(), count);
        }
        assert_eq!(
            View::CYCLE.map(View::label),
            ["All", "In progress", "Unread/Blocked", "Idle"]
        );
    }

    #[test]
    fn typed_ahead_y_cannot_confirm_before_the_prompt_is_shown() {
        let mut app = kill_app();
        app.handle(key(KeyCode::Tab));
        app.handle(key(KeyCode::Enter));
        for event in [
            key(KeyCode::Char('y')),
            key(KeyCode::Char('n')),
            key(KeyCode::Esc),
            ctrl('e'),
        ] {
            assert_eq!(app.handle(event), Action::None);
            assert_eq!(app.mode(), Mode::Confirm);
            assert_eq!(app.marked_count(), 1);
        }
        app.confirmation_shown();
        assert!(matches!(
            app.handle(key(KeyCode::Char('y'))),
            Action::Kill(_)
        ));
        // Each new confirmation needs a new render acknowledgement.
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.handle(key(KeyCode::Char('y'))), Action::None);
        assert_eq!(app.mode(), Mode::Confirm);
        assert_eq!(app.handle(ctrl('c')), Action::Cancel);
    }

    #[test]
    fn vanished_refresh_removes_marks_and_frozen_confirmation_targets() {
        let mut app = kill_app();
        app.handle(key(KeyCode::Tab));
        app.handle(key(KeyCode::Enter));
        app.confirmation_shown();
        app.loaded(vec![rows()[1].clone()]);
        assert_eq!(app.marked_count(), 0);
        assert_eq!(app.mode(), Mode::Kill);
        assert!(app.confirmation().is_none());
        assert_eq!(app.handle(key(KeyCode::Char('y'))), Action::None);
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.mode(), Mode::Kill);
    }

    #[test]
    fn refreshed_review_keeps_only_visible_frozen_targets_and_needs_redraw() {
        let mut app = kill_app();
        app.handle(key(KeyCode::Tab));
        app.handle(key(KeyCode::Tab));
        app.handle(key(KeyCode::Enter));
        app.confirmation_shown();
        app.loaded(vec![rows()[1].clone()]);
        assert_eq!(app.marked_count(), 1);
        assert_eq!(app.confirmation().unwrap()[0].pane_id, "w1:p2");
        assert_eq!(app.handle(key(KeyCode::Char('y'))), Action::None);
        app.confirmation_shown();
        let Action::Kill(targets) = app.handle(key(KeyCode::Char('y'))) else {
            panic!("visible target not confirmed");
        };
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].pane_id, "w1:p2");
    }

    #[test]
    fn confirmation_waits_for_pending_catalog_before_arming() {
        let mut app = kill_app();
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.handle(ctrl('r')), Action::Refresh);
        app.handle(key(KeyCode::Enter));
        app.confirmation_shown();
        assert_eq!(app.handle(key(KeyCode::Char('y'))), Action::None);
        app.loaded(vec![rows()[1].clone()]);
        assert_eq!(app.marked_count(), 0);
        assert_eq!(app.mode(), Mode::Kill);
        assert_eq!(app.handle(key(KeyCode::Char('y'))), Action::None);
    }

    #[test]
    fn superseded_catalog_cannot_arm_confirmation_while_latest_is_pending() {
        let mut app = kill_app();
        app.handle(key(KeyCode::Tab));
        let older = app.request_catalog();
        let latest = app.request_catalog();
        app.handle(key(KeyCode::Enter));
        assert!(!app.catalog_finished(older, Ok(listing(rows()))));
        assert!(app.loading);
        assert!(!app.catalog_finished(older, Err("old error".into())));
        assert!(app.message.is_none());
        app.confirmation_shown();
        assert_eq!(app.handle(key(KeyCode::Char('y'))), Action::None);
        assert_eq!(app.mode(), Mode::Confirm);
        assert!(app.catalog_finished(latest, Ok(listing(vec![rows()[1].clone()]))));
        assert!(!app.loading);
        assert_eq!(app.mode(), Mode::Kill);
        assert_eq!(app.marked_count(), 0);
        assert!(!app.catalog_finished(older, Ok(listing(rows()))));
        assert_eq!(app.candidates.len(), 1);
    }

    /// An `ft` workspace's agents count in every view and match queries in kill
    /// mode, like any other workspace's: Find hides no workspace.
    #[test]
    fn every_workspace_contributes_to_view_counts_and_queries() {
        let states = ["working", "blocked", "done", "idle", ""];
        let panes: Vec<_> = ["w1", "w2"]
            .into_iter()
            .flat_map(|workspace| {
                states.into_iter().enumerate().map(move |(i, state)| json!({
            "pane_id":format!("{workspace}:p{i}"),"terminal_id":format!("{workspace}:term{i}"),
            "workspace_id":workspace,"tab_id":format!("{workspace}:t1"),
            "title":if workspace == "w2" { "ft-only" } else { "main-only" },
            "agent":if state.is_empty() { None } else { Some("codex") },"agent_status":state
        }))
            })
            .collect();
        let value = json!({"workspaces":[{"workspace_id":"w1","label":"MAIN"},{"workspace_id":"w2","label":"ft"}],
            "tabs":[{"tab_id":"w1:t1","workspace_id":"w1"},{"tab_id":"w2:t1","workspace_id":"w2"}],"panes":panes});
        let rows = catalog::catalog_with_own(value, "w2:p0", "").unwrap();
        let mut app = App::new();
        app.loaded(rows);
        for (view, count) in View::CYCLE.into_iter().zip([10, 2, 4, 2]) {
            assert_eq!(app.view_count(view), count);
        }
        app.handle(Event::Paste("ft-only".into()));
        app.handle(ctrl('x'));
        for (view, count) in View::CYCLE.into_iter().zip([5, 1, 2, 1]) {
            assert_eq!(app.view(), view);
            assert_eq!(app.hits.len(), count);
            app.handle(ctrl('e'));
        }
    }
}
