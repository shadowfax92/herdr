//! The client's side of focus and hide (`crate::scope`).
//!
//! The scope comes only from the Local endpoint's `scope.projection` and
//! applies to every endpoint's rows, keys and notifications. Remote servers'
//! scopes are never read: the owner's local server is the one place focus and
//! hide live. The client hands its scope to each remote server instead
//! (`super::scope_mirror`), so plugins there filter as the panels do.
//!
//! Surfaces ask in one of two ways:
//!
//! ```text
//!  effective_scope() ──► scope_admits_tab / scope_shows_workspace / scope_shows_machine
//!         │                one question: a notification, LastWorkspace
//!         └──────────────► ScopeView::new(scope, endpoints)   once per frame or key action
//!                            └─ EndpointScope per machine: verdicts by workspace and tab id
//!                               rows, agent keys, workspace keys, navigator, status rollups
//! ```
//!
//! Row loops run per render × workspaces × machines, so they never run the
//! predicates themselves: an `EndpointScope` judges each tab once and rows look
//! verdicts up. A scope that hides nothing on a machine (the usual case)
//! builds no verdicts at all and every gate answers yes.
//!
//! The scope is presentation only. Snapshots keep every agent's real status
//! (`endpoint_agent_state` stays unscoped, so unread marks survive a focus),
//! and explicit jumps, the tab bar and the bell ignore it.
use std::borrow::Cow;

use crate::api::schema::AgentStatus;
use crate::protocol::ClientShellAgent;
use crate::scope::{ScopeMachineRef, ScopePickMode, ScopeState, ScopeTabRef, ScopeWorkspaceRef};

use super::*;

/// One endpoint's snapshot judged against the scope once, so per-row gates are
/// lookups. Borrows ids from the snapshot it was built from; build a fresh one
/// per frame or key action rather than keeping it across snapshot updates.
#[derive(Debug, Default)]
pub(crate) struct EndpointScope<'a> {
    /// `None` while nothing on this machine can be out of scope: every gate is
    /// a constant yes, with no lookups and no allocation.
    restricted: Option<RestrictedEndpoint<'a>>,
}

#[derive(Debug)]
struct RestrictedEndpoint<'a> {
    machine_shown: bool,
    /// The verdict for a workspace id the snapshot does not list.
    unlisted_workspace: bool,
    workspaces: HashMap<&'a str, WorkspaceScope<'a>>,
}

#[derive(Debug)]
struct WorkspaceScope<'a> {
    shown: bool,
    /// The workspace judged as a whole, for a tab the snapshot does not list
    /// (an agent reported before its tab).
    whole: bool,
    tabs: Vec<(&'a str, bool)>,
    /// The status rollup over admitted tabs, so a workspace never lights up
    /// for an out-of-scope tab. `None` when every tab is admitted and the
    /// snapshot's own rollup stands.
    status: Option<AgentStatus>,
}

/// Shared by every endpoint a `ScopeView` has no verdicts for.
static ADMITS_EVERYTHING: EndpointScope<'static> = EndpointScope { restricted: None };

impl<'a> EndpointScope<'a> {
    /// Judges every workspace and tab of one machine's snapshot. A machine with
    /// no snapshot (connecting, offline) gets only its header verdict.
    pub(super) fn new(
        scope: &ScopeState,
        endpoint_id: &ClientEndpointId,
        label: &str,
        snapshot: Option<&'a ClientShellSnapshot>,
    ) -> Self {
        let key = endpoint_id.storage_key();
        let machine = ScopeMachineRef { key: &key, label };
        if !scope.focus_engaged()
            && !scope
                .hidden
                .iter()
                .any(|target| target.covers_machine(machine))
        {
            return Self::default();
        }
        let mut workspaces = HashMap::new();
        let mut any_shown = false;
        if let Some(snapshot) = snapshot {
            let mut tabs_by_workspace = HashMap::<&str, Vec<&ClientShellTab>>::new();
            for tab in &snapshot.tabs {
                tabs_by_workspace
                    .entry(tab.workspace_id.as_str())
                    .or_default()
                    .push(tab);
            }
            for workspace in &snapshot.workspaces {
                let workspace_ref = ScopeWorkspaceRef {
                    machine,
                    id: &workspace.workspace_id,
                    label: &workspace.label,
                };
                let tabs = tabs_by_workspace
                    .get(workspace.workspace_id.as_str())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let tab_ref = |tab: &&'a ClientShellTab| ScopeTabRef {
                    workspace: workspace_ref,
                    id: &tab.tab_id,
                    label: &tab.label,
                };
                let verdicts = tabs
                    .iter()
                    .map(|tab| (tab.tab_id.as_str(), scope.admits_tab(tab_ref(tab))))
                    .collect::<Vec<_>>();
                let shown = scope.shows_workspace(workspace_ref, tabs.iter().map(tab_ref));
                let status = verdicts.iter().any(|(_, admitted)| !admitted).then(|| {
                    tabs.iter()
                        .zip(&verdicts)
                        .filter(|(_, (_, admitted))| *admitted)
                        .map(|(tab, _)| tab.agent_status)
                        .max_by_key(|status| super::status_priority(*status))
                        .unwrap_or(AgentStatus::Unknown)
                });
                any_shown |= shown;
                workspaces.insert(
                    workspace.workspace_id.as_str(),
                    WorkspaceScope {
                        shown,
                        whole: scope.shows_workspace(workspace_ref, []),
                        tabs: verdicts,
                        status,
                    },
                );
            }
        }
        let unlisted = ScopeWorkspaceRef {
            machine,
            id: "",
            label: "",
        };
        Self {
            restricted: Some(RestrictedEndpoint {
                machine_shown: scope.shows_machine(machine, any_shown),
                unlisted_workspace: scope.shows_workspace(unlisted, []),
                workspaces,
            }),
        }
    }

    pub(super) fn admits_everything(&self) -> bool {
        self.restricted.is_none()
    }

    /// Whether the machine's header row shows.
    pub(super) fn shows_machine(&self) -> bool {
        self.restricted
            .as_ref()
            .is_none_or(|restricted| restricted.machine_shown)
    }

    /// Whether the workspace's row shows: any of its tabs is admitted.
    pub(super) fn shows_workspace(&self, workspace_id: &str) -> bool {
        self.restricted.as_ref().is_none_or(|restricted| {
            restricted
                .workspaces
                .get(workspace_id)
                .map_or(restricted.unlisted_workspace, |workspace| workspace.shown)
        })
    }

    pub(super) fn admits_tab(&self, workspace_id: &str, tab_id: &str) -> bool {
        self.restricted.as_ref().is_none_or(|restricted| {
            restricted.workspaces.get(workspace_id).map_or(
                restricted.unlisted_workspace,
                |workspace| {
                    workspace
                        .tabs
                        .iter()
                        .find(|(id, _)| *id == tab_id)
                        .map_or(workspace.whole, |(_, admitted)| *admitted)
                },
            )
        })
    }

    /// An agent is in scope exactly when its tab is.
    pub(super) fn admits_agent(&self, agent: &ClientShellAgent) -> bool {
        self.admits_tab(&agent.workspace_id, &agent.tab_id)
    }

    /// The workspace's status counting only admitted tabs.
    pub(super) fn workspace_status(&self, workspace: &ClientShellWorkspace) -> AgentStatus {
        self.restricted
            .as_ref()
            .and_then(|restricted| restricted.workspaces.get(workspace.workspace_id.as_str()))
            .and_then(|scoped| scoped.status)
            .unwrap_or(workspace.agent_status)
    }
}

/// Every endpoint judged once, for one frame or one key action.
#[derive(Debug, Default)]
pub(crate) struct ScopeView<'a> {
    /// Empty when the scope hides and focuses nothing, the common case.
    endpoints: Vec<(&'a ClientEndpointId, EndpointScope<'a>)>,
}

impl<'a> ScopeView<'a> {
    /// Takes the scope by reference only while judging: the view borrows the
    /// endpoints alone, so callers can hold it beside `&mut` shell fields.
    pub(super) fn new(scope: &ScopeState, endpoints: &'a [ClientShellEndpoint]) -> Self {
        if !scope.focus_engaged() && scope.hidden.is_empty() {
            return Self::default();
        }
        Self {
            endpoints: endpoints
                .iter()
                .map(|endpoint| {
                    (
                        &endpoint.endpoint_id,
                        EndpointScope::new(
                            scope,
                            &endpoint.endpoint_id,
                            &endpoint.label,
                            endpoint.snapshot.as_deref(),
                        ),
                    )
                })
                .collect(),
        }
    }

    /// One endpoint's verdicts. An endpoint the view was not built with is
    /// unfiltered.
    pub(super) fn endpoint(&self, endpoint_id: &ClientEndpointId) -> &EndpointScope<'a> {
        self.endpoints
            .iter()
            .find(|(id, _)| *id == endpoint_id)
            .map_or(&ADMITS_EVERYTHING, |(_, scope)| scope)
    }
}

impl ClientShellState {
    /// The scope every surface filters by. A Local server without the
    /// `scope.projection` capability leaves the config's hidden workspaces in
    /// force, as before this feature.
    pub(crate) fn effective_scope(&self) -> Cow<'_, ScopeState> {
        match &self.scope_projection {
            Some(scope) => Cow::Borrowed(scope),
            None => Cow::Owned(ScopeState::from_config_hidden(
                &self.config.agents.hidden_workspaces,
            )),
        }
    }

    /// Every endpoint judged against the effective scope. Build it once per
    /// frame or key action; it borrows only `self.endpoints`.
    pub(super) fn scope_view(&self) -> ScopeView<'_> {
        ScopeView::new(&self.effective_scope(), &self.endpoints)
    }

    /// The active endpoint judged against the effective scope, over `snapshot`
    /// (normally `self.snapshot`, the active endpoint's mirror).
    pub(super) fn active_endpoint_scope<'a>(
        &self,
        snapshot: &'a ClientShellSnapshot,
    ) -> EndpointScope<'a> {
        EndpointScope::new(
            &self.effective_scope(),
            &self.active_endpoint_id,
            self.endpoint_label(&self.active_endpoint_id),
            Some(snapshot),
        )
    }

    /// One tab of `endpoint_id`'s snapshot, judged on its own. For a single
    /// question; loops use a `ScopeView`.
    #[cfg(test)]
    pub(super) fn scope_admits_tab(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace: &ClientShellWorkspace,
        tab: &ClientShellTab,
    ) -> bool {
        let key = endpoint_id.storage_key();
        let workspace = ScopeWorkspaceRef {
            machine: ScopeMachineRef {
                key: &key,
                label: self.endpoint_label(endpoint_id),
            },
            id: &workspace.workspace_id,
            label: &workspace.label,
        };
        self.effective_scope().admits_tab(ScopeTabRef {
            workspace,
            id: &tab.tab_id,
            label: &tab.label,
        })
    }

    /// Whether `workspace`'s row shows: any of its tabs in `snapshot` is
    /// admitted.
    pub(super) fn scope_shows_workspace(
        &self,
        endpoint_id: &ClientEndpointId,
        snapshot: &ClientShellSnapshot,
        workspace: &ClientShellWorkspace,
    ) -> bool {
        let key = endpoint_id.storage_key();
        let workspace_ref = ScopeWorkspaceRef {
            machine: ScopeMachineRef {
                key: &key,
                label: self.endpoint_label(endpoint_id),
            },
            id: &workspace.workspace_id,
            label: &workspace.label,
        };
        self.effective_scope().shows_workspace(
            workspace_ref,
            snapshot
                .tabs
                .iter()
                .filter(|tab| tab.workspace_id == workspace.workspace_id)
                .map(|tab| ScopeTabRef {
                    workspace: workspace_ref,
                    id: &tab.tab_id,
                    label: &tab.label,
                }),
        )
    }

    /// Whether `endpoint_id`'s machine header shows, judged over its cached
    /// snapshot.
    #[cfg(test)]
    pub(super) fn scope_shows_machine(&self, endpoint_id: &ClientEndpointId) -> bool {
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id);
        EndpointScope::new(
            &self.effective_scope(),
            endpoint_id,
            self.endpoint_label(endpoint_id),
            endpoint.and_then(|endpoint| endpoint.snapshot.as_deref()),
        )
        .shows_machine()
    }

    /// Whether a semantic notification is in scope. One that names no
    /// workspace, tab or pane (a plugin's `notification.show`, update ready) is
    /// unscoped and always passes. A target the snapshot does not list yet is
    /// judged by its ids alone.
    pub(super) fn scope_admits_notification(
        &self,
        scope: &ScopeState,
        endpoint_id: &ClientEndpointId,
        event: &SemanticNotification,
    ) -> bool {
        if event.workspace_id.is_none() && event.tab_id.is_none() && event.pane_id.is_none() {
            return true;
        }
        let snapshot = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| endpoint.snapshot.as_deref());
        // Resolve the most specific location the event names: its tab, else
        // the pane's tab, else its workspace.
        let pane = event.pane_id.as_deref().and_then(|pane_id| {
            snapshot.and_then(|snapshot| snapshot.panes.iter().find(|pane| pane.pane_id == pane_id))
        });
        let tab_id = event
            .tab_id
            .as_deref()
            .or_else(|| pane.map(|pane| pane.tab_id.as_str()));
        let tab = tab_id.and_then(|tab_id| {
            snapshot.and_then(|snapshot| {
                snapshot.tabs.iter().find(|tab| {
                    tab.tab_id == tab_id
                        && event
                            .workspace_id
                            .as_deref()
                            .is_none_or(|workspace_id| tab.workspace_id == workspace_id)
                })
            })
        });
        let workspace_id = event
            .workspace_id
            .as_deref()
            .or_else(|| tab.map(|tab| tab.workspace_id.as_str()))
            .or_else(|| pane.map(|pane| pane.workspace_id.as_str()));
        let workspace = workspace_id.and_then(|workspace_id| {
            snapshot.and_then(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
            })
        });
        let key = endpoint_id.storage_key();
        let workspace_ref = ScopeWorkspaceRef {
            machine: ScopeMachineRef {
                key: &key,
                label: self.endpoint_label(endpoint_id),
            },
            id: workspace_id.unwrap_or_default(),
            label: workspace.map_or("", |workspace| workspace.label.as_str()),
        };
        match tab_id {
            Some(tab_id) => scope.admits_tab(ScopeTabRef {
                workspace: workspace_ref,
                id: tab_id,
                label: tab.map_or("", |tab| tab.label.as_str()),
            }),
            None => scope.shows_workspace(workspace_ref, []),
        }
    }

    /// Takes the Local endpoint's latest projection: `None` when Local has
    /// none, or lost the capability on reconnect.
    pub(crate) fn set_scope_projection(&mut self, scope: Option<ScopeState>) {
        // Local has caught up with the focus last asked for, or will never
        // report it (no projection at all): toggles follow the projection again.
        if scope
            .as_ref()
            .is_none_or(|scope| self.scope_focus_pending == Some(scope.focus.active))
        {
            self.scope_focus_pending = None;
        }
        if self.scope_projection == scope {
            return;
        }
        self.scope_projection = scope;
        self.scope_changed();
    }

    /// Runs after every scope change, before the next frame. Rows re-filter on
    /// their own at the next compose; what outlives a frame is cleaned here:
    /// toasts already queued or showing for something now out of scope go.
    /// Pending notifications stay, since they are judged again on delivery.
    fn scope_changed(&mut self) {
        self.retire_out_of_scope_notifications();
        // Remote servers judge their plugins' `scope.get` by this client's
        // scope; hand them the new one.
        self.sync_scope_mirrors();
        // Rows above the scroll position may have left or returned; the next
        // compose clamps both lists and brings the focused workspace back into
        // view instead of leaving the old offset over a shorter list.
        self.reveal_focused_workspace = true;
        self.reveal_mobile_workspace = true;
    }

    /// Opens the focus/hide picker, from a keybind, a menu, or a plugin's
    /// `scope.pick` (`super::scope_picker` owns the overlay).
    pub(crate) fn open_scope_picker(&mut self, mode: ScopePickMode) {
        self.open_scope_picker_overlay(mode);
    }

    /// The focus state a toggle flips from: the value last asked of Local while
    /// its projection has not reported it yet, else the projected one. Two
    /// toggles in quick succession therefore send opposite values.
    pub(super) fn scope_focus_for_toggle(&self) -> bool {
        self.scope_focus_pending
            .unwrap_or_else(|| self.effective_scope().focus.active)
    }

    /// A server's `scope.toggle` (a plugin menu, on whichever machine is
    /// active): the `toggle_focus` keybind's path, run outside a key press. The
    /// returned outcome carries the jump into the new scope; the `scope.set`
    /// itself goes to Local through `request_scope_set`.
    pub(crate) fn toggle_scope_focus_from_endpoint(&mut self) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        self.toggle_scope_focus(&mut outcome);
        outcome
    }

    /// The visible endpoint notice as `title: body`, for client-loop tests.
    #[cfg(test)]
    pub(crate) fn endpoint_notice_text(&self) -> Option<String> {
        self.visible_endpoint_notice
            .as_ref()
            .map(|notice| format!("{}: {}", notice.title, notice.body))
    }

    /// The open picker's mode, so client-loop tests can see a control opened it.
    #[cfg(test)]
    pub(crate) fn scope_picker_mode(&self) -> Option<ScopePickMode> {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::ScopePicker(picker)) => Some(picker.mode),
            _ => None,
        }
    }

    /// Sends `scope.set` to the Local server, even while a remote machine is
    /// active: the scope lives only there. The new scope arrives back as a
    /// projection; nothing changes locally until it does.
    ///
    /// The request is queued here and leaves with the client loop's next
    /// action dispatch (`take_scope_requests`), which every input path runs.
    /// `dispatch_client_shell_actions` lets it onto Local's lane while another
    /// endpoint is active, and the Local server answers it with its surface
    /// off. Its id is tracked apart from `pending_requests` (which belong to the
    /// active endpoint): `finish_scope_request` settles it from any endpoint
    /// state, silently on success and with a notice on any failure.
    pub(crate) fn request_scope_set(&mut self, params: crate::scope::ScopeSetParams) {
        let local = ClientEndpointId::Local;
        let boot_id = self
            .endpoint_is_online(&local)
            .then(|| self.endpoint_boot_id(&local).map(str::to_owned))
            .flatten();
        let Some(boot_id) = boot_id else {
            self.receive_endpoint_unavailable(
                "Local is not ready, so focus and hide did not change".into(),
            );
            return;
        };
        let method = crate::api::schema::Method::ScopeSet(params);
        let method_name = crate::api::api_method_name(&method);
        let supported = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == local)
            .and_then(|endpoint| endpoint.methods.as_ref())
            .is_none_or(|methods| methods.contains(method_name));
        if !supported {
            self.push_endpoint_notice(
                ClientEndpointNoticeKind::Unsupported,
                method_name,
                "Focus and hide unavailable",
                "The local server does not support focus and hide yet. Update and restart it to enable them.",
            );
            return;
        }
        let request_id = format!("client-shell:{}", self.next_request_id);
        self.next_request_id = self.next_request_id.saturating_add(1);
        if let crate::api::schema::Method::ScopeSet(params) = &method {
            if let Some(active) = params.focus_active {
                self.scope_focus_pending = Some(active);
            }
        }
        self.scope_request_ids.insert(request_id.clone());
        self.scope_requests.push(ClientShellAction::Endpoint {
            endpoint_id: local,
            boot_id,
            request: Box::new(crate::api::schema::Request {
                id: request_id,
                method,
            }),
        });
    }

    /// Hands queued `scope.set` requests for Local and `scope.mirror` requests
    /// for remote servers to the client loop's dispatcher.
    pub(crate) fn take_scope_requests(&mut self) -> Vec<ClientShellAction> {
        std::mem::take(&mut self.scope_requests)
    }

    /// Whether `request_id` is an unsettled scope request: a `scope.set` for
    /// Local or a `scope.mirror` for a remote server. Both are settled by
    /// `finish_scope_request` whichever endpoint is active.
    pub(crate) fn owns_scope_request(&self, request_id: &str) -> bool {
        self.scope_request_ids.contains(request_id) || self.owns_scope_mirror_request(request_id)
    }

    /// Settles one of Local's `scope.set` requests, from a response, a timeout,
    /// or a cancellation (lane retirement, disconnect, unsendable). Success says
    /// nothing: the projection shows it. Any failure tells the user that focus
    /// and hide did not change, in the endpoint notice banner, and drops the
    /// pending toggle state so the next toggle flips from what Local reports.
    /// A remote `scope.mirror` settles silently (`finish_scope_mirror`).
    /// Returns whether a frame is needed; `false` for ids it does not own.
    pub(crate) fn finish_scope_request(
        &mut self,
        request_id: &str,
        result: &Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        if let Some(repaint) = self.finish_scope_mirror(request_id, result) {
            return repaint;
        }
        if !self.scope_request_ids.remove(request_id) {
            return false;
        }
        let Err(error) = result else {
            return false;
        };
        self.scope_focus_pending = None;
        let code = error.code.as_deref().unwrap_or("invalid_response");
        let body = match code {
            "endpoint_timeout" => "The local server did not respond.".to_owned(),
            "endpoint_cancelled" => "The request to the local server was interrupted.".to_owned(),
            _ => format!("The local server refused it: {}", error.message),
        };
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            format!("scope.set:{code}"),
            "Focus and hide did not change",
            body,
        )
    }
}
