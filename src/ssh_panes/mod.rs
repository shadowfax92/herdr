//! Server-owned SSH link registry and the fork's local/remote transport seam.
//!
//! ```text
//! local pane -> connector -> ssh -R -> remote shell + watcher
//!                    API <- attach/report <- watcher
//!               registry -> probe override / API cwd / split inheritance
//! ```
//! This integration skeleton accepts no links. Keeping the hooks here lets later
//! implementations change fork-owned files without spreading rebase conflicts.

// These public seams are intentionally unused until the core/remote pieces land.
#![allow(dead_code)]

pub(crate) mod api;
pub(crate) mod cli;
pub(crate) mod config;
pub(crate) mod connector;
pub(crate) mod remote;
pub(crate) mod sync;

use std::time::Instant;

use crate::{layout::PaneId, terminal::TerminalId};

pub(crate) fn disabled_message() -> String {
    format!("herdr ssh: SSH panes are disabled ([ssh_panes] enabled = false in {}); use plain ssh <host>", crate::config::config_path().display())
}
pub(crate) const NOT_IMPLEMENTED_MESSAGE: &str = "ssh panes: not implemented yet";

/// Local server authority over links; runtime identity remains the terminal's identity.
pub(crate) struct Registry {
    config: config::SshPanesConfig,
}

impl Registry {
    pub fn new(cfg: &config::SshPanesConfig) -> Self {
        Self {
            config: cfg.clone(),
        }
    }

    pub fn set_config(&mut self, cfg: &config::SshPanesConfig) -> ConfigChange {
        let disabled_now = self.enabled() && !cfg.enabled;
        self.config = cfg.clone();
        ConfigChange { disabled_now }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn attach(&mut self, _a: AttachInput, _now: Instant) -> Result<&Link, SshPanesError> {
        Err(self.unavailable())
    }

    pub fn report(
        &mut self,
        _r: &api::ReportParams,
        _now: Instant,
    ) -> Result<ReportOutcome, SshPanesError> {
        Err(self.unavailable())
    }

    pub fn detach(&mut self, _link_id: &str) -> Option<Link> {
        None
    }

    pub fn link_for_terminal(&self, _t: &TerminalId, _now: Instant) -> Option<LinkView<'_>> {
        None
    }

    pub fn inherit_target(&self, _t: &TerminalId, _now: Instant) -> Option<InheritTarget> {
        None
    }

    pub fn inherit_new_tab(&self) -> bool {
        false
    }

    pub fn links(&self, _now: Instant) -> Vec<LinkView<'_>> {
        Vec::new()
    }

    pub fn linked_terminals(&self) -> impl Iterator<Item = &TerminalId> {
        std::iter::empty()
    }

    fn unavailable(&self) -> SshPanesError {
        if self.enabled() {
            SshPanesError::NotImplemented
        } else {
            SshPanesError::Disabled
        }
    }
}

pub(crate) struct ConfigChange {
    pub disabled_now: bool,
}

/// Resolved by the app after public pane IDs have been translated to runtime IDs.
pub(crate) struct AttachInput {
    pub params: api::AttachParams,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    pub local_cwd: Option<String>,
}

pub(crate) struct Link {
    pub link_id: String,
    pub host: String,
    pub terminal_id: TerminalId,
    pub connector_pgid: u32,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub cwd_hint: Option<String>,
    pub agent_label: Option<String>,
    pub last_seq: u64,
    pub attached_at: Instant,
    pub last_seen_at: Instant,
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
    NotImplemented,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_panes_registry_is_empty_in_both_config_modes() {
        let mut registry = Registry::new(&config::SshPanesConfig::default());
        let terminal = TerminalId::alloc();
        for enabled in [false, true] {
            registry.set_config(&config::SshPanesConfig {
                enabled,
                ..Default::default()
            });
            assert_eq!(registry.enabled(), enabled);
            assert!(!registry.inherit_new_tab());
            assert!(registry.links(Instant::now()).is_empty());
            assert!(registry
                .link_for_terminal(&terminal, Instant::now())
                .is_none());
            assert!(registry.inherit_target(&terminal, Instant::now()).is_none());
            assert_eq!(registry.linked_terminals().count(), 0);
        }
        assert!(
            registry
                .set_config(&config::SshPanesConfig::default())
                .disabled_now
        );
    }
}
