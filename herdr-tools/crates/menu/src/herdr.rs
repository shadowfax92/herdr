//! The menu's Herdr seam and its three entry points: `open`, `menu` and
//! `copy-context`.
//!
//! [`Herdr`] is a thin adapter over `tools_core::Host` that keeps the old
//! plugin's method names, so the fork, layout and redistribution controllers
//! carry over unchanged. CLI calls go through `Host::cli`/`cli_json`, the two
//! layout-tree operations the CLI lacks through `Host::request`, and the popup
//! opens with `Host::open_popup_sized`.
//!
//! ```text
//!  open (action) ──▶ source pane + cwd ──▶ popup size ──▶ plugin.pane.open "menu"
//!                                          [popup.size].menu / profile, else
//!                                          [menu] width × automatic height
//!  menu (popup)  ──▶ [menu] ──▶ MenuState + ui ──▶ fork · rotate · redistribute
//!                   (scope.get once,              · copy context · shell items
//!                    when scope rows exist)       · focus / hide picker · focus switch
//! ```
//!
//! Popups cannot see their caller, so `open` hands the source pane and cwd to
//! the popup as `HERDR_MENU_SOURCE_PANE_ID` / `HERDR_MENU_SOURCE_CWD`. Every
//! later action targets that captured pane, never whatever is focused by then.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use ratatui::crossterm::event;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tools_core::{Host, Invocation, Paths, PopupDims, PopupSize};

use crate::agent_panes::AgentPaneController;
use crate::app::{InputOutcome, MenuState};
use crate::config::{Config, ExecutionMode, MenuItem};
use crate::execute::ShellExecutor;
use crate::handoff::HandoffController;
use crate::redistribute::{ApplyOutcome, Redistributor};
use crate::scope::{spawn_pick_helper, ScopeActions, ScopeSupport};
use crate::{ui, POPUP};

/// Title of every notification the menu shows.
const NOTIFICATION_TITLE: &str = "Herdr Menu";

/// Popup size when `config.toml` cannot be loaded: the popup still opens, at
/// the old launcher's fallback size, and shows the config error.
const FALLBACK_SIZE: (u16, u16) = (64, 16);

/// Narrow adapter around the Herdr client the menu's features share.
#[derive(Clone, Debug)]
pub struct Herdr {
    host: Host,
}

impl Herdr {
    /// The Herdr that started this process (`HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`).
    pub fn from_env() -> Self {
        Self {
            host: Host::from_env(),
        }
    }

    /// An explicit binary and no socket, for tests and probes; CLI calls still
    /// inherit the process environment. Add a socket with [`Herdr::with_socket`].
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            host: Host::new(binary, None),
        }
    }

    pub fn with_socket(self, socket: impl Into<PathBuf>) -> Self {
        Self {
            host: Host::new(self.host.binary(), Some(socket.into())),
        }
    }

    /// The tools-core client underneath.
    pub fn host(&self) -> &Host {
        &self.host
    }

    /// The layout-tree socket operations missing from Herdr's CLI
    /// (`layout.export`, `layout.set_split_ratio`).
    pub(crate) fn layout_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.host.request(method, params)
    }

    pub(crate) fn socket_path(&self) -> Option<&Path> {
        self.host.socket_path()
    }

    pub fn binary(&self) -> &Path {
        self.host.binary()
    }

    /// Opens the menu popup over the invoking pane. `config` is None when
    /// `config.toml` could not be loaded; the popup then opens at the fallback
    /// size and reports the error itself.
    pub fn launch(
        &self,
        invocation: &Invocation,
        config: Option<&tools_core::Config>,
    ) -> Result<()> {
        let (source_pane_id, source_cwd) = self.source_context(invocation)?;
        let source_cwd_text = source_cwd
            .to_str()
            .context("source pane working directory is not valid UTF-8")?;
        let size = popup_size(config, || self.host.client_width(&source_pane_id).ok());
        let rtr_binary = std::env::var_os("RTR_BIN_PATH")
            .map(|binary| {
                binary
                    .into_string()
                    .map_err(|_| anyhow!("RTR_BIN_PATH is not valid UTF-8"))
            })
            .transpose()?;
        let mut environment = vec![
            ("HERDR_MENU_SOURCE_CWD", source_cwd_text),
            ("HERDR_MENU_SOURCE_PANE_ID", source_pane_id.as_str()),
        ];
        if let Some(rtr_binary) = rtr_binary.as_deref() {
            environment.push(("RTR_BIN_PATH", rtr_binary));
        }
        self.host
            .open_popup_sized(POPUP, Some(size), &environment)
            .context("opening the menu popup")
    }

    pub fn launch_with_reporting(
        &self,
        invocation: &Invocation,
        config: Option<&tools_core::Config>,
    ) -> Result<()> {
        let result = self.launch(invocation, config);
        if let Err(error) = &result {
            let _ = self.show_notification(&format!("Could not open menu: {error:#}"));
        }
        result
    }

    fn source_context(&self, invocation: &Invocation) -> Result<(String, PathBuf)> {
        let pane_id = invocation
            .source_pane()
            .map_err(|_| anyhow!("plugin context has no valid source pane ID"))?
            .to_string();
        // A context cwd is validated as given (a blank one is an error, not a
        // reason to ask Herdr); only an absent one falls back to `pane get`.
        let context_cwd = invocation
            .context
            .as_ref()
            .and_then(|context| context.focused_pane_cwd.as_deref());
        if let Some(cwd) = context_cwd {
            return Ok((pane_id, validate_directory(cwd)?));
        }
        let cwd = self
            .pane(&pane_id)?
            .cwd
            .context("source pane has no working directory")?;
        Ok((pane_id, validate_directory(&cwd)?))
    }

    pub fn pane(&self, pane_id: &str) -> Result<PaneInfo> {
        let response: PaneEnvelope = self.json(["pane", "get", pane_id])?;
        Ok(response.result.pane)
    }

    pub fn split_pane(&self, pane_id: &str, direction: &str, cwd: &Path) -> Result<String> {
        let cwd = cwd
            .to_str()
            .context("fork working directory is not valid UTF-8")?;
        let response: PaneEnvelope = self.json([
            "pane",
            "split",
            "--pane",
            pane_id,
            "--direction",
            direction,
            "--cwd",
            cwd,
            "--focus",
        ])?;
        Ok(response.result.pane.pane_id)
    }

    pub fn run_in_pane(&self, pane_id: &str, command: &str) -> Result<()> {
        self.output(["pane", "run", pane_id, command])?;
        Ok(())
    }

    pub fn swap_pane(&self, pane_id: &str, direction: &str) -> Result<bool> {
        let response: SwapEnvelope =
            self.json(["pane", "swap", "--pane", pane_id, "--direction", direction])?;
        Ok(response.result.swap.changed)
    }

    pub fn close_pane(&self, pane_id: &str) -> Result<()> {
        self.output(["pane", "close", pane_id])?;
        Ok(())
    }

    pub fn show_notification(&self, body: &str) -> Result<()> {
        self.host.notify(NOTIFICATION_TITLE, body)
    }

    /// Feature controllers own response shapes; this shared process boundary
    /// keeps command failure and JSON decoding behavior consistent across them.
    pub(crate) fn json<T, I, S>(&self, args: I) -> Result<T>
    where
        T: DeserializeOwned,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.host.cli_json(args)
    }

    /// Runs a Herdr CLI command for its effect; a refusal is a `HostError`.
    pub(crate) fn output<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.host.cli(args)
    }
}

/// The menu popup's outer size.
///
/// A configured `[popup.size].menu`, or a matching `[[popup.profile]]` entry,
/// fixes both dimensions (the old `width` + `height`). Without one the menu is
/// [`Config::width`] wide and [`Config::popup_height`] tall, as the old file's
/// unset `height` was. `client_width` is read only when profiles exist, since
/// only they match on it.
pub fn popup_size(
    config: Option<&tools_core::Config>,
    client_width: impl FnOnce() -> Option<u16>,
) -> PopupDims {
    let cells = |(width, height): (u16, u16)| PopupDims {
        width: PopupSize::Cells(width),
        height: PopupSize::Cells(height),
    };
    let Some(config) = config else {
        return cells(FALLBACK_SIZE);
    };
    let width = if config.popup_config().profile.is_empty() {
        None
    } else {
        client_width()
    };
    if let Some(resolved) = config.popup(POPUP, width) {
        return resolved.dims;
    }
    match Config::from_core(config) {
        Ok(menu) => cells((menu.width, menu.popup_height())),
        Err(_) => cells(FALLBACK_SIZE),
    }
}

pub fn launch_from_environment() -> Result<()> {
    let invocation = Invocation::current()?;
    let config = tools_core::Config::current().ok();
    Herdr::from_env().launch_with_reporting(invocation, config)
}

/// The popup and CLI share a copy boundary: capture the explicit source, wait
/// for clipboard success, then notify. Notification delivery cannot undo a
/// successful copy, so it must not turn that success into a retryable failure.
struct CopyContextAction {
    controller: HandoffController,
    herdr: Herdr,
}

impl CopyContextAction {
    fn new(herdr: Herdr, source_pane_id: String) -> Self {
        Self {
            controller: HandoffController::new(herdr.clone(), source_pane_id),
            herdr,
        }
    }

    fn copy(&self) -> Result<()> {
        let document = self.controller.copy()?;
        if let Err(error) = self.herdr.show_notification(&format!(
            "Copied context for {} ({} pane)",
            self.controller.source_pane_id(),
            document.pane_count
        )) {
            eprintln!("Pane context copied; notification failed: {error:#}");
        }
        Ok(())
    }
}

pub fn copy_context_from_environment(source_pane: Option<String>, stdout: bool) -> Result<()> {
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        bail!("copy-context must run inside a Herdr-managed pane (HERDR_ENV=1)");
    }
    // A popup's inherited pane is the popup itself. Its pre-open source takes
    // precedence; CLI use falls back only to the caller, never the focused pane.
    let source_pane = source_pane
        .or_else(|| std::env::var("HERDR_MENU_SOURCE_PANE_ID").ok())
        .or_else(|| std::env::var("HERDR_PANE_ID").ok())
        .filter(|value| !value.trim().is_empty() && !value.chars().any(char::is_control))
        .context("no source pane; set HERDR_PANE_ID or pass --source-pane")?;
    let action = CopyContextAction::new(Herdr::from_env(), source_pane);
    if stdout {
        use std::io::Write;
        std::io::stdout()
            .lock()
            .write_all(action.controller.capture()?.text.as_bytes())?;
        Ok(())
    } else {
        action.copy()
    }
}

pub fn run_menu_from_environment() -> Result<()> {
    let source_cwd = std::env::var_os("HERDR_MENU_SOURCE_CWD")
        .map(PathBuf::from)
        .context("HERDR_MENU_SOURCE_CWD is not set")?;
    let source_pane_id = std::env::var("HERDR_MENU_SOURCE_PANE_ID")
        .context("HERDR_MENU_SOURCE_PANE_ID is not set")?;
    // Every mode keeps its state under `<plugin state dir>/<mode>/`, so no
    // mode's files or cleanup can reach another's. The menu's are the fork
    // rotation record (`last-fork.json` + lock) and `redistribute.lock`.
    let state_dir = Paths::from_env().state_dir().join(crate::MODE);
    let rtr_binary = std::env::var_os("RTR_BIN_PATH").unwrap_or_else(|| "rtr".into());
    let herdr = Herdr::from_env();
    let redistributor = Redistributor::new(
        herdr.clone(),
        source_pane_id.clone(),
        source_cwd.clone(),
        state_dir.clone(),
    );
    let handoff = CopyContextAction::new(herdr.clone(), source_pane_id.clone());
    let scope = ScopeActions::new(herdr.host().clone());
    let controller = AgentPaneController::new(
        herdr,
        source_pane_id,
        source_cwd.clone(),
        state_dir.join("last-fork.json"),
        rtr_binary,
    );
    let config = tools_core::Config::current().and_then(Config::from_core);
    run_menu(
        config,
        &source_cwd,
        &controller,
        &redistributor,
        &handoff,
        &scope,
    )
}

fn run_menu(
    config: Result<Config>,
    source_cwd: &Path,
    controller: &AgentPaneController,
    redistributor: &Redistributor,
    handoff: &CopyContextAction,
    scope: &ScopeActions,
) -> Result<()> {
    let mut state = match config {
        Ok(config) => {
            for warning in &config.warnings {
                eprintln!("Herdr Menu config: {warning}");
            }
            // Read before the first frame, so a Herdr without scope never
            // flashes enabled rows, and only when a row needs it.
            let support = if config.has_scope_items() {
                ScopeSupport::probe(scope.host())
            } else {
                ScopeSupport::Unknown
            };
            MenuState::with_scope(config, support)
        }
        Err(error) => MenuState::fatal(format!("{error:#}")),
    };
    let executor = ShellExecutor::from_environment(source_cwd);
    let mut terminal = ratatui::try_init().context("failed to initialize terminal")?;
    let mut terminal_active = true;
    let result = run_event_loop(
        &mut terminal,
        &mut terminal_active,
        &mut state,
        &executor,
        controller,
        redistributor,
        handoff,
        scope,
    );
    let restore = terminal_active
        .then(ratatui::try_restore)
        .transpose()
        .context("failed to restore terminal");
    result?;
    restore?;
    Ok(())
}

fn run_event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    terminal_active: &mut bool,
    state: &mut MenuState,
    executor: &ShellExecutor,
    controller: &AgentPaneController,
    redistributor: &Redistributor,
    handoff: &CopyContextAction,
    scope: &ScopeActions,
) -> Result<()> {
    loop {
        terminal.draw(|frame| ui::render(state, frame))?;
        match state.handle_event(event::read()?) {
            InputOutcome::Continue => {}
            InputOutcome::Cancel => return Ok(()),
            InputOutcome::ApplyLayout(plan) => match redistributor.apply(&plan) {
                Ok(ApplyOutcome::Applied) => return Ok(()),
                Ok(ApplyOutcome::Refresh(snapshot)) => state.refresh_redistribution(*snapshot),
                Err(error) => state.fail_redistribution(format!("{error:#}")),
            },
            InputOutcome::Execute(MenuItem::Shell(item)) => match item.mode {
                ExecutionMode::Detached => match executor.detached(&item) {
                    Ok(()) => return Ok(()),
                    Err(error) => state.set_failure(format!("{}: {error:#}", item.label)),
                },
                ExecutionMode::Foreground => {
                    ratatui::try_restore().context("failed to release terminal")?;
                    *terminal_active = false;
                    let outcome = executor.foreground(&item);
                    match outcome {
                        Ok(status) if status.success() => return Ok(()),
                        Ok(status) => {
                            state.set_failure(format!("{}: command {status}", item.label))
                        }
                        Err(error) => state.set_failure(format!("{}: {error:#}", item.label)),
                    }
                    *terminal = ratatui::try_init().context("failed to reinitialize terminal")?;
                    *terminal_active = true;
                }
            },
            InputOutcome::Execute(MenuItem::Fork(item)) => match controller.fork(item.placement) {
                Ok(()) => return Ok(()),
                Err(error) => state.set_fork_failure(format!("{}: {error:#}", item.label)),
            },
            InputOutcome::Execute(MenuItem::RotateFork(item)) => match controller.rotate() {
                Ok(()) => return Ok(()),
                Err(error) => state.set_failure(format!("{}: {error:#}", item.label)),
            },
            InputOutcome::Execute(MenuItem::Redistribute(item)) => match redistributor.capture() {
                Ok(snapshot) => state.open_redistribution(snapshot),
                Err(error) => state.set_failure(format!("{}: {error:#}", item.label)),
            },
            InputOutcome::Execute(MenuItem::CopyContext(_)) => match handoff.copy() {
                Ok(()) => return Ok(()),
                Err(error) => state.set_copy_failure(format!("{error:#}")),
            },
            // The picker opens on the client once this popup is gone, so the
            // menu hands the pick to a helper, closes its popup and exits.
            InputOutcome::Execute(MenuItem::ScopePick(item)) => {
                match scope.hand_off_pick(item.mode, spawn_pick_helper) {
                    Ok(()) => return Ok(()),
                    Err(error) => state.set_failure(format!("{}: {error:#}", item.label)),
                }
            }
            InputOutcome::Execute(MenuItem::FocusToggle(item)) => match scope.toggle_focus() {
                Ok(_) => return Ok(()),
                Err(error) => state.set_failure(format!("{}: {error:#}", item.label)),
            },
            // Separators are not selectable.
            InputOutcome::Execute(MenuItem::Separator) => {}
        }
    }
}

fn validate_directory(value: &str) -> Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        bail!("source pane working directory is invalid");
    }
    let path = PathBuf::from(value);
    if !path.is_dir() {
        bail!(
            "source pane working directory does not exist: {}",
            path.display()
        );
    }
    Ok(path)
}

#[derive(Deserialize)]
struct PaneEnvelope {
    result: PaneResult,
}

#[derive(Deserialize)]
struct PaneResult {
    pane: PaneInfo,
}

/// Subset of Herdr pane metadata needed to preserve session and cwd identity.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PaneInfo {
    pub pane_id: String,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub agent: Option<String>,
    pub agent_session: Option<AgentSessionInfo>,
}

/// Hook-owned native session identity reported for an agent pane.
///
/// Callers must validate `source`, `agent`, and `kind` together before trusting
/// `value` as a hard resume/fork ID.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AgentSessionInfo {
    pub source: String,
    pub agent: String,
    pub kind: String,
    pub value: String,
}

#[derive(Deserialize)]
struct SwapEnvelope {
    result: SwapResult,
}

#[derive(Deserialize)]
struct SwapResult {
    swap: SwapInfo,
}

#[derive(Deserialize)]
struct SwapInfo {
    changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(source: &str) -> tools_core::Config {
        tools_core::Config::parse(source).unwrap()
    }

    fn cells(width: u16, height: u16) -> PopupDims {
        PopupDims {
            width: PopupSize::Cells(width),
            height: PopupSize::Cells(height),
        }
    }

    #[test]
    fn unconfigured_size_is_the_menu_width_and_item_count_height() {
        let menu = "[menu]\nmax_height = 18\n[[menu.items]]\nkey='a'\nlabel='a'\ncommand='a'\n\
                    [[menu.items]]\nkey='b'\nlabel='b'\ncommand='b'\n";
        // Two items and the four frame rows, raised to the six-row minimum.
        assert_eq!(popup_size(Some(&core(menu)), || None), cells(64, 6));
        // The built-in menu (no [menu] table) is 68 wide: 13 items, the
        // missing-table warning and the frame.
        assert_eq!(popup_size(Some(&core("")), || None), cells(68, 19));
        // An unreadable config still opens, at the old fallback.
        assert_eq!(popup_size(None, || None), cells(64, 16));
        // A broken [menu] table does too; the popup shows why.
        assert_eq!(
            popup_size(Some(&core("[menu]\ntitle = 3\n")), || None),
            cells(64, 16)
        );
    }

    #[test]
    fn configured_size_wins_and_profiles_match_on_client_width() {
        let config = core(
            r#"
[popup.size]
menu = { width = 80, height = 32 }
[[popup.profile]]
name = "laptop"
max_client_width = 200
size.menu = { width = "50%", height = "60%" }
[menu]
max_height = 80
"#,
        );
        assert_eq!(popup_size(Some(&config), || Some(300)), cells(80, 32));
        assert_eq!(
            popup_size(Some(&config), || Some(174)),
            PopupDims {
                width: PopupSize::Percent(50),
                height: PopupSize::Percent(60),
            }
        );
        // An unknown client width skips profiles.
        assert_eq!(popup_size(Some(&config), || None), cells(80, 32));
    }

    #[test]
    fn client_width_is_read_only_when_a_profile_could_use_it() {
        let config = core("[popup.size]\nmenu = { width = 80, height = 32 }\n");
        popup_size(Some(&config), || {
            panic!("no profile needs the client width")
        });
    }
}
