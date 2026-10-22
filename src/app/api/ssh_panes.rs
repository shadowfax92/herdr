//! App/API adapter for server-owned SSH links. Projection is intentionally separate
//! from terminal runtime cwd so remote paths cannot leak into local persistence or Git.

use super::responses::{encode_error, encode_success};
use crate::{
    api::schema::{PaneInfo, ResponseResult},
    app::App,
    persist::SessionSnapshot,
    ssh_panes::{
        api::{LinkInfo, PaneSshInfo, SshLinkState, SshPanesRequest, SshPanesResult},
        config::SshPanesConfig,
        AttachInput, Link, LinkState, SshPanesError, SOFT_LEASE,
    },
    terminal::TerminalId,
};
use std::{collections::HashSet, time::Instant};

impl App {
    pub(super) fn handle_ssh_panes(&mut self, id: String, params: SshPanesRequest) -> String {
        let now = Instant::now();
        if let SshPanesRequest::List(params) = params {
            if !self.ssh_panes.enabled() {
                return encode_success(
                    id,
                    ResponseResult::SshPanes {
                        ssh_panes: SshPanesResult::Links {
                            enabled: false,
                            links: Vec::new(),
                        },
                    },
                );
            }
            let filter = match params.pane_id {
                Some(pane) => {
                    let Some((ws_idx, pane_id)) = self.parse_pane_id(&pane) else {
                        return encode_error(id, "pane_not_found", "pane not found");
                    };
                    self.state.workspaces[ws_idx].terminal_id(pane_id).cloned()
                }
                None => None,
            };
            let links = self
                .ssh_panes
                .links(now)
                .into_iter()
                .filter(|view| {
                    filter
                        .as_ref()
                        .is_none_or(|terminal| terminal == &view.link.terminal_id)
                })
                .filter_map(|view| self.ssh_link_info(view.link, view.state, now))
                .collect();
            return encode_success(
                id,
                ResponseResult::SshPanes {
                    ssh_panes: SshPanesResult::Links {
                        enabled: self.ssh_panes.enabled(),
                        links,
                    },
                },
            );
        }
        if !self.ssh_panes.enabled() {
            return ssh_error(id, SshPanesError::Disabled);
        }
        let result = match params {
            SshPanesRequest::Attach(params) => {
                let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
                    return encode_error(id, "pane_not_found", "pane not found");
                };
                let Some(terminal_id) = self.state.workspaces[ws_idx].terminal_id(pane_id).cloned()
                else {
                    return encode_error(id, "pane_not_found", "pane not found");
                };
                let before = self.pane_metadata(ws_idx, pane_id).map(Projection::from);
                let local_cwd = before.as_ref().and_then(|p| {
                    p.ssh
                        .as_ref()
                        .map_or_else(|| p.cwd.clone(), |ssh| ssh.local_cwd.clone())
                });
                let runtime = self.terminal_runtimes.get(&terminal_id);
                let runtime_key = runtime.map_or(pane_id, |r| r.ssh_probe_key());
                let shell_pid = runtime.and_then(|r| r.child_pid());
                if let Err(error) = self.ssh_panes.attach(
                    AttachInput {
                        params,
                        pane_id: runtime_key,
                        terminal_id: terminal_id.clone(),
                        local_cwd,
                    },
                    now,
                ) {
                    return ssh_error(id, error);
                }
                crate::pane::ssh_probe::set_shell_pid(runtime_key, shell_pid);
                let Some(view) = self.ssh_panes.link_for_terminal(&terminal_id, now) else {
                    return ssh_error(id, SshPanesError::UnknownLink);
                };
                let Some(link) = self.ssh_link_info(view.link, view.state, now) else {
                    return encode_error(id, "pane_not_found", "pane not found");
                };
                if before != self.pane_metadata(ws_idx, pane_id).map(Projection::from) {
                    self.emit_pane_updated(ws_idx, pane_id);
                }
                SshPanesResult::Attached {
                    link,
                    lease_ms: SOFT_LEASE.as_millis() as u64,
                }
            }
            SshPanesRequest::Report(params) => {
                if params.shell_pid == 0 || params.shell_name.is_empty() {
                    return ssh_error(id, SshPanesError::InvalidRequest);
                }
                let outcome = match self.ssh_panes.report(&params, now) {
                    Ok(outcome) => outcome,
                    Err(error) => return ssh_error(id, error),
                };
                if outcome.changed {
                    let target = self
                        .ssh_panes
                        .links(now)
                        .into_iter()
                        .find(|view| view.link.link_id == params.link_id)
                        .and_then(|view| {
                            self.resolve_terminal_target(view.link.terminal_id.as_str())
                                .ok()
                        });
                    if let Some(target) = target {
                        self.emit_pane_updated(target.ws_idx, target.pane_id);
                    }
                }
                SshPanesResult::Reported {
                    accepted: outcome.accepted,
                }
            }
            SshPanesRequest::Detach(params) => {
                // Idempotent detach permits connector cleanup after hard expiry.
                if let Some(link) = self.ssh_panes.detach(&params.link_id) {
                    if let Ok(target) = self.resolve_terminal_target(link.terminal_id.as_str()) {
                        self.emit_pane_updated(target.ws_idx, target.pane_id);
                    }
                }
                SshPanesResult::Detached {}
            }
            SshPanesRequest::List(_) => unreachable!("list handled before mutations"),
        };
        encode_success(id, ResponseResult::SshPanes { ssh_panes: result })
    }

    fn ssh_link_info(&self, link: &Link, state: LinkState, now: Instant) -> Option<LinkInfo> {
        let target = self
            .resolve_terminal_target(link.terminal_id.as_str())
            .ok()?;
        let pane = self.pane_metadata(target.ws_idx, target.pane_id)?;
        Some(LinkInfo {
            link_id: link.link_id.clone(),
            host: link.host.clone(),
            pane_id: pane.pane_id,
            terminal_id: link.terminal_id.to_string(),
            state: api_state(state),
            cwd: link.cwd.clone().or_else(|| link.cwd_hint.clone()),
            foreground_cwd: link.foreground_cwd.clone(),
            local_cwd: pane.ssh.and_then(|ssh| ssh.local_cwd),
            agent: link.agent_label.clone(),
            attached_ms_ago: elapsed_ms(now, link.attached_at),
            last_report_ms_ago: link.last_report_at.map(|at| elapsed_ms(now, at)),
        })
    }

    pub(crate) fn apply_ssh_panes_config(&mut self, cfg: &SshPanesConfig) {
        let affected: Vec<_> = self.ssh_panes.linked_terminals().cloned().collect();
        if self.ssh_panes.set_config(cfg).disabled_now {
            for terminal in affected {
                if let Ok(target) = self.resolve_terminal_target(terminal.as_str()) {
                    self.emit_pane_updated(target.ws_idx, target.pane_id);
                }
            }
        }
    }

    pub(crate) fn project_ssh_pane_info(&self, terminal: &TerminalId, info: &mut PaneInfo) {
        let Some(view) = self.ssh_panes.link_for_terminal(terminal, Instant::now()) else {
            return;
        };
        info.ssh = Some(PaneSshInfo {
            host: view.link.host.clone(),
            link_id: view.link.link_id.clone(),
            state: api_state(view.state),
            local_cwd: info.cwd.clone(),
        });
        info.cwd = view.link.cwd.clone().or_else(|| view.link.cwd_hint.clone());
        info.foreground_cwd = view.link.foreground_cwd.clone();
    }

    pub(crate) fn scrub_ssh_pane_snapshot(&self, snapshot: &mut SessionSnapshot) {
        let linked: HashSet<_> = self
            .ssh_panes
            .links(Instant::now())
            .into_iter()
            .map(|view| &view.link.terminal_id)
            .collect();
        if linked.is_empty() {
            return;
        }
        for workspace in &mut snapshot.workspaces {
            let Some(current) = self
                .state
                .workspaces
                .iter()
                .find(|ws| Some(&ws.id) == workspace.id.as_ref())
            else {
                continue;
            };
            for tab in &mut workspace.tabs {
                for (raw, pane) in &mut tab.panes {
                    let terminal = current.terminal_id(crate::layout::PaneId::from_raw(*raw));
                    if terminal.is_some_and(|id| linked.contains(id)) {
                        // Remote session IDs and managed-agent fallback must never type a
                        // local resume. Connector launch_argv remains the reconnect recipe.
                        pane.agent_session = None;
                        pane.agent_resume = None;
                        pane.managed_agent_kind = None;
                        if pane
                            .launch_argv
                            .as_ref()
                            .is_none_or(|argv| !crate::ssh_panes::connector_argv(argv))
                        {
                            pane.launch_argv = None;
                        }
                    }
                }
            }
        }
    }
}

#[derive(PartialEq, Eq)]
struct Projection {
    cwd: Option<String>,
    foreground_cwd: Option<String>,
    ssh: Option<PaneSshInfo>,
    agent: Option<String>,
}
impl From<PaneInfo> for Projection {
    fn from(pane: PaneInfo) -> Self {
        Self {
            cwd: pane.cwd,
            foreground_cwd: pane.foreground_cwd,
            ssh: pane.ssh,
            agent: pane.agent,
        }
    }
}

fn api_state(state: LinkState) -> SshLinkState {
    match state {
        LinkState::Live => SshLinkState::Live,
        LinkState::Stale => SshLinkState::Stale,
        LinkState::Gone => SshLinkState::Unknown,
    }
}
fn elapsed_ms(now: Instant, at: Instant) -> u64 {
    now.saturating_duration_since(at)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
fn ssh_error(id: String, error: SshPanesError) -> String {
    let (code, message) = match error {
        SshPanesError::Disabled => ("ssh_panes_disabled", crate::ssh_panes::disabled_message()),
        SshPanesError::HostNotAllowed => (
            "ssh_panes_host_not_allowed",
            "SSH host is not in ssh_panes.allowed_hosts".into(),
        ),
        SshPanesError::PaneLinked => (
            "ssh_panes_pane_linked",
            "another live SSH link holds this pane".into(),
        ),
        SshPanesError::UnknownLink => (
            "unknown_link",
            "SSH link is unknown or expired; attach again".into(),
        ),
        SshPanesError::InvalidRequest => (
            "invalid_ssh_panes_request",
            "invalid SSH link identity, process IDs, or protocol (expected 1)".into(),
        ),
    };
    encode_error(id, code, message)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::{
        api::schema::PaneSplitParams,
        config::Config,
        ssh_panes::api::{DetachParams, ListParams},
    };

    use super::*;

    fn test_app() -> App {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            rx,
            crate::api::EventHub::default(),
        )
    }

    #[test]
    fn ssh_panes_unlinked_and_disabled_hooks_preserve_stock_json_bytes() {
        let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
        let mut app = test_app();
        let mut pane: PaneInfo = serde_json::from_value(serde_json::json!({
            "pane_id":"w1:p2", "terminal_id":"term", "workspace_id":"w1", "tab_id":"w1:t1", "focused":false,
            "cwd":"/local", "foreground_cwd":"/local/child", "agent_status":"idle", "revision":1
        })).unwrap();
        let original = serde_json::to_vec(&pane).unwrap();
        let mut snapshot = crate::persist::capture(
            &app.state.workspaces,
            &app.state.terminals,
            &app.terminal_runtimes,
            app.state.active,
            app.state.selected,
        );
        let original_snapshot = serde_json::to_vec(&snapshot).unwrap();
        let terminal = TerminalId::alloc();
        let split: PaneSplitParams =
            serde_json::from_value(serde_json::json!({"direction":"right"})).unwrap();
        for enabled in [false, true] {
            app.apply_ssh_panes_config(&SshPanesConfig {
                enabled,
                ..Default::default()
            });
            assert!(app
                .ssh_panes_split("id", 0, crate::layout::PaneId::alloc(), &split, 24, 80)
                .is_none());
            assert!(app
                .ssh_panes_tab_create("id", 0, None, &HashMap::new(), true, None)
                .is_none());
            app.project_ssh_pane_info(&terminal, &mut pane);
            app.scrub_ssh_pane_snapshot(&mut snapshot);
            assert_eq!(serde_json::to_vec(&snapshot).unwrap(), original_snapshot);
            assert_eq!(serde_json::to_vec(&pane).unwrap(), original);
            assert!(!serde_json::to_value(&pane)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("ssh"));
            let request = SshPanesRequest::List(ListParams::default());
            assert!(!crate::api::request_changes_ui(
                &crate::api::schema::Request {
                    id: "id".into(),
                    method: crate::api::schema::Method::SshPanes(request.clone())
                }
            ));
            let response: serde_json::Value =
                serde_json::from_str(&app.handle_ssh_panes("id".into(), request)).unwrap();
            assert_eq!(
                response["result"],
                serde_json::json!({"type":"ssh_panes", "ssh_panes":{"result":"links", "enabled":enabled, "links":[]}})
            );
            let response: serde_json::Value = serde_json::from_str(&app.handle_ssh_panes(
                "id".into(),
                SshPanesRequest::Detach(DetachParams {
                    link_id: "missing".into(),
                    exit_code: None,
                }),
            ))
            .unwrap();
            if enabled {
                assert_eq!(response["result"]["ssh_panes"]["result"], "detached");
            } else {
                assert_eq!(response["error"]["code"], "ssh_panes_disabled");
            }
        }
    }

    #[test]
    fn ssh_panes_invalid_live_section_keeps_last_good_config() {
        let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
        let mut app = test_app();
        app.apply_ssh_panes_config(&SshPanesConfig {
            enabled: true,
            ..Default::default()
        });
        app.apply_live_config(&Config::default(), &[], &["ssh_panes".into()], false);
        assert!(app.ssh_panes.enabled());
        app.apply_live_config(&Config::default(), &[], &[], false);
        assert!(!app.ssh_panes.enabled());
    }
}

#[cfg(test)]
#[path = "ssh_panes_tests.rs"]
mod lifecycle_tests;
