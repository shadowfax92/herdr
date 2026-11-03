use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ratatui::crossterm::event;
use serde::Deserialize;

use crate::agent_panes::AgentPaneController;
use crate::agents_policy::{Client as PolicyClient, Operation, PendingRequest};
use crate::app::{InputOutcome, MenuState};
use crate::config::{Config, ExecutionMode, MenuItem};
use crate::execute::ShellExecutor;
use crate::handoff::HandoffController;
use crate::redistribute::{ApplyOutcome, Redistributor};
use crate::{ui, PLUGIN_ID};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct InvocationContext {
    pub focused_pane_id: Option<String>,
    pub focused_pane_cwd: Option<String>,
}

impl InvocationContext {
    pub fn parse(source: &str) -> Result<Self> {
        serde_json::from_str(source).context("invalid HERDR_PLUGIN_CONTEXT_JSON")
    }
}

/// Narrow adapter around the Herdr CLI protocol used by plugin actions.
#[derive(Clone, Debug)]
pub struct Herdr {
    binary: PathBuf,
    socket: Option<PathBuf>,
}

impl Herdr {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            socket: std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from),
        }
    }

    pub fn with_socket(mut self, socket: impl Into<PathBuf>) -> Self {
        self.socket = Some(socket.into());
        self
    }

    pub(crate) fn layout_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        crate::socket::request(
            self.socket
                .as_deref()
                .context("HERDR_SOCKET_PATH is not set")?,
            method,
            params,
        )
    }

    pub(crate) fn socket_path(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    pub fn launch(&self, context: &InvocationContext, config_path: &Path) -> Result<()> {
        let (source_pane_id, source_cwd) = self.source_context(context)?;
        let source_cwd_text = source_cwd
            .to_str()
            .context("source pane working directory is not valid UTF-8")?;
        let (width, height) = Config::load(config_path)
            .map(|config| (config.width, config.popup_height()))
            .unwrap_or((64, 16));
        let width = width.to_string();
        let height = height.to_string();
        let cwd_environment = format!("HERDR_MENU_SOURCE_CWD={source_cwd_text}");
        let pane_environment = format!("HERDR_MENU_SOURCE_PANE_ID={source_pane_id}");
        let mut arguments = vec![
            OsString::from("plugin"),
            OsString::from("pane"),
            OsString::from("open"),
            OsString::from("--plugin"),
            OsString::from(PLUGIN_ID),
            OsString::from("--entrypoint"),
            OsString::from("menu"),
            OsString::from("--placement"),
            OsString::from("popup"),
            OsString::from("--width"),
            OsString::from(width),
            OsString::from("--height"),
            OsString::from(height),
            OsString::from("--env"),
            OsString::from(cwd_environment),
            OsString::from("--env"),
            OsString::from(pane_environment),
        ];
        if let Some(rtr_binary) = std::env::var_os("RTR_BIN_PATH") {
            let rtr_binary = rtr_binary
                .to_str()
                .context("RTR_BIN_PATH is not valid UTF-8")?;
            arguments.push(OsString::from("--env"));
            arguments.push(OsString::from(format!("RTR_BIN_PATH={rtr_binary}")));
        }
        self.output(arguments)?;
        Ok(())
    }

    pub fn launch_with_reporting(
        &self,
        context: &InvocationContext,
        config_path: &Path,
    ) -> Result<()> {
        let result = self.launch(context, config_path);
        if let Err(error) = &result {
            let _ = self.show_notification(&format!("Could not open menu: {error:#}"));
        }
        result
    }

    fn source_context(&self, context: &InvocationContext) -> Result<(String, PathBuf)> {
        let pane_id = context
            .focused_pane_id
            .as_deref()
            .map(str::trim)
            .filter(|pane_id| !pane_id.is_empty() && !pane_id.chars().any(char::is_control))
            .context("plugin context has no valid source pane ID")?
            .to_string();
        if let Some(cwd) = context.focused_pane_cwd.as_deref() {
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
        let body = body.chars().take(220).collect::<String>();
        self.output([
            OsStr::new("notification"),
            OsStr::new("show"),
            OsStr::new("Herdr Menu"),
            OsStr::new("--body"),
            OsStr::new(&body),
        ])?;
        Ok(())
    }

    pub fn reload_config(&self) -> Result<()> {
        self.output([OsStr::new("server"), OsStr::new("reload-config")])?;
        Ok(())
    }

    /// Feature controllers own response shapes; this shared process boundary
    /// keeps command failure and JSON decoding behavior consistent across them.
    pub(crate) fn json<T, I, S>(&self, args: I) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        serde_json::from_slice(&self.output(args)?).context("failed to decode Herdr response")
    }

    pub(crate) fn output<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.command(args)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "Herdr command failed with {}: {}",
                output.status,
                stderr.trim()
            );
        }
        Ok(output.stdout)
    }

    fn command<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = args
            .into_iter()
            .map(|value| value.as_ref().to_os_string())
            .collect::<Vec<OsString>>();
        Command::new(&self.binary)
            .args(args)
            .output()
            .with_context(|| format!("failed to run {}", self.binary.display()))
    }
}

pub fn launch_from_environment() -> Result<()> {
    let binary = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .context("HERDR_PLUGIN_CONTEXT_JSON is not set")?;
    let config_dir = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .context("HERDR_PLUGIN_CONFIG_DIR is not set")?;
    Herdr::new(binary).launch_with_reporting(
        &InvocationContext::parse(&context)?,
        &config_dir.join("config.toml"),
    )
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
    let binary = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let action = CopyContextAction::new(Herdr::new(binary), source_pane);
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
    let config_dir = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .context("HERDR_PLUGIN_CONFIG_DIR is not set")?;
    let source_cwd = std::env::var_os("HERDR_MENU_SOURCE_CWD")
        .map(PathBuf::from)
        .context("HERDR_MENU_SOURCE_CWD is not set")?;
    let source_pane_id = std::env::var("HERDR_MENU_SOURCE_PANE_ID")
        .context("HERDR_MENU_SOURCE_PANE_ID is not set")?;
    let state_dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .context("HERDR_PLUGIN_STATE_DIR is not set")?;
    let herdr_binary = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let rtr_binary = std::env::var_os("RTR_BIN_PATH").unwrap_or_else(|| "rtr".into());
    let redistributor = Redistributor::new(
        Herdr::new(herdr_binary.clone()),
        source_pane_id.clone(),
        source_cwd.clone(),
        state_dir.clone(),
    );
    let handoff = CopyContextAction::new(Herdr::new(herdr_binary.clone()), source_pane_id.clone());
    let controller = AgentPaneController::new(
        Herdr::new(herdr_binary),
        source_pane_id,
        source_cwd.clone(),
        state_dir.join("last-fork.json"),
        rtr_binary,
    );
    run_menu(
        &config_dir.join("config.toml"),
        &source_cwd,
        &controller,
        &redistributor,
        &handoff,
    )
}

fn run_menu(
    config_path: &Path,
    source_cwd: &Path,
    controller: &AgentPaneController,
    redistributor: &Redistributor,
    handoff: &CopyContextAction,
) -> Result<()> {
    let mut state = match Config::load(config_path) {
        Ok(config) => {
            for warning in &config.warnings {
                eprintln!("Herdr Menu config: {warning}");
            }
            MenuState::new(config)
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
) -> Result<()> {
    let policy_client = PolicyClient::from_environment();
    let mut pending_policy = None;
    if state.has_visibility_action() {
        match PendingRequest::spawn(policy_client.clone(), Operation::Read) {
            Ok(task) => pending_policy = Some(task),
            Err(error) => state.complete_visibility(Err(error)),
        }
    }
    loop {
        terminal.draw(|frame| ui::render(state, frame))?;
        let waiting_for_policy = pending_policy.is_some();
        let outcome = next_menu_input(
            state,
            |timeout| {
                if timeout != Duration::ZERO && !waiting_for_policy {
                    return event::read().map(Some);
                }
                if event::poll(timeout)? {
                    event::read().map(Some)
                } else {
                    Ok(None)
                }
            },
            || {
                let result = pending_policy
                    .as_ref()
                    .and_then(PendingRequest::try_complete);
                if result.is_some() {
                    pending_policy = None;
                }
                result
            },
        )?;
        match outcome {
            InputOutcome::Continue => {}
            InputOutcome::Cancel => return Ok(()),
            InputOutcome::SetVisibility(show) => {
                match PendingRequest::spawn(policy_client.clone(), Operation::Set(show)) {
                    Ok(task) => pending_policy = Some(task),
                    Err(error) => state.complete_visibility(Err(error)),
                }
            }
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
            // Visibility reaches execution only as SetVisibility with captured
            // intent; the static config item never triggers an implicit flip.
            InputOutcome::Execute(MenuItem::WorkspaceVisibility(_) | MenuItem::Separator) => {}
        }
    }
}

/// Prefer queued input over publishing an asynchronous reply. Inputs typed
/// while visibility was disabled must see that same state, even if a reply is
/// already waiting. Real terminal input and deterministic tests use this seam.
fn next_menu_input(
    state: &mut MenuState,
    mut input: impl FnMut(Duration) -> std::io::Result<Option<event::Event>>,
    mut completion: impl FnMut() -> Option<
        std::result::Result<crate::agents_policy::Snapshot, crate::agents_policy::PolicyError>,
    >,
) -> std::io::Result<InputOutcome> {
    if let Some(event) = input(Duration::ZERO)? {
        return Ok(state.handle_event(event));
    }
    if let Some(reply) = completion() {
        state.complete_visibility(reply);
        return Ok(InputOutcome::Continue);
    }
    Ok(input(Duration::from_millis(50))?
        .map_or(InputOutcome::Continue, |event| state.handle_event(event)))
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
mod visibility_input_tests {
    use super::*;
    use crate::agents_policy::Snapshot;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::collections::VecDeque;
    use std::time::Duration;

    #[test]
    fn tests_that_queued_input_is_consumed_before_visibility_completion() {
        let mut state = MenuState::new(Config::parse("[[items]]\ntype='workspace_visibility'\nkey='v'\nlabel='workspaces'\n[[items]]\nkey='x'\nlabel='custom'\ncommand='custom'\n").unwrap());
        let reply = Snapshot {
            version: 1,
            herdr_socket: "/tmp/host.sock".into(),
            excluded_labels: vec![],
            workspace_ids: vec![],
            excluded_workspace_ids: vec![],
            show_excluded: true,
        };
        for loading in [true, false] {
            if !loading {
                state.complete_visibility(Ok(Snapshot {
                    show_excluded: false,
                    ..reply.clone()
                }));
                state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
                assert_eq!(
                    state.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
                    InputOutcome::SetVisibility(true)
                );
            }
            let mut queue = VecDeque::from([
                Event::Resize(80, 20),
                Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            ]);
            let mut completion = Some(Ok(reply.clone()));
            for expected in [InputOutcome::Continue, InputOutcome::Continue] {
                let outcome =
                    next_menu_input(&mut state, |_| Ok(queue.pop_front()), || completion.take())
                        .unwrap();
                assert_eq!(outcome, expected);
                assert!(
                    completion.is_some(),
                    "reply was published ahead of queued input"
                );
            }
            assert!(matches!(
                next_menu_input(&mut state, |_| Ok(queue.pop_front()), || completion.take())
                    .unwrap(),
                InputOutcome::Execute(MenuItem::Shell(_))
            ));
            assert!(completion.is_some());
            assert_eq!(
                next_menu_input(
                    &mut state,
                    |timeout| {
                        assert_eq!(timeout, Duration::ZERO);
                        Ok(None)
                    },
                    || completion.take()
                )
                .unwrap(),
                InputOutcome::Continue
            );
            assert!(completion.is_none());
            assert_eq!(
                state.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
                InputOutcome::Continue
            );
        }
    }
}
