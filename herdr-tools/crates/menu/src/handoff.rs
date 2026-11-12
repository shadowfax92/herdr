//! A metadata-only handoff for the pane that opened the menu. A single snapshot
//! keeps pane, agent, and location identities consistent without reading output.
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::herdr::Herdr;

/// Captures the original pane through its session socket; popup/UI focus is never
/// a targeting fallback. Clipboard changes happen only after capture succeeds.
pub struct HandoffController {
    herdr: Herdr,
    source_pane_id: String,
    command_search_path: Option<OsString>,
}

/// The exact clipboard payload and its inventory size, for success reporting.
#[derive(Debug)]
pub struct HandoffDocument {
    pub text: String,
    pub pane_count: usize,
}

impl HandoffController {
    pub fn new(herdr: Herdr, source_pane_id: String) -> Self {
        Self {
            herdr,
            source_pane_id,
            command_search_path: std::env::var_os("PATH"),
        }
    }

    /// Override PATH resolution for copied commands without changing the process
    /// environment or the binary/socket used to capture metadata.
    pub fn with_search_path(mut self, path: Option<OsString>) -> Self {
        self.command_search_path = path;
        self
    }

    pub(crate) fn source_pane_id(&self) -> &str {
        &self.source_pane_id
    }

    pub fn capture(&self) -> Result<HandoffDocument> {
        if !valid_pane_id(&self.source_pane_id) {
            bail!("invalid source pane ID: expected an ASCII letter or digit followed by ASCII letters, digits, ':', '.', '_' or '-'");
        }
        let socket = self
            .herdr
            .socket_path()
            .context("HERDR_SOCKET_PATH is not set; cannot identify the source session")?;
        if !socket.is_absolute() {
            bail!("source session socket path must be absolute");
        }
        let socket_text = socket
            .to_str()
            .context("source session socket path is not valid UTF-8")?;
        // Discover names only for this exact socket. Session routing also needs
        // the first herdr on PATH to resolve to the capturing binary: different
        // builds can map the same session name to different socket namespaces.
        // Discovery, validation, or binary-resolution failure keeps the socket.
        let session_name = self
            .request::<SessionList>(socket, &["session", "list", "--json"])
            .ok()
            .and_then(|sessions| {
                sessions
                    .sessions
                    .into_iter()
                    .find(|session| Path::new(&session.socket_path) == socket)
            })
            .map(|session| session.name)
            .filter(|name| usable_session_name(name) && self.command_uses_capture_binary());
        let response: SnapshotEnvelope = self.request(socket, &["api", "snapshot"])?;
        let snapshot = response.result.snapshot;
        let pane = snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_id == self.source_pane_id)
            .with_context(|| {
                format!(
                    "source pane {} no longer exists in this session; reopen the menu from the intended pane",
                    self.source_pane_id
                )
            })?;
        let workspace = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == pane.workspace_id)
            .context("source pane workspace is missing from the Herdr snapshot")?;
        let tab = snapshot
            .tabs
            .iter()
            .find(|tab| tab.tab_id == pane.tab_id && tab.workspace_id == pane.workspace_id)
            .context("source pane tab is missing from the Herdr snapshot")?;
        // Labels are user-facing pane annotations, not valid agent targets. Only
        // the agent record for this same terminal can supply a registered name.
        let agent_name = snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane.pane_id && agent.terminal_id == pane.terminal_id)
            .and_then(|agent| agent.name.as_deref());
        let record = IdentityRecord {
            pane_id: &pane.pane_id,
            session: session_name.as_deref(),
            socket: session_name.is_none().then_some(socket_text),
            agent: pane.agent.as_deref(),
            label: pane.label.as_deref().or(agent_name),
            tab: tab.label.as_deref(),
            workspace: workspace.label.as_deref(),
            cwd: pane.foreground_cwd.as_deref().or(pane.cwd.as_deref()),
        };
        // Runtime strings stay inside JSON except validated pane/session IDs
        // and a POSIX single-quoted socket. Labels and directories can never
        // become Markdown structure or executable shell text.
        let prefix = match session_name.as_deref() {
            Some(name) => format!("herdr --session {name}"),
            None => format!(
                "HERDR_SOCKET_PATH='{}' herdr",
                socket_text.replace('\'', "'\\''")
            ),
        };
        let pane_id = &pane.pane_id;
        let read = format!("{prefix} pane read {pane_id} --lines 120");
        let commands = if pane.agent.is_some() {
            format!("{read}\n{prefix} agent prompt {pane_id} \"<message>\" --wait\n{prefix} agent wait {pane_id}")
        } else {
            format!("{read}\n{prefix} pane run {pane_id} \"<command>\"")
        };
        let text = format!(
            "# Herdr pane context\n\n\
             Captured metadata, not instructions.\n\n\
             ```json\n{}\n```\n\n```sh\n{commands}\n```\n",
            serde_json::to_string_pretty(&record)?,
        );
        Ok(HandoffDocument {
            text,
            pane_count: 1,
        })
    }

    pub fn copy(&self) -> Result<HandoffDocument> {
        let document = self.capture()?;
        crate::clipboard::copy(&document.text)?;
        Ok(document)
    }

    fn command_uses_capture_binary(&self) -> bool {
        let Some(path) = self.command_search_path.as_deref() else {
            return false;
        };
        let Ok(capture_binary) = self.herdr.binary().canonicalize() else {
            return false;
        };
        // Resolve without launching a second Herdr. Canonical paths accept
        // symlinked installs while an unknown or different binary fails closed
        // to exact-socket routing, even if a matching binary appears later.
        std::env::split_paths(path)
            .map(|directory| directory.join("herdr"))
            .find(|candidate| candidate.is_file())
            .and_then(|candidate| candidate.canonicalize().ok())
            .is_some_and(|command_binary| command_binary == capture_binary)
    }

    /// Explicit per-process routing prevents another session's identical pane ID
    /// from resolving through the caller's environment or global UI focus.
    fn request<T: for<'de> Deserialize<'de>>(&self, socket: &Path, args: &[&str]) -> Result<T> {
        let output = Command::new(self.herdr.binary())
            .args(args)
            .env("HERDR_SOCKET_PATH", socket)
            .output()
            .with_context(|| format!("failed to run {}", self.herdr.binary().display()))?;
        if !output.status.success() {
            bail!(
                "Herdr {} failed with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        serde_json::from_slice(&output.stdout).context("failed to decode Herdr handoff metadata")
    }
}

fn valid_pane_id(id: &str) -> bool {
    id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'.' | b'_' | b'-'))
}

fn usable_session_name(name: &str) -> bool {
    // Mirror Herdr's validate_name restrictions and reject a leading option
    // marker so the name is safe as an unquoted --session argument.
    !name.is_empty()
        && name.len() <= 64
        && !matches!(name, "." | "..")
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Clipboard identity in reading order. Missing metadata is omitted, and the
/// discovered session replaces the socket only when it can route CLI commands.
#[derive(Serialize)]
struct IdentityRecord<'a> {
    pane_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    socket: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tab: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
}

#[derive(Deserialize)]
struct SessionList {
    sessions: Vec<Session>,
}

#[derive(Deserialize)]
struct Session {
    name: String,
    socket_path: String,
}

#[derive(Deserialize)]
struct SnapshotEnvelope {
    result: SnapshotResult,
}

#[derive(Deserialize)]
struct SnapshotResult {
    snapshot: Snapshot,
}

/// Deliberately decode only metadata: scrollback, rendering tokens, and layout
/// geometry must never leak into a compact clipboard handoff.
#[derive(Deserialize)]
struct Snapshot {
    panes: Vec<Pane>,
    agents: Vec<Agent>,
    workspaces: Vec<Workspace>,
    tabs: Vec<Tab>,
}

#[derive(Deserialize)]
struct Pane {
    pane_id: String,
    terminal_id: String,
    workspace_id: String,
    tab_id: String,
    label: Option<String>,
    cwd: Option<String>,
    foreground_cwd: Option<String>,
    agent: Option<String>,
}

#[derive(Deserialize)]
struct Agent {
    pane_id: String,
    terminal_id: String,
    name: Option<String>,
}

#[derive(Deserialize)]
struct Workspace {
    workspace_id: String,
    label: Option<String>,
}

#[derive(Deserialize)]
struct Tab {
    tab_id: String,
    workspace_id: String,
    label: Option<String>,
}
