//! Herdr's focus and hide scope, as plugins read it through `scope.get`.
//!
//! Focus and hide are native to the fork: the local server owns one
//! `ScopeState`, and the client applies it to every machine. A plugin only sees
//! the local server, so it never judges scope itself. `scope.get` returns the
//! server's verdict for each local tab (`local.tabs[].admitted`), and popups
//! that list local panes (find) drop the panes whose tab is out of scope.
//!
//! ```text
//!  find catalog ── session.snapshot ──▶ panes ─┐
//!               ── scope.get ─▶ local.tabs ────┴─▶ ScopeFilter::admits_tab ─▶ rows
//!  menu ─────────── scope.get ─▶ Some / None (whether its scope rows are usable)
//! ```
//!
//! Everything fails open. A Herdr without `scope.get` answers `None` (filter
//! nothing, say nothing), and a tab the report does not name is admitted, so a
//! tab created between the snapshot and the scope read still shows.

use std::collections::HashSet;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::host::{Host, HostError};

/// The socket method that reads the scope.
pub const SCOPE_GET: &str = "scope.get";

/// The server's verdict for one local tab.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ScopeTab {
    pub workspace_id: String,
    pub tab_id: String,
    pub admitted: bool,
}

/// Focus: its targets and whether it is switched on. Targets stay opaque JSON:
/// plugins never interpret them, they only count or forward them.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ScopeFocus {
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub targets: Vec<Value>,
}

/// The whole scope as the server stores it (`scope.get`'s `scope`).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ScopeState {
    #[serde(default)]
    pub focus: ScopeFocus,
    #[serde(default)]
    pub hidden: Vec<Value>,
}

/// `scope.get`'s `local`: one verdict per tab of the local server.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ScopeLocal {
    #[serde(default)]
    pub tabs: Vec<ScopeTab>,
}

/// A `scope.get` (or `scope.set`) reply:
/// `{"type":"scope","scope":{focus,hidden},"local":{"tabs":[…]}}`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ScopeReport {
    #[serde(default)]
    pub scope: ScopeState,
    #[serde(default)]
    pub local: ScopeLocal,
}

impl ScopeReport {
    /// Decodes a `scope.get` / `scope.set` result.
    pub fn from_result(result: Value) -> Result<Self> {
        serde_json::from_value(result).context("Herdr's scope reply is malformed")
    }

    /// Whether focus currently narrows anything: switched on with targets. The
    /// server's predicates ignore an active focus with no targets.
    pub fn focus_engaged(&self) -> bool {
        self.scope.focus.active && !self.scope.focus.targets.is_empty()
    }
}

/// Which local tabs a popup may list. The default admits every tab, which is
/// what a Herdr without scope (or an unreadable scope) gets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeFilter {
    out_of_scope: HashSet<String>,
}

impl ScopeFilter {
    /// A filter from the server's verdicts. Only tabs judged out of scope are
    /// kept, so a tab missing from the report is admitted.
    pub fn new(tabs: impl IntoIterator<Item = ScopeTab>) -> Self {
        Self {
            out_of_scope: tabs
                .into_iter()
                .filter(|tab| !tab.admitted)
                .map(|tab| tab.tab_id)
                .collect(),
        }
    }

    /// Whether a pane in `tab_id` belongs in the list.
    pub fn admits_tab(&self, tab_id: &str) -> bool {
        !self.out_of_scope.contains(tab_id)
    }

    /// True when the filter drops nothing.
    pub fn admits_everything(&self) -> bool {
        self.out_of_scope.is_empty()
    }
}

impl Host {
    /// `scope.get`. `Ok(None)` when this Herdr does not know the method, so a
    /// caller can treat scope as absent; other failures are errors.
    pub fn scope(&self) -> Result<Option<ScopeReport>> {
        match self.request(SCOPE_GET, json!({})) {
            Ok(result) => ScopeReport::from_result(result).map(Some),
            Err(error) if is_unknown_method(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// `scope.get`'s `local.tabs`: the server's verdict per local tab, or
    /// `None` when this Herdr has no scope (filter nothing).
    pub fn scope_tabs(&self) -> Result<Option<Vec<ScopeTab>>> {
        Ok(self.scope()?.map(|report| report.local.tabs))
    }

    /// The filter a pane list applies. Fails open: no scope support, a timeout
    /// or a malformed reply all admit every tab, because a visibility filter
    /// must never cost the user a working popup.
    pub fn scope_filter(&self) -> ScopeFilter {
        match self.scope_tabs() {
            Ok(Some(tabs)) => ScopeFilter::new(tabs),
            Ok(None) | Err(_) => ScopeFilter::default(),
        }
    }
}

/// Whether `error` is Herdr refusing a method it does not have, through any
/// anyhow context. See [`HostError::is_unknown_method`].
pub fn is_unknown_method(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<HostError>()
        .is_some_and(HostError::is_unknown_method)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{fake_host, ok};

    fn unknown(request: &Value) -> Value {
        // Herdr 0.9.5's exact refusal: its request enum rejects the method name.
        let method = request["method"].as_str().unwrap();
        json!({"id": request["id"], "error": {"code": "invalid_request",
            "message": format!("invalid request: unknown variant `{method}`, expected one of `ping`, `server.stop`")}})
    }

    fn report() -> Value {
        json!({"type": "scope",
               "scope": {"focus": {"active": true, "targets": [{"machine": "local", "workspace_label": "setup"}]},
                         "hidden": [{"machine": "*", "workspace_label": "ft", "config": true}]},
               "local": {"tabs": [
                   {"workspace_id": "w1", "tab_id": "w1:t1", "admitted": true},
                   {"workspace_id": "w1", "tab_id": "w1:t2", "admitted": false},
                   {"workspace_id": "w2", "tab_id": "w2:t1", "admitted": false}]}})
    }

    #[test]
    fn scope_tabs_parses_the_local_verdicts() {
        let (_dir, host) = fake_host(|request| {
            assert_eq!(request["method"], "scope.get");
            assert_eq!(request["params"], json!({}));
            ok(&request, report())
        });
        let tabs = host.scope_tabs().unwrap().unwrap();
        assert_eq!(
            tabs,
            [
                ScopeTab {
                    workspace_id: "w1".into(),
                    tab_id: "w1:t1".into(),
                    admitted: true
                },
                ScopeTab {
                    workspace_id: "w1".into(),
                    tab_id: "w1:t2".into(),
                    admitted: false
                },
                ScopeTab {
                    workspace_id: "w2".into(),
                    tab_id: "w2:t1".into(),
                    admitted: false
                },
            ]
        );
        let scope = host.scope().unwrap().unwrap();
        assert!(scope.scope.focus.active);
        assert!(scope.focus_engaged());
        assert_eq!(scope.scope.hidden.len(), 1);
    }

    #[test]
    fn a_herdr_without_scope_answers_none_and_filters_nothing() {
        let (_dir, host) = fake_host(|request| unknown(&request));
        assert_eq!(host.scope().unwrap(), None);
        assert_eq!(host.scope_tabs().unwrap(), None);
        assert!(host.scope_filter().admits_everything());

        // A future Herdr may name the refusal outright.
        let (_dir, host) = fake_host(
            |request| json!({"id": request["id"], "error": {"code": "unknown_method", "message": "unknown method: scope.get"}}),
        );
        assert_eq!(host.scope_tabs().unwrap(), None);
    }

    #[test]
    fn other_refusals_are_errors_but_the_filter_still_fails_open() {
        // An invalid_request about something else is not "method unknown".
        let (_dir, host) = fake_host(|request| {
            json!({"id": request["id"], "error": {"code": "invalid_request",
                "message": "invalid request: unknown variant `sideways`, expected `focus` or `hide`"}})
        });
        let error = host.scope_tabs().unwrap_err();
        assert!(!is_unknown_method(&error), "{error:#}");
        assert!(host.scope_filter().admits_everything());

        let (_dir, host) = fake_host(|request| ok(&request, json!({"local": {"tabs": "nope"}})));
        assert!(host.scope_tabs().is_err());
        assert!(host.scope_filter().admits_everything());
    }

    #[test]
    fn the_filter_drops_only_tabs_judged_out_of_scope() {
        let (_dir, host) = fake_host(|request| ok(&request, report()));
        let filter = host.scope_filter();
        assert!(filter.admits_tab("w1:t1"));
        assert!(!filter.admits_tab("w1:t2"));
        assert!(!filter.admits_tab("w2:t1"));
        // Created after the scope read: unknown to the report, so admitted.
        assert!(filter.admits_tab("w3:t1"));
        assert!(!filter.admits_everything());
    }

    #[test]
    fn focus_without_targets_is_not_engaged() {
        let report = ScopeReport::from_result(
            json!({"scope": {"focus": {"active": true, "targets": []}}, "local": {"tabs": []}}),
        )
        .unwrap();
        assert!(report.scope.focus.active);
        assert!(!report.focus_engaged());
    }
}
