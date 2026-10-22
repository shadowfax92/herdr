//! Server-owned SSH link registry and the fork's local/remote transport seam.
//!
//! ```text
//! local pane -> connector -> ssh -R -> remote shell + watcher
//!                    API <- attach/report <- watcher
//!               registry -> probe override / API cwd / split inheritance
//! ```
//! Only this registry installs probe entries. API projection never changes the
//! terminal's local cwd, so persistence and local filesystem consumers stay local.

// The parallel transport implementation consumes these public seams.
#![allow(dead_code)]

pub(crate) mod api;
pub(crate) mod cli;
pub(crate) mod config;
pub(crate) mod connector;
pub(crate) mod remote;
pub(crate) mod sync;

use crate::{layout::PaneId, pane::ssh_probe, terminal::TerminalId};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub(crate) const SOFT_LEASE: Duration = Duration::from_secs(15);
pub(crate) const PROVISIONAL_LEASE: Duration = Duration::from_secs(30);
pub(crate) const HARD_LEASE: Duration = Duration::from_secs(120);

pub(crate) fn disabled_message() -> String {
    format!("herdr ssh: SSH panes are disabled ([ssh_panes] enabled = false in {}); use plain ssh <host>", crate::config::config_path().display())
}
pub(crate) const NOT_IMPLEMENTED_MESSAGE: &str = "ssh panes: not implemented yet";

/// Owns link leases and semantic transitions on the app thread. Detection tasks
/// share local ownership facts through the probe table; callers supply time and
/// tests inject process observations to exercise expiry without PTYs.
pub(crate) struct Registry {
    config: config::SshPanesConfig,
    links: HashMap<String, Link>,
    next_refresh: Option<Instant>,
}

impl Registry {
    pub fn new(cfg: &config::SshPanesConfig) -> Self {
        Self {
            config: cfg.clone(),
            links: HashMap::new(),
            next_refresh: None,
        }
    }

    pub fn set_config(&mut self, cfg: &config::SshPanesConfig) -> ConfigChange {
        let disabled_now = self.enabled() && !cfg.enabled;
        self.config = cfg.clone();
        let revoked: Vec<_> = self
            .links
            .values()
            .filter(|link| {
                !cfg.enabled
                    || (!cfg.allowed_hosts.is_empty() && !cfg.allowed_hosts.contains(&link.host))
            })
            .map(|link| link.link_id.clone())
            .collect();
        let affected = revoked
            .into_iter()
            .filter_map(|id| self.detach(&id).map(|link| link.terminal_id))
            .collect();
        ConfigChange {
            disabled_now,
            affected,
        }
    }

    /// App-loop maintenance is per linked pane at most once per 300 ms, independent
    /// of render/client fanout. Explicit API mutations/save use refresh directly.
    pub fn refresh_due(&mut self, now: Instant) -> Vec<TerminalId> {
        if self.links.is_empty() || self.next_refresh.is_some_and(|at| now < at) {
            return Vec::new();
        }
        self.next_refresh = Some(now + Duration::from_millis(300));
        self.refresh(now)
    }

    pub fn refresh(&mut self, now: Instant) -> Vec<TerminalId> {
        let mut changed = Vec::new();
        self.links.retain(|_, link| {
            let state = link.state(now);
            if state != link.observed_state {
                changed.push(link.terminal_id.clone());
                link.observed_state = state;
            }
            if state == LinkState::Gone {
                ssh_probe::unlink(link.pane_id);
                false
            } else {
                true
            }
        });
        changed
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn attach(&mut self, a: AttachInput, now: Instant) -> Result<&Link, SshPanesError> {
        self.require_enabled()?;
        self.refresh(now);
        let p = &a.params;
        if p.protocol != 1
            || p.connector_pid == 0
            || p.connector_pgid == 0
            || p.link_id.is_empty()
            || p.host.is_empty()
            || [&p.link_id, &p.host]
                .iter()
                .any(|s| s.chars().any(char::is_control))
        {
            return Err(SshPanesError::InvalidRequest);
        }
        if !self.config.allowed_hosts.is_empty() && !self.config.allowed_hosts.contains(&p.host) {
            return Err(SshPanesError::HostNotAllowed);
        }
        if let Some(existing) = self.links.get(&p.link_id) {
            // A link ID cannot be moved to another pane or connector by a heartbeat.
            if existing.terminal_id != a.terminal_id
                || existing.pane_id != a.pane_id
                || existing.host != p.host
                || existing.connector_pgid != p.connector_pgid
                || existing.connector_pid != p.connector_pid
            {
                return Err(SshPanesError::InvalidRequest);
            }
        }
        let provisional = self
            .links
            .values()
            .find(|link| link.provisional && link.terminal_id == a.terminal_id);
        if let Some(link) = provisional {
            if link.host != p.host
                || link.pane_id != a.pane_id
                || link.connector_pid != p.connector_pid
                || link.connector_pgid != p.connector_pgid
            {
                return Err(SshPanesError::InvalidRequest);
            }
            let id = link.link_id.clone();
            self.detach(&id);
        }
        if self.links.values().any(|link| {
            link.link_id != p.link_id
                && (link.terminal_id == a.terminal_id || link.pane_id == a.pane_id)
                && link.state(now) != LinkState::Gone
        }) {
            return Err(SshPanesError::PaneLinked);
        }
        let obsolete: Vec<_> = self
            .links
            .values()
            .filter(|link| {
                (link.terminal_id == a.terminal_id || link.pane_id == a.pane_id)
                    && (link.link_id != p.link_id || link.state(now) == LinkState::Gone)
            })
            .map(|link| link.link_id.clone())
            .collect();
        for id in obsolete {
            self.detach(&id);
        }
        let link = self.links.entry(p.link_id.clone()).or_insert_with(|| {
            ssh_probe::link_at(a.pane_id, p.connector_pgid, now);
            ssh_probe::set_connector_pid(a.pane_id, p.connector_pid);
            Link {
                link_id: p.link_id.clone(),
                host: p.host.clone(),
                terminal_id: a.terminal_id,
                pane_id: a.pane_id,
                connector_pid: p.connector_pid,
                connector_pgid: p.connector_pgid,
                local_cwd: a.local_cwd,
                cwd: None,
                foreground_cwd: None,
                cwd_hint: p.cwd_hint.clone(),
                agent_label: None,
                last_seq: 0,
                attached_at: now,
                last_seen_at: now,
                last_report_at: None,
                provisional: false,
                observed_state: LinkState::Live,
            }
        });
        link.last_seen_at = now;
        ssh_probe::heartbeat(link.pane_id, now);
        Ok(link)
    }

    /// Register before emitting PaneCreated/TabCreated so immediate consumers see
    /// the remote launch cwd. The connector's attach replaces this temporary ID.
    pub fn provision(&mut self, input: AttachInput, now: Instant) -> Result<(), SshPanesError> {
        let link = self.attach(input, now)?;
        let id = link.link_id.clone();
        if let Some(link) = self.links.get_mut(&id) {
            link.provisional = true;
            link.cwd = link.cwd_hint.clone();
        }
        Ok(())
    }

    pub fn report(
        &mut self,
        r: &api::ReportParams,
        now: Instant,
    ) -> Result<ReportOutcome, SshPanesError> {
        self.require_enabled()?;
        let link = self
            .links
            .get_mut(&r.link_id)
            .filter(|l| !l.provisional && l.state(now) != LinkState::Gone)
            .ok_or(SshPanesError::UnknownLink)?;
        // Ignored/replayed reports cannot extend the lease or overwrite newer state.
        if link.last_report_at.is_some() && r.seq <= link.last_seq {
            return Ok(ReportOutcome {
                accepted: false,
                changed: false,
            });
        }
        let changed = link.state(now) != LinkState::Live
            || link.cwd != r.cwd
            || link.foreground_cwd != r.foreground_cwd
            || link.agent_label != r.foreground.agent;
        link.cwd = r.cwd.clone();
        link.foreground_cwd = r.foreground_cwd.clone();
        link.agent_label = r.foreground.agent.clone();
        link.last_seq = r.seq;
        link.last_report_at = Some(now);
        link.observed_state = LinkState::Live;
        link.last_seen_at = now;
        ssh_probe::store(link.pane_id, &r.foreground, now);
        Ok(ReportOutcome {
            accepted: true,
            changed,
        })
    }

    pub fn detach(&mut self, link_id: &str) -> Option<Link> {
        let link = self.links.remove(link_id)?;
        ssh_probe::unlink(link.pane_id);
        Some(link)
    }

    pub fn link_for_terminal(&self, t: &TerminalId, now: Instant) -> Option<LinkView<'_>> {
        if !self.enabled() {
            return None;
        }
        self.links
            .values()
            .find(|link| &link.terminal_id == t && link.state(now) != LinkState::Gone)
            .map(|link| LinkView {
                link,
                state: link.state(now),
            })
    }

    pub fn inherit_target(&self, t: &TerminalId, now: Instant) -> Option<InheritTarget> {
        let view = self.link_for_terminal(t, now)?;
        Some(InheritTarget {
            host: view.link.host.clone(),
            cwd: (view.state == LinkState::Live)
                .then(|| view.link.foreground_cwd.clone())
                .flatten()
                .or_else(|| view.link.cwd.clone())
                .or_else(|| view.link.cwd_hint.clone()),
        })
    }

    pub fn inherit_new_tab(&self) -> bool {
        self.enabled() && self.config.inherit_new_tab
    }

    pub fn links(&self, now: Instant) -> Vec<LinkView<'_>> {
        if !self.enabled() {
            return Vec::new();
        }
        let mut links: Vec<_> = self
            .links
            .values()
            .filter(|link| link.state(now) != LinkState::Gone)
            .map(|link| LinkView {
                link,
                state: link.state(now),
            })
            .collect();
        links.sort_by(|a, b| a.link.link_id.cmp(&b.link.link_id));
        links
    }

    pub fn linked_terminals(&self) -> impl Iterator<Item = &TerminalId> {
        // All retained records are needed for disable/drop cleanup. Capture-time
        // ownership uses links(now), so expired records cannot scrub new local agents.
        self.links.values().map(|link| &link.terminal_id)
    }

    fn require_enabled(&self) -> Result<(), SshPanesError> {
        if self.enabled() {
            Ok(())
        } else {
            Err(SshPanesError::Disabled)
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        // Pane IDs are never reused, but removing owned entries bounds global state.
        for link in self.links.values() {
            ssh_probe::unlink(link.pane_id);
        }
    }
}

pub(crate) struct ConfigChange {
    pub disabled_now: bool,
    pub affected: Vec<TerminalId>,
}

/// Public IDs are resolved to the terminal and its runtime identity by the app.
pub(crate) struct AttachInput {
    pub params: api::AttachParams,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    pub local_cwd: Option<String>,
}

/// An attached terminal's remote observation and lease. Runtime PaneId is kept
/// separately from public layout IDs so detection follows the actual process.
pub(crate) struct Link {
    pub link_id: String,
    pub host: String,
    pub terminal_id: TerminalId,
    pub pane_id: PaneId,
    pub connector_pid: u32,
    pub connector_pgid: u32,
    pub local_cwd: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub cwd_hint: Option<String>,
    pub agent_label: Option<String>,
    pub last_seq: u64,
    pub attached_at: Instant,
    pub last_seen_at: Instant,
    pub last_report_at: Option<Instant>,
    provisional: bool,
    observed_state: LinkState,
}

impl Link {
    pub fn state(&self, now: Instant) -> LinkState {
        if !ssh_probe::owner_is_live(self.pane_id)
            || now.saturating_duration_since(self.last_seen_at) >= HARD_LEASE
            || (self.provisional
                && now.saturating_duration_since(self.attached_at) >= PROVISIONAL_LEASE)
        {
            LinkState::Gone
        } else if self.provisional {
            LinkState::Live
        } else if now.saturating_duration_since(self.last_report_at.unwrap_or(self.attached_at))
            >= SOFT_LEASE
        {
            LinkState::Stale
        } else {
            LinkState::Live
        }
    }
}

pub(crate) struct LinkView<'a> {
    pub link: &'a Link,
    pub state: LinkState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkState {
    Live,
    Stale,
    Gone,
}

impl LinkState {
    pub fn at(last_seen: Instant, now: Instant) -> Self {
        let age = now.saturating_duration_since(last_seen);
        if age >= HARD_LEASE {
            Self::Gone
        } else if age >= SOFT_LEASE {
            Self::Stale
        } else {
            Self::Live
        }
    }
}

pub(crate) struct InheritTarget {
    pub host: String,
    pub cwd: Option<String>,
}
pub(crate) struct ReportOutcome {
    pub accepted: bool,
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshPanesError {
    Disabled,
    HostNotAllowed,
    PaneLinked,
    UnknownLink,
    InvalidRequest,
}

/// Recognize only the argv shape produced by SSH inheritance. Generic command
/// panes keep stock restore policy even if one argument happens to say "ssh".
pub(crate) fn connector_argv(argv: &[String]) -> bool {
    if !matches!(argv.len(), 3 | 5)
        || argv[1] != "ssh"
        || argv[2].is_empty()
        || (argv.len() == 5 && argv[3] != "--cwd")
    {
        return false;
    }
    let executable = std::path::Path::new(&argv[0]).file_name();
    executable.is_some_and(|name| {
        name == "herdr"
            || crate::platform::launch_executable()
                .ok()
                .and_then(|path| path.file_name().map(|s| s.to_os_string()))
                .as_deref()
                == Some(name)
    })
}

// Cold restore is synchronous. A thread-local scope carries saved connector
// commands through stock persistence without adding argv to its shared protocol
// or teaching every upstream runtime constructor about SSH.
thread_local! {
    static RESTORE_COMMANDS: std::cell::RefCell<HashMap<String, Vec<String>>> = std::cell::RefCell::new(HashMap::new());
}

/// Restore the previous command scope on success or unwind; no later spawn
/// may inherit a completed restore's argv overrides.
struct RestoreScope(HashMap<String, Vec<String>>);
impl RestoreScope {
    fn install(commands: HashMap<String, Vec<String>>) -> Self {
        Self(RESTORE_COMMANDS.with(|slot| slot.replace(commands)))
    }
}
impl Drop for RestoreScope {
    fn drop(&mut self) {
        RESTORE_COMMANDS.with(|slot| slot.replace(std::mem::take(&mut self.0)));
    }
}

/// Replace only the command registered for this managed public pane identity.
pub(crate) fn apply_restore_command(cmd: &mut portable_pty::CommandBuilder) -> std::io::Result<()> {
    let argv = RESTORE_COMMANDS.with(|slot| {
        let pane = cmd.get_env("HERDR_PANE_ID")?.to_str()?;
        slot.borrow().get(pane).cloned()
    });
    if let Some(argv) = argv {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or_else(|| std::io::Error::other("HOME is not set for SSH connector restore"))?;
        *cmd.get_argv_mut() = argv.into_iter().map(std::ffi::OsString::from).collect();
        cmd.cwd(home);
        cmd.env("HERDR_SSH_PANES_SPAWNED", "1");
    }
    Ok(())
}

type RestoredSession = (
    Vec<crate::workspace::Workspace>,
    HashMap<TerminalId, crate::terminal::TerminalState>,
    HashMap<TerminalId, crate::terminal::TerminalRuntime>,
);

/// Restore split-created connectors through the stock layout/identity/history
/// pipeline. The backend consumes the scoped argv before spawning, and the
/// returned terminal retains it for the next capture; typed SSH stays a shell.
// Match persistence's inputs plus the live startup config, keeping the upstream hook small.
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore(
    config: &config::SshPanesConfig,
    snapshot: &crate::persist::SessionSnapshot,
    history: Option<&crate::persist::SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
    default_shell: &str,
    shell_mode: crate::config::ShellModeConfig,
    resume_agents_on_restore: bool,
    events: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    render_notify: std::sync::Arc<tokio::sync::Notify>,
    render_dirty: std::sync::Arc<crate::render_signal::RenderSignal>,
) -> RestoredSession {
    let stock_restore = || {
        crate::persist::restore(
            snapshot,
            history,
            rows,
            cols,
            scrollback_limit_bytes,
            default_shell,
            shell_mode,
            resume_agents_on_restore,
            events,
            render_notify,
            render_dirty,
        )
    };
    if !config.enabled {
        return stock_restore();
    }
    let mut commands = HashMap::new();
    for workspace in &snapshot.workspaces {
        let Some(workspace_id) = workspace.id.as_deref() else {
            continue;
        };
        for tab in &workspace.tabs {
            for (raw, pane) in &tab.panes {
                let Some(argv) = pane
                    .launch_argv
                    .as_ref()
                    .filter(|argv| connector_argv(argv))
                else {
                    continue;
                };
                let Some(number) = workspace.public_pane_numbers.get(raw) else {
                    continue;
                };
                commands.insert(
                    crate::workspace::public_pane_id_for_number(workspace_id, *number),
                    argv.clone(),
                );
            }
        }
    }
    if commands.is_empty() {
        return stock_restore();
    }
    let _scope = RestoreScope::install(commands.clone());
    let (workspaces, mut terminals, runtimes) = stock_restore();
    for workspace in &workspaces {
        for tab in &workspace.tabs {
            for (pane_id, pane) in &tab.panes {
                let Some(number) = workspace.public_pane_number(*pane_id) else {
                    continue;
                };
                let public = crate::workspace::public_pane_id_for_number(&workspace.id, number);
                if let Some(argv) = commands.get(&public) {
                    if let Some(terminal) = terminals.get_mut(&pane.attached_terminal_id) {
                        terminal.launch_argv = Some(argv.clone());
                    }
                }
            }
        }
    }
    (workspaces, terminals, runtimes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_panes_attach_report_and_heartbeat_preserve_remote_state() {
        let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
        let mut registry = Registry::new(&config::SshPanesConfig {
            enabled: true,
            ..Default::default()
        });
        let now = Instant::now();
        let terminal = TerminalId::alloc();
        let pane = PaneId::alloc();
        ssh_probe::test_owner(pane, true, Some(42));
        let attach = || AttachInput {
            params: api::AttachParams {
                pane_id: "w1:p1".into(),
                link_id: "one".into(),
                host: "studio".into(),
                connector_pid: 42,
                connector_pgid: 42,
                cwd_hint: Some("/hint".into()),
                protocol: 1,
            },
            pane_id: pane,
            terminal_id: terminal.clone(),
            local_cwd: Some("/local".into()),
        };
        registry.attach(attach(), now).unwrap();
        let report = api::ReportParams {
            link_id: "one".into(),
            seq: 1,
            shell_pid: 80,
            shell_name: "fish".into(),
            cwd: Some("/remote".into()),
            foreground_cwd: Some("/remote/job".into()),
            foreground: api::RemoteForeground {
                process_group_id: Some(90),
                foreground_is_pane_shell: false,
                agent: Some("claude".into()),
                process_name: Some("claude".into()),
            },
        };
        let outcome = registry.report(&report, now).unwrap();
        assert!(outcome.accepted && outcome.changed);
        assert!(!registry.report(&report, now).unwrap().accepted);
        registry.attach(attach(), now).unwrap();
        let target = registry.inherit_target(&terminal, now).unwrap();
        assert_eq!(target.host, "studio");
        assert_eq!(target.cwd.as_deref(), Some("/remote/job"));
        assert_eq!(
            registry.links(now)[0].link.agent_label.as_deref(),
            Some("claude")
        );
        registry.detach("one").unwrap();
        assert!(registry.links(now).is_empty());
    }
}

#[cfg(test)]
mod registry_tests;

#[cfg(all(test, unix))]
mod restore_tests;
