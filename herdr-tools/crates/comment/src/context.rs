//! What a comment action needs to know about where it was invoked.
//!
//! The source pane, its cwd and any Copy-mode selection come from core's
//! [`Invocation`] (`HERDR_PLUGIN_CONTEXT_JSON`), and the state dir from core's
//! [`Paths`]. The session identity is the raw `HERDR_SOCKET_PATH` string. It is
//! hashed with the pane id into every comment scope (`store::scope_id`), so it
//! must stay exactly the string Herdr passes: changing it would orphan every
//! collected comment and ready draft.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tools_core::{Invocation, Paths};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionContext {
    pub pane_id: String,
    pub pane_cwd: Option<PathBuf>,
    pub selected_text: Option<String>,
    pub state_dir: PathBuf,
    pub herdr_bin: PathBuf,
    pub session_identity: String,
}

impl ActionContext {
    /// The context of the running action. Herdr sets every input for plugin
    /// actions; a missing one means the command was not started by Herdr.
    pub fn from_env() -> Result<Self> {
        let invocation = Invocation::current()?;
        if invocation.context.is_none() {
            bail!("HERDR_PLUGIN_CONTEXT_JSON is missing");
        }
        Self::from_invocation(
            invocation,
            Paths::from_env().state_dir(),
            &std::env::var("HERDR_BIN_PATH").context("HERDR_BIN_PATH is missing")?,
            &std::env::var("HERDR_SOCKET_PATH").context("HERDR_SOCKET_PATH is missing")?,
        )
    }

    pub fn from_values(
        raw_context: &str,
        state_dir: &str,
        herdr_bin: &str,
        session_identity: &str,
    ) -> Result<Self> {
        Self::from_invocation(
            &Invocation::from_context_json(raw_context)?,
            Path::new(state_dir),
            herdr_bin,
            session_identity,
        )
    }

    fn from_invocation(
        invocation: &Invocation,
        state_dir: &Path,
        herdr_bin: &str,
        session_identity: &str,
    ) -> Result<Self> {
        let pane_id = invocation
            .source_pane()
            .context("Herdr did not provide a focused pane")?
            .to_owned();
        if state_dir.as_os_str().to_string_lossy().trim().is_empty() {
            bail!("Herdr did not provide a plugin state directory");
        }
        let herdr_bin = required(herdr_bin, "Herdr binary")?;
        let session_identity = required(session_identity, "Herdr session identity")?;

        Ok(Self {
            pane_id,
            pane_cwd: invocation.source_cwd().map(Path::to_path_buf),
            selected_text: invocation.selected_text().map(str::to_owned),
            state_dir: state_dir.to_path_buf(),
            herdr_bin: PathBuf::from(herdr_bin),
            session_identity,
        })
    }
}

fn required(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("Herdr did not provide a {label}");
    }
    Ok(value.to_owned())
}
