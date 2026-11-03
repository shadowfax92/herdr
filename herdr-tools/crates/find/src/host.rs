//! Find's Herdr calls, built on [`tools_core::Host`]'s socket primitive, which
//! gives each request a whole-request deadline, reply cap and correlation ID;
//! the inherited socket pins this popup to its host. Everything mode-specific
//! (snapshot, identity checks, focus, preview, kill, paste, popup sizing) stays
//! here rather than in core.
use crate::{
    age::Starts,
    catalog::{self, Candidate, Pane},
    pickers::Picker,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tools_core::{Config, PopupDims, PopupSize};

/// Host refusals keep their code across anyhow contexts. Text is presentation,
/// never the authority for deciding that an absent pane is safe to skip.
pub(crate) use tools_core::HostError;

#[derive(Clone)]
/// A session-pinned driver shared by read workers and the foreground navigator.
pub(crate) struct Host {
    inner: tools_core::Host,
}

/// Columns a narrowed popup leaves free on each side of Herdr's pane area.
const POPUP_MARGIN: u16 = 2;
/// The popup pane in herdr-plugin.toml that every picker runs in.
const ENTRYPOINT: &str = "find";
/// Jump's own deadline: every read is on a worker, but a jump, kill or paste
/// waits for its reply on the foreground loop.
const TIMEOUT: Duration = Duration::from_secs(2);

impl Host {
    pub fn from_environment() -> Result<Self> {
        let inner = tools_core::Host::from_env();
        if inner.socket_path().is_none() {
            bail!("Run herdr-tools find as a Herdr plugin action (HERDR_SOCKET_PATH is missing or not absolute)");
        }
        Ok(Self {
            inner: inner.with_timeout(TIMEOUT),
        })
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.inner.request(method, params)
    }

    /// Every open pane in this session except the popup itself, as rows, minus
    /// the panes whose tab Herdr's focus/hide scope leaves out.
    ///
    /// The server judges scope (`scope.get`'s per-tab verdict); Find never
    /// reimplements the rule. The scope read fails open: a Herdr without
    /// scope, a timeout or a malformed reply lists every pane, as Find did
    /// before scope existed. Only the list is scoped: an explicit jump to a
    /// pane Find did list still revalidates by identity, not by scope.
    pub fn candidates(&self, source: &str, own: &str) -> Result<Vec<Candidate>> {
        let mut rows = self.all_candidates(source, own)?;
        // Read after the snapshot, so every listed tab most likely has a
        // verdict; a tab the report misses is admitted.
        let scope = self.inner.scope_filter();
        rows.retain(|row| scope.admits_tab(&row.tab_id));
        Ok(rows)
    }

    fn all_candidates(&self, source: &str, own: &str) -> Result<Vec<Candidate>> {
        let reply = self.request("session.snapshot", json!({}))?;
        let mut snapshot = reply
            .get("snapshot")
            .cloned()
            .context("Herdr omitted snapshot")?;
        // This method is invoked only by the catalog read worker. One recovery
        // context shares directory walks across all sessions in this snapshot.
        let mut starts = Starts::from_environment();
        // Fall through to catalog's structural validation for malformed
        // snapshots; mutable JSON indexing can panic and strand this worker.
        if let Some(panes) = snapshot.get_mut("panes").and_then(Value::as_array_mut) {
            for pane in panes {
                // The popup never becomes a row, so it needs no session start.
                if pane["pane_id"].as_str() == Some(own) {
                    continue;
                }
                let start = starts.resolve(pane["agent"].as_str(), &pane["agent_session"]);
                pane.as_object_mut()
                    .context("invalid pane catalog")?
                    .insert("started_at".into(), json!(start));
            }
        }
        catalog::catalog_with_own(snapshot, source, own)
    }

    fn validate(&self, target: &Candidate, method: &str) -> Result<()> {
        let result = self.request(method, json!({"pane_id":target.pane_id}))?;
        let pane: Pane =
            serde_json::from_value(result.get("pane").cloned().context("Herdr omitted pane")?)?;
        if pane.pane_id != target.pane_id
            || pane.terminal_id != target.terminal_id
            || pane.tab_id != target.tab_id
            || pane.workspace_id != target.workspace_id
        {
            return Err(IdentityChanged.into());
        }
        Ok(())
    }

    pub fn jump(&self, target: &Candidate) -> Result<()> {
        self.validate(target, "pane.get")?;
        // No conditional focus transaction exists in this host version. Check
        // immediately before mutation and validate its reply, then explicitly
        // project the tab to attached TUI clients (as Beacon does).
        self.validate(target, "pane.focus")?;
        let reply = self.request("tab.focus", json!({"tab_id":target.tab_id}))?;
        if reply["tab"]["tab_id"].as_str() != Some(&target.tab_id) {
            bail!("Herdr did not confirm the destination tab");
        }
        Ok(())
    }

    pub fn preview(&self, target: &Candidate) -> Result<String> {
        self.validate(target, "pane.get")?;
        let reply = self.request("pane.read", json!({"pane_id": target.pane_id, "source":"recent_unwrapped", "format":"text", "strip_ansi":true,"lines":60}))?;
        let read = &reply["read"];
        if read["pane_id"].as_str() != Some(&target.pane_id) {
            bail!("Preview pane changed");
        }
        // The preview is never written directly to the terminal. Strip control
        // characters even if a host sends them despite strip_ansi=true.
        Ok(read["text"]
            .as_str()
            .context("Herdr omitted preview text")?
            .lines()
            .map(|line| catalog::plain(&line.replace('\t', "    ")))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Opens `picker` as a popup over `source`. The popup cannot see which
    /// pane invoked it (herdr strips HERDR_PANE_ID from popups), so the source
    /// pane and picker id travel in its environment.
    ///
    /// The size comes from `config`'s `[popup]` for the picker's popup key (its
    /// manifest action id: `find` for the navigator, `find-paths` for paths),
    /// matched on the source client's width; unconfigured pickers keep their
    /// `pickers.toml` width and 90% of the height, as herdr-jump did.
    ///
    /// Herdr fits an oversized popup to the area beside its sidebar flush to
    /// both edges, so on a terminal too narrow for a width in cells the request
    /// shrinks to leave `POPUP_MARGIN` columns on each side. If the area can't
    /// be read the request goes out as declared and Herdr's own fit applies.
    ///
    /// The size is fixed when the popup opens: Herdr has no request to resize
    /// an open popup, and one size can't say "`width` or the area less a
    /// margin". A popup opened on a small terminal keeps its size if the
    /// terminal grows while it is open (shrinking still fits it); reopening
    /// fits it again.
    pub fn open(&self, source: &str, picker: &Picker, config: &Config) -> Result<()> {
        // One layout read serves both numbers: profiles match the client's
        // width (sidebar included), the margin narrows to the pane area.
        let area = self.inner.pane_area(source).ok();
        let client_width = area.map(|area| area.x.saturating_add(area.width));
        let dims = config.popup(&picker.action, client_width).map_or(
            PopupDims {
                width: PopupSize::Cells(picker.width),
                height: PopupSize::Percent(90),
            },
            |resolved| resolved.dims,
        );
        let width = match (dims.width, area) {
            (PopupSize::Cells(width), Some(area)) => {
                PopupSize::Cells(width.min(area.width.saturating_sub(2 * POPUP_MARGIN)))
            }
            (width, _) => width,
        };
        self.inner.open_popup_sized(
            ENTRYPOINT,
            Some(PopupDims { width, ..dims }),
            &[
                ("HERDR_JUMP_SOURCE_PANE", source),
                ("HERDR_JUMP_PICKER", &picker.id),
            ],
        )
    }

    /// Types `text` into a pane without pressing Enter. herdr wraps it in
    /// bracketed paste when the pane's program enabled that mode, so shells
    /// (even fish in vi normal mode) and agent TUIs insert it as text rather
    /// than interpreting it as keys. Works while this popup has focus.
    pub fn send_input(&self, pane_id: &str, text: &str) -> Result<()> {
        self.request("pane.send_input", json!({"pane_id":pane_id, "text":text}))?;
        Ok(())
    }

    /// Where a line picker's command runs: the pane's foreground cwd (where a
    /// shell or editor is now), else its launch cwd.
    pub fn pane_cwd(&self, pane_id: &str) -> Result<Option<PathBuf>> {
        let reply = self.request("pane.get", json!({"pane_id":pane_id}))?;
        let pane = &reply["pane"];
        Ok(["foreground_cwd", "cwd"]
            .into_iter()
            .filter_map(|key| pane[key].as_str())
            .map(PathBuf::from)
            .find(|path| path.is_absolute() && path.is_dir()))
    }
}

/// A private socket that answers each request with `reply`, for tests of any
/// module that talks to Herdr: core's fake host behind find's [`Host`], with
/// jump's 250 ms deadline. Dropping the tempdir unlinks the socket.
#[cfg(test)]
pub(crate) fn fake_host(
    reply: impl Fn(Value) -> Value + Send + 'static,
) -> (tempfile::TempDir, Host) {
    let (dir, inner) = tools_core::testing::fake_host(reply);
    let inner = inner.with_timeout(Duration::from_millis(250));
    (dir, Host { inner })
}

/// Identity failures carry a type so kill results can use clear action-specific
/// wording without matching or rewriting the navigation error's display text.
#[derive(Debug)]
struct IdentityChanged;
impl std::fmt::Display for IdentityChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Selected pane moved or changed; select it again")
    }
}
impl std::error::Error for IdentityChanged {}

/// Retain machine identity separately from the task name shown to the human.
#[derive(Debug)]
pub(crate) struct KillRefusal {
    pub pane_id: String,
    pub title: String,
    pub reason: String,
}
impl KillRefusal {
    fn new(target: &Candidate, reason: String) -> Self {
        Self {
            pane_id: target.pane_id.clone(),
            title: target.title.clone(),
            reason,
        }
    }
}

/// Per-target results keep partial success visible without retrying refusals.
#[derive(Debug, Default)]
pub(crate) struct KillReport {
    pub closed: Vec<String>,
    pub already_closed: Vec<String>,
    pub refused: Vec<KillRefusal>,
}
impl KillReport {
    pub fn summary(&self) -> String {
        let killed = self.closed.len();
        let total = killed + self.already_closed.len() + self.refused.len();
        let mut parts = Vec::new();
        if killed > 0 {
            parts.push(if killed == total {
                format!(
                    "Killed {killed} {}",
                    if killed == 1 { "pane" } else { "panes" }
                )
            } else {
                format!("Killed {killed} of {total}")
            });
        }
        if !self.already_closed.is_empty() {
            parts.push(format!("{} already closed", self.already_closed.len()));
        }
        for refusal in &self.refused {
            let title = catalog::clean(&refusal.title);
            let name = if title.is_empty() {
                catalog::clean(&refusal.pane_id)
            } else {
                title
            };
            let verb = if parts.is_empty() {
                "Refused"
            } else {
                "refused"
            };
            parts.push(format!(
                "{verb} “{name}”: {}",
                catalog::clean(&refusal.reason)
            ));
        }
        if parts.is_empty() {
            "No panes killed".into()
        } else {
            parts.join(" · ")
        }
    }
}

/// Destructive work belongs to the popup's foreground owner. Rc makes this
/// capability !Send + !Sync, so it cannot enter a read worker's closure. The
/// thread check additionally pins execution to its original owner.
pub(crate) struct KillExecutor {
    host: Host,
    source: String,
    own: String,
    owner: std::thread::ThreadId,
    _foreground: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl KillExecutor {
    pub fn new(host: Host, source: String, own: String) -> Self {
        Self {
            host,
            source,
            own,
            owner: std::thread::current().id(),
            _foreground: std::marker::PhantomData,
        }
    }

    pub fn kill(&self, targets: &[Candidate]) -> KillReport {
        let mut report = KillReport::default();
        for target in targets {
            let refusal = if std::thread::current().id() != self.owner {
                Some("kill requires the foreground owner thread")
            } else if target.pane_id == self.source || target.pane_id == self.own {
                Some("protected source or picker pane")
            } else {
                None
            };
            if let Some(reason) = refusal {
                report.refused.push(KillRefusal::new(target, reason.into()));
                continue;
            }
            // pane.close has no conditional identity argument. Revalidate all
            // four IDs immediately before each close; never retry or force a
            // refusal, and never replace the explicitly confirmed target set.
            let result = self.host.validate(target, "pane.get").and_then(|()| {
                self.host
                    .request("pane.close", json!({"pane_id":target.pane_id}))
                    .map(|_| ())
            });
            match result {
                Ok(()) => report.closed.push(target.pane_id.clone()),
                Err(error)
                    if error
                        .downcast_ref::<HostError>()
                        .is_some_and(|e| e.code == "pane_not_found") =>
                {
                    report.already_closed.push(target.pane_id.clone());
                }
                Err(error) => {
                    let reason = if error.is::<IdentityChanged>() {
                        "changed since it was marked".into()
                    } else if let Some(error) = error.downcast_ref::<HostError>() {
                        // Keep refusal codes visible, especially the host's
                        // confirmation_required guard, without protocol jargon.
                        format!("{} ({})", error.message, error.code)
                    } else {
                        format!("{error:#}")
                    };
                    report.refused.push(KillRefusal::new(target, reason));
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn target() -> Candidate {
        crate::catalog::catalog(json!({"workspaces":[{"workspace_id":"w1"}],"tabs":[{"tab_id":"w1:t1","workspace_id":"w1"}],"panes":[pane()]}),"w1:p1").unwrap().remove(0)
    }
    fn pane() -> Value {
        json!({"pane_id":"w1:p1","terminal_id":"term1","tab_id":"w1:t1","workspace_id":"w1"})
    }

    fn server(reply: impl Fn(Value) -> Value + Send + 'static) -> (tempfile::TempDir, Host) {
        fake_host(reply)
    }

    #[test]
    fn tests_that_jump_revalidates_shell_identity_and_displays_its_tab() {
        let methods = Arc::new(Mutex::new(Vec::new()));
        let seen = methods.clone();
        let (_dir, host) = server(move |req| {
            let method = req["method"].as_str().unwrap();
            seen.lock().unwrap().push(method.to_owned());
            let result = if method == "tab.focus" {
                json!({"tab":{"tab_id":"w1:t1"}})
            } else {
                json!({"pane":pane()})
            };
            json!({"id": req["id"], "result": result})
        });
        host.jump(&target()).unwrap();
        assert_eq!(
            *methods.lock().unwrap(),
            ["pane.get", "pane.focus", "tab.focus"]
        );
    }

    #[test]
    fn tests_that_replaced_or_moved_target_is_never_focused() {
        for field in ["terminal_id", "tab_id", "workspace_id"] {
            let methods = Arc::new(Mutex::new(Vec::new()));
            let seen = methods.clone();
            let (_dir, host) = server(move |req| {
                seen.lock()
                    .unwrap()
                    .push(req["method"].as_str().unwrap().to_owned());
                let mut p = pane();
                p[field] = json!("changed");
                json!({"id":req["id"], "result":{"pane":p}})
            });
            assert!(host.jump(&target()).is_err());
            assert_eq!(*methods.lock().unwrap(), ["pane.get"]);
        }
    }

    #[test]
    fn tests_that_response_ids_are_correlated_and_errors_propagate() {
        let (_dir, host) = server(|_| json!({"id":"wrong", "result":{}}));
        assert!(host
            .request("pane.get", json!({}))
            .unwrap_err()
            .to_string()
            .contains("correlation"));
        let (_dir, host) = server(
            |req| json!({"id":req["id"],"error":{"code":"pane_not_found","message":"gone"}}),
        );
        assert!(host
            .request("pane.get", json!({}))
            .unwrap_err()
            .to_string()
            .contains("gone"));
    }

    #[test]
    fn tests_that_slow_host_read_has_a_finite_deadline() {
        let (_dir, mut host) = server(|req| {
            std::thread::sleep(Duration::from_millis(300));
            json!({"id":req["id"], "result":{}})
        });
        host.inner = host.inner.clone().with_timeout(Duration::from_millis(30));
        let started = std::time::Instant::now();
        assert!(host.request("session.snapshot", json!({})).is_err());
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn tests_that_preview_is_read_only_and_keeps_indentation() {
        let (_dir, host) = server(|req| {
            let result = match req["method"].as_str().unwrap() {
                "pane.get" => json!({"pane":pane()}),
                "pane.read" => {
                    json!({"read":{"pane_id":"w1:p1","text":"  let x = 1;\n    nested();\n"}})
                }
                other => panic!("preview must not mutate: {other}"),
            };
            json!({"id":req["id"],"result":result})
        });
        assert_eq!(
            host.preview(&target()).unwrap(),
            "  let x = 1;\n    nested();"
        );
    }
    fn executor(host: Host) -> KillExecutor {
        KillExecutor::new(host, "source".into(), "own".into())
    }

    #[test]
    fn protected_panes_are_refused_without_any_host_request() {
        let (_dir, host) = server(|_| panic!("protected pane reached socket"));
        for (source, own) in [("w1:p1", "own"), ("source", "w1:p1")] {
            let report =
                KillExecutor::new(host.clone(), source.into(), own.into()).kill(&[target()]);
            assert!(report.closed.is_empty());
            assert_eq!(report.refused.len(), 1);
            assert!(report.refused[0].reason.contains("protected"));
        }
    }

    #[test]
    fn invariant_5_replaced_terminal_or_moved_pane_is_never_closed() {
        for field in ["pane_id", "terminal_id", "tab_id", "workspace_id"] {
            let methods = Arc::new(Mutex::new(Vec::new()));
            let seen = methods.clone();
            let (_dir, host) = server(move |req| {
                seen.lock()
                    .unwrap()
                    .push(req["method"].as_str().unwrap().to_owned());
                let mut pane = pane();
                pane[field] = json!("changed");
                json!({"id":req["id"],"result":{"pane":pane}})
            });
            let report = executor(host).kill(&[target()]);
            assert_eq!(report.refused.len(), 1);
            assert!(report.closed.is_empty());
            assert_eq!(*methods.lock().unwrap(), ["pane.get"]);
        }
    }

    #[test]
    fn invariant_7_kill_capability_cannot_run_on_a_worker_thread() {
        static_assertions::assert_not_impl_any!(KillExecutor: Send, Sync);
        let (_dir, host) = server(|_| panic!("wrong thread reached socket"));
        let mut executor = executor(host);
        executor.owner = std::thread::spawn(|| std::thread::current().id())
            .join()
            .unwrap();
        let report = executor.kill(&[target()]);
        assert!(report.closed.is_empty());
        assert_eq!(report.refused.len(), 1);
        assert!(report.refused[0].reason.contains("foreground"));
    }

    #[test]
    fn already_gone_is_counted_at_get_or_close_and_not_retried() {
        for missing_at in ["pane.get", "pane.close"] {
            let methods = Arc::new(Mutex::new(Vec::new()));
            let seen = methods.clone();
            let (_dir, host) = server(move |req| {
                let method = req["method"].as_str().unwrap();
                seen.lock().unwrap().push(method.to_owned());
                if method == missing_at {
                    json!({"id":req["id"],"error":{"code":"pane_not_found","message":"gone"}})
                } else {
                    json!({"id":req["id"],"result":{"pane":pane()}})
                }
            });
            let report = executor(host).kill(&[target()]);
            assert_eq!(report.already_closed, ["w1:p1"]);
            assert!(report.refused.is_empty());
            assert_eq!(
                methods.lock().unwrap().len(),
                if missing_at == "pane.get" { 1 } else { 2 }
            );
        }
    }

    #[test]
    fn confirmation_required_is_reported_once_without_force_or_retry() {
        let methods = Arc::new(Mutex::new(Vec::new()));
        let seen = methods.clone();
        let (_dir, host) = server(move |req| {
            let method = req["method"].as_str().unwrap();
            seen.lock().unwrap().push(method.to_owned());
            assert_eq!(req["params"], json!({"pane_id":"w1:p1"}));
            if method == "pane.close" {
                json!({"id":req["id"],"error":{"code":"confirmation_required","message":"root pane"}})
            } else {
                json!({"id":req["id"],"result":{"pane":pane()}})
            }
        });
        let report = executor(host).kill(&[target()]);
        assert!(report.closed.is_empty());
        assert_eq!(report.refused.len(), 1);
        assert!(report.summary().contains("confirmation_required"));
        assert_eq!(*methods.lock().unwrap(), ["pane.get", "pane.close"]);
    }

    #[test]
    fn kill_checks_then_closes_each_target_in_confirmation_order() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let (_dir, host) = server(move |req| {
            let method = req["method"].as_str().unwrap();
            let id = req["params"]["pane_id"].as_str().unwrap();
            seen.lock()
                .unwrap()
                .push((method.to_owned(), id.to_owned()));
            let mut pane = pane();
            pane["pane_id"] = json!(id);
            json!({"id":req["id"],"result":{"pane":pane}})
        });
        let a = target();
        let mut b = a.clone();
        b.pane_id = "w1:p2".into();
        let report = executor(host).kill(&[b, a]);
        assert_eq!(report.closed, ["w1:p2", "w1:p1"]);
        assert_eq!(
            *requests.lock().unwrap(),
            [
                ("pane.get".into(), "w1:p2".into()),
                ("pane.close".into(), "w1:p2".into()),
                ("pane.get".into(), "w1:p1".into()),
                ("pane.close".into(), "w1:p1".into())
            ]
        );
    }

    #[test]
    fn host_error_codes_survive_the_protocol_boundary() {
        let (_dir, host) = server(
            |req| json!({"id":req["id"],"error":{"code":"confirmation_required","message":"stop"}}),
        );
        let error = host.request("pane.close", json!({})).unwrap_err();
        assert_eq!(
            error.downcast_ref::<HostError>().unwrap().code,
            "confirmation_required"
        );
    }

    #[test]
    fn buffered_large_reply_is_read_after_peer_closes() {
        // Crossing many read chunks used to repeatedly refresh SO_RCVTIMEO on
        // the closed peer. macOS may reject that even with unread buffered data.
        let payload = "x".repeat(128 * 1024);
        let expected = payload.clone();
        let (_dir, host) = server(move |req| json!({"id":req["id"],"result":{"text":payload}}));
        assert_eq!(host.request("large", json!({})).unwrap()["text"], expected);
    }

    #[test]
    fn malformed_catalog_returns_an_error_instead_of_panicking_the_worker() {
        for snapshot in [
            json!("invalid"),
            json!([]),
            json!(null),
            json!({"workspaces":[],"tabs":[],"panes":[false]}),
        ] {
            let (_dir, host) =
                server(move |req| json!({"id":req["id"],"result":{"snapshot":snapshot}}));
            assert!(host.candidates("source", "own").is_err());
        }
    }

    /// Three panes over three tabs of two workspaces; `setup/scratch` and `ft`
    /// are what a scope would leave out.
    fn scoped_snapshot() -> Value {
        json!({"snapshot": {
            "workspaces": [{"workspace_id": "w1", "label": "setup"}, {"workspace_id": "w2", "label": "ft"}],
            "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1", "label": "main"},
                     {"tab_id": "w1:t2", "workspace_id": "w1", "label": "scratch"},
                     {"tab_id": "w2:t1", "workspace_id": "w2", "label": "main"}],
            "panes": [
                {"pane_id": "w1:p1", "terminal_id": "term1", "tab_id": "w1:t1", "workspace_id": "w1"},
                {"pane_id": "w1:p2", "terminal_id": "term2", "tab_id": "w1:t2", "workspace_id": "w1"},
                {"pane_id": "w2:p1", "terminal_id": "term3", "tab_id": "w2:t1", "workspace_id": "w2"}]}})
    }

    fn listed(host: &Host) -> Vec<String> {
        host.candidates("w1:p1", "own")
            .unwrap()
            .into_iter()
            .map(|row| row.pane_id)
            .collect()
    }

    #[test]
    fn panes_in_tabs_out_of_scope_are_not_listed() {
        let (_dir, host) = server(|req| {
            let result = match req["method"].as_str().unwrap() {
                "session.snapshot" => scoped_snapshot(),
                "scope.get" => json!({"type": "scope", "scope": {}, "local": {"tabs": [
                    {"workspace_id": "w1", "tab_id": "w1:t1", "admitted": true},
                    {"workspace_id": "w1", "tab_id": "w1:t2", "admitted": false},
                    {"workspace_id": "w2", "tab_id": "w2:t1", "admitted": false}]}}),
                other => panic!("listing must only read: {other}"),
            };
            json!({"id": req["id"], "result": result})
        });
        assert_eq!(listed(&host), ["w1:p1"]);
    }

    #[test]
    fn a_herdr_without_scope_lists_every_pane() {
        let (_dir, host) = server(|req| match req["method"].as_str().unwrap() {
            "session.snapshot" => json!({"id": req["id"], "result": scoped_snapshot()}),
            method => json!({"id": req["id"], "error": {"code": "invalid_request",
                "message": format!("invalid request: unknown variant `{method}`, expected one of `ping`")}}),
        });
        assert_eq!(listed(&host), ["w1:p1", "w1:p2", "w2:p1"]);

        // Any other scope failure fails open too, rather than emptying Find.
        let (_dir, host) = server(|req| match req["method"].as_str().unwrap() {
            "session.snapshot" => json!({"id": req["id"], "result": scoped_snapshot()}),
            _ => json!({"id": req["id"], "error": {"code": "internal", "message": "boom"}}),
        });
        assert_eq!(listed(&host), ["w1:p1", "w1:p2", "w2:p1"]);
    }

    #[test]
    fn successful_kill_summary_omits_zero_counts() {
        let report = KillReport {
            closed: vec!["a".into(), "b".into()],
            ..KillReport::default()
        };
        assert_eq!(report.summary(), "Killed 2 panes");
    }

    #[test]
    fn partial_kill_summary_names_the_task_and_explains_the_refusal() {
        let (_dir, host) = server(|req| {
            let mut pane = pane();
            pane["terminal_id"] = json!("replacement");
            json!({"id":req["id"],"result":{"pane":pane}})
        });
        let mut target = target();
        target.title = "Explain RNN weight sharing".into();
        let mut report = executor(host).kill(&[target]);
        report.closed.push("closed".into());
        report.already_closed.push("gone".into());
        assert_eq!(report.summary(), "Killed 1 of 3 · 1 already closed · refused “Explain RNN weight sharing”: changed since it was marked");
    }

    #[test]
    fn refusal_summary_falls_back_to_pane_id_and_keeps_host_guard_code() {
        let (_dir, host) = server(|req| {
            if req["method"] == "pane.close" {
                json!({"id":req["id"],"error":{"code":"confirmation_required","message":"root pane"}})
            } else {
                json!({"id":req["id"],"result":{"pane":pane()}})
            }
        });
        let mut target = target();
        target.title = "  \n".into();
        let report = executor(host).kill(&[target]);
        assert_eq!(report.refused[0].pane_id, "w1:p1");
        assert_eq!(
            report.summary(),
            "Refused “w1:p1”: root pane (confirmation_required)"
        );
        let report = KillReport {
            already_closed: vec!["gone".into()],
            ..KillReport::default()
        };
        assert_eq!(report.summary(), "1 already closed");
        let report = KillReport {
            closed: vec!["closed".into()],
            ..KillReport::default()
        };
        assert_eq!(report.summary(), "Killed 1 pane");
    }

    #[test]
    fn send_input_types_text_without_keys_and_open_names_the_picker() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let (_dir, host) = server(move |req| {
            log.lock().unwrap().push(req.clone());
            json!({"id":req["id"],"result":{}})
        });
        host.send_input("w1:p1", "'/a b'").unwrap();
        host.open("w1:p1", &picker("paths"), &no_config()).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["method"], "pane.send_input");
        assert_eq!(
            seen[0]["params"],
            json!({"pane_id":"w1:p1","text":"'/a b'"})
        );
        assert_eq!(
            seen[1],
            json!({"id":seen[1]["id"],"method":"pane.layout","params":{"pane_id":"w1:p1"}})
        );
        assert_eq!(seen[2]["method"], "plugin.pane.open");
        let open = &seen[2]["params"];
        assert_eq!(open["plugin_id"], "shadowfax.tools");
        assert_eq!(open["entrypoint"], "find");
        assert_eq!(open["env"]["HERDR_JUMP_PICKER"], "paths");
        assert_eq!(open["env"]["HERDR_JUMP_SOURCE_PANE"], "w1:p1");
        // No layout in the reply: the declared width goes out unchanged.
        assert_eq!(open["width"], 110);
        assert_eq!(open["height"], "90%");
    }

    #[test]
    fn open_keeps_a_margin_where_the_terminal_is_narrower_than_the_popup() {
        for (area, width) in [(474, 132), (136, 132), (114, 110), (44, 40)] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = seen.clone();
            let (_dir, host) = server(move |req| {
                log.lock().unwrap().push(req.clone());
                if req["method"] == "pane.layout" {
                    json!({"id":req["id"],"result":{"layout":{"area":{"width":area,"height":40}}}})
                } else {
                    json!({"id":req["id"],"result":{}})
                }
            });
            host.open("w1:p1", &picker("panes"), &no_config()).unwrap();
            assert_eq!(seen.lock().unwrap()[1]["params"]["width"], width, "{area}");
        }
    }

    fn picker(id: &str) -> Picker {
        crate::pickers::find(id).unwrap()
    }

    fn no_config() -> Config {
        Config::parse("").unwrap()
    }

    /// The `plugin.pane.open` params `open` sends for `picker` under `config`,
    /// with the source pane's area at `x` and `width` cells (None: unreadable).
    fn opened(picker_id: &str, config: &str, area: Option<(u16, u16)>) -> Value {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let (_dir, host) = server(move |req| {
            log.lock().unwrap().push(req.clone());
            match (req["method"].as_str(), area) {
                (Some("pane.layout"), Some((x, width))) => json!({"id":req["id"],
                    "result":{"layout":{"area":{"x":x,"y":0,"width":width,"height":40}}}}),
                _ => json!({"id":req["id"],"result":{}}),
            }
        });
        let config = Config::parse(config).unwrap();
        host.open("w1:p1", &picker(picker_id), &config).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[1]["method"], "plugin.pane.open");
        seen[1]["params"].clone()
    }

    #[test]
    fn configured_sizes_win_per_picker_key_and_cells_still_keep_the_margin() {
        let config = r#"
[popup.size]
find = { width = 100, height = 30 }
find-paths = { width = "70%", height = "80%" }
"#;
        let size = |params: &Value| (params["width"].clone(), params["height"].clone());
        // The navigator's key is `find`, the paths picker's `find-paths`.
        assert_eq!(
            size(&opened("panes", config, Some((26, 300)))),
            (json!(100), json!(30))
        );
        assert_eq!(
            size(&opened("paths", config, Some((26, 300)))),
            (json!("70%"), json!("80%"))
        );
        // A configured width in cells is narrowed like a built-in one; a
        // percentage is already relative to the area and goes out as written.
        assert_eq!(
            size(&opened("panes", config, Some((26, 90)))),
            (json!(86), json!(30))
        );
        assert_eq!(
            size(&opened("paths", config, Some((26, 90)))),
            (json!("70%"), json!("80%"))
        );
        // Unconfigured keys keep the picker's own width and 90% height.
        let only_paths = "[popup.size]\nfind-paths = { width = 60, height = 20 }\n";
        assert_eq!(
            size(&opened("panes", only_paths, None)),
            (json!(132), json!("90%"))
        );
    }

    #[test]
    fn profiles_match_the_client_width_including_the_sidebar() {
        let config = r#"
[popup.size]
find = { width = 120, height = "90%" }

[[popup.profile]]
name = "laptop"
max_client_width = 200
size.find = { width = "95%", height = "95%" }
"#;
        // x + width = 200: the laptop profile applies.
        let params = opened("panes", config, Some((26, 174)));
        assert_eq!(
            (params["width"].clone(), params["height"].clone()),
            (json!("95%"), json!("95%"))
        );
        // 201 cells: past the profile, so `[popup.size]` applies.
        let params = opened("panes", config, Some((27, 174)));
        assert_eq!(
            (params["width"].clone(), params["height"].clone()),
            (json!(120), json!("90%"))
        );
        // An unreadable layout skips profiles but still uses `[popup.size]`.
        let params = opened("panes", config, None);
        assert_eq!(
            (params["width"].clone(), params["height"].clone()),
            (json!(120), json!("90%"))
        );
    }

    #[test]
    fn pane_cwd_prefers_an_existing_foreground_directory() {
        let dir = tempfile::tempdir().unwrap();
        let foreground = dir.path().display().to_string();
        let (_tmp, host) = server(
            move |req| json!({"id":req["id"],"result":{"pane":{"foreground_cwd":foreground,"cwd":"/"}}}),
        );
        assert_eq!(host.pane_cwd("p").unwrap().as_deref(), Some(dir.path()));
        let (_tmp, host) = server(
            |req| json!({"id":req["id"],"result":{"pane":{"foreground_cwd":"/gone/away","cwd":"/"}}}),
        );
        assert_eq!(host.pane_cwd("p").unwrap().as_deref(), Some(Path::new("/")));
    }
}
