//! [`Host`], the one Herdr client every mode uses.
//!
//! Herdr offers two doors and modes use both: the `herdr` CLI (what most old
//! plugins called) and the newline-delimited JSON socket API (what jump, menu and
//! layouts called for operations the CLI lacks). Both reach the same server: the
//! CLI finds it through the inherited `HERDR_SOCKET_PATH`, and [`Host::request`]
//! connects to that path directly, which pins a popup to the session that opened it.
//!
//! Socket requests carry a correlation id, a whole-request deadline and a reply
//! cap, so a wedged server can delay one request but never hang a popup. Refusals
//! from either door become [`HostError`], whose `code` survives anyhow contexts:
//! callers decide on the code (`pane_not_found`, `ui_busy`), never on the text.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use socket2::{Domain, SockAddr, Socket, Type};

use crate::config::Config;
use crate::invocation::Invocation;
use crate::popup::{PopupDims, ResolvedPopup};
use crate::PLUGIN_ID;

/// Replies above this are refused rather than buffered without bound.
const MAX_REPLY: usize = 8 * 1024 * 1024;
/// Herdr's notification body limit, as the old plugins enforced it.
const MAX_NOTIFICATION_CHARS: usize = 220;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
static REQUEST_ID: AtomicU64 = AtomicU64::new(0);

/// The `{"result": …}` envelope `herdr <noun> <verb>` prints on success:
/// `host.cli_json::<Envelope<T>>(args)?.result`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Envelope<T> {
    pub result: T,
}

/// The area Herdr gives panes beside its sidebar, from `pane.layout`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct PaneArea {
    #[serde(default)]
    pub x: u16,
    #[serde(default)]
    pub y: u16,
    pub width: u16,
    #[serde(default)]
    pub height: u16,
}

/// A Herdr refusal from the socket (`error.code`) or the CLI (its JSON error on
/// stderr, else `command_failed`). Downcast with `error.downcast_ref::<HostError>()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostError {
    /// The socket method, or `herdr <noun> <verb>` for the CLI.
    pub method: String,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.method, self.code, self.message)
    }
}

impl std::error::Error for HostError {}

impl HostError {
    /// Whether Herdr refused because it does not have this socket method, so a
    /// caller can fall back to older behavior. Herdr 0.9.5 rejects an unknown
    /// method while decoding the request, as `invalid_request` with
    /// ``unknown variant `<method>` ``; retired methods answer `unknown_method`.
    /// Any other `invalid_request` (bad params) is a real error.
    pub fn is_unknown_method(&self) -> bool {
        self.code == "unknown_method"
            || (self.code == "invalid_request"
                && self
                    .message
                    .contains(&format!("unknown variant `{}`", self.method)))
    }
}

/// A Herdr client: the binary for CLI calls and the socket for API calls.
#[derive(Clone, Debug)]
pub struct Host {
    binary: PathBuf,
    socket: Option<PathBuf>,
    timeout: Duration,
}

impl Host {
    /// `HERDR_BIN_PATH` (when it names an existing file, or a bare name to look
    /// up), else `herdr` on `PATH`; and `HERDR_SOCKET_PATH` when it is absolute.
    pub fn from_env() -> Self {
        let socket = std::env::var_os("HERDR_SOCKET_PATH")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        Self::new(runtime_binary(std::env::var_os("HERDR_BIN_PATH")), socket)
    }

    /// A client for an explicit binary and socket (tests, custom sessions).
    pub fn new(binary: impl Into<PathBuf>, socket: Option<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            socket,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Replaces the whole-request socket deadline (default 5 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The Herdr executable CLI calls run, for handing to child processes.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The socket API path, when Herdr provided one.
    pub fn socket_path(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    /// Runs `herdr <args>` and returns its stdout; a non-zero exit is a [`HostError`].
    pub fn cli<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = collect(args);
        let output = self.cli_output(&args)?;
        if !output.status.success() {
            return Err(cli_error(&args, &output).into());
        }
        String::from_utf8(output.stdout).context("Herdr returned non-UTF-8 output")
    }

    /// Runs `herdr <args>` and decodes its whole stdout as JSON; wrap `T` in
    /// [`Envelope`] for the usual `{"result": …}` reply.
    pub fn cli_json<T, I, S>(&self, args: I) -> Result<T>
    where
        T: DeserializeOwned,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = collect(args);
        let stdout = self.cli(&args)?;
        serde_json::from_str(&stdout)
            .with_context(|| format!("failed to decode the reply of {}", cli_method(&args)))
    }

    /// Runs `herdr <args>` and returns its raw output whatever the exit status,
    /// for callers that classify failures themselves.
    pub fn cli_output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(&self.binary)
            .args(collect(args))
            .output()
            .with_context(|| format!("failed to run {}", self.binary.display()))
    }

    /// Sends one socket API request and returns its `result`. An `error` reply is
    /// a [`HostError`]; a missing socket, timeout, oversized or mismatched reply
    /// is a plain error.
    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let path = self.socket.as_deref().context(
            "HERDR_SOCKET_PATH is not set: run herdr-tools from a Herdr action, popup, or pane",
        )?;
        let deadline = Instant::now() + self.timeout;
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
                .context("Herdr request timed out")
        };
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        socket
            .connect_timeout(&SockAddr::unix(path)?, remaining()?)
            .context("Herdr is unavailable")?;
        let fd: std::os::fd::OwnedFd = socket.into();
        let mut stream = UnixStream::from(fd);
        // Refreshing SO_RCVTIMEO after a peer closes returns EINVAL on macOS,
        // even with a complete reply buffered. Nonblocking I/O keeps the
        // whole-request deadline without touching socket timeouts after connect.
        stream.set_nonblocking(true)?;
        let id = format!(
            "herdr-tools-{}-{}",
            std::process::id(),
            REQUEST_ID.fetch_add(1, Ordering::Relaxed)
        );
        let mut request =
            serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))?;
        request.push(b'\n');
        let mut sent = 0;
        while sent < request.len() {
            remaining()?;
            match stream.write(&request[sent..]) {
                Ok(0) => bail!("Herdr closed the request"),
                Ok(count) => sent += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(remaining()?.min(Duration::from_millis(1)));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
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
            let end = chunk[..count].iter().position(|byte| *byte == b'\n');
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

    /// Shows a Herdr notification; the body is cut to 220 characters.
    pub fn notify(&self, title: &str, body: &str) -> Result<()> {
        let body = body
            .chars()
            .take(MAX_NOTIFICATION_CHARS)
            .collect::<String>();
        self.cli(["notification", "show", title, "--body", &body])?;
        Ok(())
    }

    /// Asks the running server to reload Herdr's `config.toml`.
    pub fn reload_config(&self) -> Result<()> {
        self.cli(["server", "reload-config"])?;
        Ok(())
    }

    /// The pane area of `pane_id`'s client, from `pane.layout`.
    pub fn pane_area(&self, pane_id: &str) -> Result<PaneArea> {
        let reply = self.request("pane.layout", json!({"pane_id": pane_id}))?;
        serde_json::from_value(reply["layout"]["area"].clone())
            .context("Herdr's pane.layout reply has no area")
    }

    /// The client's full width in cells (sidebar included): the pane area's right
    /// edge. Popup profiles match on this number.
    pub fn client_width(&self, pane_id: &str) -> Result<u16> {
        let area = self.pane_area(pane_id)?;
        Ok(area.x.saturating_add(area.width))
    }

    /// Opens plugin pane `entrypoint` as a focused popup sized for `popup_key`.
    ///
    /// The size comes from [`Config::current`] and the invoking pane's
    /// [`Host::client_width`] (unknown width: base sizes only). Unconfigured keys
    /// send no size, so Herdr uses the manifest pane's default. `env` reaches the
    /// popup process, which is how it learns its source pane. Returns the size
    /// config supplied, if any.
    pub fn open_popup(
        &self,
        entrypoint: &str,
        popup_key: &str,
        env: &[(&str, &str)],
    ) -> Result<Option<ResolvedPopup>> {
        let config = Config::current()?;
        let client_width = Invocation::current()
            .ok()
            .and_then(|invocation| invocation.source_pane().ok())
            .and_then(|pane| self.client_width(pane).ok());
        let resolved = config.popup(popup_key, client_width);
        self.open_popup_sized(entrypoint, resolved.as_ref().map(|popup| popup.dims), env)?;
        Ok(resolved)
    }

    /// Opens plugin pane `entrypoint` as a focused popup with an explicit size
    /// (None: the manifest default), for modes that compute or narrow sizes.
    pub fn open_popup_sized(
        &self,
        entrypoint: &str,
        size: Option<PopupDims>,
        env: &[(&str, &str)],
    ) -> Result<()> {
        let mut params = json!({
            "plugin_id": PLUGIN_ID,
            "entrypoint": entrypoint,
            "placement": "popup",
            "focus": true,
            "env": env.iter().copied().collect::<BTreeMap<_, _>>(),
        });
        if let Some(size) = size {
            params["width"] = json!(size.width);
            params["height"] = json!(size.height);
        }
        self.request("plugin.pane.open", params)?;
        Ok(())
    }
}

fn collect<I, S>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect()
}

/// `herdr pane close`: the noun and verb name a CLI call without dumping its
/// arguments (which can be whole documents) into error text.
fn cli_method(args: &[OsString]) -> String {
    std::iter::once("herdr".into())
        .chain(args.iter().take(2).map(|arg| arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Herdr's CLI prints refusals as `{"error":{"code","message"}}` on stderr.
fn cli_error(args: &[OsString], output: &Output) -> HostError {
    #[derive(Deserialize)]
    struct ErrorEnvelope {
        error: ErrorBody,
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        code: String,
        #[serde(default)]
        message: String,
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let method = cli_method(args);
    match serde_json::from_str::<ErrorEnvelope>(stderr.trim()) {
        Ok(envelope) => HostError {
            method,
            code: envelope.error.code,
            message: envelope.error.message,
        },
        Err(_) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = [stderr.trim(), stdout.trim()]
                .into_iter()
                .find(|text| !text.is_empty())
                .unwrap_or("no output");
            HostError {
                method,
                code: "command_failed".into(),
                message: format!("{}: {detail}", output.status),
            }
        }
    }
}

/// An injected absolute path is used only if it exists; a relative one is
/// looked up on `PATH`. Anything else falls back to `herdr` on `PATH`.
fn runtime_binary(injected: Option<OsString>) -> OsString {
    match injected {
        Some(binary) if !Path::new(&binary).is_absolute() || Path::new(&binary).is_file() => binary,
        _ => OsString::from("herdr"),
    }
}

/// Test doubles for code that talks to Herdr. Enable with the `test-util`
/// feature from a mode crate's dev-dependencies.
#[cfg(any(test, feature = "test-util"))]
pub mod testing {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::os::unix::prelude::PermissionsExt;

    /// A private socket that answers each request with `reply(request)`; the
    /// request is the whole `{"id","method","params"}` object and the reply the
    /// whole response, so tests can also forge ids. The server stops once the
    /// returned tempdir (which holds the socket) is dropped.
    pub fn fake_host(reply: impl Fn(Value) -> Value + Send + 'static) -> (tempfile::TempDir, Host) {
        // macOS caps socket paths at 104 bytes; its default TMPDIR is too deep.
        let dir = tempfile::Builder::new()
            .prefix("tools-rpc-")
            .tempdir_in("/tmp")
            .expect("create fake host dir");
        let path = dir.path().join("s");
        let listener = UnixListener::bind(&path).expect("bind fake host socket");
        listener
            .set_nonblocking(true)
            .expect("nonblocking fake host");
        let alive = path.clone();
        std::thread::spawn(move || {
            while alive.exists() {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                        let mut line = String::new();
                        if BufReader::new(&stream).read_line(&mut line).is_err() {
                            continue;
                        }
                        let Ok(request) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let _ = writeln!(stream, "{}", reply(request));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        });
        let host = Host::new("herdr", Some(path)).with_timeout(Duration::from_millis(500));
        (dir, host)
    }

    /// `{"id": <request id>, "result": result}`, the usual success reply.
    pub fn ok(request: &Value, result: Value) -> Value {
        json!({"id": request["id"], "result": result})
    }

    /// A `herdr` stand-in: an executable shell script with `body`, in `dir`.
    /// Point [`Host::new`] at the returned path to test CLI calls.
    pub fn fake_cli(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("herdr");
        write_executable(&path, &format!("#!/bin/sh\n{body}\n"));
        path
    }

    /// Writes `source` to `path` as an executable a test can run right away.
    ///
    /// Tests run on parallel threads, and a child forked by one thread holds
    /// every file the others have open until it execs. A script this process
    /// had open for writing can therefore still be open in a sibling's child
    /// when the test runs it, and Linux refuses that exec with "Text file
    /// busy" (ETXTBSY). Staging the text under another name and letting `cp`
    /// create the real file means no write handle to it exists in this process.
    pub fn write_executable(path: &Path, source: &str) {
        let staged = path.with_extension("staged");
        std::fs::write(&staged, source).expect("stage fake executable");
        let copied = std::process::Command::new("cp")
            .arg(&staged)
            .arg(path)
            .status()
            .expect("run cp");
        assert!(copied.success(), "cp to {} failed", path.display());
        std::fs::remove_file(&staged).expect("remove staged executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod fake executable");
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{fake_cli, fake_host, ok};
    use super::*;
    use std::sync::{Arc, Mutex};

    fn recorder() -> (
        Arc<Mutex<Vec<Value>>>,
        impl Fn(Value) -> Value + Send + 'static,
    ) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        (seen, move |request: Value| {
            log.lock().unwrap().push(request.clone());
            if request["method"] == "pane.layout" {
                ok(
                    &request,
                    json!({"layout": {"area": {"x": 30, "y": 0, "width": 482, "height": 60}}}),
                )
            } else {
                ok(&request, json!({"type": "ok"}))
            }
        })
    }

    #[test]
    fn response_ids_are_correlated_and_error_codes_survive() {
        let (_dir, host) = fake_host(|_| json!({"id": "wrong", "result": {}}));
        let error = host.request("pane.get", json!({})).unwrap_err();
        assert!(error.to_string().contains("correlation"), "{error}");

        let (_dir, host) = fake_host(
            |request| json!({"id": request["id"], "error": {"code": "confirmation_required", "message": "stop"}}),
        );
        let error = host
            .request("pane.close", json!({}))
            .context("closing")
            .unwrap_err();
        let refusal = error.downcast_ref::<HostError>().unwrap();
        assert_eq!(refusal.code, "confirmation_required");
        assert_eq!(
            refusal.to_string(),
            "pane.close (confirmation_required): stop"
        );
    }

    #[test]
    fn slow_host_read_has_a_finite_deadline() {
        let (_dir, host) = fake_host(|request| {
            std::thread::sleep(Duration::from_millis(300));
            ok(&request, json!({}))
        });
        let host = host.with_timeout(Duration::from_millis(30));
        let started = Instant::now();
        assert!(host.request("session.snapshot", json!({})).is_err());
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn buffered_large_reply_is_read_after_peer_closes() {
        // Crossing many read chunks used to repeatedly refresh SO_RCVTIMEO on
        // the closed peer. macOS may reject that even with unread buffered data.
        let payload = "x".repeat(128 * 1024);
        let expected = payload.clone();
        let (_dir, host) = fake_host(move |request| ok(&request, json!({"text": payload})));
        assert_eq!(host.request("large", json!({})).unwrap()["text"], expected);
    }

    #[test]
    fn requests_without_a_socket_explain_themselves() {
        let error = Host::new("herdr", None)
            .request("pane.get", json!({}))
            .unwrap_err();
        assert!(error.to_string().contains("HERDR_SOCKET_PATH"), "{error}");
    }

    #[test]
    fn client_width_is_the_pane_area_right_edge() {
        let (seen, reply) = recorder();
        let (_dir, host) = fake_host(reply);
        assert_eq!(host.client_width("w2:p1").unwrap(), 512);
        assert_eq!(host.pane_area("w2:p1").unwrap().width, 482);
        assert_eq!(
            seen.lock().unwrap()[0]["params"],
            json!({"pane_id": "w2:p1"})
        );
    }

    #[test]
    fn sized_popups_send_size_and_env_and_unsized_ones_defer_to_the_manifest() {
        let (seen, reply) = recorder();
        let (_dir, host) = fake_host(reply);
        let size = PopupDims {
            width: "70%".parse().unwrap(),
            height: "95%".parse().unwrap(),
        };
        host.open_popup_sized("scratch", Some(size), &[("HERDR_SCRATCH_NAME", "nvim")])
            .unwrap();
        host.open_popup_sized("move", None, &[]).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["method"], "plugin.pane.open");
        assert_eq!(
            seen[0]["params"],
            json!({"plugin_id": "shadowfax.tools", "entrypoint": "scratch", "placement": "popup",
                   "focus": true, "width": "70%", "height": "95%",
                   "env": {"HERDR_SCRATCH_NAME": "nvim"}})
        );
        assert!(seen[1]["params"].get("width").is_none());
        assert!(seen[1]["params"].get("height").is_none());
    }

    #[test]
    fn cli_returns_stdout_and_decodes_envelopes() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_cli(
            dir.path(),
            r#"[ "$1 $2" = "pane get" ] && echo '{"result":{"pane":{"pane_id":"'"$3"'"}}}'"#,
        );
        let host = Host::new(bin, None);
        let reply: Envelope<Value> = host.cli_json(["pane", "get", "w1:p2"]).unwrap();
        assert_eq!(reply.result["pane"]["pane_id"], "w1:p2");
    }

    #[test]
    fn cli_refusals_keep_herdrs_code() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_cli(
            dir.path(),
            r#"echo '{"error":{"code":"pane_not_found","message":"no pane w9:p9"}}' >&2; exit 1"#,
        );
        let error = Host::new(bin, None)
            .cli(["pane", "close", "w9:p9"])
            .unwrap_err();
        let refusal = error.downcast_ref::<HostError>().unwrap();
        assert_eq!(refusal.method, "herdr pane close");
        assert_eq!(refusal.code, "pane_not_found");
        assert_eq!(refusal.message, "no pane w9:p9");

        let bin = fake_cli(dir.path(), "echo 'plain failure' >&2; exit 3");
        let error = Host::new(bin, None).cli(["tab", "list"]).unwrap_err();
        let refusal = error.downcast_ref::<HostError>().unwrap();
        assert_eq!(refusal.code, "command_failed");
        assert!(refusal.message.contains("plain failure"), "{refusal}");
    }

    #[test]
    fn notifications_are_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args");
        let bin = fake_cli(
            dir.path(),
            &format!("printf '%s\\n' \"$@\" > '{}'", log.display()),
        );
        Host::new(bin, None)
            .notify("Ferry", &"x".repeat(300))
            .unwrap();
        let args = std::fs::read_to_string(log).unwrap();
        let args = args.lines().collect::<Vec<_>>();
        assert_eq!(args[..4], ["notification", "show", "Ferry", "--body"]);
        assert_eq!(args[4].len(), 220);
    }

    #[test]
    fn missing_injected_binary_falls_back_to_path_lookup() {
        let temporary = tempfile::tempdir().unwrap();
        let existing = temporary.path().join("herdr");
        std::fs::write(&existing, "").unwrap();
        assert_eq!(
            runtime_binary(Some(existing.clone().into_os_string())),
            existing.into_os_string()
        );
        assert_eq!(
            runtime_binary(Some(temporary.path().join("missing").into_os_string())),
            OsString::from("herdr")
        );
        assert_eq!(
            runtime_binary(Some(OsString::from("herdr-custom"))),
            OsString::from("herdr-custom")
        );
        assert_eq!(runtime_binary(None), OsString::from("herdr"));
    }
}
