//! App/API adapter for server-owned SSH links. Projection is intentionally separate
//! from terminal runtime cwd so remote paths cannot leak into local persistence or Git.

use crate::{
    api::schema::{PaneInfo, ResponseResult},
    app::App,
    persist::SessionSnapshot,
    ssh_panes::{
        api::{SshPanesRequest, SshPanesResult},
        config::SshPanesConfig,
    },
    terminal::TerminalId,
};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_ssh_panes(&mut self, id: String, params: SshPanesRequest) -> String {
        if let SshPanesRequest::List(_) = params {
            return encode_success(
                id,
                ResponseResult::SshPanes {
                    ssh_panes: SshPanesResult::Links {
                        enabled: self.ssh_panes.enabled(),
                        links: Vec::new(),
                    },
                },
            );
        }
        if !self.ssh_panes.enabled() {
            return encode_error(
                id,
                "ssh_panes_disabled",
                crate::ssh_panes::disabled_message(),
            );
        }
        encode_error(
            id,
            "ssh_panes_not_implemented",
            crate::ssh_panes::NOT_IMPLEMENTED_MESSAGE,
        )
    }

    pub(crate) fn apply_ssh_panes_config(&mut self, cfg: &SshPanesConfig) {
        if self.ssh_panes.set_config(cfg).disabled_now {
            crate::pane::ssh_probe::clear_all();
        }
    }

    pub(crate) fn project_ssh_pane_info(&self, _terminal: &TerminalId, _info: &mut PaneInfo) {}

    pub(crate) fn scrub_ssh_pane_snapshot(&self, _snapshot: &mut SessionSnapshot) {}
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
    fn ssh_panes_api_and_hooks_are_inert_in_both_config_modes() {
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
            assert_eq!(
                response["error"]["code"],
                if enabled {
                    "ssh_panes_not_implemented"
                } else {
                    "ssh_panes_disabled"
                }
            );
        }
    }

    #[test]
    fn ssh_panes_invalid_live_section_keeps_last_good_config() {
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
