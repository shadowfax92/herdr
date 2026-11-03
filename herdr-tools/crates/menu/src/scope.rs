//! Focus and hide from the menu: open Herdr's native picker, or flip focus.
//!
//! Focus and hide are the fork's own (one `ScopeState` on the local server,
//! applied by the client to every machine). The menu never reads or edits the
//! scope itself: it asks Herdr to open its picker (`scope.pick`) or to flip
//! focus (`scope.toggle`), and the owner's client does either against Local.
//!
//! The menu talks to the server of the machine its source pane is on, which
//! is a remote one (studio) when that pane is. Reading and setting the scope
//! there would change studio's scope, which no client applies: the client
//! reads the scope only from Local. Both requests therefore hand the work to
//! the client, which knows where Local is.
//!
//! `scope.toggle` opens nothing, so the menu sends it from its popup and exits.
//! The picker opens on the client, and the menu is a popup on that client, so
//! the popup must be gone first or the picker would open beneath it. The menu
//! process is the popup and closing it ends the process, so a detached helper
//! sends the pick once the menu has exited:
//!
//! ```text
//!  menu popup                          helper (herdr-tools menu scope-pick <mode>)
//!   spawn helper ─ stdin: pipe ───────▶ blocks reading stdin
//!   popup.close ─▶ Herdr closes the popup, ending the menu process
//!   (exits)       ─ pipe closes (EOF) ─▶ scope.pick {mode}; a refusal is a notification
//! ```
//!
//! Ordering: Herdr's app loop handles one request at a time, and `popup.close`
//! clears the popup before it signals the popup's processes. The menu dies
//! from that signal (or exits once the close returns), so the pipe closes
//! only after Herdr has taken `popup.close`, and the helper's `scope.pick`
//! queues behind it. Seen on Herdr 0.9.5: the menu died about 2 ms into a
//! 25 ms `popup.close`, and the helper's next request completed after it.
//!
//! The helper must start its own session (`setsid`) before `popup.close`:
//! Herdr tears a closed popup down by signalling every process in the popup's
//! session (SIGHUP, then SIGTERM, then SIGKILL), so a helper that only had its
//! own process group died with the menu before it ran a line.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use tools_core::scope::is_unknown_method;
use tools_core::{Host, HostError};

use crate::config::ScopePickMode;

/// The socket method that flips focus on the owner's client.
pub const SCOPE_TOGGLE: &str = "scope.toggle";

/// The menu reads `scope.get` before its first frame; a wedged server may
/// delay the menu this long, never longer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// The helper waits at most this long for the menu to exit before picking.
const MENU_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Title of the helper's notifications (the menu's own title).
const NOTIFICATION_TITLE: &str = "Herdr Menu";

/// What this Herdr offers for focus and hide, read once as the menu opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScopeSupport {
    /// Not read, or the read failed for a reason other than a missing method.
    /// The rows stay usable, and a failure shows when one is used.
    #[default]
    Unknown,
    /// This Herdr has no scope methods: the rows read "(unavailable)".
    Missing,
    /// This Herdr has scope.
    Available,
}

impl ScopeSupport {
    /// Reads `scope.get`. Neither `scope.pick` nor `scope.toggle` can be probed
    /// without acting (opening the picker, flipping focus), and the scope
    /// methods ship as one feature, so a Herdr that knows `scope.get` is taken
    /// to know them all. A build with `scope.get` but without one of the others
    /// shows that refusal when its row is used.
    pub fn probe(host: &Host) -> Self {
        match host.clone().with_timeout(PROBE_TIMEOUT).scope() {
            Ok(Some(_)) => Self::Available,
            Ok(None) => Self::Missing,
            Err(_) => Self::Unknown,
        }
    }
}

/// The menu's two scope actions, over the menu's Herdr client.
#[derive(Clone, Debug)]
pub struct ScopeActions {
    host: Host,
}

impl ScopeActions {
    pub fn new(host: Host) -> Self {
        Self { host }
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    /// Asks the owner's client to flip focus on Local (`scope.toggle`, no
    /// params), keeping its targets. The client does the flip, so this works
    /// from a pane on any machine. A refusal becomes a readable error for the
    /// menu to show, and the popup then stays open.
    pub fn toggle_focus(&self) -> Result<()> {
        self.host
            .request(SCOPE_TOGGLE, json!({}))
            .map(drop)
            .map_err(|error| anyhow!(toggle_failure(&error)))
    }

    /// Hands the pick to a detached helper, then closes the menu's popup; the
    /// caller exits right after. `spawn_helper` starts
    /// `herdr-tools menu scope-pick <mode>` with its stdin piped from this
    /// process (see [`spawn_pick_helper`]).
    ///
    /// The helper picks at EOF on that pipe, which comes when this process
    /// drops the child or dies, and both happen only after `popup.close` has
    /// returned or ended the process. Dropping the child here, before the
    /// close, would let the picker open beneath the popup.
    pub fn hand_off_pick(
        &self,
        mode: ScopePickMode,
        spawn_helper: impl FnOnce(ScopePickMode) -> Result<std::process::Child>,
    ) -> Result<()> {
        let helper = spawn_helper(mode).context("starting the scope picker")?;
        // A failed close (no popup, timeout) still ends in this process
        // exiting, which closes the popup; only the ordering is then loose.
        if let Err(error) = self.host.request("popup.close", json!({})) {
            eprintln!(
                "Herdr Menu: popup.close before the {} picker: {error:#}",
                mode.as_str()
            );
        }
        drop(helper);
        Ok(())
    }
}

/// Starts the pick helper: this executable as `menu scope-pick <mode>`,
/// detached (see [`detach`]). It inherits the popup's Herdr environment
/// (`HERDR_SOCKET_PATH`, `HERDR_BIN_PATH`).
pub fn spawn_pick_helper(mode: ScopePickMode) -> Result<std::process::Child> {
    let mut command = tools_core::self_command(crate::MODE)?;
    command.args(["scope-pick", mode.as_str()]);
    detach(command).context("failed to start the scope picker helper")
}

/// Spawns `command` in a new session, with its stdin piped from this process
/// (EOF tells it this process has exited) and no stdout or stderr. The new
/// session is what survives the popup's teardown; by the time `spawn` returns
/// the child has left the popup's session, so a `popup.close` sent next cannot
/// reach it.
pub fn detach(mut command: Command) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the hook runs between fork and exec and only calls setsid, which
    // is async-signal-safe and touches no state shared with the parent.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command.spawn()?)
}

/// `herdr-tools menu scope-pick <mode>`: waits for the menu to exit (EOF on
/// stdin), then opens the picker. A refusal becomes a notification, since the
/// helper has no screen of its own.
pub fn run_pick_helper(mode: ScopePickMode) -> Result<()> {
    wait_for_eof(std::io::stdin(), MENU_EXIT_TIMEOUT);
    let host = Host::from_env();
    let result = pick(&host, mode);
    if let Err(error) = &result {
        let _ = host.notify(NOTIFICATION_TITLE, &pick_failure(mode, error));
    }
    result
}

/// `scope.pick {mode}`: opens Herdr's picker on the foreground client.
pub fn pick(host: &Host, mode: ScopePickMode) -> Result<()> {
    host.request("scope.pick", json!({"mode": mode.as_str()}))?;
    Ok(())
}

/// The notification a failed pick shows, by Herdr's refusal code.
pub fn pick_failure(mode: ScopePickMode, error: &anyhow::Error) -> String {
    let mode = mode.as_str();
    if is_unknown_method(error) {
        return format!("Could not open the {mode} picker: this Herdr has no focus and hide");
    }
    match error
        .downcast_ref::<HostError>()
        .map(|error| error.code.as_str())
    {
        Some("no_client") => {
            format!("Could not open the {mode} picker: no Herdr client is attached")
        }
        _ => format!("Could not open the {mode} picker: {error:#}"),
    }
}

/// Why `scope.toggle` was refused, by Herdr's refusal code.
pub fn toggle_failure(error: &anyhow::Error) -> String {
    if is_unknown_method(error) {
        return format!("this Herdr has no {SCOPE_TOGGLE}; update Herdr");
    }
    match error
        .downcast_ref::<HostError>()
        .map(|error| error.code.as_str())
    {
        Some("no_client") => "no Herdr client is attached".into(),
        _ => format!("{error:#}"),
    }
}

/// Blocks until `input` reaches EOF or `timeout` passes. The read runs on a
/// thread so a menu that never exits cannot hold the helper forever.
fn wait_for_eof(mut input: impl Read + Send + 'static, timeout: Duration) {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let mut sink = [0; 64];
        while matches!(input.read(&mut sink), Ok(count) if count > 0) {}
        let _ = done.send(());
    });
    let _ = finished.recv_timeout(timeout);
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use serde_json::Value;
    use tools_core::testing::{fake_cli, fake_host, ok};

    use super::*;

    fn unknown(request: &Value) -> Value {
        let method = request["method"].as_str().unwrap();
        json!({"id": request["id"], "error": {"code": "invalid_request",
            "message": format!("invalid request: unknown variant `{method}`, expected one of `ping`")}})
    }

    /// A fake Herdr that records every request, answers scope.get with an
    /// empty scope and anything else with ok.
    fn recording() -> (Arc<Mutex<Vec<Value>>>, tempfile::TempDir, Host) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let (dir, host) = fake_host(move |request| {
            log.lock().unwrap().push(request.clone());
            match request["method"].as_str().unwrap() {
                "scope.get" => ok(
                    &request,
                    json!({"type": "scope", "scope": {"focus": {"active": true, "targets": []}},
                           "local": {"tabs": []}}),
                ),
                _ => ok(&request, json!({"type": "ok"})),
            }
        });
        (seen, dir, host)
    }

    fn methods(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|request| request["method"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn probe_tells_available_from_missing_and_unknown() {
        let (seen, _dir, host) = recording();
        assert_eq!(ScopeSupport::probe(&host), ScopeSupport::Available);
        assert_eq!(methods(&seen), ["scope.get"]);
        let (_dir, host) = fake_host(|request| unknown(&request));
        assert_eq!(ScopeSupport::probe(&host), ScopeSupport::Missing);
        let (_dir, host) = fake_host(
            |request| json!({"id": request["id"], "error": {"code": "internal", "message": "boom"}}),
        );
        assert_eq!(ScopeSupport::probe(&host), ScopeSupport::Unknown);
        assert_eq!(
            ScopeSupport::probe(&Host::new("herdr", None)),
            ScopeSupport::Unknown
        );
    }

    #[test]
    fn toggle_hands_the_flip_to_the_client_and_never_touches_the_scope() {
        // The menu may be on a remote machine's server; reading or setting the
        // scope there would flip a scope no client applies.
        let (seen, _dir, host) = recording();
        ScopeActions::new(host).toggle_focus().unwrap();
        assert_eq!(methods(&seen), ["scope.toggle"]);
        assert_eq!(seen.lock().unwrap()[0]["params"], json!({}));
    }

    #[test]
    fn toggle_refusals_read_by_code() {
        let (_dir, host) = fake_host(
            |request| json!({"id": request["id"], "error": {"code": "no_client", "message": "no client attached"}}),
        );
        let error = ScopeActions::new(host).toggle_focus().unwrap_err();
        assert_eq!(error.to_string(), "no Herdr client is attached");

        // A Herdr with scope.get but without scope.toggle, or without scope.
        let (_dir, host) = fake_host(|request| unknown(&request));
        let error = ScopeActions::new(host).toggle_focus().unwrap_err();
        assert_eq!(
            error.to_string(),
            "this Herdr has no scope.toggle; update Herdr"
        );

        let (_dir, host) = fake_host(
            |request| json!({"id": request["id"], "error": {"code": "internal", "message": "boom"}}),
        );
        let error = ScopeActions::new(host).toggle_focus().unwrap_err();
        assert!(error.to_string().contains("boom"), "{error}");
    }

    #[test]
    fn hand_off_starts_the_helper_before_closing_the_popup() {
        for mode in [ScopePickMode::Focus, ScopePickMode::Hide] {
            let (seen, _dir, host) = recording();
            let log = seen.clone();
            ScopeActions::new(host)
                .hand_off_pick(mode, |picked| {
                    assert_eq!(picked, mode);
                    // The popup must still be open when the helper starts.
                    assert!(log.lock().unwrap().is_empty());
                    Ok(Command::new("true").spawn().unwrap())
                })
                .unwrap();
            // The menu never picks itself: the helper does, after it exits.
            assert_eq!(methods(&seen), ["popup.close"]);
        }
    }

    #[test]
    fn a_helper_that_cannot_start_leaves_the_popup_open() {
        let (seen, _dir, host) = recording();
        let error = ScopeActions::new(host)
            .hand_off_pick(ScopePickMode::Hide, |_| Err(anyhow!("no exe")))
            .unwrap_err();
        assert!(format!("{error:#}").contains("no exe"));
        assert!(methods(&seen).is_empty());
    }

    #[test]
    fn pick_sends_the_mode_and_refusals_read_by_code() {
        let (seen, _dir, host) = recording();
        pick(&host, ScopePickMode::Hide).unwrap();
        pick(&host, ScopePickMode::Focus).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["method"], "scope.pick");
        assert_eq!(seen[0]["params"], json!({"mode": "hide"}));
        assert_eq!(seen[1]["params"], json!({"mode": "focus"}));

        let (_dir, host) = fake_host(
            |request| json!({"id": request["id"], "error": {"code": "no_client", "message": "no client attached"}}),
        );
        let error = pick(&host, ScopePickMode::Focus).unwrap_err();
        assert_eq!(
            pick_failure(ScopePickMode::Focus, &error),
            "Could not open the focus picker: no Herdr client is attached"
        );
        let (_dir, host) = fake_host(|request| unknown(&request));
        let error = pick(&host, ScopePickMode::Hide).unwrap_err();
        assert!(pick_failure(ScopePickMode::Hide, &error).contains("no focus and hide"));
    }

    #[test]
    fn helper_failures_reach_the_user_as_a_notification() {
        let (dir, socket_host) = fake_host(
            |request| json!({"id": request["id"], "error": {"code": "no_client", "message": "no client"}}),
        );
        let log = dir.path().join("notified");
        let cli = fake_cli(
            dir.path(),
            &format!("printf '%s\\n' \"$@\" > '{}'", log.display()),
        );
        let host = Host::new(cli, socket_host.socket_path().map(Into::into));
        let error = pick(&host, ScopePickMode::Hide).unwrap_err();
        host.notify(
            NOTIFICATION_TITLE,
            &pick_failure(ScopePickMode::Hide, &error),
        )
        .unwrap();
        let args = std::fs::read_to_string(log).unwrap();
        assert!(args.contains("Herdr Menu"), "{args}");
        assert!(args.contains("no Herdr client is attached"), "{args}");
    }

    #[test]
    fn the_helper_leaves_the_popup_session_and_hears_the_menu_exit() {
        let mut child = detach({
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "cat >/dev/null"]);
            command
        })
        .unwrap();
        let pid = child.id() as libc::pid_t;
        // SAFETY: getsid only reads the session ID of a live process.
        let (helper_session, own_session) = unsafe { (libc::getsid(pid), libc::getsid(0)) };
        assert_eq!(helper_session, pid);
        assert_ne!(helper_session, own_session);
        // Dropping the pipe is how the menu's exit reaches the helper.
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn the_helper_waits_for_eof_but_not_forever() {
        let started = Instant::now();
        wait_for_eof(std::io::empty(), Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));

        struct Never;
        impl Read for Never {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_secs(10));
                Ok(0)
            }
        }
        let started = Instant::now();
        wait_for_eof(Never, Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn modes_round_trip_their_wire_spelling() {
        for mode in [ScopePickMode::Focus, ScopePickMode::Hide] {
            assert_eq!(mode.as_str().parse::<ScopePickMode>().unwrap(), mode);
        }
        assert!("sideways".parse::<ScopePickMode>().is_err());
    }
}
