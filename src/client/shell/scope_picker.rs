//! The focus/hide picker: one overlay that edits the scope (`crate::scope`)
//! for every machine the client can see, plus the one-shot scope edits from
//! keybinds and menus.
//!
//! ```text
//!  focus_picker / hide_picker keys, the sidebar menu, a plugin's scope.pick
//!     └─► open_scope_picker(mode): staged = a copy of effective_scope()
//!  rows: every endpoint's unfiltered snapshot, machine → workspace → tab
//!  typing edits the query; refresh_scope_picker keeps `view` (rows + what
//!  the query shows) current before every key, click and frame
//!  tab / ^f / ^x / ^o / ^t / ^r edit only `staged`; esc drops it
//!  enter ─► one ScopeSetParams (changed fields only) ─► request_scope_set (Local)
//!     └─► focus on and the displayed workspace now out of scope:
//!         focus_or_activate the first admitted workspace in panel order
//! ```
//!
//! Nothing here changes what the shell shows: the Local server owns the scope
//! and the new one comes back as a projection. Rows come from unfiltered
//! snapshots on purpose, so a hidden workspace can be un-hidden. Targets that
//! name no row (an offline machine's, a closed workspace's label) are never
//! touched by a toggle, so they survive an apply. The query only narrows what
//! is listed: marks live in `staged`, so a filtered-out row keeps its mark and
//! enter applies it.

use std::collections::{HashMap, HashSet};

use crate::scope::{
    ScopeMachineRef, ScopePickMode, ScopeSetParams, ScopeState, ScopeTabRef, ScopeTarget,
    ScopeWorkspaceRef,
};

use super::fuzzy::{Field, Haystack, MatchClass, Matcher, Query};
use super::*;

/// Names a picker row across rebuilds: snapshots can change while it is open.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ScopePickerRowKey {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: Option<String>,
    pub(super) tab_id: Option<String>,
}

/// The picker's clickable controls besides its rows, so it works mouse-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScopePickerControl {
    Mode(ScopePickMode),
    ToggleActive,
    Clear,
    Apply,
    Cancel,
}

#[derive(Debug)]
pub(super) struct ClientScopePickerOverlay {
    /// Which mark Tab and a row click toggle; ctrl+f and ctrl+x reach both.
    pub(super) mode: ScopePickMode,
    /// The scope when the picker opened. Apply sends only the fields that
    /// differ from it, so a field another client changed meanwhile is left
    /// alone unless this picker edited it too.
    pub(super) opened: ScopeState,
    /// The edited scope. It starts as a full copy of `opened`, which is what
    /// keeps targets no row shows when the picker applies.
    pub(super) staged: ScopeState,
    pub(super) selected: Option<ScopePickerRowKey>,
    /// The first listed row in the viewport, as a position in `view.listed`.
    pub(super) scroll: usize,
    /// The fzf-style search line. Empty lists the whole tree.
    pub(super) query: TextEditor,
    /// What the picker draws. Only `refresh_scope_picker` writes it.
    pub(super) view: ScopePickerView,
}

/// The picker's rows and which of them the query lists, rebuilt by
/// `refresh_scope_picker` before every picker key, click and frame so render
/// only reads it. Rebuilding rows does no matching: a row is matched once per
/// query and set of labels, and the verdict is reused from `matches` until
/// either changes. Matching therefore follows typing, not frames.
#[derive(Debug, Default)]
pub(super) struct ScopePickerView {
    /// Every row in panel order, unfiltered.
    pub(super) rows: Vec<ScopePickerRow>,
    /// The rows on screen, in panel order: every row for an empty query, else
    /// each match plus its machine and workspace as context.
    pub(super) listed: Vec<ScopePickerListedRow>,
    /// Rows the query matched, context rows not counted; every row when the
    /// query is empty. The `n/m` count is `matched / rows.len()`.
    pub(super) matched: usize,
    /// The query text `matches` holds verdicts for.
    query: String,
    matches: HashMap<ScopePickerRowKey, ScopePickerRowMatch>,
    /// How many rows have been matched against a query, so tests can tell
    /// that frames reuse verdicts instead of matching again.
    #[cfg(test)]
    pub(super) rows_judged: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ScopePickerListedRow {
    /// Index into `ScopePickerView::rows`.
    pub(super) index: usize,
    /// Listed only as the machine or workspace of a match. Drawn dimmed, and
    /// still selectable and markable.
    pub(super) context: bool,
    /// Matched graphemes in the row's own label, for highlighting.
    pub(super) label_indices: Vec<u32>,
}

/// A row's verdict under `ScopePickerView::query`, and the labels it was
/// judged on, so a renamed row is judged again.
#[derive(Debug)]
struct ScopePickerRowMatch {
    labels: Vec<String>,
    hit: Option<ScopePickerHit>,
}

#[derive(Debug)]
struct ScopePickerHit {
    /// Find's row order: the worst term's match class, the best label any
    /// term chose (the row's own first), then the total score. Lower is
    /// better.
    rank: (MatchClass, usize, std::cmp::Reverse<i32>),
    label_indices: Vec<u32>,
}

#[derive(Clone, Debug)]
pub(super) struct ScopePickerRow {
    pub(super) key: ScopePickerRowKey,
    /// 0 machine, 1 workspace, 2 tab.
    pub(super) depth: u8,
    pub(super) label: String,
    /// What marking this row adds: the endpoint's storage key as the machine,
    /// ids and labels for workspace and tab, and the machine label for display.
    pub(super) target: ScopeTarget,
    pub(super) endpoint_status: ClientEndpointStatus,
    /// Whether the row's machine, workspace or tab is visible under the staged
    /// scope, so the picker previews the result before it applies.
    pub(super) shown: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ScopePickerMarks {
    pub(super) focus: bool,
    pub(super) hidden: bool,
    /// A config hide (`[ui.sidebar.agents].hidden_workspaces`) names this row.
    pub(super) config: bool,
}

/// Whether `target` names exactly this row: the same specificity, a machine
/// that covers the row's, and matching workspace and tab by id or label. A
/// covering-but-broader target (a machine hide over a workspace row) does not
/// mark the row; the row's `shown` preview reflects it instead.
fn names_row(target: &ScopeTarget, row: &ScopeTarget) -> bool {
    fn part(
        id: &Option<String>,
        label: &Option<String>,
        row_id: &Option<String>,
        row_label: &Option<String>,
    ) -> bool {
        match (id, label) {
            (Some(id), _) => row_id.as_ref() == Some(id),
            (None, Some(label)) => row_label.as_ref() == Some(label),
            (None, None) => true,
        }
    }
    target.specificity() == row.specificity()
        && target.covers_machine(ScopeMachineRef {
            key: &row.machine,
            label: row.machine_label.as_deref().unwrap_or(&row.machine),
        })
        && part(
            &target.workspace_id,
            &target.workspace_label,
            &row.workspace_id,
            &row.workspace_label,
        )
        && part(
            &target.tab_id,
            &target.tab_label,
            &row.tab_id,
            &row.tab_label,
        )
}

pub(super) fn scope_picker_marks(scope: &ScopeState, row: &ScopePickerRow) -> ScopePickerMarks {
    let hides = || {
        scope
            .hidden
            .iter()
            .filter(|target| names_row(target, &row.target))
    };
    ScopePickerMarks {
        focus: scope
            .focus
            .targets
            .iter()
            .any(|target| names_row(target, &row.target)),
        hidden: hides().next().is_some(),
        config: hides().any(|target| target.config),
    }
}

/// Targets that name no row. They stay in the scope untouched on apply; the
/// picker reports how many so the owner knows they exist.
pub(super) fn unlisted_scope_targets(scope: &ScopeState, rows: &[ScopePickerRow]) -> usize {
    scope
        .focus
        .targets
        .iter()
        .chain(&scope.hidden)
        .filter(|target| !rows.iter().any(|row| names_row(target, &row.target)))
        .count()
}

fn tab_ref<'a>(workspace: ScopeWorkspaceRef<'a>, tab: &'a ClientShellTab) -> ScopeTabRef<'a> {
    ScopeTabRef {
        workspace,
        id: &tab.tab_id,
        label: &tab.label,
    }
}

/// Every endpoint's workspaces in panel order, unfiltered by the current scope
/// (the picker must list hidden workspaces) and with no worktree group
/// collapsed. The jump on focus walks the same order.
fn panel_workspaces(snapshot: &ClientShellSnapshot) -> impl Iterator<Item = &ClientShellWorkspace> {
    // An admit-everything scope: the picker lists hidden rows so they can be
    // shown again.
    render::workspace_entries(
        snapshot,
        &HashSet::new(),
        &super::scope::EndpointScope::default(),
    )
    .into_iter()
    .map(|entry| &snapshot.workspaces[entry.index])
}

fn workspace_shown(
    scope: &ScopeState,
    machine: ScopeMachineRef<'_>,
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
) -> bool {
    let workspace_ref = ScopeWorkspaceRef {
        machine,
        id: &workspace.workspace_id,
        label: &workspace.label,
    };
    scope.shows_workspace(
        workspace_ref,
        snapshot
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace.workspace_id)
            .map(|tab| tab_ref(workspace_ref, tab)),
    )
}

/// The picker's rows: machine → workspace → tab for every endpoint, always
/// expanded. An endpoint without a snapshot (never connected) shows only its
/// machine row.
pub(super) fn scope_picker_rows(
    endpoints: &[ClientShellEndpoint],
    scope: &ScopeState,
) -> Vec<ScopePickerRow> {
    let mut rows = Vec::new();
    for endpoint in endpoints {
        let machine_key = endpoint.endpoint_id.storage_key();
        let machine = ScopeMachineRef {
            key: &machine_key,
            label: &endpoint.label,
        };
        let machine_target = ScopeTarget {
            machine: machine_key.clone(),
            machine_label: Some(endpoint.label.clone()),
            ..ScopeTarget::default()
        };
        let row_key = |workspace_id: Option<&str>, tab_id: Option<&str>| ScopePickerRowKey {
            endpoint_id: endpoint.endpoint_id.clone(),
            workspace_id: workspace_id.map(str::to_owned),
            tab_id: tab_id.map(str::to_owned),
        };
        let header = rows.len();
        rows.push(ScopePickerRow {
            key: row_key(None, None),
            depth: 0,
            label: endpoint.label.clone(),
            target: machine_target.clone(),
            endpoint_status: endpoint.status,
            shown: true,
        });
        let mut any_workspace_shown = false;
        if let Some(snapshot) = endpoint.snapshot.as_deref() {
            for workspace in panel_workspaces(snapshot) {
                let workspace_ref = ScopeWorkspaceRef {
                    machine,
                    id: &workspace.workspace_id,
                    label: &workspace.label,
                };
                let tabs = snapshot
                    .tabs
                    .iter()
                    .filter(|tab| tab.workspace_id == workspace.workspace_id)
                    .collect::<Vec<_>>();
                let shown = scope.shows_workspace(
                    workspace_ref,
                    tabs.iter().map(|tab| tab_ref(workspace_ref, tab)),
                );
                any_workspace_shown |= shown;
                let workspace_target = ScopeTarget {
                    workspace_id: Some(workspace.workspace_id.clone()),
                    workspace_label: Some(workspace.label.clone()),
                    ..machine_target.clone()
                };
                rows.push(ScopePickerRow {
                    key: row_key(Some(&workspace.workspace_id), None),
                    depth: 1,
                    label: workspace.label.clone(),
                    target: workspace_target.clone(),
                    endpoint_status: endpoint.status,
                    shown,
                });
                for tab in tabs {
                    rows.push(ScopePickerRow {
                        key: row_key(Some(&workspace.workspace_id), Some(&tab.tab_id)),
                        depth: 2,
                        label: tab.label.clone(),
                        target: ScopeTarget {
                            tab_id: Some(tab.tab_id.clone()),
                            tab_label: Some(tab.label.clone()),
                            ..workspace_target.clone()
                        },
                        endpoint_status: endpoint.status,
                        shown: scope.admits_tab(tab_ref(workspace_ref, tab)),
                    });
                }
            }
        }
        rows[header].shown = scope.shows_machine(machine, any_workspace_shown);
    }
    rows
}

impl ScopePickerRow {
    /// The labels a query matches this row against: its own first, then its
    /// workspace's and machine's. Together they are the row's path, so
    /// `studio fcf` finds studio's fcf workspace and its tabs.
    fn search_labels(&self) -> impl Iterator<Item = &str> {
        [
            self.target.tab_label.as_deref(),
            self.target.workspace_label.as_deref(),
            self.target.machine_label.as_deref(),
        ]
        .into_iter()
        .flatten()
    }
}

/// Judges one row the way Find judges a pane: every term must match one of
/// the row's labels, each term scores on its best label (the row's own wins
/// ties), and the highlights are every term's matches in the row's own label.
fn match_scope_picker_row(
    matcher: &mut Matcher,
    query: &Query,
    labels: &[String],
) -> Option<ScopePickerHit> {
    let haystacks: Vec<_> = labels.iter().map(|label| Haystack::new(label)).collect();
    let fields: Vec<_> = haystacks
        .iter()
        .enumerate()
        .map(|(priority, haystack)| Field { haystack, priority })
        .collect();
    let matched = matcher.match_fields(query, &fields)?;
    let mut best_label = labels.len();
    let mut label_indices = Vec::new();
    for atom in matched.atoms {
        best_label = best_label.min(atom.best().field_index);
        for found in atom.into_fields() {
            if found.field_index == 0 {
                label_indices.extend(found.matched.indices);
            }
        }
    }
    label_indices.sort_unstable();
    label_indices.dedup();
    Some(ScopePickerHit {
        rank: (
            matched.worst_class,
            best_label,
            std::cmp::Reverse(matched.total_score),
        ),
        label_indices,
    })
}

impl ScopePickerView {
    /// Replaces the rows and relists them under `query`. Only rows the cache
    /// has no verdict for (new or renamed, or every row after the query
    /// changed) are matched.
    fn refresh(&mut self, rows: Vec<ScopePickerRow>, query: &str) {
        if self.query != query {
            self.query = query.to_owned();
            self.matches.clear();
        }
        self.rows = rows;
        if query.trim().is_empty() {
            self.matched = self.rows.len();
            self.listed = (0..self.rows.len())
                .map(|index| ScopePickerListedRow {
                    index,
                    context: false,
                    label_indices: Vec::new(),
                })
                .collect();
            return;
        }
        let mut parsed = None;
        let mut matcher = Matcher::default();
        for row in &self.rows {
            if self.matches.get(&row.key).is_some_and(|cached| {
                cached
                    .labels
                    .iter()
                    .map(String::as_str)
                    .eq(row.search_labels())
            }) {
                continue;
            }
            let labels: Vec<_> = row.search_labels().map(str::to_owned).collect();
            let query = parsed.get_or_insert_with(|| Query::parse(query));
            let hit = match_scope_picker_row(&mut matcher, query, &labels);
            #[cfg(test)]
            {
                self.rows_judged += 1;
            }
            self.matches
                .insert(row.key.clone(), ScopePickerRowMatch { labels, hit });
        }
        // Rows arrive in tree order, so a match's machine and workspace rows
        // are already behind it when it is seen.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum Listed {
            No,
            Context,
            Match,
        }
        let mut listed = vec![Listed::No; self.rows.len()];
        let (mut machine, mut workspace) = (0, None);
        for (index, row) in self.rows.iter().enumerate() {
            match row.depth {
                0 => (machine, workspace) = (index, None),
                1 => workspace = Some(index),
                _ => {}
            }
            if self.hit(row).is_none() {
                continue;
            }
            listed[index] = Listed::Match;
            for ancestor in [Some(machine), workspace].into_iter().flatten() {
                if ancestor != index {
                    listed[ancestor] = listed[ancestor].max(Listed::Context);
                }
            }
        }
        self.matched = listed.iter().filter(|kind| **kind == Listed::Match).count();
        self.listed = listed
            .iter()
            .enumerate()
            .filter(|(_, kind)| **kind != Listed::No)
            .map(|(index, kind)| ScopePickerListedRow {
                index,
                context: *kind == Listed::Context,
                label_indices: self
                    .hit(&self.rows[index])
                    .map(|hit| hit.label_indices.clone())
                    .unwrap_or_default(),
            })
            .collect();
    }

    fn hit(&self, row: &ScopePickerRow) -> Option<&ScopePickerHit> {
        self.matches.get(&row.key)?.hit.as_ref()
    }

    /// The best-ranked match, ties going to the first in panel order, or
    /// `None` for an empty query or when nothing matches.
    fn best_match(&self) -> Option<&ScopePickerRowKey> {
        if self.query.trim().is_empty() {
            return None;
        }
        self.listed
            .iter()
            .filter_map(|listed| {
                let row = &self.rows[listed.index];
                self.hit(row).map(|hit| (hit.rank, listed.index, &row.key))
            })
            .min_by_key(|(rank, index, _)| (*rank, *index))
            .map(|(_, _, key)| key)
    }

    /// Where `selected` sits in `listed`. A selection the query filtered out
    /// (or none) falls back to the first listed row, which is the one drawn
    /// as selected; `None` only when nothing is listed.
    pub(super) fn selected_position(&self, selected: Option<&ScopePickerRowKey>) -> Option<usize> {
        if self.listed.is_empty() {
            return None;
        }
        Some(
            selected
                .and_then(|key| {
                    self.listed
                        .iter()
                        .position(|listed| &self.rows[listed.index].key == key)
                })
                .unwrap_or(0),
        )
    }

    /// The row at `selected_position`.
    fn selected_row(&self, selected: Option<&ScopePickerRowKey>) -> Option<&ScopePickerRow> {
        self.selected_position(selected)
            .map(|position| &self.rows[self.listed[position].index])
    }
}

/// Adds `row` to the list, or removes every target naming it. Adding a focus
/// target also switches focus on, as the workspace menu's "Focus" does:
/// picking what to focus and then seeing nothing change would read as broken.
fn toggle_mark(scope: &mut ScopeState, row: &ScopeTarget, mark: ScopePickMode) {
    let list = match mark {
        ScopePickMode::Focus => &mut scope.focus.targets,
        ScopePickMode::Hide => &mut scope.hidden,
    };
    let before = list.len();
    list.retain(|target| !names_row(target, row));
    if list.len() == before {
        list.push(row.clone());
        if mark == ScopePickMode::Focus {
            scope.focus.active = true;
        }
    }
}

/// Adds `target` unless a target already names the same thing.
fn add_target(list: &mut Vec<ScopeTarget>, target: ScopeTarget) {
    if !list.iter().any(|existing| names_row(existing, &target)) {
        list.push(target);
    }
}

impl ClientScopePickerOverlay {
    /// One `scope.set` holding only the fields the picker changed, or `None`
    /// when it changed nothing.
    pub(super) fn set_params(&self) -> Option<ScopeSetParams> {
        let params = ScopeSetParams {
            focus_targets: (self.staged.focus.targets != self.opened.focus.targets)
                .then(|| self.staged.focus.targets.clone()),
            focus_active: (self.staged.focus.active != self.opened.focus.active)
                .then_some(self.staged.focus.active),
            hidden: (self.staged.hidden != self.opened.hidden).then(|| self.staged.hidden.clone()),
        };
        (params != ScopeSetParams::default()).then_some(params)
    }
}

impl ClientShellState {
    /// Opens the picker on the current scope. Reopening it only changes the
    /// mode, so a plugin's `scope.pick` cannot drop staged edits.
    pub(super) fn open_scope_picker_overlay(&mut self, mode: ScopePickMode) {
        if let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_mut() {
            picker.mode = mode;
            return;
        }
        let scope = self.effective_scope().into_owned();
        // Start on the displayed workspace, like the Go to navigator.
        let selected = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_workspace_id.clone())
            .map(|workspace_id| ScopePickerRowKey {
                endpoint_id: self.active_endpoint_id.clone(),
                workspace_id: Some(workspace_id),
                tab_id: None,
            });
        self.overlay = Some(ClientShellOverlay::ScopePicker(ClientScopePickerOverlay {
            mode,
            opened: scope.clone(),
            staged: scope,
            selected,
            scroll: 0,
            query: TextEditor::default(),
            view: ScopePickerView::default(),
        }));
        self.refresh_scope_picker();
    }

    /// Brings the open picker's `view` up to date with the endpoints, the
    /// staged scope and the query. Called before every picker key and click
    /// and by `compose` before each frame, because snapshots can change while
    /// the picker is open; render then only reads the view. Rebuilding rows
    /// is the same work render used to do per frame, and matching runs only
    /// for rows the query has not judged yet (`ScopePickerView::refresh`).
    pub(super) fn refresh_scope_picker(&mut self) {
        if let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_mut() {
            let rows = scope_picker_rows(&self.endpoints, &picker.staged);
            picker.view.refresh(rows, picker.query.as_str());
        }
    }

    /// Every row of the open picker, whatever the query lists.
    #[cfg(test)]
    pub(super) fn scope_picker_rows_now(&self) -> Vec<ScopePickerRow> {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::ScopePicker(picker)) => {
                scope_picker_rows(&self.endpoints, &picker.staged)
            }
            _ => Vec::new(),
        }
    }

    /// Moves the selection by `delta` listed rows, stopping at either end,
    /// and scrolls so it stays in the last rendered viewport.
    pub(super) fn move_scope_picker_selection(&mut self, delta: isize) {
        let viewport = self.hits.scope_picker_viewport_rows.max(1);
        self.refresh_scope_picker();
        let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        // With nothing listed the selection is kept, so clearing the query
        // returns to it.
        let Some(current) = picker.view.selected_position(picker.selected.as_ref()) else {
            return;
        };
        let last = picker.view.listed.len() - 1;
        let next = current.saturating_add_signed(delta).min(last);
        picker.selected = Some(picker.view.rows[picker.view.listed[next].index].key.clone());
        picker.scroll = picker.scroll.clamp(next.saturating_sub(viewport - 1), next);
    }

    /// Toggles `mark` on the row `key` names (a click), or on the selected
    /// listed row.
    pub(super) fn toggle_scope_picker_row(
        &mut self,
        key: Option<&ScopePickerRowKey>,
        mark: ScopePickMode,
    ) {
        self.refresh_scope_picker();
        let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        let row = match key {
            Some(key) => picker.view.rows.iter().find(|row| &row.key == key),
            None => picker.view.selected_row(picker.selected.as_ref()),
        };
        let Some((target, key)) = row.map(|row| (row.target.clone(), row.key.clone())) else {
            return;
        };
        toggle_mark(&mut picker.staged, &target, mark);
        picker.selected = Some(key);
    }

    /// After the query text changes: relist, and select the best match the
    /// way fzf does. An emptied query keeps the selected row, which the full
    /// tree lists again.
    fn scope_picker_query_changed(&mut self) {
        self.refresh_scope_picker();
        if let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_mut() {
            if let Some(best) = picker.view.best_match() {
                picker.selected = Some(best.clone());
            }
            picker.scroll = 0;
        }
    }

    /// Typed or pasted text that arrives as text rather than keys (a paste,
    /// an input-method commit). Returns false when the picker is not open.
    pub(super) fn insert_scope_picker_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_mut() else {
            return false;
        };
        if picker.query.insert(text) {
            self.scope_picker_query_changed();
        }
        true
    }

    /// Keys while the picker is open; returns false when it is not.
    ///
    /// Letters and Space type into the query, so every command is a control
    /// key, following fzf. The picker owns the keyboard ahead of the prefix
    /// and every binding (`route_key_press`), so each chord here reaches it
    /// whatever the prefix is. ctrl+a stays the editor's line start, not a
    /// command: it is a common prefix, and a command there would surprise.
    pub(super) fn route_scope_picker_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crossterm::event::KeyModifiers;

        let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_ref() else {
            return false;
        };
        let mode = picker.mode;
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.is_empty();
        let control = modifiers == KeyModifiers::CONTROL;
        let page = self.hits.scope_picker_viewport_rows.max(1) as isize;
        match code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter => self.apply_scope_picker(outcome),
            KeyCode::Up => self.move_scope_picker_selection(-1),
            KeyCode::Down => self.move_scope_picker_selection(1),
            KeyCode::Char('k' | 'p') if control => self.move_scope_picker_selection(-1),
            KeyCode::Char('j' | 'n') if control => self.move_scope_picker_selection(1),
            KeyCode::PageUp => self.move_scope_picker_selection(-page),
            KeyCode::PageDown => self.move_scope_picker_selection(page),
            KeyCode::Tab | KeyCode::BackTab if plain => {
                self.toggle_scope_picker_row(None, mode);
                self.move_scope_picker_selection(if code == KeyCode::Tab { 1 } else { -1 });
            }
            KeyCode::Char('f') if control => {
                self.toggle_scope_picker_row(None, ScopePickMode::Focus)
            }
            KeyCode::Char('x') if control => {
                self.toggle_scope_picker_row(None, ScopePickMode::Hide)
            }
            // ctrl+o switches the list, the key every Herdr picker uses for
            // that (Find's reach, the paths picker's Dirs/Files); here the
            // two lists are the marks Tab sets. ctrl+t switches focus on/off.
            KeyCode::Char('o') if control => {
                let other = match mode {
                    ScopePickMode::Focus => ScopePickMode::Hide,
                    ScopePickMode::Hide => ScopePickMode::Focus,
                };
                self.activate_scope_picker_control(ScopePickerControl::Mode(other), outcome);
            }
            KeyCode::Char('t') if control => {
                self.activate_scope_picker_control(ScopePickerControl::ToggleActive, outcome)
            }
            KeyCode::Char('r') if control => {
                self.activate_scope_picker_control(ScopePickerControl::Clear, outcome)
            }
            // Everything else belongs to the query line: printable keys,
            // Space included, and the editor's own keys (ctrl+u, ctrl+w,
            // Backspace, Left/Right, Home/End). Keys it has no use for are
            // swallowed so nothing reaches a pane behind the picker.
            _ => {
                let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_mut() else {
                    return true;
                };
                match picker.query.handle_key(key) {
                    Some(true) => self.scope_picker_query_changed(),
                    Some(false) => {}
                    None => return true,
                }
            }
        }
        outcome.repaint = true;
        true
    }

    pub(super) fn activate_scope_picker_control(
        &mut self,
        control: ScopePickerControl,
        outcome: &mut ClientShellInput,
    ) {
        outcome.repaint = true;
        match control {
            ScopePickerControl::Apply => return self.apply_scope_picker(outcome),
            ScopePickerControl::Cancel => {
                self.overlay = None;
                return;
            }
            _ => {}
        }
        // ctrl+r restores the config hides the client knows, which are the ones
        // the Local server seeded from the same config file.
        let config_hidden = self.config.agents.hidden_workspaces.clone();
        let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        match control {
            ScopePickerControl::Mode(mode) => picker.mode = mode,
            ScopePickerControl::ToggleActive => {
                picker.staged.focus.active = !picker.staged.focus.active
            }
            // Clears everything, unlisted targets included: the owner asked
            // for an empty scope, like `herdr scope clear`.
            ScopePickerControl::Clear => picker.staged.clear(&config_hidden),
            ScopePickerControl::Apply | ScopePickerControl::Cancel => {}
        }
    }

    /// Closes the picker and sends its edits as one `scope.set`.
    pub(super) fn apply_scope_picker(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.take() else {
            return;
        };
        outcome.repaint = true;
        let Some(params) = picker.set_params() else {
            return;
        };
        self.submit_scope_set(params, outcome);
        self.jump_into_scope(&picker.staged, outcome);
    }

    /// `toggle_focus`: flips focus on or off, keeping its targets.
    pub(super) fn toggle_scope_focus(&mut self, outcome: &mut ClientShellInput) {
        let mut next = self.effective_scope().into_owned();
        next.focus.active = !self.scope_focus_for_toggle();
        self.submit_scope_set(
            ScopeSetParams {
                focus_active: Some(next.focus.active),
                ..ScopeSetParams::default()
            },
            outcome,
        );
        self.jump_into_scope(&next, outcome);
        outcome.repaint = true;
    }

    /// The workspace menu's "Focus" and "Hide". The menu only opens on the
    /// active endpoint's rows, so the machine is the active endpoint.
    pub(super) fn add_workspace_to_scope(
        &mut self,
        workspace_id: &str,
        mark: ScopePickMode,
        outcome: &mut ClientShellInput,
    ) {
        let Some(label) = self.snapshot.as_deref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id)
                .map(|workspace| workspace.label.clone())
        }) else {
            return;
        };
        let target = ScopeTarget {
            machine: self.active_endpoint_id.storage_key(),
            machine_label: Some(self.endpoint_label(&self.active_endpoint_id).to_owned()),
            workspace_id: Some(workspace_id.to_owned()),
            workspace_label: Some(label),
            ..ScopeTarget::default()
        };
        let mut next = self.effective_scope().into_owned();
        let params = match mark {
            ScopePickMode::Focus => {
                add_target(&mut next.focus.targets, target);
                next.focus.active = true;
                ScopeSetParams {
                    focus_targets: Some(next.focus.targets.clone()),
                    focus_active: Some(true),
                    hidden: None,
                }
            }
            ScopePickMode::Hide => {
                add_target(&mut next.hidden, target);
                ScopeSetParams {
                    hidden: Some(next.hidden.clone()),
                    ..ScopeSetParams::default()
                }
            }
        };
        self.submit_scope_set(params, outcome);
        if mark == ScopePickMode::Focus {
            self.jump_into_scope(&next, outcome);
        }
        outcome.repaint = true;
    }

    /// Every scope edit from this module leaves through here. Tests read the
    /// payload from `scope_set_requests`, since `request_scope_set` talks to
    /// the Local server. `outcome` is the input pass the edit came from, kept
    /// at this one seam in case the Local request needs to ride on it.
    pub(super) fn submit_scope_set(
        &mut self,
        params: ScopeSetParams,
        _outcome: &mut ClientShellInput,
    ) {
        #[cfg(test)]
        self.scope_set_requests.push(params.clone());
        self.request_scope_set(params);
    }

    /// After focus is applied: when the displayed workspace is outside
    /// `scope`, focus the first admitted workspace in panel order (machines
    /// top to bottom, then their workspaces), skipping machines that cannot be
    /// reached. Judged against the scope just sent, because its projection
    /// has not come back yet. Leaves the view alone when focus is off or
    /// nothing is admitted.
    pub(super) fn jump_into_scope(&mut self, scope: &ScopeState, outcome: &mut ClientShellInput) {
        if !scope.focus_engaged() {
            return;
        }
        let active_key = self.active_endpoint_id.storage_key();
        let current_shown = self.snapshot.as_deref().is_none_or(|snapshot| {
            let machine = ScopeMachineRef {
                key: &active_key,
                label: self.endpoint_label(&self.active_endpoint_id),
            };
            snapshot
                .focused_workspace_id
                .as_deref()
                .and_then(|id| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == id)
                })
                .is_none_or(|workspace| workspace_shown(scope, machine, snapshot, workspace))
        });
        if current_shown {
            return;
        }
        let destination = self.endpoints.iter().find_map(|endpoint| {
            if endpoint.status != ClientEndpointStatus::Online && !endpoint.endpoint_id.is_local() {
                return None;
            }
            let snapshot = endpoint.snapshot.as_deref()?;
            let key = endpoint.endpoint_id.storage_key();
            let machine = ScopeMachineRef {
                key: &key,
                label: &endpoint.label,
            };
            panel_workspaces(snapshot)
                .find(|workspace| workspace_shown(scope, machine, snapshot, workspace))
                .map(|workspace| (endpoint.endpoint_id.clone(), workspace.workspace_id.clone()))
        });
        if let Some((endpoint_id, workspace_id)) = destination {
            self.focus_or_activate(
                endpoint_id,
                ClientEndpointFocusTarget::Workspace(workspace_id),
                outcome,
            );
        }
    }
}
