//! The reload config key driven through `ClientShellState`: the bytes Ghostty
//! sends go through the input parser, the client reloads a real config file,
//! and the server's reply comes back through `handle_endpoint_result`. What an
//! observer sees is the `server.reload_config` request and the toast.

use super::*;
use crate::api::schema::{Method, ResponseResult};
use crate::app::state::FeedbackTone;
use crate::config::ConfigReloadStatus;

/// The owner's binding, next to their `alt+r` resize binding.
const OWNER_KEYS: &str =
    "onboarding = false\n[keys]\nreload_config = \"ctrl+alt+r\"\nresize_mode = \"alt+r\"\n";
/// Ghostty's ctrl+alt+r without the kitty keyboard protocol: ESC, then ^R.
const LEGACY_CTRL_ALT_R: &[u8] = b"\x1b\x12";
/// Ghostty's ctrl+alt+r under the kitty flags Herdr pushes (7 or 31).
const KITTY_CTRL_ALT_R: &[u8] = b"\x1b[114;7u";
const KITTY_CTRL_ALT_R_RELEASE: &[u8] = b"\x1b[114;7:3u";
const STALE_SERVER_ISSUE: &str = "unknown config section [agent_key_rules]; ignoring section";

/// Points `load_live_config` at a temp `config.toml` for one test, holding the
/// shared config env lock so no other test reloads from it meanwhile.
struct ConfigFile {
    path: std::path::PathBuf,
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl ConfigFile {
    fn new(name: &str, toml: &str) -> Self {
        let lock = crate::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir =
            std::env::temp_dir().join(format!("herdr-reload-notice-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp config dir");
        let path = dir.join("config.toml");
        std::fs::write(&path, toml).expect("write config");
        let previous = std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR);
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        Self {
            path,
            previous,
            _lock: lock,
        }
    }

    fn write(&self, toml: &str) {
        std::fs::write(&self.path, toml).expect("rewrite config");
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(previous) => std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, previous),
            None => std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR),
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// A client started from the file's config. It takes the file so the caller
/// holds the env lock for as long as the client reloads from it.
fn client(_file: &ConfigFile) -> ClientShellState {
    let config = crate::config::load_live_config()
        .expect("test config loads")
        .config;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    state
}

/// The id of the one `server.reload_config` request `outcome` sends.
fn reload_request(outcome: &ClientShellInput) -> Option<String> {
    let requests = outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.method, Method::ServerReloadConfig(_)) =>
            {
                Some(request.id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(requests.len() <= 1, "one reload per press: {requests:?}");
    requests.into_iter().next()
}

fn press_reload(state: &mut ClientShellState) -> String {
    let outcome = state.handle_input_bytes(KITTY_CTRL_ALT_R);
    reload_request(&outcome).expect("ctrl+alt+r sends server.reload_config")
}

fn reply(
    state: &mut ClientShellState,
    request_id: &str,
    status: ConfigReloadStatus,
    diagnostics: &[&str],
) -> bool {
    let (repaint, actions) = state.handle_endpoint_result(
        "boot-1",
        request_id,
        Ok(ResponseResult::ConfigReload {
            status,
            diagnostics: diagnostics
                .iter()
                .map(|issue| (*issue).to_owned())
                .collect(),
        }),
    );
    assert!(actions.is_empty());
    repaint
}

fn notice(state: &ClientShellState) -> Option<(&str, FeedbackTone)> {
    state
        .copy_feedback
        .as_ref()
        .map(|feedback| (feedback.message.as_str(), feedback.tone))
}

#[test]
fn ctrl_alt_r_reloads_config_in_both_ghostty_encodings() {
    let file = ConfigFile::new("encodings", OWNER_KEYS);
    let mut state = client(&file);

    for bytes in [LEGACY_CTRL_ALT_R, KITTY_CTRL_ALT_R] {
        let outcome = state.handle_input_bytes(bytes);
        assert!(reload_request(&outcome).is_some(), "{bytes:?} reloads");
        assert_eq!(state.mode, ClientShellMode::Terminal, "{bytes:?}");
        assert!(
            outcome.requests.is_empty(),
            "{bytes:?} never reaches the pane"
        );
    }
    let release = state.handle_input_bytes(KITTY_CTRL_ALT_R_RELEASE);
    assert!(
        reload_request(&release).is_none(),
        "the release stays with herdr"
    );
    assert!(release.requests.is_empty());

    // alt+r alone is still the owner's resize key.
    let alt_r = state.handle_input_bytes(b"\x1br");
    assert!(reload_request(&alt_r).is_none());
    assert_eq!(state.mode, ClientShellMode::Resize);
}

#[test]
fn a_clean_reload_says_so_on_screen() {
    let file = ConfigFile::new("clean", OWNER_KEYS);
    let mut state = client(&file);

    let request = press_reload(&mut state);
    assert_eq!(notice(&state), None, "the notice waits for the server");
    assert!(reply(
        &mut state,
        &request,
        ConfigReloadStatus::Applied,
        &[]
    ));

    assert_eq!(
        notice(&state),
        Some(("config reloaded", FeedbackTone::Success))
    );
    // The reload dropped the pane surface; the server's next frame restores it.
    state.set_pane_surface(surface());
    let frame = state.compose(100, 30).expect("frame");
    assert!(
        frame_rows(&frame)
            .iter()
            .any(|row| row.contains("● config reloaded")),
        "the toast is drawn"
    );

    // Pressing again shows it again, not only the first time.
    state.tick_copy_feedback(std::time::Instant::now() + std::time::Duration::from_secs(3));
    assert_eq!(notice(&state), None);
    let request = press_reload(&mut state);
    reply(&mut state, &request, ConfigReloadStatus::Applied, &[]);
    assert_eq!(
        notice(&state),
        Some(("config reloaded", FeedbackTone::Success))
    );
}

#[test]
fn an_issue_only_the_client_reports_is_named_as_the_clients() {
    let file = ConfigFile::new(
        "client-issue",
        &format!("{OWNER_KEYS}[ui]\nmouse_captur = true\n"),
    );
    let mut state = client(&file);

    let request = press_reload(&mut state);
    reply(&mut state, &request, ConfigReloadStatus::Applied, &[]);

    assert_eq!(
        notice(&state),
        Some((
            "config reloaded, 1 issue (client): unknown config key ui.mouse_captur; ignoring key",
            FeedbackTone::Warning
        ))
    );
}

/// A server older than the client rejects a section the client knows. The
/// notice names it as the server's so the config does not look broken.
#[test]
fn an_issue_only_a_stale_server_reports_is_named_as_the_servers() {
    let file = ConfigFile::new(
        "server-issue",
        &format!("{OWNER_KEYS}[agent_key_rules.claude]\n\"ctrl+d\" = \"drop\"\n"),
    );
    let mut state = client(&file);

    let request = press_reload(&mut state);
    reply(
        &mut state,
        &request,
        ConfigReloadStatus::Partial,
        &[STALE_SERVER_ISSUE],
    );

    assert_eq!(
        notice(&state),
        Some((
            "config reloaded, 1 issue (server): unknown config section [agent_key_rules]; ignoring section",
            FeedbackTone::Warning
        ))
    );
}

#[test]
fn an_issue_both_sides_report_counts_once() {
    let file = ConfigFile::new(
        "shared-issue",
        &format!("{OWNER_KEYS}[ui]\nmouse_captur = true\n"),
    );
    let mut state = client(&file);
    let shared = "unknown config key ui.mouse_captur; ignoring key";

    let request = press_reload(&mut state);
    reply(&mut state, &request, ConfigReloadStatus::Partial, &[shared]);
    assert_eq!(
        notice(&state),
        Some((
            "config reloaded, 1 issue: unknown config key ui.mouse_captur; ignoring key",
            FeedbackTone::Warning
        ))
    );

    // A shared issue plus one of the server's own: two issues, no single side.
    let request = press_reload(&mut state);
    reply(
        &mut state,
        &request,
        ConfigReloadStatus::Partial,
        &[STALE_SERVER_ISSUE, shared],
    );
    assert_eq!(
        notice(&state),
        Some((
            "config reloaded, 2 issues: unknown config key ui.mouse_captur; ignoring key",
            FeedbackTone::Warning
        ))
    );
}

#[test]
fn a_config_that_does_not_parse_fails_with_its_first_line() {
    let file = ConfigFile::new("broken", OWNER_KEYS);
    let mut state = client(&file);
    file.write(&format!("{OWNER_KEYS}[ui\n"));
    let parse_error = match crate::config::load_live_config() {
        Err(diagnostics) => diagnostics,
        Ok(_) => panic!("the broken config must not load"),
    };

    let request = press_reload(&mut state);
    let server = parse_error.iter().map(String::as_str).collect::<Vec<_>>();
    reply(&mut state, &request, ConfigReloadStatus::Failed, &server);

    let (message, tone) = notice(&state).expect("failure notice");
    assert_eq!(tone, FeedbackTone::Error);
    assert!(
        message.starts_with(
            "config reload failed, 1 issue: config parse error: TOML parse error at line 5"
        ),
        "{message}"
    );
    assert!(
        !message.contains('\n'),
        "the toast is one line: {message:?}"
    );
}

#[test]
fn a_failed_reload_request_says_why() {
    let file = ConfigFile::new("request-failed", OWNER_KEYS);
    let mut state = client(&file);

    let request = press_reload(&mut state);
    let (repaint, _) = state.handle_endpoint_result(
        "boot-1",
        &request,
        Err(ClientShellEndpointError {
            code: Some("endpoint_timeout".into()),
            message: "This server did not respond to server.reload_config.".into(),
        }),
    );

    assert!(repaint);
    assert_eq!(
        notice(&state),
        Some((
            "config reload failed: This server did not respond to server.reload_config.",
            FeedbackTone::Error
        ))
    );
}

#[test]
fn a_reload_with_no_server_to_ask_fails_at_once() {
    let file = ConfigFile::new("offline", OWNER_KEYS);
    let mut state = client(&file);
    for endpoint in &mut state.endpoints {
        endpoint.status = ClientEndpointStatus::Connecting;
    }

    // Twice: the endpoint notice for an unready server shows only once.
    for _ in 0..2 {
        state.copy_feedback = None;
        let outcome = state.handle_input_bytes(LEGACY_CTRL_ALT_R);
        assert!(reload_request(&outcome).is_none());
        assert!(outcome.repaint);
        let (message, tone) = notice(&state).expect("failure notice");
        assert_eq!(tone, FeedbackTone::Error);
        assert!(
            message.starts_with("config reload failed: ") && message.ends_with(" is not ready"),
            "{message}"
        );
    }
}

#[test]
fn a_settings_change_shows_the_reload_notice() {
    let file = ConfigFile::new("settings", OWNER_KEYS);
    let mut state = client(&file);
    state.open_settings_overlay();

    let mut outcome = ClientShellInput::default();
    state.apply_settings_choice(&mut outcome);
    let request = reload_request(&outcome).expect("the settings edit reloads config");
    reply(&mut state, &request, ConfigReloadStatus::Applied, &[]);

    assert_eq!(
        notice(&state),
        Some(("config reloaded", FeedbackTone::Success))
    );
}
