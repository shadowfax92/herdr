//! Fork native agent conversations into Herdr panes and track their layout.
//!
//! Herdr owns pane/process lifecycle while RTR owns conversation discovery and
//! native profile isolation. This module is the protocol seam between them.

use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Error, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::ForkPlacement;
use crate::fork_layout::ForkLayout;
use crate::herdr::{Herdr, PaneInfo};

const ROTATION_DIRECTIONS: [&str; 4] = ["right", "down", "left", "up"];

/// Coordinates one popup invocation with its exact originating Herdr pane.
#[derive(Clone, Debug)]
pub struct AgentPaneController {
    herdr: Herdr,
    source_pane_id: String,
    source_cwd: PathBuf,
    state_path: PathBuf,
    rtr_binary: OsString,
}

/// Single-record contract shared by separate menu popup invocations.
///
/// The source and fork IDs identify the only pane pair eligible for rotation;
/// replacing this record transfers that ownership to the most recent fork.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct ForkState {
    schema_version: u8,
    source_pane_id: String,
    fork_pane_id: String,
    next_direction: usize,
}

impl AgentPaneController {
    pub fn new(
        herdr: Herdr,
        source_pane_id: impl Into<String>,
        source_cwd: impl Into<PathBuf>,
        state_path: impl Into<PathBuf>,
        rtr_binary: impl Into<OsString>,
    ) -> Self {
        Self {
            herdr,
            source_pane_id: source_pane_id.into(),
            source_cwd: source_cwd.into(),
            state_path: state_path.into(),
            rtr_binary: rtr_binary.into(),
        }
    }

    /// Place and balance an empty shell before starting the source's RTR fork.
    pub fn fork(&self, placement: ForkPlacement) -> Result<()> {
        // Popup processes share one rotation record. Hold its advisory lock
        // across snapshot, pane creation, launch, and rollback so an older
        // failure cannot restore stale bytes over a newer successful fork.
        let _state_lock = self.lock_state()?;
        // Snapshot before creating a pane so a failed launch can restore the
        // previous rotation target instead of silently forgetting it.
        let previous_state = self
            .snapshot_state()
            .context("preserving the previous fork state")?;
        let source = self
            .herdr
            .pane(&self.source_pane_id)
            .context("reading the source pane")?;
        let cwd = pane_cwd(&source).unwrap_or_else(|| self.source_cwd.clone());
        let state_dir = self
            .state_path
            .parent()
            .context("fork state path has no parent")?;
        let mut layout = ForkLayout::capture(&self.herdr, &self.source_pane_id, placement)?;
        layout.prepare(&self.herdr, &cwd, state_dir)?;
        let fork_pane_id = self
            .herdr
            .split_pane(&layout.target, layout.direction, &cwd)
            .context("creating the fork pane")?;

        // Herdr only creates right/down splits. Opposite-side placements first
        // split in the supported direction, then swap the new pane across its
        // source. Swapping preserves both pane IDs and their running processes.
        if let Some((swap_direction, placement_name)) = layout.swap {
            match self.herdr.swap_pane(&fork_pane_id, swap_direction) {
                Ok(true) => {}
                Ok(false) => {
                    let error = anyhow!(
                        "Herdr created the pane but could not place it on the {placement_name}"
                    );
                    return Err(self.close_created_pane(&fork_pane_id, error));
                }
                Err(error) => {
                    let error =
                        error.context(format!("placing the fork pane on the {placement_name}"));
                    return Err(self.close_created_pane(&fork_pane_id, error));
                }
            }
        }

        if let Err(error) = layout.finish(&self.herdr, &fork_pane_id, &cwd, state_dir) {
            let error = error.context("arranging the fork row");
            return Err(
                match layout
                    .cleanup(&self.herdr, &fork_pane_id, &cwd, state_dir)
                    .result
                {
                    Ok(()) => error,
                    Err(cleanup) => append_cleanup_error(
                        error,
                        "restoring the layout after the failed fork",
                        cleanup,
                    ),
                },
            );
        }

        let command = self.fork_command(&source);
        let state = ForkState {
            schema_version: 1,
            source_pane_id: self.source_pane_id.clone(),
            fork_pane_id: fork_pane_id.clone(),
            next_direction: 0,
        };

        // `pane run` only acknowledges an asynchronous input handoff. Persist
        // first so a state-write failure can still close an empty pane rather
        // than leave a live native fork that the rotate action cannot find.
        if let Err(error) = self.write_state(&state) {
            let error = error.context("remembering the fork pane for rotation");
            return Err(
                match layout
                    .cleanup(&self.herdr, &fork_pane_id, &cwd, state_dir)
                    .result
                {
                    Ok(()) => error,
                    Err(cleanup) => append_cleanup_error(
                        error,
                        "restoring the layout after the failed fork",
                        cleanup,
                    ),
                },
            );
        }
        if let Err(error) = self.herdr.run_in_pane(&fork_pane_id, &command) {
            let mut error = error.context("starting RTR in the fork pane");
            let cleanup = layout.cleanup(&self.herdr, &fork_pane_id, &cwd, state_dir);
            if cleanup.pane_closed {
                if let Err(restore_error) = self.restore_state(&previous_state) {
                    error = append_cleanup_error(
                        error,
                        "restoring the previous fork state",
                        restore_error,
                    );
                }
            }
            if let Err(cleanup_error) = cleanup.result {
                // Keep the new record only while its pane still exists. A
                // geometry-recovery error can happen after successful removal.
                error = append_cleanup_error(
                    error,
                    &format!("cleaning up newly-created pane {fork_pane_id}"),
                    cleanup_error,
                );
            }
            return Err(error);
        }
        Ok(())
    }

    /// Swap the last fork with the next available directional neighbor.
    pub fn rotate(&self) -> Result<()> {
        let _state_lock = self.lock_state()?;
        let mut state = self.read_state()?;
        if state.source_pane_id != self.source_pane_id && state.fork_pane_id != self.source_pane_id
        {
            bail!("the last fork belongs to a different pane pair");
        }
        self.herdr
            .pane(&state.fork_pane_id)
            .context("the last fork pane no longer exists")?;

        for offset in 0..ROTATION_DIRECTIONS.len() {
            let index = (state.next_direction + offset) % ROTATION_DIRECTIONS.len();
            if self
                .herdr
                .swap_pane(&state.fork_pane_id, ROTATION_DIRECTIONS[index])?
            {
                state.next_direction = (index + 1) % ROTATION_DIRECTIONS.len();
                self.write_state(&state)?;
                return Ok(());
            }
        }
        bail!("the fork pane has no neighboring pane to rotate with")
    }

    fn fork_command(&self, pane: &PaneInfo) -> String {
        let session = pane.agent_session.as_ref().filter(|session| {
            session.kind == "id"
                && !session.value.trim().is_empty()
                && !session.value.chars().any(char::is_control)
                // A nested agent can leave hook metadata for a different tool.
                // The foreground detection owns which tool the pane can fork.
                && pane.agent.as_deref().is_none_or(|agent| agent == session.agent)
                && matches!(
                    (session.source.as_str(), session.agent.as_str()),
                    ("herdr:claude", "claude") | ("herdr:codex", "codex")
                )
        });
        let agent = session
            .map(|session| session.agent.as_str())
            .or(pane.agent.as_deref())
            .filter(|agent| matches!(*agent, "claude" | "codex"));

        let mut command = shell_quote(self.rtr_binary.as_os_str());
        command.push_str(" fork");
        if let Some(session) = session {
            command.push(' ');
            command.push_str(&shell_quote(OsStr::new(&session.value)));
        } else {
            // Missing hook metadata must never be replaced with a guessed
            // "latest" session. RTR's cwd-filtered picker is the safe fallback.
            command.push_str(" --here");
        }
        if let Some(agent) = agent {
            command.push_str(" --tool ");
            command.push_str(agent);
        }
        command
    }

    fn read_state(&self) -> Result<ForkState> {
        let bytes = fs::read(&self.state_path).with_context(|| {
            format!(
                "no fork pane has been recorded yet ({})",
                self.state_path.display()
            )
        })?;
        let state: ForkState = serde_json::from_slice(&bytes)
            .with_context(|| format!("reading {}", self.state_path.display()))?;
        if state.schema_version != 1 || state.next_direction >= ROTATION_DIRECTIONS.len() {
            bail!("unsupported fork state in {}", self.state_path.display());
        }
        Ok(state)
    }

    fn lock_state(&self) -> Result<fs::File> {
        let parent = self
            .state_path
            .parent()
            .context("fork state path has no parent directory")?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let lock_path = self.state_path.with_extension("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        fs4::FileExt::lock(&file).with_context(|| format!("locking {}", lock_path.display()))?;
        // The OS releases the advisory lock when this file is dropped, including
        // abrupt popup exit, so no stale lock cleanup protocol is needed.
        Ok(file)
    }

    fn snapshot_state(&self) -> Result<Option<Vec<u8>>> {
        match fs::read(&self.state_path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("reading {}", self.state_path.display()))
            }
        }
    }

    fn restore_state(&self, snapshot: &Option<Vec<u8>>) -> Result<()> {
        match snapshot {
            Some(bytes) => self.write_state_bytes(bytes),
            None => match fs::remove_file(&self.state_path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => {
                    Err(error).with_context(|| format!("removing {}", self.state_path.display()))
                }
            },
        }
    }

    fn write_state(&self, state: &ForkState) -> Result<()> {
        let mut bytes = serde_json::to_vec(state)?;
        bytes.push(b'\n');
        self.write_state_bytes(&bytes)
    }

    fn write_state_bytes(&self, bytes: &[u8]) -> Result<()> {
        let parent = self
            .state_path
            .parent()
            .context("fork state path has no parent directory")?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let temporary = parent.join(format!(".last-fork-{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.state_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.with_context(|| format!("writing {}", self.state_path.display()))
    }

    fn close_created_pane(&self, pane_id: &str, error: Error) -> Error {
        match self.herdr.close_pane(pane_id) {
            Ok(()) => error,
            Err(close_error) => append_cleanup_error(
                error,
                &format!("closing newly-created pane {pane_id}"),
                close_error,
            ),
        }
    }
}

fn append_cleanup_error(error: Error, action: &str, cleanup_error: Error) -> Error {
    anyhow!("{error:#}; cleanup failed while {action}: {cleanup_error:#}")
}

fn pane_cwd(pane: &PaneInfo) -> Option<PathBuf> {
    pane.foreground_cwd
        .as_deref()
        .or(pane.cwd.as_deref())
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

fn shell_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_@%+=:,./-".contains(character))
    {
        return value.into_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::herdr::AgentSessionInfo;

    #[test]
    fn shell_quote_protects_native_session_ids_and_binary_paths() {
        assert_eq!(shell_quote(OsStr::new("rtr")), "rtr");
        assert_eq!(
            shell_quote(OsStr::new("/Applications/RTR tools/rtr")),
            "'/Applications/RTR tools/rtr'"
        );
        assert_eq!(shell_quote(OsStr::new("a'b")), "'a'\\''b'");
    }

    #[test]
    fn fork_command_uses_hook_identity_and_never_guesses_when_it_is_missing() {
        let controller = AgentPaneController::new(
            Herdr::new("herdr"),
            "w1:p1",
            "/work",
            "/state/last-fork.json",
            "rtr",
        );
        let mut pane = PaneInfo {
            pane_id: "w1:p1".into(),
            cwd: Some("/work".into()),
            foreground_cwd: None,
            agent: Some("codex".into()),
            agent_session: Some(AgentSessionInfo {
                source: "herdr:codex".into(),
                agent: "codex".into(),
                kind: "id".into(),
                value: "native-id".into(),
            }),
        };

        assert_eq!(
            controller.fork_command(&pane),
            "rtr fork native-id --tool codex"
        );
        pane.agent_session.as_mut().unwrap().value.clear();
        assert_eq!(
            controller.fork_command(&pane),
            "rtr fork --here --tool codex"
        );
        pane.agent_session.as_mut().unwrap().value = "native-id".into();
        pane.agent_session.as_mut().unwrap().source = "third-party".into();
        assert_eq!(
            controller.fork_command(&pane),
            "rtr fork --here --tool codex"
        );
        pane.agent_session = None;
        assert_eq!(
            controller.fork_command(&pane),
            "rtr fork --here --tool codex"
        );
        pane.agent = Some("shell".into());
        assert_eq!(controller.fork_command(&pane), "rtr fork --here");
    }

    #[test]
    fn fork_command_uses_detected_agent_when_session_belongs_to_another_tool() {
        let controller = AgentPaneController::new(
            Herdr::new("herdr"),
            "w1:p1",
            "/work",
            "/state/last-fork.json",
            "rtr",
        );
        for (detected, reported) in [("codex", "claude"), ("claude", "codex")] {
            let pane = PaneInfo {
                pane_id: "w1:p1".into(),
                cwd: Some("/work".into()),
                foreground_cwd: None,
                agent: Some(detected.into()),
                agent_session: Some(AgentSessionInfo {
                    source: format!("herdr:{reported}"),
                    agent: reported.into(),
                    kind: "id".into(),
                    value: "nested-session".into(),
                }),
            };
            assert_eq!(
                controller.fork_command(&pane),
                format!("rtr fork --here --tool {detected}")
            );
        }
    }

    #[test]
    fn state_lock_serializes_overlapping_popup_transactions() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("last-fork.json");
        let controller = AgentPaneController::new(
            Herdr::new("herdr"),
            "w1:p1",
            directory.path(),
            &state_path,
            "rtr",
        );
        let first_lock = controller.lock_state().unwrap();
        let second = AgentPaneController::new(
            Herdr::new("herdr"),
            "w1:p2",
            directory.path(),
            &state_path,
            "rtr",
        );
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let result = second.lock_state().map(|_lock| ());
            acquired_tx
                .send(result.map_err(|error| format!("{error:#}")))
                .unwrap();
        });

        assert!(matches!(
            acquired_rx.recv_timeout(Duration::from_millis(75)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(first_lock);
        assert_eq!(
            acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(())
        );
        waiter.join().unwrap();
    }
}
