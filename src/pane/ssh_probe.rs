//! Detection-task handoff for remote foreground observations, keyed by runtime PaneId.
//! A process-global table avoids changing every upstream runtime constructor; this
//! deliberate rebase tradeoff requires unlinking on detach/disable and unique IDs
//! in tests. The atomic count keeps stock detection lock-free when there are no links.

// Transport and app adapters consume these seams independently.
#![allow(dead_code)]

use crate::{
    detect::Agent,
    layout::PaneId,
    ssh_panes::{api::RemoteForeground, LinkState},
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    time::Instant,
};

// clear_all is process-wide. Serialize table-using tests as well as giving
// them unique IDs, so ordinary cargo test is safe alongside nextest isolation.
#[cfg(test)]
pub(crate) static TEST_TABLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static COUNT: AtomicUsize = AtomicUsize::new(0);
static TABLE: OnceLock<Mutex<HashMap<PaneId, Entry>>> = OnceLock::new();

struct Entry {
    #[cfg(test)]
    test_foreground: Option<Option<u32>>,
    connector_pgid: u32,
    shell_pid: Option<u32>,
    last_verified_pgid: Option<u32>,
    last_seen: Instant,
    foreground: Option<RemoteForeground>,
}

fn table() -> &'static Mutex<HashMap<PaneId, Entry>> {
    TABLE.get_or_init(Mutex::default)
}

pub(crate) fn link(pane: PaneId, connector_pgid: u32) {
    link_at(pane, connector_pgid, Instant::now());
}

pub(crate) fn link_at(pane: PaneId, connector_pgid: u32, now: Instant) {
    let mut entries = table().lock().unwrap_or_else(|p| p.into_inner());
    entries.insert(
        pane,
        Entry {
            #[cfg(test)]
            test_foreground: None,
            connector_pgid,
            shell_pid: None,
            last_verified_pgid: None,
            last_seen: now,
            foreground: None,
        },
    );
    COUNT.store(entries.len(), Ordering::Release);
}

/// Used by prompt validation to check the *local* foreground again, instead of
/// trusting a detection tick that may have run just before the connector exited.
pub(crate) fn set_shell_pid(pane: PaneId, shell_pid: Option<u32>) {
    let mut entries = table().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(entry) = entries.get_mut(&pane) {
        entry.shell_pid = shell_pid;
    }
}

pub(crate) fn heartbeat(pane: PaneId, now: Instant) {
    let mut entries = table().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(entry) = entries.get_mut(&pane) {
        entry.last_seen = now;
    }
}

pub(crate) fn store(pane: PaneId, fg: &RemoteForeground, now: Instant) {
    let mut entries = table().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(entry) = entries.get_mut(&pane) {
        entry.foreground = Some(fg.clone());
        entry.last_seen = now;
    }
}

pub(crate) fn unlink(pane: PaneId) {
    if COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let mut entries = table().lock().unwrap_or_else(|p| p.into_inner());
    entries.remove(&pane);
    COUNT.store(entries.len(), Ordering::Release);
}

pub(crate) fn clear_all() {
    let mut entries = table().lock().unwrap_or_else(|p| p.into_inner());
    entries.clear();
    COUNT.store(0, Ordering::Release);
}

pub(crate) fn remote_agent(pane: PaneId) -> Option<Option<Agent>> {
    if COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    let (shell_pid, last_verified_pgid) = {
        let entries = table().lock().unwrap_or_else(|p| p.into_inner());
        let entry = entries.get(&pane)?;
        (entry.shell_pid, entry.last_verified_pgid)
    };
    // Do not hold the process-global table while querying the OS. Real runtimes
    // always supply their local shell pid; channel-only tests use the last tick.
    let local_fg = match shell_pid {
        Some(pid) => crate::detect::foreground_process_group_id(pid),
        None => last_verified_pgid,
    };
    override_for(pane, local_fg).map(|remote| remote.probe_result().agent)
}

pub(super) fn override_for(pane: PaneId, local_fg_pgid: Option<u32>) -> Option<RemoteOverride> {
    override_at(pane, local_fg_pgid, Instant::now())
}

fn override_at(pane: PaneId, local_fg_pgid: Option<u32>, now: Instant) -> Option<RemoteOverride> {
    if COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    let mut entries = table().lock().unwrap_or_else(|p| p.into_inner());
    let entry = entries.get_mut(&pane)?;
    #[cfg(test)]
    let local_fg_pgid = entry.test_foreground.unwrap_or(local_fg_pgid);
    entry.last_verified_pgid = local_fg_pgid;
    if local_fg_pgid != Some(entry.connector_pgid)
        || LinkState::at(entry.last_seen, now) == LinkState::Gone
    {
        return None;
    }
    // Soft-stale reports stay frozen until a newer ordered report replaces them.
    Some(RemoteOverride {
        foreground: entry.foreground.clone()?,
    })
}

/// An observation valid only while the local connector owns the pane foreground.
pub(super) struct RemoteOverride {
    foreground: RemoteForeground,
}

impl RemoteOverride {
    pub(super) fn foreground_pgid(&self) -> Option<u32> {
        self.foreground.process_group_id
    }

    pub(super) fn probe_result(&self) -> super::ProcessProbeResult {
        super::ProcessProbeResult {
            process_group_id: self.foreground.process_group_id,
            foreground_is_pane_shell: self.foreground.foreground_is_pane_shell,
            agent: self
                .foreground
                .agent
                .as_deref()
                .and_then(crate::detect::parse_agent_label),
            // Remote PIDs must never enter the local background-job liveness tracker.
            agent_pid: None,
            process_name: self.foreground.process_name.clone(),
        }
    }
}

impl super::PaneRuntime {
    pub(crate) fn ssh_probe_key(&self) -> PaneId {
        self.pane_id
    }
}

#[cfg(unix)]
pub(crate) fn remote_side_probe(shell_pid: u32) -> RemoteForeground {
    let foreground_pgid = crate::detect::foreground_process_group_id(shell_pid);
    let probe = super::probe_foreground_process(shell_pid, foreground_pgid);
    RemoteForeground {
        process_group_id: probe.process_group_id,
        foreground_is_pane_shell: probe.foreground_is_pane_shell,
        agent: probe
            .agent
            .map(|agent| crate::detect::agent_label(agent).to_string()),
        process_name: probe.process_name,
    }
}

/// Cwd belongs at report level, outside the four-field foreground wire object.
/// Read it on the remote host; projecting it must never require local path lookup.
#[cfg(unix)]
pub(crate) fn remote_side_cwds(
    shell_pid: u32,
    foreground_pgid: Option<u32>,
) -> (Option<String>, Option<String>) {
    let cwd = |pid| crate::platform::process_cwd(pid).map(|path| path.display().to_string());
    (cwd(shell_pid), foreground_pgid.and_then(cwd))
}

/// The cold-restore scope may replace a shell command with its saved connector
/// before any process is spawned. Outside that synchronous scope, argv/env/cwd
/// reach the stock backend unchanged. Tests use a socket-only backend here.
pub(super) fn spawn_with_portable_pty(
    rows: u16,
    cols: u16,
    mut cmd: portable_pty::CommandBuilder,
) -> std::io::Result<crate::pty::backend::SpawnedPty> {
    crate::ssh_panes::apply_restore_command(&mut cmd)?;
    #[cfg(all(test, unix))]
    {
        test_support::spawn_with_portable_pty(rows, cols, cmd)
    }
    #[cfg(not(all(test, unix)))]
    {
        crate::pty::backend::spawn_with_portable_pty(rows, cols, cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_panes_probe_checks_connector_and_hard_lease() {
        let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
        let pane = PaneId::alloc();
        let now = Instant::now();
        let foreground = RemoteForeground {
            process_group_id: Some(90),
            foreground_is_pane_shell: false,
            agent: Some("claude".into()),
            process_name: Some("claude".into()),
        };
        link_at(pane, 42, now);
        assert!(override_at(pane, Some(42), now).is_none());
        store(pane, &foreground, now);
        assert!(override_at(pane, Some(41), now).is_none());
        let remote = override_at(pane, Some(42), now + crate::ssh_panes::SOFT_LEASE).unwrap();
        assert_eq!(remote.probe_result().agent, Some(Agent::Claude));
        assert_eq!(remote.probe_result().agent_pid, None);
        assert!(override_at(pane, Some(42), now + crate::ssh_panes::HARD_LEASE).is_none());
        assert!(override_at(pane, None, now).is_none());
        unlink(pane);
        assert!(override_at(pane, Some(42), now).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn ssh_panes_remote_probe_and_cwd_reader_use_real_processes() {
        let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
        let pid = std::process::id();
        let expected = super::super::probe_foreground_process(
            pid,
            crate::detect::foreground_process_group_id(pid),
        );
        let actual = remote_side_probe(pid);
        assert_eq!(actual.process_group_id, expected.process_group_id);
        assert_eq!(
            actual.foreground_is_pane_shell,
            expected.foreground_is_pane_shell
        );
        assert_eq!(
            actual.agent,
            expected
                .agent
                .map(|agent| crate::detect::agent_label(agent).to_string())
        );
        assert_eq!(actual.process_name, expected.process_name);
        assert_eq!(
            remote_side_cwds(pid, Some(pid)),
            (
                Some(std::env::current_dir().unwrap().display().to_string()),
                Some(std::env::current_dir().unwrap().display().to_string())
            )
        );
    }
}

#[cfg(all(test, unix))]
pub(crate) mod test_support;

#[cfg(all(test, unix))]
mod detection_tests;
