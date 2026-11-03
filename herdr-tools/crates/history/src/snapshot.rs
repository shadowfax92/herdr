//! `history open`: freeze the source pane's recent output and hand it to the
//! picker popup.
//!
//! ```text
//!  history open (action process)              history picker (popup process)
//!    read pane history, match targets
//!    RunStore::write ── <state>/history/runs/<uuid>.json (0600, dir 0700)
//!    open popup `history` ── env HERDR_HISTORY_RUN_ID=<uuid> ──▶ RunStore::claim
//!    (a failed open removes the handoff)                         (rename, read, delete)
//! ```
//!
//! The handoff is one-shot and private: the picker claims it by rename, so a
//! second picker for the same id fails, and stale files older than an hour are
//! reaped on the next write. Handoffs are transient, so they live in the new
//! plugin state dir, not talon's.

use std::fs::{self, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tools_core::{Host, Invocation, Paths};
use uuid::Uuid;

use crate::config::Config;
use crate::herdr::{read_recent_unwrapped, read_visible};
use crate::matcher::{find_targets, Target};
use crate::{MODE, NOTIFICATION_TITLE, POPUP_KEY};

/// How `history open` tells the picker popup which handoff to claim (talon's
/// `HERDR_TALON_RUN_ID`). Popups cannot see their caller, so it travels in the
/// popup's env.
pub const RUN_ID_ENV: &str = "HERDR_HISTORY_RUN_ID";

pub const CAPTURE_ROWS: u32 = 1_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RunSnapshot {
    pub source_pane_id: String,
    pub text: String,
    pub ansi: String,
    pub targets: Vec<Target>,
    pub alphabet: Vec<char>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchOutcome {
    Opened { run_id: String },
}

#[derive(Clone, Debug)]
pub struct RunStore {
    root: PathBuf,
}

impl RunStore {
    pub fn new(state_dir: impl AsRef<Path>) -> Result<Self> {
        let state_dir = state_dir.as_ref();
        fs::create_dir_all(state_dir)
            .with_context(|| format!("failed to create {}", state_dir.display()))?;
        let root = state_dir.join("runs");
        match fs::symlink_metadata(&root) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!("run directory is not a private directory");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&root)
                    .with_context(|| format!("failed to create {}", root.display()))?;
            }
            Err(error) => return Err(error).context("failed to inspect run directory"),
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write(&self, snapshot: &RunSnapshot) -> Result<String> {
        self.reap_stale(Duration::from_secs(3600))?;
        let run_id = Uuid::new_v4().to_string();
        let final_path = self.path(&run_id)?;
        let temporary_path = self.root.join(format!(".{run_id}.tmp"));
        let result: Result<String> = (|| {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary_path)?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer(&mut writer, snapshot)?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            if final_path.exists() {
                bail!("run ID collision");
            }
            fs::rename(&temporary_path, &final_path)?;
            Ok(run_id)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result.context("failed to write history handoff")
    }

    pub fn claim(&self, run_id: &str) -> Result<RunSnapshot> {
        let path = self.path(run_id)?;
        let metadata = fs::symlink_metadata(&path).context("history handoff does not exist")?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("history handoff is not a regular file");
        }
        let claimed = self
            .root
            .join(format!(".{run_id}.{}.claim", std::process::id()));
        fs::rename(&path, &claimed).context("history handoff was already claimed")?;
        let result = (|| {
            let metadata = fs::symlink_metadata(&claimed)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("claimed history handoff is not a regular file");
            }
            let reader = BufReader::new(fs::File::open(&claimed)?);
            serde_json::from_reader(reader).context("failed to decode history handoff")
        })();
        let _ = fs::remove_file(&claimed);
        result
    }

    pub fn remove(&self, run_id: &str) -> Result<()> {
        let path = self.path(run_id)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("failed to remove history handoff"),
        }
    }

    pub fn reap_stale(&self, age: Duration) -> Result<usize> {
        let now = SystemTime::now();
        let mut removed = 0;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                continue;
            }
            let stale = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|elapsed| elapsed >= age);
            if stale {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn path(&self, run_id: &str) -> Result<PathBuf> {
        let parsed = Uuid::parse_str(run_id).context("invalid history run ID")?;
        if parsed.to_string() != run_id {
            bail!("history run ID is not canonical");
        }
        Ok(self.root.join(format!("{run_id}.json")))
    }
}

/// Where this mode's [`RunStore`] lives: `<plugin state dir>/history`. Every
/// herdr-tools mode shares one plugin state dir, and [`RunStore::reap_stale`]
/// deletes any old regular file in its `runs/`, so the store must never sit at
/// the shared root where another mode's files could be reaped.
pub fn store_dir(paths: &Paths) -> PathBuf {
    paths.state_dir().join(MODE)
}

pub fn launch(
    host: &Host,
    invocation: &Invocation,
    config: &Config,
    store: &RunStore,
) -> Result<LaunchOutcome> {
    let source_pane_id = invocation.source_pane()?;
    // Read before capturing, as talon did: an unreadable layout fails the launch.
    let client_width = host.client_width(source_pane_id)?;
    let (text, ansi) = capture_history(host, source_pane_id)?;
    let targets = find_targets(&text, &config.patterns)?;
    let snapshot = RunSnapshot {
        source_pane_id: source_pane_id.to_string(),
        text,
        ansi,
        targets,
        alphabet: config.alphabet.clone(),
    };
    let run_id = store.write(&snapshot)?;
    // Config wins; with no size for `history`, Herdr uses the manifest pane's.
    let popup = config.popup(Some(client_width)).map(|popup| popup.dims);
    match host.open_popup_sized(POPUP_KEY, popup, &[(RUN_ID_ENV, &run_id)]) {
        Ok(()) => {}
        Err(error) => {
            let _ = store.remove(&run_id);
            return Err(error);
        }
    }
    Ok(LaunchOutcome::Opened { run_id })
}

fn capture_history(host: &Host, pane_id: &str) -> Result<(String, String)> {
    let recent_text =
        normalize_newlines(read_recent_unwrapped(host, pane_id, false, CAPTURE_ROWS)?);
    let recent_ansi = normalize_newlines(read_recent_unwrapped(host, pane_id, true, CAPTURE_ROWS)?);
    let (text, ansi) = if recent_text.is_empty() {
        (
            normalize_newlines(read_visible(host, pane_id, false)?),
            normalize_newlines(read_visible(host, pane_id, true)?),
        )
    } else {
        (recent_text, recent_ansi)
    };
    let ansi = if row_count(&ansi) == row_count(&text) {
        ansi
    } else {
        text.clone()
    };
    Ok((text, ansi))
}

fn normalize_newlines(text: String) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn row_count(text: &str) -> usize {
    text.split_terminator('\n').count().max(1)
}

/// [`launch`] from explicit inputs (tests): any failure, including a broken
/// context or config, is also shown as a Herdr notification.
pub fn launch_with_reporting(
    host: &Host,
    context_json: &str,
    config_path: &Path,
    state_dir: &Path,
) -> Result<LaunchOutcome> {
    report(host, || {
        let invocation = Invocation::from_context_json(context_json)?;
        let config = Config::load(config_path)?;
        let store = RunStore::new(state_dir)?;
        launch(host, &invocation, &config, &store)
    })
}

/// `history open`: the inputs Herdr gives an action process.
pub fn launch_from_environment() -> Result<LaunchOutcome> {
    let host = Host::from_env();
    // Like talon, only a process Herdr started (one with a plugin context) notifies.
    if std::env::var_os("HERDR_PLUGIN_CONTEXT_JSON").is_none() {
        bail!("HERDR_PLUGIN_CONTEXT_JSON is not set");
    }
    report(&host, || {
        let invocation = Invocation::current()?;
        let config = Config::from_tools(tools_core::Config::current()?)?;
        let store = RunStore::new(store_dir(&Paths::from_env()))?;
        launch(&host, invocation, &config, &store)
    })
}

fn report(host: &Host, launch: impl FnOnce() -> Result<LaunchOutcome>) -> Result<LaunchOutcome> {
    let result = launch();
    if let Err(error) = &result {
        let _ = host.notify(
            NOTIFICATION_TITLE,
            &format!("History could not start: {error:#}"),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use std::fs::FileTimes;
    use std::os::unix::fs::MetadataExt;
    use std::time::SystemTime;

    use tempfile::tempdir;

    use crate::matcher::Occurrence;

    use super::*;

    fn sample_snapshot() -> RunSnapshot {
        RunSnapshot {
            source_pane_id: "w1:p1".into(),
            text: "deadbeef\n".into(),
            ansi: "\u{1b}[33mdeadbeef\u{1b}[0m\n".into(),
            targets: vec![Target {
                text: "deadbeef".into(),
                occurrences: vec![Occurrence {
                    row: 0,
                    highlight_col: 0,
                    highlight_width: 8,
                    hint_col: 0,
                    hint_width: 8,
                }],
            }],
            alphabet: vec!['a', 's'],
        }
    }

    #[test]
    fn handoff_is_private_claimed_once_and_removed() {
        let dir = tempdir().unwrap();
        let store = RunStore::new(dir.path()).unwrap();
        let run_id = store.write(&sample_snapshot()).unwrap();
        let path = store.root().join(format!("{run_id}.json"));

        assert_eq!(
            std::fs::metadata(store.root()).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(store.claim(&run_id).unwrap(), sample_snapshot());
        assert!(!path.exists());
        assert!(store.claim(&run_id).is_err());
    }

    #[test]
    fn invalid_ids_and_symlink_handoffs_are_rejected() {
        let dir = tempdir().unwrap();
        let store = RunStore::new(dir.path()).unwrap();
        let outside = dir.path().join("outside.json");
        std::fs::write(&outside, serde_json::to_vec(&sample_snapshot()).unwrap()).unwrap();
        let run_id = uuid::Uuid::new_v4().to_string();
        std::os::unix::fs::symlink(&outside, store.root().join(format!("{run_id}.json"))).unwrap();

        assert!(store.claim("../../outside").is_err());
        assert!(store.claim(&run_id).is_err());
        assert!(outside.exists());
    }

    #[test]
    fn stale_regular_handoffs_are_reaped_without_following_links() {
        let dir = tempdir().unwrap();
        let store = RunStore::new(dir.path()).unwrap();
        let run_id = store.write(&sample_snapshot()).unwrap();
        let path = store.root().join(format!("{run_id}.json"));
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_times(
            FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(7200)),
        )
        .unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, "keep").unwrap();
        let link = store.root().join("stale.json");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        assert_eq!(store.reap_stale(Duration::from_secs(3600)).unwrap(), 1);
        assert!(!path.exists());
        assert!(link.exists());
        assert!(outside.exists());
    }

    #[test]
    fn store_rejects_a_symlinked_run_directory() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("runs")).unwrap();

        assert!(RunStore::new(dir.path()).is_err());
    }
}
