//! Detection-task handoff for remote foreground observations, keyed by runtime PaneId.
//! The future process-global table avoids adding fields to every upstream runtime
//! constructor: a deliberate rebase tradeoff. Entries must follow runtime lifetimes,
//! and tests must use distinct pane IDs. This skeleton never installs an override.

// The core and watcher builders consume these deliberately unwired signatures.
#![allow(dead_code)]

use std::time::Instant;

use crate::{detect::Agent, layout::PaneId, ssh_panes::api::RemoteForeground};

pub(crate) fn link(_pane: PaneId, _connector_pgid: u32) {}
pub(crate) fn store(_pane: PaneId, _fg: &RemoteForeground, _now: Instant) {}
pub(crate) fn unlink(_pane: PaneId) {}
pub(crate) fn clear_all() {}

pub(crate) fn remote_agent(_pane: PaneId) -> Option<Option<Agent>> {
    None
}

pub(super) fn override_for(_pane: PaneId, _local_fg_pgid: Option<u32>) -> Option<RemoteOverride> {
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_panes_probe_table_is_inert() {
        let pane = PaneId::alloc();
        let foreground = RemoteForeground {
            process_group_id: Some(42),
            foreground_is_pane_shell: false,
            agent: Some("claude".into()),
            process_name: Some("claude".into()),
        };
        link(pane, 42);
        store(pane, &foreground, Instant::now());
        assert!(override_for(pane, Some(42)).is_none());
        assert!(remote_agent(pane).is_none());
        unlink(pane);
    }

    #[cfg(unix)]
    #[test]
    fn ssh_panes_remote_probe_and_cwd_reader_use_real_processes() {
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
