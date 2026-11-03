//! Focus and hide: one runtime filter over what the shell shows, across every
//! machine.
//!
//! ```text
//!  herdr scope … / plugin scope.set / picker ──► local server: AppState.scope
//!                                                  │ seeded from [ui.sidebar.agents].hidden_workspaces
//!                                                  ▼ scope.projection (EndpointControl)
//!  client: reads the scope from the Local endpoint only, applies it to every
//!          endpoint's rows, agent and workspace keys, and notifications
//!  plugins: scope.get returns the server's verdict for local tabs
//! ```
//!
//! The local server owns the state (in memory: it ends with the server), so it
//! outlives client detach and every attached client sees the same scope. The
//! client, the server and plugins all judge with the predicates here, so a
//! single rule decides what is visible:
//!
//! - Focus, while active and non-empty, admits only the tabs its targets cover.
//! - Hide removes the tabs its targets cover.
//! - When both cover a tab, the more specific target (machine < workspace <
//!   tab) wins and focus wins ties, so focusing `setup` while hiding
//!   `setup/scratch` shows `setup` without `scratch`, and focusing `ft`
//!   overrides the config's hide of `ft`.
//!
//! Visibility is presentation only: out-of-scope agents keep their state, and
//! an explicit jump to an out-of-scope pane still works.

use serde::{Deserialize, Serialize};

/// `ClientEndpointId::Local.storage_key()`.
pub(crate) const LOCAL_MACHINE: &str = "local";
/// The label the client shows for the local endpoint.
pub(crate) const LOCAL_MACHINE_LABEL: &str = "Local";
/// A target machine that matches every machine (config hides use it).
pub(crate) const ANY_MACHINE: &str = "*";

/// One focused or hidden thing: a whole machine, a workspace, or one tab.
///
/// `machine` is an endpoint storage key (`local`, `ssh:<hex>`), a machine label
/// (`studio`, matched case-insensitively), or `*`. Workspace and tab are each
/// matched by id when one is given (exact, survives renames) and otherwise by
/// label (exact, re-matches a recreated workspace). A target with no workspace
/// covers its whole machine; one with a workspace but no tab covers that
/// workspace.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScopeTarget {
    pub machine: String,
    /// The machine's label when the target was made; display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_label: Option<String>,
    /// Seeded from `[ui.sidebar.agents].hidden_workspaces`. The server keeps
    /// these in step with the config; everything else is runtime-only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub config: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScopeFocus {
    /// Focus can be switched off without forgetting its targets.
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub targets: Vec<ScopeTarget>,
}

/// The whole scope, as the server stores it, projects it and returns it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScopeState {
    #[serde(default)]
    pub focus: ScopeFocus,
    #[serde(default)]
    pub hidden: Vec<ScopeTarget>,
}

/// `scope.set`: each field given replaces that part of the scope outright. A
/// set is never a flip, so a caller that timed out and retries cannot toggle
/// twice.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScopeSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_targets: Option<Vec<ScopeTarget>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<Vec<ScopeTarget>>,
}

/// `scope.pick`: open the picker on the foreground client.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScopePickParams {
    #[serde(default)]
    pub mode: ScopePickMode,
}

/// `scope.mirror`: an attached client hands a remote server the scope it
/// filters by, so that server's `scope.get` judges its tabs as the client's
/// panels do.
///
/// `machine` and `machine_label` are how the client names this server
/// (`ClientEndpointId::storage_key()` and the endpoint label, exactly what its
/// `ScopeView` uses). The server judges its tabs as that machine instead of
/// `local`, so targets need no rewriting: both sides run the same predicate on
/// the same names. `scope: null` clears the mirror.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScopeMirrorParams {
    #[serde(default)]
    pub scope: Option<ScopeState>,
    pub machine: String,
    pub machine_label: String,
}

/// One client's mirrored scope, as a server stores it per client connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeMirror {
    pub(crate) scope: ScopeState,
    pub(crate) machine: String,
    pub(crate) machine_label: String,
}

impl ScopeMirror {
    /// The mirror `params` asks for; `None` when it clears one.
    pub(crate) fn from_params(params: ScopeMirrorParams) -> Option<Self> {
        let ScopeMirrorParams {
            scope,
            machine,
            machine_label,
        } = params;
        scope.map(|scope| Self {
            scope,
            machine,
            machine_label,
        })
    }

    /// This server as the mirroring client names it.
    pub(crate) fn machine(&self) -> ScopeMachineRef<'_> {
        ScopeMachineRef {
            key: &self.machine,
            label: &self.machine_label,
        }
    }
}

/// The server's verdict for one local tab, so plugins filter without
/// reimplementing the rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScopeLocalTab {
    pub workspace_id: String,
    pub tab_id: String,
    pub admitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScopeLocalReport {
    pub tabs: Vec<ScopeLocalTab>,
}

/// Which mark the picker's `space` toggles; both marks stay one key away.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ScopePickMode {
    #[default]
    Focus,
    Hide,
}

/// A machine as the caller sees it: endpoint storage key and display label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScopeMachineRef<'a> {
    pub(crate) key: &'a str,
    pub(crate) label: &'a str,
}

impl ScopeMachineRef<'static> {
    pub(crate) const LOCAL: Self = Self {
        key: LOCAL_MACHINE,
        label: LOCAL_MACHINE_LABEL,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScopeWorkspaceRef<'a> {
    pub(crate) machine: ScopeMachineRef<'a>,
    pub(crate) id: &'a str,
    pub(crate) label: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScopeTabRef<'a> {
    pub(crate) workspace: ScopeWorkspaceRef<'a>,
    pub(crate) id: &'a str,
    pub(crate) label: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ScopeSpecificity {
    Machine,
    Workspace,
    Tab,
}

impl ScopeTarget {
    pub(crate) fn specificity(&self) -> ScopeSpecificity {
        if self.tab_id.is_some() || self.tab_label.is_some() {
            ScopeSpecificity::Tab
        } else if self.workspace_id.is_some() || self.workspace_label.is_some() {
            ScopeSpecificity::Workspace
        } else {
            ScopeSpecificity::Machine
        }
    }

    pub(crate) fn covers_machine(&self, machine: ScopeMachineRef<'_>) -> bool {
        self.machine == ANY_MACHINE
            || self.machine == machine.key
            || self.machine.eq_ignore_ascii_case(machine.label)
    }

    /// Whether this target names `workspace` or the machine it is on. A tab
    /// target also matches here; `covers_tab` narrows it.
    fn matches_workspace(&self, workspace: ScopeWorkspaceRef<'_>) -> bool {
        self.covers_machine(workspace.machine)
            && match (&self.workspace_id, &self.workspace_label) {
                (Some(id), _) => id == workspace.id,
                (None, Some(label)) => label == workspace.label,
                (None, None) => true,
            }
    }

    pub(crate) fn covers_tab(&self, tab: ScopeTabRef<'_>) -> bool {
        self.matches_workspace(tab.workspace)
            && match (&self.tab_id, &self.tab_label) {
                (Some(id), _) => id == tab.id,
                (None, Some(label)) => label == tab.label,
                (None, None) => true,
            }
    }

    /// Covers the workspace as a whole: a machine or workspace target. Only
    /// used for a workspace that has no tabs to judge.
    fn covers_whole_workspace(&self, workspace: ScopeWorkspaceRef<'_>) -> bool {
        self.specificity() != ScopeSpecificity::Tab && self.matches_workspace(workspace)
    }

    /// Whether two targets name the same thing (config flag and display-only
    /// labels aside). Used to toggle a target in or out of a list. Each part
    /// compares like with like: ids when both sides carry one, labels when
    /// neither does. An id never stands in for a label, so a workspace
    /// labelled `w1` is not the workspace whose id is `w1`; matching a typed
    /// label against a stored id is the CLI's separate, explicit step.
    pub(crate) fn same_target(&self, other: &ScopeTarget) -> bool {
        fn same_part(
            (id, label): (&Option<String>, &Option<String>),
            (other_id, other_label): (&Option<String>, &Option<String>),
        ) -> bool {
            match (id, other_id) {
                (Some(id), Some(other_id)) => id == other_id,
                (None, None) => label == other_label,
                _ => false,
            }
        }
        self.machine == other.machine
            && same_part(
                (&self.workspace_id, &self.workspace_label),
                (&other.workspace_id, &other.workspace_label),
            )
            && same_part(
                (&self.tab_id, &self.tab_label),
                (&other.tab_id, &other.tab_label),
            )
    }

    /// Parses the CLI form `[machine:]workspace[/tab]`. The machine defaults to
    /// `local`; `studio:` alone names the whole machine. Every part is a label
    /// (or a key such as `local`); the server swaps in ids where it can.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("empty scope target".into());
        }
        let (machine, rest) = match text.split_once(':') {
            Some((machine, rest)) => (machine.trim(), rest.trim()),
            None => (LOCAL_MACHINE, text),
        };
        if machine.is_empty() {
            return Err(format!("scope target {text:?} names no machine"));
        }
        let mut target = Self {
            machine: machine.to_string(),
            ..Self::default()
        };
        if rest.is_empty() {
            return Ok(target);
        }
        let (workspace, tab) = match rest.split_once('/') {
            Some((workspace, tab)) => (workspace.trim(), Some(tab.trim())),
            None => (rest, None),
        };
        if workspace.is_empty() || tab.is_some_and(str::is_empty) {
            return Err(format!(
                "scope target {text:?} has an empty workspace or tab"
            ));
        }
        target.workspace_label = Some(workspace.to_string());
        target.tab_label = tab.map(str::to_string);
        Ok(target)
    }

    /// The CLI form, preferring labels; ids stand in when a label is missing.
    pub(crate) fn display(&self) -> String {
        let machine = self.machine_label.as_deref().unwrap_or(&self.machine);
        let workspace = self
            .workspace_label
            .as_deref()
            .or(self.workspace_id.as_deref());
        let tab = self.tab_label.as_deref().or(self.tab_id.as_deref());
        match (workspace, tab) {
            (None, _) => format!("{machine}:"),
            (Some(workspace), None) => format!("{machine}:{workspace}"),
            (Some(workspace), Some(tab)) => format!("{machine}:{workspace}/{tab}"),
        }
    }

    fn config_hide(label: &str) -> Self {
        Self {
            machine: ANY_MACHINE.into(),
            workspace_label: Some(label.to_string()),
            config: true,
            ..Self::default()
        }
    }
}

impl ScopeState {
    /// The scope a client uses when its local server cannot project one: the
    /// config's hidden workspaces, nothing focused.
    pub(crate) fn from_config_hidden(labels: &[String]) -> Self {
        let mut state = Self::default();
        state.seed_config_hidden(labels);
        state
    }

    /// Focus filters only while it is switched on and names something.
    pub(crate) fn focus_engaged(&self) -> bool {
        self.focus.active && !self.focus.targets.is_empty()
    }

    pub(crate) fn admits_tab(&self, tab: ScopeTabRef<'_>) -> bool {
        self.verdict(|target| target.covers_tab(tab))
    }

    /// A workspace row shows when any of its tabs is admitted. A workspace
    /// with no tabs is judged by the machine and workspace targets alone.
    pub(crate) fn shows_workspace<'a>(
        &self,
        workspace: ScopeWorkspaceRef<'_>,
        tabs: impl IntoIterator<Item = ScopeTabRef<'a>>,
    ) -> bool {
        let mut has_tabs = false;
        for tab in tabs {
            has_tabs = true;
            if self.admits_tab(tab) {
                return true;
            }
        }
        !has_tabs && self.verdict(|target| target.covers_whole_workspace(workspace))
    }

    /// A machine header shows while any of its workspaces does, and otherwise
    /// only when focus is off and no machine-level hide covers it (so an empty
    /// or offline machine still shows outside focus).
    pub(crate) fn shows_machine(
        &self,
        machine: ScopeMachineRef<'_>,
        any_workspace_shown: bool,
    ) -> bool {
        any_workspace_shown
            || (!self.focus_engaged()
                && !self.hidden.iter().any(|target| {
                    target.specificity() == ScopeSpecificity::Machine
                        && target.covers_machine(machine)
                }))
    }

    fn verdict(&self, covers: impl Fn(&ScopeTarget) -> bool) -> bool {
        let strongest = |targets: &[ScopeTarget]| {
            targets
                .iter()
                .filter(|target| covers(target))
                .map(ScopeTarget::specificity)
                .max()
        };
        let hidden = strongest(&self.hidden);
        if self.focus_engaged() {
            strongest(&self.focus.targets)
                .is_some_and(|focus| hidden.is_none_or(|hidden| focus >= hidden))
        } else {
            hidden.is_none()
        }
    }

    /// Applies `scope.set`: every field given replaces its part outright.
    pub(crate) fn apply_set(&mut self, params: ScopeSetParams) {
        if let Some(targets) = params.focus_targets {
            self.focus.targets = targets;
        }
        if let Some(active) = params.focus_active {
            self.focus.active = active;
        }
        if let Some(hidden) = params.hidden {
            self.hidden = hidden;
        }
    }

    /// Adds a config hide for each label not already hidden by config.
    pub(crate) fn seed_config_hidden(&mut self, labels: &[String]) {
        for label in labels {
            let target = ScopeTarget::config_hide(label);
            if !self
                .hidden
                .iter()
                .any(|hidden| hidden.config && hidden.same_target(&target))
            {
                self.hidden.push(target);
            }
        }
    }

    /// Follows a config reload: drops config hides for labels the config no
    /// longer lists and adds the newly listed ones. Labels in both lists are
    /// left alone, so a config hide the owner removed at runtime stays removed
    /// until the config itself changes.
    pub(crate) fn apply_config_hidden_change(&mut self, old: &[String], new: &[String]) {
        let dropped: Vec<&String> = old.iter().filter(|label| !new.contains(label)).collect();
        self.hidden.retain(|target| {
            !(target.config
                && target
                    .workspace_label
                    .as_ref()
                    .is_some_and(|label| dropped.contains(&label)))
        });
        let added: Vec<String> = new
            .iter()
            .filter(|label| !old.contains(label))
            .cloned()
            .collect();
        self.seed_config_hidden(&added);
    }

    /// `herdr scope clear`: forgets focus and runtime hides, and restores the
    /// config's hides.
    pub(crate) fn clear(&mut self, config_labels: &[String]) {
        *self = Self::from_config_hidden(config_labels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STUDIO: ScopeMachineRef<'static> = ScopeMachineRef {
        key: "ssh:0123456789abcdef0123456789abcdef",
        label: "studio",
    };

    fn ws<'a>(machine: ScopeMachineRef<'a>, id: &'a str, label: &'a str) -> ScopeWorkspaceRef<'a> {
        ScopeWorkspaceRef { machine, id, label }
    }

    fn tab<'a>(workspace: ScopeWorkspaceRef<'a>, id: &'a str, label: &'a str) -> ScopeTabRef<'a> {
        ScopeTabRef {
            workspace,
            id,
            label,
        }
    }

    fn target(text: &str) -> ScopeTarget {
        ScopeTarget::parse(text).unwrap()
    }

    fn focus(targets: &[&str]) -> ScopeState {
        ScopeState {
            focus: ScopeFocus {
                active: true,
                targets: targets.iter().map(|text| target(text)).collect(),
            },
            hidden: Vec::new(),
        }
    }

    #[test]
    fn an_empty_scope_admits_everything() {
        let scope = ScopeState::default();
        let setup = ws(ScopeMachineRef::LOCAL, "w1", "setup");
        assert!(scope.admits_tab(tab(setup, "w1:t1", "herdr")));
        assert!(scope.shows_workspace(setup, [tab(setup, "w1:t1", "herdr")]));
        assert!(scope.shows_machine(STUDIO, false));
    }

    #[test]
    fn focus_admits_only_covered_tabs_on_any_machine() {
        let scope = focus(&["setup", "studio:main/outreach"]);
        let setup = ws(ScopeMachineRef::LOCAL, "w1", "setup");
        let admin = ws(ScopeMachineRef::LOCAL, "w2", "admin");
        let main = ws(STUDIO, "w1", "main");
        assert!(scope.admits_tab(tab(setup, "w1:t1", "herdr")));
        assert!(scope.admits_tab(tab(setup, "w1:t2", "nvim")));
        assert!(!scope.admits_tab(tab(admin, "w2:t1", "fish")));
        assert!(scope.admits_tab(tab(main, "w1:t3", "outreach")));
        assert!(!scope.admits_tab(tab(main, "w1:t1", "fish")));
        // A focused tab keeps its workspace row; an unfocused machine leaves.
        assert!(scope.shows_workspace(
            main,
            [tab(main, "w1:t1", "fish"), tab(main, "w1:t3", "outreach")]
        ));
        assert!(!scope.shows_workspace(admin, [tab(admin, "w2:t1", "fish")]));
        assert!(!scope.shows_machine(STUDIO, false));
        assert!(scope.shows_machine(STUDIO, true));
    }

    #[test]
    fn inactive_or_empty_focus_filters_nothing() {
        let mut scope = focus(&["setup"]);
        scope.focus.active = false;
        let admin = ws(ScopeMachineRef::LOCAL, "w2", "admin");
        assert!(scope.admits_tab(tab(admin, "w2:t1", "fish")));
        let empty = ScopeState {
            focus: ScopeFocus {
                active: true,
                targets: Vec::new(),
            },
            hidden: Vec::new(),
        };
        assert!(!empty.focus_engaged());
        assert!(empty.admits_tab(tab(admin, "w2:t1", "fish")));
    }

    #[test]
    fn hide_removes_covered_tabs_and_rows() {
        let scope = ScopeState {
            hidden: vec![target("*:ft"), target("setup/scratch")],
            ..ScopeState::default()
        };
        let ft = ws(STUDIO, "w4", "ft");
        let setup = ws(ScopeMachineRef::LOCAL, "w1", "setup");
        assert!(!scope.admits_tab(tab(ft, "w4:t1", "workers")));
        assert!(!scope.shows_workspace(ft, [tab(ft, "w4:t1", "workers")]));
        assert!(!scope.admits_tab(tab(setup, "w1:t2", "scratch")));
        assert!(scope.admits_tab(tab(setup, "w1:t1", "herdr")));
        // Hiding a workspace keeps its machine outside focus.
        assert!(scope.shows_machine(STUDIO, false));
    }

    #[test]
    fn the_more_specific_target_wins_and_focus_wins_ties() {
        let setup = ws(ScopeMachineRef::LOCAL, "w1", "setup");
        let ft = ws(ScopeMachineRef::LOCAL, "w3", "ft");

        let mut scope = focus(&["setup"]);
        scope.hidden.push(target("setup/scratch"));
        assert!(scope.admits_tab(tab(setup, "w1:t1", "herdr")));
        assert!(!scope.admits_tab(tab(setup, "w1:t2", "scratch")));

        let mut scope = focus(&["ft"]);
        scope.seed_config_hidden(&["ft".into()]);
        assert!(scope.admits_tab(tab(ft, "w3:t1", "workers")));

        let mut scope = focus(&["local:"]);
        scope.hidden.push(target("ft"));
        assert!(!scope.admits_tab(tab(ft, "w3:t1", "workers")));
        assert!(scope.admits_tab(tab(setup, "w1:t1", "herdr")));
    }

    #[test]
    fn a_machine_hide_drops_the_header_unless_focus_reaches_inside() {
        let main = ws(STUDIO, "w1", "main");
        let mut scope = ScopeState {
            hidden: vec![target("studio:")],
            ..ScopeState::default()
        };
        assert!(!scope.admits_tab(tab(main, "w1:t1", "fish")));
        assert!(!scope.shows_machine(STUDIO, false));
        assert!(scope.shows_machine(ScopeMachineRef::LOCAL, false));

        scope.focus = focus(&["studio:main"]).focus;
        assert!(scope.admits_tab(tab(main, "w1:t1", "fish")));
        assert!(scope.shows_machine(STUDIO, true));
    }

    #[test]
    fn ids_win_over_labels_and_labels_rematch_recreated_workspaces() {
        let by_id = ScopeState {
            hidden: vec![ScopeTarget {
                machine: LOCAL_MACHINE.into(),
                workspace_id: Some("w1".into()),
                workspace_label: Some("setup".into()),
                ..ScopeTarget::default()
            }],
            ..ScopeState::default()
        };
        let renamed = ws(ScopeMachineRef::LOCAL, "w1", "renamed");
        let recreated = ws(ScopeMachineRef::LOCAL, "w9", "setup");
        assert!(!by_id.admits_tab(tab(renamed, "w1:t1", "fish")));
        assert!(by_id.admits_tab(tab(recreated, "w9:t1", "fish")));

        let by_label = ScopeState::from_config_hidden(&["setup".into()]);
        assert!(!by_label.admits_tab(tab(recreated, "w9:t1", "fish")));
        assert!(by_label.admits_tab(tab(
            ws(ScopeMachineRef::LOCAL, "w5", "setup-2"),
            "w5:t1",
            "fish"
        )));
    }

    #[test]
    fn machines_match_by_key_or_label_case_insensitively() {
        assert!(target("local:setup").covers_machine(ScopeMachineRef::LOCAL));
        assert!(target("Local:setup").covers_machine(ScopeMachineRef::LOCAL));
        assert!(target("Studio:main").covers_machine(STUDIO));
        assert!(target("*:ft").covers_machine(STUDIO));
        assert!(!target("studio:main").covers_machine(ScopeMachineRef::LOCAL));
    }

    #[test]
    fn a_workspace_without_tabs_is_judged_as_a_whole() {
        let scope = focus(&["setup/herdr"]);
        let setup = ws(ScopeMachineRef::LOCAL, "w1", "setup");
        assert!(!scope.shows_workspace(setup, []));
        assert!(focus(&["setup"]).shows_workspace(setup, []));
    }

    #[test]
    fn parse_and_display_round_trip_the_cli_form() {
        assert_eq!(
            target("setup"),
            ScopeTarget {
                machine: "local".into(),
                workspace_label: Some("setup".into()),
                ..ScopeTarget::default()
            }
        );
        assert_eq!(
            target("studio:main/outreach").tab_label.as_deref(),
            Some("outreach")
        );
        assert_eq!(target("studio:").specificity(), ScopeSpecificity::Machine);
        for text in ["local:setup", "studio:main/outreach", "studio:"] {
            assert_eq!(target(text).display(), text);
        }
        for bad in ["", ":setup", "setup/", "studio:/x"] {
            assert!(ScopeTarget::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn config_reload_applies_only_the_diff() {
        let mut scope = ScopeState::from_config_hidden(&["ft".into(), "old".into()]);
        // The owner un-hides ft at runtime and hides admin by hand.
        scope
            .hidden
            .retain(|target| target.workspace_label.as_deref() != Some("ft"));
        scope.hidden.push(target("admin"));

        scope
            .apply_config_hidden_change(&["ft".into(), "old".into()], &["ft".into(), "new".into()]);
        let labels: Vec<_> = scope
            .hidden
            .iter()
            .map(|target| (target.workspace_label.as_deref().unwrap(), target.config))
            .collect();
        assert_eq!(labels, [("admin", false), ("new", true)]);

        scope.clear(&["ft".into()]);
        assert_eq!(scope, ScopeState::from_config_hidden(&["ft".into()]));
    }

    #[test]
    fn same_target_never_mistakes_an_id_for_a_label() {
        let pinned = ScopeTarget {
            machine: LOCAL_MACHINE.into(),
            workspace_id: Some("w1".into()),
            workspace_label: Some("setup".into()),
            ..ScopeTarget::default()
        };
        // A different workspace whose label happens to be another's id.
        assert!(!pinned.same_target(&target("w1")));
        assert!(!pinned.same_target(&target("setup")));
        let renamed = ScopeTarget {
            workspace_label: Some("renamed".into()),
            ..pinned.clone()
        };
        assert!(
            pinned.same_target(&renamed),
            "the same id is the same workspace"
        );
        assert!(target("setup/herdr").same_target(&target("setup/herdr")));
        assert!(!target("setup").same_target(&target("setup/herdr")));
    }

    #[test]
    fn set_replaces_only_the_fields_it_names() {
        let mut scope = ScopeState::from_config_hidden(&["ft".into()]);
        scope.apply_set(ScopeSetParams {
            focus_targets: Some(vec![target("setup")]),
            focus_active: Some(true),
            hidden: None,
        });
        assert!(scope.focus_engaged());
        assert_eq!(scope.hidden.len(), 1);
        scope.apply_set(ScopeSetParams {
            focus_active: Some(false),
            ..ScopeSetParams::default()
        });
        assert_eq!(scope.focus.targets, [target("setup")]);
        assert!(!scope.focus_engaged());
    }

    #[test]
    fn the_wire_form_omits_empty_fields() {
        let json = serde_json::to_value(ScopeState::from_config_hidden(&["ft".into()])).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "focus": {"active": false, "targets": []},
                "hidden": [{"machine": "*", "workspace_label": "ft", "config": true}],
            })
        );
        let back: ScopeState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(back, ScopeState::default());
    }
}
