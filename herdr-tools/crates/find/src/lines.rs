//! The line engine's state machine: items from a picker's command, the shared
//! matcher's ranking, selection, marks, and the Enter latch. Like `app`, it
//! performs no I/O; the popup loop in `picker` loads items and delivers the
//! chosen ones.
//!
//! ```text
//! command lines ─ Item::new (value, display, leaf) ─ rank(query) ─ hits ─┬─ Enter ─ Outcome::Deliver(values)
//!                                                     Tab / Shift-Tab ─ marks ─┘ (marks win over the cursor)
//! ```
use std::collections::HashSet;
use std::path::Path;

use crate::catalog;
use crate::matcher::{Field, Haystack, MatchClass, Matcher, Query};
use crate::pickers::{Deliver, Kind};
use crate::prompt;
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

/// One selectable line. `value` is what Enter delivers; `display` is what the
/// row shows. They differ only for paths (`~` for $HOME, made absolute).
#[derive(Debug)]
pub(crate) struct Item {
    pub value: String,
    pub display: String,
    /// Grapheme index in `display` where the last path component starts; rows
    /// dim everything before it. Zero for text items.
    pub leaf: usize,
    full: Haystack,
    leaf_field: Option<Haystack>,
}

impl Item {
    /// `cwd` is where the command ran, so relative path output becomes the
    /// absolute path the user means. Joining is lexical: no symlink resolution,
    /// so the typed path is the one zoxide (or the command) recorded.
    pub fn new(line: &str, kind: Kind, home: Option<&str>, cwd: &Path) -> Self {
        match kind {
            Kind::Text => Self {
                value: line.to_owned(),
                display: line.to_owned(),
                leaf: 0,
                full: Haystack::new(line),
                leaf_field: None,
            },
            Kind::Path => {
                let value = if Path::new(line).is_absolute() {
                    line.to_owned()
                } else {
                    cwd.join(line.strip_prefix("./").unwrap_or(line))
                        .display()
                        .to_string()
                };
                let display = catalog::display_path(&value, home);
                let graphemes: Vec<&str> = display.graphemes(true).collect();
                // A trailing `/` still names its directory: `/a/b/` has leaf `b/`.
                let named =
                    graphemes.len() - graphemes.iter().rev().take_while(|g| **g == "/").count();
                let leaf = graphemes[..named]
                    .iter()
                    .rposition(|g| *g == "/")
                    .map_or(0, |slash| slash + 1);
                Self {
                    leaf_field: Some(Haystack::new(graphemes[leaf..].concat())),
                    full: Haystack::new(display.clone()),
                    value,
                    display,
                    leaf,
                }
            }
        }
    }
}

/// A ranked reference into `items` with grapheme indices into its `display`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LineHit {
    pub index: usize,
    pub indices: Vec<u32>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    None,
    Cancel,
    Reload,
    /// Run these Enter effects on these values; the popup closes only if
    /// they all succeed.
    Deliver(Delivery),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Delivery {
    /// The marked values in the order they were marked, or the selected one.
    pub values: Vec<String>,
    /// The picker's Enter effects, minus any already done for these values by
    /// an earlier, partly failed Enter (so a retry never types them twice).
    pub effects: Vec<Deliver>,
}

/// Items one load produced, plus what it had to leave out. Built on the load
/// worker so preparing hundreds of haystacks never delays a keystroke.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    pub items: Vec<Item>,
    /// Lines dropped because they contained control characters.
    pub skipped: usize,
    /// The output cap stopped reading; later lines are missing.
    pub truncated: bool,
}

/// One line picker popup's foreground state, from the first load until Enter
/// succeeds or Escape. Owned by the `picker` loop, which feeds it key events and
/// load results and performs whatever `Outcome` it returns; `lines_ui` draws it.
pub(crate) struct LinePicker {
    pub query: String,
    pub items: Vec<Item>,
    pub hits: Vec<LineHit>,
    pub selected: usize,
    pub loading: bool,
    /// Why the last load failed (e.g. zoxide missing). It stays on screen while
    /// the user types, until a reload succeeds.
    pub error: Option<String>,
    /// A transient notice (skipped lines, a failed copy); editing or moving clears it.
    pub message: Option<String>,
    kind: Kind,
    enter: Vec<Deliver>,
    /// Only pickers declared `multi` let Tab mark.
    multi: bool,
    /// Marked values in the order they were marked, which is the order Enter
    /// delivers them. Values rather than item indices, so marks survive every
    /// query (they are not tied to `hits`) and a reload that reorders items.
    marks: Vec<String>,
    /// Effects that already succeeded for one set of values during a partly
    /// failed Enter.
    done: Option<(Vec<String>, Vec<Deliver>)>,
    // Unix key repeat arrives as Press. After a failed delivery, a held Enter
    // must not deliver again; editing or moving re-arms it (as in `app`).
    enter_armed: bool,
}

impl LinePicker {
    pub fn new(kind: Kind, enter: Vec<Deliver>, multi: bool) -> Self {
        Self {
            query: String::new(),
            items: Vec::new(),
            hits: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
            message: None,
            kind,
            enter,
            multi,
            marks: Vec::new(),
            done: None,
            enter_armed: true,
        }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Marked values in delivery order.
    pub fn marks(&self) -> &[String] {
        &self.marks
    }

    pub fn is_marked(&self, item: &Item) -> bool {
        self.marks.contains(&item.value)
    }

    /// Marks the current query hides; the heading says how many, so a mark
    /// made under another search is never a surprise at Enter.
    pub fn hidden_marks(&self) -> usize {
        // Distinct values: two items can share one (`rel` and `./rel` both
        // become the same absolute path), and a mark is one value.
        let shown: HashSet<&str> = self
            .hits
            .iter()
            .map(|hit| self.items[hit.index].value.as_str())
            .filter(|value| self.marks.iter().any(|mark| mark == value))
            .collect();
        self.marks.len() - shown.len()
    }

    /// What Enter would deliver now: every mark, else the selected item.
    pub fn chosen(&self) -> Vec<&str> {
        if self.marks.is_empty() {
            self.selected()
                .map(|item| vec![item.value.as_str()])
                .unwrap_or_default()
        } else {
            self.marks.iter().map(String::as_str).collect()
        }
    }

    pub fn handle(&mut self, event: Event) -> Outcome {
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Release {
                return Outcome::None;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => return Outcome::Cancel,
                KeyCode::Char('c') if ctrl => return Outcome::Cancel,
                KeyCode::Enter => {
                    let values: Vec<String> =
                        self.chosen().into_iter().map(str::to_owned).collect();
                    if key.kind == KeyEventKind::Press
                        && self.enter_armed
                        && !self.loading
                        && !values.is_empty()
                    {
                        let done = match &self.done {
                            Some((done_values, done)) if *done_values == values => done.as_slice(),
                            _ => &[],
                        };
                        let effects = self
                            .enter
                            .iter()
                            .filter(|effect| !done.contains(effect))
                            .copied()
                            .collect();
                        self.enter_armed = false;
                        return Outcome::Deliver(Delivery { values, effects });
                    }
                    return Outcome::None;
                }
                // fzf's multi-select keys, and the same toggle-then-move as
                // the navigator's kill mode. Space stays query text here:
                // paths are searched with several words.
                KeyCode::Tab | KeyCode::BackTab
                    if self.multi
                        && !ctrl
                        && key.modifiers.difference(KeyModifiers::SHIFT).is_empty() =>
                {
                    return self.toggle_mark(if key.code == KeyCode::BackTab { -1 } else { 1 });
                }
                KeyCode::Char('r') if ctrl => {
                    self.loading = true;
                    return Outcome::Reload;
                }
                KeyCode::Down => return self.moved(1),
                KeyCode::Char('n') if ctrl => return self.moved(1),
                KeyCode::Up => return self.moved(-1),
                KeyCode::Char('p') if ctrl => return self.moved(-1),
                _ => {}
            }
        }
        if prompt::edit(&mut self.query, &event) {
            self.rank(None);
            self.message = None;
            self.enter_armed = true;
        }
        Outcome::None
    }

    pub fn loaded(&mut self, result: Result<Batch, String>) {
        let keep = self.selected().map(|item| item.value.clone());
        self.loading = false;
        match result {
            Ok(batch) => {
                self.items = batch.items;
                self.error = None;
                // A mark whose item is gone can no longer be seen or unmarked,
                // so it must not be delivered either. A failed load keeps them.
                let items = &self.items;
                self.marks
                    .retain(|mark| items.iter().any(|item| item.value == *mark));
                let mut notes = Vec::new();
                if batch.skipped > 0 {
                    notes.push(format!(
                        "Skipped {} unprintable {}",
                        batch.skipped,
                        if batch.skipped == 1 { "line" } else { "lines" }
                    ));
                }
                if batch.truncated {
                    notes.push(format!("Showing the first {} lines", self.items.len()));
                }
                self.message = (!notes.is_empty()).then(|| notes.join(" · "));
            }
            Err(message) => {
                self.items.clear();
                self.error = Some(format!("{message} · ctrl-r retries"));
                self.message = None;
            }
        }
        self.rank(keep.as_deref());
    }

    /// The popup stays open after a failed copy or paste so the message is
    /// seen; Enter stays disarmed until the user edits, moves or marks. `done`
    /// lists the effects that did succeed for `values`; a later Enter on the
    /// same values skips them.
    pub fn delivery_failed(&mut self, values: Vec<String>, done: Vec<Deliver>, message: String) {
        match &mut self.done {
            Some((done_values, already)) if *done_values == values => already.extend(done),
            _ => self.done = Some((values, done)),
        }
        self.message = Some(message);
    }

    pub fn selected(&self) -> Option<&Item> {
        self.hits
            .get(self.selected)
            .and_then(|hit| self.items.get(hit.index))
    }

    /// Marks the selected item, or unmarks it if already marked (moving it to
    /// the end of the order if marked again), then moves the cursor.
    fn toggle_mark(&mut self, direction: isize) -> Outcome {
        if let Some(value) = self.selected().map(|item| item.value.clone()) {
            if let Some(at) = self.marks.iter().position(|mark| *mark == value) {
                self.marks.remove(at);
            } else {
                self.marks.push(value);
            }
            self.enter_armed = true;
            self.message = None;
            self.moved(direction);
        }
        Outcome::None
    }

    fn moved(&mut self, direction: isize) -> Outcome {
        if !self.hits.is_empty() {
            self.selected = self
                .selected
                .saturating_add_signed(direction)
                .min(self.hits.len() - 1);
            self.message = None;
            self.enter_armed = true;
        }
        Outcome::None
    }

    /// Empty query: source order (zoxide's own ranking). With a query: the
    /// worst atom's match class, then how many atoms only matched outside the
    /// last path component, then score, then source order. The leaf is a
    /// separate, higher-priority matcher field so `jump` prefers `…/jump` over
    /// `…/jump/sub`, and its indices shift into `display` coordinates.
    fn rank(&mut self, keep: Option<&str>) {
        let query = Query::parse(&self.query);
        let mut matcher = Matcher::default();
        let mut ranked: Vec<((MatchClass, usize, i32, usize), LineHit)> = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            let mut fields = Vec::with_capacity(2);
            if let Some(leaf) = &item.leaf_field {
                fields.push(Field {
                    haystack: leaf,
                    priority: 0,
                });
            }
            fields.push(Field {
                haystack: &item.full,
                priority: 1,
            });
            let Some(found) = matcher.match_fields(&query, &fields) else {
                continue;
            };
            let mut indices = Vec::new();
            let mut outside_leaf = 0;
            for atom in &found.atoms {
                let best = atom.best();
                let offset = if fields[best.field_index].priority == 0 {
                    item.leaf as u32
                } else {
                    outside_leaf += usize::from(item.leaf_field.is_some());
                    0
                };
                indices.extend(best.matched.indices.iter().map(|i| i + offset));
            }
            indices.sort_unstable();
            indices.dedup();
            ranked.push((
                (found.worst_class, outside_leaf, -found.total_score, index),
                LineHit { index, indices },
            ));
        }
        ranked.sort_by_key(|(key, _)| *key);
        self.hits = ranked.into_iter().map(|(_, hit)| hit).collect();
        self.selected = keep
            .and_then(|value| {
                self.hits
                    .iter()
                    .position(|hit| self.items[hit.index].value == value)
            })
            .unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    const HOME: &str = "/Users/me";
    const ENTER: [Deliver; 2] = [Deliver::Paste, Deliver::Copy];

    fn deliver(value: &str, effects: &[Deliver]) -> Outcome {
        deliver_all(&[value], effects)
    }

    fn deliver_all(values: &[&str], effects: &[Deliver]) -> Outcome {
        Outcome::Deliver(Delivery {
            values: values.iter().map(|v| v.to_string()).collect(),
            effects: effects.to_vec(),
        })
    }

    fn paths(lines: &[&str]) -> LinePicker {
        let mut picker = LinePicker::new(Kind::Path, ENTER.to_vec(), true);
        let items = lines
            .iter()
            .map(|line| Item::new(line, Kind::Path, Some(HOME), Path::new("/work")))
            .collect();
        picker.loaded(Ok(Batch {
            items,
            ..Batch::default()
        }));
        picker
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(ch: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL))
    }

    fn shown(picker: &LinePicker) -> Vec<&str> {
        picker
            .hits
            .iter()
            .map(|hit| picker.items[hit.index].display.as_str())
            .collect()
    }

    fn marked(picker: &LinePicker, hit: usize) -> String {
        let hit = &picker.hits[hit];
        let graphemes: Vec<_> = picker.items[hit.index].display.graphemes(true).collect();
        hit.indices.iter().map(|&i| graphemes[i as usize]).collect()
    }

    #[test]
    fn paths_display_home_as_tilde_and_deliver_the_absolute_path() {
        let mut picker = paths(&[
            "/Users/me/code/herdr-jump",
            "/Users/meg/x",
            "rel/dir",
            "/",
            "/a/b/",
        ]);
        assert_eq!(
            shown(&picker),
            [
                "~/code/herdr-jump",
                "/Users/meg/x",
                "/work/rel/dir",
                "/",
                "/a/b/"
            ]
        );
        assert_eq!(picker.items[0].leaf, "~/code/".len());
        assert_eq!(picker.items[3].leaf, 0);
        assert_eq!(picker.items[4].leaf, "/a/".len());
        assert_eq!(
            picker.handle(key(KeyCode::Enter)),
            deliver("/Users/me/code/herdr-jump", &ENTER)
        );
    }

    #[test]
    fn empty_query_keeps_source_order_and_queries_prefer_the_leaf() {
        let mut picker = paths(&[
            "/Users/me/code/herdr-jump/.wt/feat/jump-zoxide",
            "/Users/me/notes",
            "/Users/me/code/herdr-jump",
            "/Users/me/code/jump",
        ]);
        assert_eq!(shown(&picker)[1], "~/notes");
        picker.handle(Event::Paste("jump".into()));
        // Exact leaf, then the two leaves that start with or contain a `jump`
        // word, in zoxide's order; notes is gone.
        assert_eq!(
            shown(&picker),
            [
                "~/code/jump",
                "~/code/herdr-jump/.wt/feat/jump-zoxide",
                "~/code/herdr-jump"
            ]
        );
        picker.handle(ctrl('u'));
        picker.handle(Event::Paste("herdr jump".into()));
        assert_eq!(
            shown(&picker),
            [
                "~/code/herdr-jump",
                "~/code/herdr-jump/.wt/feat/jump-zoxide"
            ]
        );
        assert_eq!(marked(&picker, 0), "herdrjump");
        assert_eq!(marked(&picker, 1), "herdrjump");
        picker.handle(ctrl('u'));
        picker.handle(Event::Paste("~/code".into()));
        assert_eq!(shown(&picker).len(), 3);
        assert_eq!(marked(&picker, 0), "~/code");
    }

    #[test]
    fn scattered_letters_across_words_do_not_match() {
        let mut picker = paths(&["/Users/me/Review/ferry/kill/mode", "/Users/me/.config/nvim"]);
        picker.handle(Event::Paste("vim".into()));
        assert_eq!(shown(&picker), ["~/.config/nvim"]);
        assert_eq!(marked(&picker, 0), "vim");
    }

    #[test]
    fn text_items_match_the_whole_line() {
        let mut picker = LinePicker::new(Kind::Text, ENTER.to_vec(), false);
        let items = ["main", "feat/jump-zoxide", "feat/jump-search"]
            .iter()
            .map(|line| Item::new(line, Kind::Text, Some(HOME), Path::new("/")))
            .collect();
        picker.loaded(Ok(Batch {
            items,
            ..Batch::default()
        }));
        picker.handle(Event::Paste("zox".into()));
        assert_eq!(picker.selected().unwrap().value, "feat/jump-zoxide");
        assert_eq!(marked(&picker, 0), "zox");
    }

    #[test]
    fn escape_and_ctrl_c_cancel_and_enter_needs_a_loaded_selection() {
        let mut picker = LinePicker::new(Kind::Path, ENTER.to_vec(), true);
        assert_eq!(
            picker.handle(key(KeyCode::Enter)),
            Outcome::None,
            "still loading"
        );
        for cancel in [key(KeyCode::Esc), ctrl('c')] {
            assert_eq!(picker.handle(cancel), Outcome::Cancel);
        }
        let mut picker = paths(&["/a"]);
        picker.handle(Event::Paste("zzz".into()));
        assert!(picker.selected().is_none());
        assert_eq!(picker.handle(key(KeyCode::Enter)), Outcome::None);
    }

    #[test]
    fn failed_delivery_disarms_enter_until_the_user_edits_or_moves() {
        let mut picker = paths(&["/a", "/b"]);
        assert_eq!(picker.handle(key(KeyCode::Enter)), deliver("/a", &ENTER));
        picker.delivery_failed(
            vec!["/a".into()],
            vec![],
            "Couldn't type into the pane".into(),
        );
        assert_eq!(picker.handle(key(KeyCode::Enter)), Outcome::None);
        assert!(picker.message.is_some());
        picker.handle(key(KeyCode::Down));
        assert!(picker.message.is_none());
        assert_eq!(picker.handle(key(KeyCode::Enter)), deliver("/b", &ENTER));
        picker.delivery_failed(vec!["/b".into()], vec![], "again".into());
        picker.handle(key(KeyCode::Char('b')));
        assert_eq!(picker.handle(key(KeyCode::Enter)), deliver("/b", &ENTER));
    }

    /// Paste succeeded but copy failed: retrying the same path only copies, so
    /// the pane never receives the path twice. Another path gets every effect.
    #[test]
    fn retry_after_a_partial_failure_skips_the_effects_that_succeeded() {
        let mut picker = paths(&["/one", "/two"]);
        assert_eq!(picker.handle(key(KeyCode::Enter)), deliver("/one", &ENTER));
        picker.delivery_failed(
            vec!["/one".into()],
            vec![Deliver::Paste],
            "couldn't copy".into(),
        );
        picker.handle(key(KeyCode::Down));
        picker.handle(key(KeyCode::Up));
        assert_eq!(
            picker.handle(key(KeyCode::Enter)),
            deliver("/one", &[Deliver::Copy])
        );
        picker.delivery_failed(vec!["/one".into()], vec![], "still couldn't copy".into());
        picker.handle(key(KeyCode::Down));
        assert_eq!(picker.handle(key(KeyCode::Enter)), deliver("/two", &ENTER));
    }

    #[test]
    fn navigation_clamps_and_reload_keeps_the_selected_value() {
        let mut picker = paths(&["/a", "/b", "/c"]);
        picker.handle(key(KeyCode::Up));
        assert_eq!(picker.selected, 0);
        for event in [key(KeyCode::Down), ctrl('n'), ctrl('n'), ctrl('p')] {
            picker.handle(event);
        }
        assert_eq!(picker.selected().unwrap().value, "/b");
        assert_eq!(picker.handle(ctrl('r')), Outcome::Reload);
        assert!(picker.loading);
        let items = ["/c", "/b"]
            .iter()
            .map(|l| Item::new(l, Kind::Path, None, Path::new("/")))
            .collect();
        picker.loaded(Ok(Batch {
            items,
            ..Batch::default()
        }));
        assert_eq!(picker.selected().unwrap().value, "/b");
    }

    #[test]
    fn load_failures_and_skipped_lines_are_reported() {
        let mut picker = paths(&["/a"]);
        picker.loaded(Err("zoxide isn't installed".into()));
        assert!(picker.items.is_empty() && picker.hits.is_empty());
        assert_eq!(
            picker.error.as_deref(),
            Some("zoxide isn't installed · ctrl-r retries")
        );
        // Typing must not hide why the list is empty.
        picker.handle(Event::Paste("x".into()));
        picker.handle(key(KeyCode::Backspace));
        assert!(picker.error.is_some());
        picker.loaded(Ok(Batch {
            items: vec![Item::new("/a", Kind::Path, None, Path::new("/"))],
            skipped: 2,
            truncated: true,
        }));
        assert!(picker.error.is_none());
        assert_eq!(
            picker.message.as_deref(),
            Some("Skipped 2 unprintable lines · Showing the first 1 lines")
        );
    }

    fn tab() -> Event {
        key(KeyCode::Tab)
    }

    fn back_tab() -> Event {
        Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
    }

    fn search(picker: &mut LinePicker, query: &str) {
        picker.handle(ctrl('u'));
        picker.handle(Event::Paste(query.into()));
    }

    /// The owner's flow: search, Tab, search again, Tab, one Enter. Marks are
    /// delivered in the order they were made, not list order.
    #[test]
    fn marks_survive_new_searches_and_enter_delivers_them_in_mark_order() {
        let mut picker = paths(&[
            "/Users/me/code/jump",
            "/Users/me/notes",
            "/Users/me/My Drive",
        ]);
        search(&mut picker, "drive");
        picker.handle(tab());
        assert_eq!(picker.marks(), ["/Users/me/My Drive"]);
        search(&mut picker, "jump");
        assert_eq!(picker.hidden_marks(), 1);
        picker.handle(tab());
        search(&mut picker, "zzz");
        assert!(picker.hits.is_empty());
        assert_eq!(picker.hidden_marks(), 2);
        assert_eq!(
            picker.chosen(),
            ["/Users/me/My Drive", "/Users/me/code/jump"]
        );
        assert_eq!(
            picker.handle(key(KeyCode::Enter)),
            deliver_all(&["/Users/me/My Drive", "/Users/me/code/jump"], &ENTER)
        );
    }

    /// Tab marks and moves down, Shift-Tab marks and moves up; on a marked
    /// row either one unmarks it. With no marks, Enter takes the cursor row.
    #[test]
    fn tab_and_shift_tab_toggle_and_move() {
        let mut picker = paths(&["/a", "/b", "/c"]);
        picker.handle(tab());
        picker.handle(tab());
        assert_eq!(picker.marks(), ["/a", "/b"]);
        assert_eq!(picker.selected().unwrap().value, "/c");
        picker.handle(back_tab());
        assert_eq!(picker.marks(), ["/a", "/b", "/c"]);
        assert_eq!(picker.selected().unwrap().value, "/b");
        picker.handle(back_tab());
        assert_eq!(picker.marks(), ["/a", "/c"]);
        assert_eq!(picker.selected().unwrap().value, "/a");
        // Remarking puts it last: Enter's order is the order of the last Tab.
        picker.handle(key(KeyCode::Down));
        picker.handle(tab());
        assert_eq!(picker.marks(), ["/a", "/c", "/b"]);
        assert!(picker.is_marked(&picker.items[1]));
        picker.handle(back_tab());
        picker.handle(back_tab());
        picker.handle(back_tab());
        assert!(picker.marks().is_empty());
        assert_eq!(picker.handle(key(KeyCode::Enter)), deliver("/a", &ENTER));
    }

    #[test]
    fn tab_is_ignored_without_multi_and_with_modifiers() {
        let mut picker = LinePicker::new(Kind::Path, ENTER.to_vec(), false);
        picker.loaded(Ok(Batch {
            items: vec![Item::new("/a", Kind::Path, None, Path::new("/"))],
            ..Batch::default()
        }));
        picker.handle(tab());
        assert!(picker.marks().is_empty());
        assert_eq!(picker.query, "");
        let mut picker = paths(&["/a", "/b"]);
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            picker.handle(Event::Key(KeyEvent::new(KeyCode::Tab, modifiers)));
        }
        assert!(picker.marks().is_empty());
        let mut loading = LinePicker::new(Kind::Path, ENTER.to_vec(), true);
        loading.handle(tab());
        assert!(loading.marks().is_empty());
    }

    /// Marking changes what Enter delivers, so it re-arms Enter after a
    /// failure, and a retry with the same marks skips the effects that
    /// already succeeded.
    #[test]
    fn marks_rearm_enter_and_retry_by_the_whole_set() {
        let mut picker = paths(&["/a", "/b", "/c"]);
        picker.handle(tab());
        picker.handle(tab());
        assert_eq!(
            picker.handle(key(KeyCode::Enter)),
            deliver_all(&["/a", "/b"], &ENTER)
        );
        picker.delivery_failed(
            vec!["/a".into(), "/b".into()],
            vec![Deliver::Paste],
            "couldn't copy".into(),
        );
        assert_eq!(picker.handle(key(KeyCode::Enter)), Outcome::None);
        picker.handle(key(KeyCode::Up));
        assert_eq!(
            picker.handle(key(KeyCode::Enter)),
            deliver_all(&["/a", "/b"], &[Deliver::Copy])
        );
        picker.delivery_failed(vec!["/a".into(), "/b".into()], vec![], "again".into());
        picker.handle(tab());
        assert!(picker.message.is_none());
        assert_eq!(
            picker.handle(key(KeyCode::Enter)),
            deliver_all(&["/a"], &ENTER)
        );
    }

    #[test]
    fn reload_keeps_marks_that_still_exist() {
        let mut picker = paths(&["/a", "/b", "/c"]);
        picker.handle(tab());
        picker.handle(key(KeyCode::Down));
        picker.handle(tab());
        assert_eq!(picker.marks(), ["/a", "/c"]);
        picker.loaded(Err("zoxide failed".into()));
        assert_eq!(picker.marks(), ["/a", "/c"]);
        let items = ["/c", "/b"]
            .iter()
            .map(|l| Item::new(l, Kind::Path, None, Path::new("/")))
            .collect();
        picker.loaded(Ok(Batch {
            items,
            ..Batch::default()
        }));
        assert_eq!(picker.marks(), ["/c"]);
        assert_eq!(picker.hidden_marks(), 0);
    }

    /// Relative output can name one directory twice; marking it marks both
    /// rows, and it is still one mark.
    #[test]
    fn a_value_listed_twice_is_one_mark() {
        let mut picker = paths(&["rel", "./rel", "/b"]);
        picker.handle(tab());
        assert_eq!(picker.marks(), ["/work/rel"]);
        assert!(picker.is_marked(&picker.items[1]));
        assert_eq!(picker.hidden_marks(), 0);
    }
}
