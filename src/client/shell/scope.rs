//! The client's side of focus and hide (`crate::scope`).
//!
//! The scope comes only from the Local endpoint's `scope.projection` and
//! applies to every endpoint's rows, keys and notifications. Remote servers'
//! scopes are never read: the owner's local server is the one place focus and
//! hide live.
// Skeleton: the projection decoder, the surface gates and the picker call
// these as their tickets land.
#![allow(dead_code)]

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
    pub(crate) fn request_scope_set(&mut self, _params: crate::scope::ScopeSetParams) {}
}
