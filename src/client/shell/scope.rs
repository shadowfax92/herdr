//! The client's side of focus and hide (`crate::scope`).
//!
//! The scope comes only from the Local endpoint's `scope.projection` and
//! applies to every endpoint's rows, keys and notifications. Remote servers'
//! scopes are never read: the owner's local server is the one place focus and
//! hide live.

use std::borrow::Cow;

use crate::scope::{ScopePickMode, ScopeState};

use super::*;

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

    /// Takes the Local endpoint's latest projection: `None` when Local has
    /// none, or lost the capability on reconnect.
    pub(crate) fn set_scope_projection(&mut self, scope: Option<ScopeState>) {
        if self.scope_projection == scope {
            return;
        }
        self.scope_projection = scope;
        self.scope_changed();
    }

    /// Runs after every scope change, before the next frame.
    fn scope_changed(&mut self) {}

    /// Opens the focus/hide picker, from a keybind, a menu, or a plugin's
    /// `scope.pick` (`super::scope_picker` owns the overlay).
    pub(crate) fn open_scope_picker(&mut self, mode: ScopePickMode) {
        self.open_scope_picker_overlay(mode);
    }

    /// Sends `scope.set` to the Local server, even while a remote machine is
    /// active: the scope lives only there. The new scope arrives back as a
    /// projection; nothing changes locally until it does.
    ///
    /// The request is queued here and leaves with the client loop's next
    /// action dispatch (`take_scope_requests`), which every input path runs.
    /// `dispatch_client_shell_actions` lets it onto Local's lane while another
    /// endpoint is active, and the Local server answers it with its surface
    /// off. It is not tracked as a pending request: success needs no follow-up
    /// beyond the projection, and a result arriving while another machine is
    /// active must not read as an interrupted action.
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
        self.scope_requests.push(ClientShellAction::Endpoint {
            endpoint_id: local,
            boot_id,
            request: Box::new(crate::api::schema::Request {
                id: request_id,
                method,
            }),
        });
    }

    /// Hands queued `scope.set` requests to the client loop's dispatcher.
    pub(crate) fn take_scope_requests(&mut self) -> Vec<ClientShellAction> {
        std::mem::take(&mut self.scope_requests)
    }
}
