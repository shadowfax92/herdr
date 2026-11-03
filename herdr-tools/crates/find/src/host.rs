//! The only Herdr protocol boundary. Each request has a whole-request deadline,
//! reply cap, and correlation ID; the inherited socket pins this popup to its host.
use crate::{
    age::Starts,
    agents_policy,
    catalog::{self, Candidate, LoadedCatalog, Pane},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use socket2::{Domain, SockAddr, Socket, Type};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::atomic::{AtomicU64, Ordering},
};

const MAX_REPLY: usize = 8 * 1024 * 1024;
static REQUEST_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
/// A session-pinned driver shared by read workers and the foreground navigator.
pub(crate) struct Host {
    path: PathBuf,
    timeout: Duration,
}

/// Columns a narrowed popup leaves free on each side of Herdr's pane area.
const POPUP_MARGIN: u16 = 2;

impl Host {
    pub fn from_environment() -> Result<Self> {
        let path = PathBuf::from(
            std::env::var_os("HERDR_SOCKET_PATH")
                .filter(|p| !p.is_empty())
                .context("Run Jump as a Herdr plugin action (HERDR_SOCKET_PATH is missing)")?,
        );
        if !path.is_absolute() {
            bail!("HERDR_SOCKET_PATH must be absolute");
        }
        Ok(Self {
            path,
            timeout: Duration::from_secs(2),
        })
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let deadline = Instant::now() + self.timeout;
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|t| !t.is_zero())
                .context("Herdr request timed out")
        };
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        socket
            .connect_timeout(&SockAddr::unix(&self.path)?, remaining()?)
            .context("Herdr is unavailable")?;
        let fd: std::os::fd::OwnedFd = socket.into();
        let mut stream = UnixStream::from(fd);
        // Refreshing SO_RCVTIMEO after a peer closes returns EINVAL on macOS,
        // even with a complete reply buffered. Nonblocking I/O preserves the
        // whole-request deadline without touching socket timeouts after connect.
        stream.set_nonblocking(true)?;
        let id = format!(
            "jump-{}-{}",
            std::process::id(),
            REQUEST_ID.fetch_add(1, Ordering::Relaxed)
        );
        let mut request = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))?;
        request.push(b'\n');
        let mut sent = 0;
        while sent < request.len() {
            remaining()?;
            match stream.write(&request[sent..]) {
                Ok(0) => bail!("Herdr closed the request"),
                Ok(count) => sent += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(remaining()?.min(Duration::from_millis(1)));
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("writing Herdr request"),
            }
        }
        let mut reply = Vec::new();
        loop {
            remaining()?;
            let mut chunk = [0; 8192];
            let count = match stream.read(&mut chunk) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(remaining()?.min(Duration::from_millis(1)));
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result.context("reading Herdr reply")?,
            };
            if count == 0 {
                bail!("Herdr reply ended before its newline");
            }
            let end = chunk[..count].iter().position(|c| *c == b'\n');
            reply.extend_from_slice(&chunk[..end.unwrap_or(count)]);
            if reply.len() > MAX_REPLY {
                bail!("Herdr reply exceeds 8 MiB");
            }
            if end.is_some() {
                break;
            }
        }
        let value: Value = serde_json::from_slice(&reply).context("invalid Herdr reply")?;
        if value["id"].as_str() != Some(&id) {
            bail!("Herdr reply correlation mismatch");
        }
        if let Some(error) = value.get("error") {
            return Err(HostError {
                method: method.to_owned(),
                code: error["code"].as_str().unwrap_or("unknown").to_owned(),
                message: error["message"]
                    .as_str()
                    .unwrap_or("host rejected request")
                    .to_owned(),
            }
            .into());
        }
        value
            .get("result")
            .cloned()
            .context("Herdr reply omitted result")
    }

    pub fn candidates(&self, source: &str, own: &str) -> Result<LoadedCatalog> {
        let reply = self.request("session.snapshot", json!({}))?;
        let mut snapshot = reply
            .get("snapshot")
            .cloned()
            .context("Herdr omitted snapshot")?;
        // The catalog worker owns both reads and publishes them as one result:
        // rows cannot flash before this load's policy outcome is known. Excluded
        // sessions need no transcript lookup, and Paths never takes this route.
        let policy = agents_policy::from_environment();
        // One recovery context shares directory walks across eligible sessions.
        let mut starts = Starts::from_environment();
        // Fall through to catalog's structural validation for malformed
        // snapshots; mutable JSON indexing can panic and strand this worker.
        if let Some(panes) = snapshot.get_mut("panes").and_then(Value::as_array_mut) {
            for pane in panes {
                if pane["pane_id"].as_str() == Some(own)
                    || policy.hides(pane["workspace_id"].as_str().unwrap_or_default())
                {
                    continue;
                }
                let start = starts.resolve(pane["agent"].as_str(), &pane["agent_session"]);
                pane.as_object_mut()
                    .context("invalid pane catalog")?
                    .insert("started_at".into(), json!(start));
            }
        }
        catalog::catalog_with_policy(snapshot, source, own, policy)
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

    /// Opens `picker` as a `width`-column popup over `source`. The popup
    /// cannot see which pane invoked it (herdr strips HERDR_PANE_ID from
    /// popups), so the source pane and picker id travel in its environment.
    ///
    /// Herdr fits an oversized popup to the area beside its sidebar flush to
    /// both edges, so on a terminal too narrow for `width` the request shrinks
    /// to leave `POPUP_MARGIN` columns on each side. If the area can't be read
    /// the request goes out as declared and Herdr's own fit applies.
    ///
    /// The size is fixed when the popup opens: Herdr has no request to resize
    /// an open popup, and one size can't say "`width` or the area less a
    /// margin". A popup opened on a small terminal keeps its size if the
    /// terminal grows while it is open (shrinking still fits it); reopening
    /// fits it again.
    pub fn open(&self, source: &str, picker: &str, width: u16) -> Result<()> {
        let area = self
            .request("pane.layout", json!({"pane_id":source}))
            .ok()
            .and_then(|reply| reply["layout"]["area"]["width"].as_u64())
            .and_then(|area| u16::try_from(area).ok());
        let width = area.map_or(width, |area| {
            width.min(area.saturating_sub(2 * POPUP_MARGIN))
        });
        self.request("plugin.pane.open", json!({"plugin_id":"shadowfax.jump", "entrypoint":"picker", "placement":"popup", "width":width, "height":"90%", "focus":true,
            "env":{"HERDR_JUMP_SOURCE_PANE":source, "HERDR_JUMP_PICKER":picker}}))?;
        Ok(())
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
/// module that talks to Herdr. Dropping the tempdir unlinks the socket.
#[cfg(test)]
pub(crate) fn fake_host(
    reply: impl Fn(Value) -> Value + Send + 'static,
) -> (tempfile::TempDir, Host) {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    let dir = tempfile::Builder::new()
        .prefix("jump-rpc-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = dir.path().join("s");
    let listener = UnixListener::bind(&path).unwrap();
    // Each test's finite reply sequence owns this listener; dropping the
    // tempdir unlinks its socket so it cannot be confused with a live host.
    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < end {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut line = String::new();
                    BufReader::new(&stream).read_line(&mut line).unwrap();
                    let value: Value = serde_json::from_str(&line).unwrap();
                    let response = reply(value);
                    let _ = writeln!(stream, "{response}");
                }
                Err(_) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    });
    (
        dir,
        Host {
            path,
            timeout: Duration::from_millis(250),
        },
    )
}

/// Host refusals retain their code across anyhow contexts. Text is presentation,
/// never the authority for deciding that an absent pane is safe to skip.
#[derive(Debug)]
pub(crate) struct HostError {
    pub code: String,
    method: String,
    message: String,
}
impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.method, self.code, self.message)
    }
}
impl std::error::Error for HostError {}

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

pub(crate) fn source_pane() -> Result<String> {
    let context: Value = match std::env::var("HERDR_PLUGIN_CONTEXT_JSON") {
        Ok(context) => serde_json::from_str(&context).context("invalid Herdr plugin context")?,
        Err(_) => Value::Null,
    };
    let source = context["focused_pane_id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| std::env::var("HERDR_PANE_ID").ok())
        .context("Herdr did not provide a source pane")?;
    if source.is_empty() || source.chars().any(char::is_control) {
        bail!("invalid source pane identity");
    }
    Ok(source)
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
        host.timeout = Duration::from_millis(30);
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
        host.open("w1:p1", "paths", 110).unwrap();
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
        let open = &seen[2]["params"];
        assert_eq!(open["env"]["HERDR_JUMP_PICKER"], "paths");
        assert_eq!(open["env"]["HERDR_JUMP_SOURCE_PANE"], "w1:p1");
        // No layout in the reply: the declared width goes out unchanged.
        assert_eq!(open["width"], 110);
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
            host.open("w1:p1", "panes", 132).unwrap();
            assert_eq!(seen.lock().unwrap()[1]["params"]["width"], width, "{area}");
        }
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
