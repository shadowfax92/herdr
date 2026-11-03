//! The focus/hide picker: one overlay that edits the scope (`crate::scope`)
//! for every machine the client can see, plus the one-shot scope edits from
//! keybinds and menus.
//!
//! ```text
//!  focus_picker / hide_picker keys, the sidebar menu, a plugin's scope.pick
//!     └─► open_scope_picker(mode): staged = a copy of effective_scope()
//!  rows: every endpoint's unfiltered snapshot, machine → workspace → tab
//!  space / f / x / a / c edit only `staged`; esc drops it
//!  enter ─► one ScopeSetParams (changed fields only) ─► request_scope_set (Local)
//!     └─► focus on and the displayed workspace now out of scope:
//!         focus_or_activate the first admitted workspace in panel order
//! ```
//!
//! Nothing here changes what the shell shows: the Local server owns the scope
//! and the new one comes back as a projection. Rows come from unfiltered
//! snapshots on purpose, so a hidden workspace can be un-hidden. Targets that
//! name no row (an offline machine's, a closed workspace's label) are never
//! touched by a toggle, so they survive an apply.

use std::collections::HashSet;

use crate::scope::{
    ScopeMachineRef, ScopePickMode, ScopeSetParams, ScopeState, ScopeTabRef, ScopeTarget,
    ScopeWorkspaceRef,
};

use super::*;

/// Names a picker row across rebuilds: snapshots can change while it is open.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Which mark `space` and a row click toggle; `f` and `x` reach both.
    pub(super) mode: ScopePickMode,
    /// The scope when the picker opened. Apply sends only the fields that
    /// differ from it, so a field another client changed meanwhile is left
    /// alone unless this picker edited it too.
    pub(super) opened: ScopeState,
    /// The edited scope. It starts as a full copy of `opened`, which is what
    /// keeps targets no row shows when the picker applies.
    pub(super) staged: ScopeState,
    pub(super) selected: Option<ScopePickerRowKey>,
    pub(super) scroll: usize,
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
    render::workspace_entries(snapshot, &HashSet::new())
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

pub(super) fn scope_picker_selected_index(
    rows: &[ScopePickerRow],
    selected: Option<&ScopePickerRowKey>,
) -> usize {
    selected
        .and_then(|key| rows.iter().position(|row| &row.key == key))
        .unwrap_or(0)
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
        }));
    }

    /// The open picker's rows as it would draw them now.
    #[cfg(test)]
    pub(super) fn scope_picker_rows_now(&self) -> Vec<ScopePickerRow> {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::ScopePicker(picker)) => {
                scope_picker_rows(&self.endpoints, &picker.staged)
            }
            _ => Vec::new(),
        }
    }

    /// Moves the selection by `delta` rows, or to an end with `isize::MIN` /
    /// `isize::MAX`, and scrolls so it stays in the last rendered viewport.
    pub(super) fn move_scope_picker_selection(&mut self, delta: isize) {
        let viewport = self.hits.scope_picker_viewport_rows.max(1);
        let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        let rows = scope_picker_rows(&self.endpoints, &picker.staged);
        let Some(last) = rows.len().checked_sub(1) else {
            picker.selected = None;
            return;
        };
        let current = scope_picker_selected_index(&rows, picker.selected.as_ref());
        let next = current.saturating_add_signed(delta).min(last);
        picker.selected = Some(rows[next].key.clone());
        picker.scroll = picker.scroll.clamp(next.saturating_sub(viewport - 1), next);
    }

    /// Toggles `mark` on the row `key` names, or on the selected row.
    pub(super) fn toggle_scope_picker_row(
        &mut self,
        key: Option<&ScopePickerRowKey>,
        mark: ScopePickMode,
    ) {
        let Some(ClientShellOverlay::ScopePicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        let rows = scope_picker_rows(&self.endpoints, &picker.staged);
        let key = key.or(picker.selected.as_ref());
        let index = scope_picker_selected_index(&rows, key);
        let Some(row) = rows.get(index) else {
            return;
        };
        toggle_mark(&mut picker.staged, &row.target, mark);
        picker.selected = Some(row.key.clone());
    }

    /// Keys while the picker is open; returns false when it is not.
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
        match code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter => self.apply_scope_picker(outcome),
            KeyCode::Up => self.move_scope_picker_selection(-1),
            KeyCode::Down => self.move_scope_picker_selection(1),
            KeyCode::Char('k') if plain => self.move_scope_picker_selection(-1),
            KeyCode::Char('j') if plain => self.move_scope_picker_selection(1),
            KeyCode::PageUp => self.move_scope_picker_selection(-8),
            KeyCode::PageDown => self.move_scope_picker_selection(8),
            KeyCode::Char('u') if control => self.move_scope_picker_selection(-8),
            KeyCode::Char('d') if control => self.move_scope_picker_selection(8),
            KeyCode::Home => self.move_scope_picker_selection(isize::MIN),
            KeyCode::Char('g') if plain => self.move_scope_picker_selection(isize::MIN),
            KeyCode::End => self.move_scope_picker_selection(isize::MAX),
            KeyCode::Char('G') if plain || modifiers == KeyModifiers::SHIFT => {
                self.move_scope_picker_selection(isize::MAX)
            }
            KeyCode::Char(' ') if plain => self.toggle_scope_picker_row(None, mode),
            KeyCode::Char('f') if plain => self.toggle_scope_picker_row(None, ScopePickMode::Focus),
            KeyCode::Char('x') if plain => self.toggle_scope_picker_row(None, ScopePickMode::Hide),
            KeyCode::Tab | KeyCode::BackTab => {
                let next = match mode {
                    ScopePickMode::Focus => ScopePickMode::Hide,
                    ScopePickMode::Hide => ScopePickMode::Focus,
                };
                self.activate_scope_picker_control(ScopePickerControl::Mode(next), outcome);
            }
            KeyCode::Char('a') if plain => {
                self.activate_scope_picker_control(ScopePickerControl::ToggleActive, outcome)
            }
            KeyCode::Char('c') if plain => {
                self.activate_scope_picker_control(ScopePickerControl::Clear, outcome)
            }
            _ => return true,
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
        // `c` restores the config hides the client knows, which are the ones
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
        next.focus.active = !next.focus.active;
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
