//! Ferry's Herdr calls: topology reads, pane moves, focus, closes and notifications.
//!
//! Every call goes through [`tools_core::Host`]. Moves, focus, closes and notifications
//! use the `herdr` CLI as ferry always did; only opening the picker popup uses the socket
//! API, through [`Host::open_popup_sized`], so the shared config can size it.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{bail, Result};
use serde::Deserialize;
use tools_core::{Config, Envelope, Host, HostError, Invocation, ScopeFilter};

use crate::layout::{LayoutSnapshot, SplitDirection};
use crate::{NOTIFICATION_TITLE, PICKER_ENTRYPOINT, POPUP_KEY};

/// Carries the invoking pane from `move open` to the picker popup, which cannot see its
/// caller (Herdr strips `HERDR_PANE_ID` from popups). Ferry's name, kept for continuity.
pub const SOURCE_PANE_ENV: &str = "HERDR_FERRY_SOURCE_PANE_ID";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub number: usize,
    #[serde(default)]
    pub tab_count: usize,
    #[serde(default)]
    pub pane_count: usize,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub worktree: Option<WorktreeInfo>,
}

/// Git worktree membership Herdr reports for a workspace. Workspaces sharing a `repo_key`
/// form a group, and closing the group's root (the non-linked checkout) closes them all.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub repo_key: String,
    #[serde(default)]
    pub is_linked_worktree: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub number: usize,
    #[serde(default)]
    pub pane_count: usize,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PaneInfo {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: String,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topology {
    pub workspaces: Vec<WorkspaceInfo>,
    pub tabs: Vec<TabInfo>,
    pub panes: Vec<PaneInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovedPane {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
}

/// How Herdr answered a close request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    Closed,
    /// Herdr no longer knows the target: someone else closed it first.
    Missing,
}

/// Ferry's Herdr client: the mode-specific calls, each built on one [`Host`] primitive.
#[derive(Clone, Debug)]
pub struct Herdr {
    host: Host,
}

impl Herdr {
    pub fn from_environment() -> Self {
        Self::new(Host::from_env())
    }

    pub fn new(host: Host) -> Self {
        Self { host }
    }

    /// The Herdr executable this client runs, for handing to processes Ferry starts.
    pub(crate) fn binary(&self) -> &Path {
        self.host.binary()
    }

    pub fn topology(&self) -> Result<Topology> {
        let workspaces: Envelope<WorkspaceListResult> =
            self.host.cli_json(["workspace", "list"])?;
        let tabs: Envelope<TabListResult> = self.host.cli_json(["tab", "list"])?;
        let panes: Envelope<PaneListResult> = self.host.cli_json(["pane", "list"])?;
        Ok(Topology {
            workspaces: workspaces.result.workspaces,
            tabs: tabs.result.tabs,
            panes: panes.result.panes,
        })
    }

    /// Which tabs the picker may list, as Herdr's server judges focus and hide (`scope.get`,
    /// over the socket). Fails open: no socket, a Herdr without scope or a failed read
    /// admits every tab, so scope can never cost a working picker.
    pub fn scope_filter(&self) -> ScopeFilter {
        self.host.scope_filter()
    }

    pub fn pane(&self, pane_id: &str) -> Result<PaneInfo> {
        let response: Envelope<PaneResult> = self.host.cli_json(["pane", "get", pane_id])?;
        Ok(response.result.pane)
    }

    pub fn layout(&self, pane_id: &str) -> Result<LayoutSnapshot> {
        let response: Envelope<LayoutResult> =
            self.host.cli_json(["pane", "layout", "--pane", pane_id])?;
        Ok(response.result.layout)
    }

    /// Opens the picker popup for `source_pane_id`.
    ///
    /// The size is `config`'s for [`POPUP_KEY`] on the source pane's client, the resolution
    /// [`Host::open_popup`] performs with the process-wide config; taking `config` keeps this
    /// testable. With no size configured, Herdr applies the manifest pane's 76×24, which ferry
    /// used to pass explicitly. The popup learns its source pane from [`SOURCE_PANE_ENV`].
    pub fn launch_picker(&self, config: &Config, source_pane_id: &str) -> Result<()> {
        let client_width = self.host.client_width(source_pane_id).ok();
        let size = config
            .popup(POPUP_KEY, client_width)
            .map(|resolved| resolved.dims);
        self.host.open_popup_sized(
            PICKER_ENTRYPOINT,
            size,
            &[(SOURCE_PANE_ENV, source_pane_id)],
        )
    }

    pub fn move_to_tab(
        &self,
        pane_id: &str,
        tab_id: &str,
        direction: SplitDirection,
        target_pane_id: Option<&str>,
        ratio: Option<f32>,
        focus: bool,
    ) -> Result<MovedPane> {
        let mut args = vec![
            OsString::from("pane"),
            OsString::from("move"),
            OsString::from(pane_id),
            OsString::from("--tab"),
            OsString::from(tab_id),
            OsString::from("--split"),
            OsString::from(direction.as_str()),
        ];
        if let Some(target_pane_id) = target_pane_id {
            args.push(OsString::from("--target-pane"));
            args.push(OsString::from(target_pane_id));
        }
        if let Some(ratio) = ratio {
            args.push(OsString::from("--ratio"));
            args.push(OsString::from(ratio.to_string()));
        }
        args.push(OsString::from(if focus { "--focus" } else { "--no-focus" }));
        self.move_pane(args)
    }

    pub fn move_to_new_tab(
        &self,
        pane_id: &str,
        workspace_id: &str,
        label: Option<&str>,
        focus: bool,
    ) -> Result<MovedPane> {
        let mut args = vec![
            OsString::from("pane"),
            OsString::from("move"),
            OsString::from(pane_id),
            OsString::from("--new-tab"),
            OsString::from("--workspace"),
            OsString::from(workspace_id),
        ];
        push_value(&mut args, "--label", label);
        args.push(OsString::from(if focus { "--focus" } else { "--no-focus" }));
        self.move_pane(args)
    }

    pub fn move_to_new_workspace(
        &self,
        pane_id: &str,
        label: Option<&str>,
        tab_label: Option<&str>,
        focus: bool,
    ) -> Result<MovedPane> {
        let mut args = vec![
            OsString::from("pane"),
            OsString::from("move"),
            OsString::from(pane_id),
            OsString::from("--new-workspace"),
        ];
        push_value(&mut args, "--label", label);
        push_value(&mut args, "--tab-label", tab_label);
        args.push(OsString::from(if focus { "--focus" } else { "--no-focus" }));
        self.move_pane(args)
    }

    pub fn focus_workspace(&self, workspace_id: &str) -> Result<()> {
        self.host.cli(["workspace", "focus", workspace_id])?;
        Ok(())
    }

    pub fn focus_tab(&self, tab_id: &str) -> Result<()> {
        self.host.cli(["tab", "focus", tab_id])?;
        Ok(())
    }

    pub(crate) fn close_pane(&self, pane_id: &str) -> Result<CloseOutcome> {
        self.close("pane", pane_id)
    }

    pub(crate) fn close_tab(&self, tab_id: &str) -> Result<CloseOutcome> {
        self.close("tab", tab_id)
    }

    pub(crate) fn close_workspace(&self, workspace_id: &str) -> Result<CloseOutcome> {
        self.close("workspace", workspace_id)
    }

    /// Shows a Herdr notification titled [`NOTIFICATION_TITLE`]; the body is cut to 220
    /// characters.
    pub fn notify(&self, body: &str) -> Result<()> {
        self.host.notify(NOTIFICATION_TITLE, body)
    }

    /// Runs `herdr <noun> close <id>`. Herdr reports failures as JSON on stderr, which
    /// [`Host::cli`] turns into a [`HostError`]; its `<noun>_not_found` code means the target
    /// is already gone, which a kill treats as done rather than failed. Other errors surface
    /// with Herdr's own message.
    fn close(&self, noun: &str, id: &str) -> Result<CloseOutcome> {
        let Err(error) = self.host.cli([noun, "close", id]) else {
            return Ok(CloseOutcome::Closed);
        };
        match error.downcast_ref::<HostError>() {
            Some(refusal) if refusal.code == format!("{noun}_not_found") => {
                Ok(CloseOutcome::Missing)
            }
            // Not Herdr's JSON error: `message` holds the exit status and output.
            Some(refusal) if refusal.code == "command_failed" => {
                bail!("Herdr command failed with {}", refusal.message)
            }
            Some(refusal) => bail!("{}", refusal.message),
            None => Err(error),
        }
    }

    fn move_pane(&self, args: Vec<OsString>) -> Result<MovedPane> {
        let response: Envelope<PaneMoveResult> = self.host.cli_json(args)?;
        let result = response.result.move_result;
        if !result.changed {
            let reason = result.reason.unwrap_or_else(|| "unknown reason".into());
            bail!("Herdr declined the move: {reason}");
        }
        Ok(MovedPane {
            pane_id: result.pane.pane_id,
            tab_id: result.pane.tab_id,
            workspace_id: result.pane.workspace_id,
        })
    }
}

/// Entry point of `herdr-tools move open`: opens the picker over the invoking pane, and
/// notifies when it cannot.
pub fn launch_from_environment() -> Result<()> {
    let herdr = Herdr::from_environment();
    let result = (|| {
        let source_pane_id = Invocation::current()?.source_pane()?;
        herdr.launch_picker(Config::current()?, source_pane_id)
    })();
    if let Err(error) = &result {
        let _ = herdr.notify(&format!("Could not open Move: {error:#}"));
    }
    result
}

fn push_value(args: &mut Vec<OsString>, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        args.push(OsString::from(key));
        args.push(OsString::from(value));
    }
}

#[derive(Deserialize)]
struct WorkspaceListResult {
    workspaces: Vec<WorkspaceInfo>,
}

#[derive(Deserialize)]
struct TabListResult {
    tabs: Vec<TabInfo>,
}

#[derive(Deserialize)]
struct PaneListResult {
    panes: Vec<PaneInfo>,
}

#[derive(Deserialize)]
struct PaneResult {
    pane: PaneInfo,
}

#[derive(Deserialize)]
struct LayoutResult {
    layout: LayoutSnapshot,
}

#[derive(Deserialize)]
struct PaneMoveResult {
    move_result: RawMoveResult,
}

#[derive(Deserialize)]
struct RawMoveResult {
    changed: bool,
    #[serde(default)]
    reason: Option<String>,
    pane: PaneInfo,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ferry parsed the context itself; `launch_from_environment` now relies on
    /// [`Invocation::source_pane`] for the same rule.
    #[test]
    fn invocation_context_requires_a_valid_focused_pane() {
        let context = Invocation::from_context_json(r#"{"focused_pane_id":"w1:p2"}"#).unwrap();

        assert_eq!(context.source_pane().unwrap(), "w1:p2");
        assert!(Invocation::from_context_json("{}")
            .unwrap()
            .source_pane()
            .is_err());
        assert!(Invocation::from_context_json(r#"{"focused_pane_id":"  "}"#)
            .unwrap()
            .source_pane()
            .is_err());
    }

    #[test]
    fn tests_that_workspace_worktree_membership_is_parsed() {
        let linked: WorkspaceInfo = serde_json::from_str(
            r#"{"workspace_id":"w1","worktree":{"repo_key":"k","repo_name":"repo","repo_root":"/r","checkout_path":"/r/wt","is_linked_worktree":true}}"#,
        )
        .unwrap();
        let plain: WorkspaceInfo = serde_json::from_str(r#"{"workspace_id":"w2"}"#).unwrap();

        assert_eq!(
            linked.worktree,
            Some(WorktreeInfo {
                repo_key: "k".into(),
                is_linked_worktree: true,
            })
        );
        assert_eq!(plain.worktree, None);
    }
}
