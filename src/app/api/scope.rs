//! `scope.get`, `scope.set` and `scope.clear`: the local server's half of focus
//! and hide (`crate::scope`).
//!
//! The server owns `AppState.scope`; these handlers read and replace it, and the
//! headless render loop projects every change to attached clients as the
//! `scope.projection` endpoint control. `scope.pick` never reaches here from a
//! running server: the headless server sends it to the foreground client
//! itself, so this layer only answers it when no client can exist.

use crate::api::schema::ResponseResult;
use crate::app::App;
use crate::scope::{
    ScopeLocalReport, ScopeLocalTab, ScopeMachineRef, ScopeSetParams, ScopeTabRef, ScopeTarget,
    ScopeWorkspaceRef, ANY_MACHINE,
};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_scope_get(&self, id: String) -> String {
        encode_success(id, self.scope_result())
    }

    /// Each field given replaces that part of the scope outright (never a
    /// flip), so a caller that timed out can safely send the same set again.
    pub(super) fn handle_scope_set(&mut self, id: String, mut params: ScopeSetParams) -> String {
        for targets in [params.focus_targets.as_mut(), params.hidden.as_mut()]
            .into_iter()
            .flatten()
        {
            for target in targets {
                self.resolve_local_scope_target(target);
            }
        }
        self.state.scope.apply_set(params);
        encode_success(id, self.scope_result())
    }

    /// Clearing needs the config's hide list, which only the server knows for
    /// sure (the caller's config may differ), hence its own method instead of
    /// a computed `scope.set`.
    pub(super) fn handle_scope_clear(&mut self, id: String) -> String {
        let config_labels = self.state.sidebar_agents.hidden_workspaces.clone();
        self.state.scope.clear(&config_labels);
        encode_success(id, self.scope_result())
    }

    /// Answered only in app mode, where no shell client can be attached.
    pub(super) fn handle_scope_pick_without_client(id: String) -> String {
        encode_error(
            id,
            "no_client",
            "no client is attached to open the scope picker",
        )
    }

    fn scope_result(&self) -> ResponseResult {
        ResponseResult::Scope {
            scope: self.state.scope.clone(),
            local: self.scope_local_report(),
        }
    }

    /// The server's verdict for every local tab, judged by the same predicate
    /// the client uses, with this server as machine `local` / `Local`. Labels
    /// are the ones clients see in their snapshots, so a label target means the
    /// same thing here and in the panels.
    pub(crate) fn scope_local_report(&self) -> ScopeLocalReport {
        let scope = &self.state.scope;
        let mut tabs = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            let label = ws.display_name_from(&self.state.terminals, &self.terminal_runtimes);
            let workspace = ScopeWorkspaceRef {
                machine: ScopeMachineRef::LOCAL,
                id: &ws.id,
                label: &label,
            };
            for tab_idx in 0..ws.tabs.len() {
                let (Some(tab_id), Some(tab_label)) = (
                    self.public_tab_id(ws_idx, tab_idx),
                    ws.tab_display_name(tab_idx),
                ) else {
                    continue;
                };
                let admitted = scope.admits_tab(ScopeTabRef {
                    workspace,
                    id: &tab_id,
                    label: &tab_label,
                });
                tabs.push(ScopeLocalTab {
                    workspace_id: ws.id.clone(),
                    tab_id,
                    admitted,
                });
            }
        }
        ScopeLocalReport { tabs }
    }

    /// Pins a local label target to the live workspace (and tab) it names, when
    /// exactly one carries that label; an ambiguous or unknown label stays a
    /// label so it can match later. Ids survive renames, which is what a
    /// picked target means. Remote and `*` targets are left alone: only their
    /// own client knows their ids. Missing labels are filled from the live
    /// workspace for display.
    fn resolve_local_scope_target(&self, target: &mut ScopeTarget) {
        if target.machine == ANY_MACHINE || !target.covers_machine(ScopeMachineRef::LOCAL) {
            return;
        }
        let workspaces = &self.state.workspaces;
        let ws_idx = match (&target.workspace_id, &target.workspace_label) {
            (Some(id), _) => workspaces.iter().position(|ws| &ws.id == id),
            (None, Some(label)) => unique_position(workspaces.iter().map(|ws| {
                ws.display_name_from(&self.state.terminals, &self.terminal_runtimes)
                    == label.as_str()
            })),
            (None, None) => return,
        };
        let Some(ws_idx) = ws_idx else {
            return;
        };
        let ws = &workspaces[ws_idx];
        if target.workspace_id.is_none() {
            target.workspace_id = Some(ws.id.clone());
        }
        if target.workspace_label.is_none() {
            target.workspace_label =
                Some(ws.display_name_from(&self.state.terminals, &self.terminal_runtimes));
        }
        let tab_idx = match (&target.tab_id, &target.tab_label) {
            (Some(id), _) => {
                (0..ws.tabs.len()).find(|&idx| self.public_tab_id(ws_idx, idx).as_ref() == Some(id))
            }
            (None, Some(label)) => unique_position(
                (0..ws.tabs.len())
                    .map(|idx| ws.tab_display_name(idx).as_deref() == Some(label.as_str())),
            ),
            (None, None) => return,
        };
        let Some(tab_idx) = tab_idx else {
            return;
        };
        if target.tab_id.is_none() {
            target.tab_id = self.public_tab_id(ws_idx, tab_idx);
        }
        if target.tab_label.is_none() {
            target.tab_label = ws.tab_display_name(tab_idx);
        }
    }
}

/// The index of the only `true`, or `None` when there are none or several.
fn unique_position(matches: impl Iterator<Item = bool>) -> Option<usize> {
    let mut found = None;
    for (idx, matched) in matches.enumerate() {
        if matched {
            if found.is_some() {
                return None;
            }
            found = Some(idx);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EmptyParams, Method, Request};
    use crate::scope::{ScopeFocus, ScopeState, LOCAL_MACHINE};
    use crate::workspace::Workspace;

    fn app_with_config_hides(hidden: &[&str]) -> App {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.agents.hidden_workspaces = hidden.iter().map(|s| s.to_string()).collect();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &config,
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    /// `setup` (tabs `herdr`, `scratch`), `admin`, and two workspaces both
    /// labelled `dup`.
    fn app_with_workspaces() -> App {
        let mut app = app_with_config_hides(&["ft"]);
        let mut setup = Workspace::test_new("setup");
        setup.tabs[0].custom_name = Some("herdr".into());
        setup.test_add_tab(Some("scratch"));
        app.state.workspaces = vec![
            setup,
            Workspace::test_new("admin"),
            Workspace::test_new("dup"),
            Workspace::test_new("dup"),
        ];
        app
    }

    fn request(app: &mut App, method: Method) -> serde_json::Value {
        let response = app.handle_api_request(Request {
            id: "scope".into(),
            method,
        });
        serde_json::from_str(&response).unwrap()
    }

    fn set(app: &mut App, params: ScopeSetParams) -> serde_json::Value {
        request(app, Method::ScopeSet(params))
    }

    fn target(text: &str) -> ScopeTarget {
        ScopeTarget::parse(text).unwrap()
    }

    /// `(workspace label, tab label) -> admitted`, from a `scope` response.
    fn verdicts(app: &App, response: &serde_json::Value) -> Vec<(String, String, bool)> {
        assert_eq!(response["result"]["type"], "scope", "{response}");
        response["result"]["local"]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tab| {
                let workspace_id = tab["workspace_id"].as_str().unwrap();
                let tab_id = tab["tab_id"].as_str().unwrap();
                let ws_idx = app.parse_workspace_id(workspace_id).unwrap();
                let ws = &app.state.workspaces[ws_idx];
                let tab_idx = (0..ws.tabs.len())
                    .find(|&idx| app.public_tab_id(ws_idx, idx).as_deref() == Some(tab_id))
                    .unwrap();
                (
                    ws.custom_name.clone().unwrap(),
                    ws.tab_display_name(tab_idx).unwrap(),
                    tab["admitted"].as_bool().unwrap(),
                )
            })
            .collect()
    }

    fn admitted(verdicts: &[(String, String, bool)]) -> Vec<(&str, &str)> {
        verdicts
            .iter()
            .filter(|(_, _, admitted)| *admitted)
            .map(|(workspace, tab, _)| (workspace.as_str(), tab.as_str()))
            .collect()
    }

    #[test]
    fn boot_seeds_config_hides_as_any_machine_targets() {
        let app = app_with_config_hides(&["ft", "bg"]);
        assert_eq!(
            app.state.scope,
            ScopeState::from_config_hidden(&["ft".into(), "bg".into()])
        );
        assert!(app.state.scope.hidden.iter().all(|target| target.config));
    }

    #[test]
    fn config_reload_applies_only_the_hidden_list_diff() {
        let mut app = app_with_config_hides(&["ft", "old"]);
        // The owner un-hides ft at runtime and hides admin by hand.
        let hidden = vec![
            ScopeTarget::parse("*:old")
                .map(|t| ScopeTarget { config: true, ..t })
                .unwrap(),
            target("admin"),
        ];
        set(
            &mut app,
            ScopeSetParams {
                hidden: Some(hidden),
                ..ScopeSetParams::default()
            },
        );

        let mut config = crate::config::Config::default();
        config.ui.sidebar.agents.hidden_workspaces = vec!["ft".into(), "new".into()];
        app.apply_live_config(&config, &[], &[], false);

        let hidden: Vec<_> = app
            .state
            .scope
            .hidden
            .iter()
            .map(|target| (target.display(), target.config))
            .collect();
        // `old` left the config, `new` joined it, and `ft` (in both lists)
        // stays un-hidden as the owner left it.
        assert_eq!(
            hidden,
            [
                ("local:admin".to_string(), false),
                ("*:new".to_string(), true)
            ]
        );
    }

    #[test]
    fn get_judges_every_local_tab_with_the_shared_predicate() {
        let mut app = app_with_workspaces();
        let response = request(&mut app, Method::ScopeGet(EmptyParams::default()));
        // Only `ft` is hidden (by config), and no local workspace has that label.
        assert_eq!(admitted(&verdicts(&app, &response)).len(), 5);
        assert_eq!(
            response["result"]["scope"]["hidden"][0],
            serde_json::json!({"machine": "*", "workspace_label": "ft", "config": true})
        );

        set(
            &mut app,
            ScopeSetParams {
                focus_targets: Some(vec![target("setup")]),
                focus_active: Some(true),
                hidden: Some(vec![target("setup/scratch"), target("dup")]),
            },
        );
        let response = request(&mut app, Method::ScopeGet(EmptyParams::default()));
        // The more specific hide of `setup/scratch` beats the focus on `setup`.
        assert_eq!(admitted(&verdicts(&app, &response)), [("setup", "herdr")]);
    }

    #[test]
    fn set_pins_unique_local_labels_to_ids_and_leaves_the_rest_alone() {
        let mut app = app_with_workspaces();
        let setup_id = app.state.workspaces[0].id.clone();
        let scratch_id = app.public_tab_id(0, 1).unwrap();
        let response = set(
            &mut app,
            ScopeSetParams {
                focus_targets: Some(vec![
                    target("setup/scratch"),
                    target("dup"),
                    target("missing"),
                    target("studio:main"),
                ]),
                focus_active: None,
                hidden: Some(vec![target("*:admin"), target("Local:admin")]),
            },
        );

        let focus = &app.state.scope.focus.targets;
        assert_eq!(focus[0].workspace_id.as_deref(), Some(setup_id.as_str()));
        assert_eq!(focus[0].tab_id.as_deref(), Some(scratch_id.as_str()));
        assert_eq!(focus[0].workspace_label.as_deref(), Some("setup"));
        assert_eq!(focus[0].tab_label.as_deref(), Some("scratch"));
        // Ambiguous and unknown labels stay labels so they can match later.
        assert_eq!(focus[1], target("dup"));
        assert_eq!(focus[2], target("missing"));
        // Remote and every-machine targets are never resolved here.
        assert_eq!(focus[3], target("studio:main"));
        assert_eq!(app.state.scope.hidden[0], target("*:admin"));
        let admin_id = app.state.workspaces[1].id.clone();
        assert_eq!(
            app.state.scope.hidden[1].workspace_id.as_deref(),
            Some(admin_id.as_str())
        );
        assert_eq!(app.state.scope.hidden[1].machine, "Local");

        // `set` answers with the same report as `get`, already reflecting the
        // change (focus stays off, so only the hides apply).
        let get = request(&mut app, Method::ScopeGet(EmptyParams::default()));
        assert_eq!(response, get);
        assert!(verdicts(&app, &response)
            .iter()
            .all(|(workspace, _, admitted)| *admitted == (workspace != "admin")));
    }

    #[test]
    fn a_pinned_id_survives_a_rename_and_fills_a_missing_label() {
        let mut app = app_with_workspaces();
        let setup_id = app.state.workspaces[0].id.clone();
        set(
            &mut app,
            ScopeSetParams {
                focus_targets: Some(vec![ScopeTarget {
                    machine: LOCAL_MACHINE.into(),
                    workspace_id: Some(setup_id.clone()),
                    ..ScopeTarget::default()
                }]),
                focus_active: Some(true),
                hidden: None,
            },
        );
        assert_eq!(
            app.state.scope.focus.targets[0].workspace_label.as_deref(),
            Some("setup")
        );
        app.state.workspaces[0].custom_name = Some("renamed".into());
        let response = request(&mut app, Method::ScopeGet(EmptyParams::default()));
        assert_eq!(
            admitted(&verdicts(&app, &response)),
            [("renamed", "herdr"), ("renamed", "scratch")]
        );
    }

    #[test]
    fn clear_restores_the_current_config_hides() {
        let mut app = app_with_workspaces();
        set(
            &mut app,
            ScopeSetParams {
                focus_targets: Some(vec![target("setup")]),
                focus_active: Some(true),
                hidden: Some(vec![target("admin")]),
            },
        );
        let response = request(&mut app, Method::ScopeClear(EmptyParams::default()));
        assert_eq!(
            app.state.scope,
            ScopeState {
                focus: ScopeFocus::default(),
                hidden: ScopeState::from_config_hidden(&["ft".into()]).hidden,
            }
        );
        assert_eq!(admitted(&verdicts(&app, &response)).len(), 5);
    }

    #[test]
    fn pick_without_a_server_reports_no_client() {
        let mut app = app_with_workspaces();
        let response = request(
            &mut app,
            Method::ScopePick(crate::scope::ScopePickParams::default()),
        );
        assert_eq!(response["error"]["code"], "no_client");
    }

    #[test]
    fn set_and_clear_change_the_ui_but_get_and_pick_do_not() {
        let request = |method| Request {
            id: "scope".into(),
            method,
        };
        assert!(crate::api::request_changes_ui(&request(Method::ScopeSet(
            ScopeSetParams::default()
        ))));
        assert!(crate::api::request_changes_ui(&request(
            Method::ScopeClear(EmptyParams::default())
        )));
        assert!(!crate::api::request_changes_ui(&request(Method::ScopeGet(
            EmptyParams::default()
        ))));
        assert!(!crate::api::request_changes_ui(&request(
            Method::ScopePick(crate::scope::ScopePickParams::default())
        )));
    }
}
