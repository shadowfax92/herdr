use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use herdr_menu::agents_policy::{Client, ErrorCode, Operation, PolicyError};
use serde_json::{json, Value};
use tempfile::TempDir;

// macOS socket paths cannot fit its long default temporary root plus the
// contract state-directory suffix. Keep each isolated fixture under /tmp.
fn tempdir() -> std::io::Result<TempDir> {
    tempfile::Builder::new().prefix("menu-").tempdir_in("/tmp")
}

const HOST: &str = "/tmp/host.sock";

fn snapshot(show: bool) -> Value {
    json!({"ok":true,"version":1,"herdr_socket":HOST,"excluded_labels":["ft"],
        "workspace_ids":["w1","w2"],"excluded_workspace_ids":["w2"],"show_excluded":show})
}

fn fixture(reply: Vec<u8>) -> (TempDir, Client, thread::JoinHandle<Value>) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let request = serde_json::from_str(&line).unwrap();
        let _ = stream.write_all(&reply);
        request
    });
    let client = Client::new(path, "/tmp/unused/.././host.sock").unwrap();
    (dir, client, server)
}

fn wire(value: Value) -> Vec<u8> {
    format!("{value}\n").into_bytes()
}

#[test]
fn tests_that_read_and_repeated_explicit_sets_use_the_frozen_wire_contract() {
    for operation in [
        Operation::Read,
        Operation::Set(true),
        Operation::Set(true),
        Operation::Set(false),
    ] {
        let show = matches!(operation, Operation::Set(true));
        let (_dir, client, server) = fixture(wire(snapshot(show)));
        let reply = client.request(operation).unwrap();
        assert_eq!(reply.show_excluded, show);
        assert_eq!(reply.excluded_workspace_ids, ["w2"]);
        let expected = match operation {
            Operation::Read => json!({"cmd":"workspace_policy","version":1,"herdr_socket":HOST}),
            Operation::Set(show) => {
                json!({"cmd":"workspace_visibility","version":1,"herdr_socket":HOST,"show_excluded":show})
            }
        };
        assert_eq!(server.join().unwrap(), expected);
    }
}

#[test]
fn tests_that_success_must_acknowledge_both_requested_boolean_directions() {
    for requested in [true, false] {
        let (_dir, client, server) = fixture(wire(snapshot(!requested)));
        assert!(client
            .request(Operation::Set(requested))
            .unwrap_err()
            .to_string()
            .contains("acknowledge"));
        server.join().unwrap();
    }
}

#[test]
fn tests_that_domain_errors_retain_codes_and_apply_pending_persisted_intent() {
    for (name, code) in [
        ("unsupported_version", ErrorCode::UnsupportedVersion),
        ("wrong_session", ErrorCode::WrongSession),
        ("invalid_policy", ErrorCode::InvalidPolicy),
        ("invalid_visibility", ErrorCode::InvalidVisibility),
        ("workspace_unavailable", ErrorCode::WorkspaceUnavailable),
        ("not_ready", ErrorCode::NotReady),
    ] {
        let (_dir, client, server) =
            fixture(wire(json!({"ok":false,"code":name,"error":"diagnostic"})));
        assert!(
            matches!(client.request(Operation::Read), Err(PolicyError::Remote {code: actual, show_excluded: None, ..}) if actual == code)
        );
        server.join().unwrap();
    }
    for show in [false, true] {
        let (_dir, client, server) = fixture(wire(
            json!({"ok":false,"code":"apply_pending","error":"host unavailable","show_excluded":show}),
        ));
        assert!(
            matches!(client.request(Operation::Set(show)), Err(PolicyError::Remote {code: ErrorCode::ApplyPending, show_excluded: Some(actual), ..}) if actual == show)
        );
        server.join().unwrap();
    }
}

#[test]
fn tests_that_malformed_wrong_session_unsorted_and_non_subset_replies_fail_closed() {
    let mut invalid = Vec::new();
    for (field, value) in [
        ("ok", json!("true")),
        ("version", json!(2)),
        ("herdr_socket", json!("/tmp/other.sock")),
        ("show_excluded", json!("false")),
        ("excluded_labels", json!(["ft", "ft"])),
        ("workspace_ids", json!(["w2", "w1"])),
        ("excluded_workspace_ids", json!(["w3"])),
        ("excluded_labels", json!([" "])),
        ("workspace_ids", json!(["w\n"])),
    ] {
        let mut reply = snapshot(false);
        reply[field] = value;
        invalid.push(reply);
    }
    for field in [
        "show_excluded",
        "version",
        "workspace_ids",
        "excluded_workspace_ids",
        "excluded_labels",
        "herdr_socket",
    ] {
        let mut reply = snapshot(false);
        reply.as_object_mut().unwrap().remove(field);
        invalid.push(reply);
    }
    invalid.extend([
        json!({"ok":false,"code":"unknown","error":"bad"}),
        json!({"ok":false,"code":"not_ready","error":""}),
        json!({"ok":false,"code":"apply_pending","error":"bad"}),
        json!({"ok":false,"code":"apply_pending","error":"bad","show_excluded":"true"}),
        json!({"ok":false,"code":"not_ready","error":"bad","show_excluded":false}),
    ]);
    for reply in invalid {
        let (_dir, client, server) = fixture(wire(reply.clone()));
        assert!(client.request(Operation::Read).is_err(), "accepted {reply}");
        server.join().unwrap();
    }
}

#[test]
fn tests_that_empty_policy_is_success_and_host_normalization_is_lexical() {
    let mut reply = snapshot(false);
    reply["herdr_socket"] = json!("/tmp/missing/../host.sock");
    for field in ["excluded_labels", "workspace_ids", "excluded_workspace_ids"] {
        reply[field] = json!([]);
    }
    let (_dir, client, server) = fixture(wire(reply));
    assert!(client
        .request(Operation::Read)
        .unwrap()
        .workspace_ids
        .is_empty());
    server.join().unwrap();
    for host in ["", "relative.sock", "/tmp/host\n.sock"] {
        assert!(Client::new("/tmp/unused.sock".into(), host).is_err());
    }
}

#[test]
fn tests_that_reply_framing_size_and_disconnect_are_bounded() {
    let mut exact = serde_json::to_vec(&snapshot(false)).unwrap();
    exact.resize(256 * 1024 - 1, b' ');
    exact.push(b'\n');
    let (_dir, client, server) = fixture(exact.clone());
    assert!(client.request(Operation::Read).is_ok());
    server.join().unwrap();
    exact.insert(0, b' ');
    for reply in [
        exact,
        vec![b'x'; 256 * 1024 + 1],
        serde_json::to_vec(&snapshot(false)).unwrap(),
        b"bad\n".to_vec(),
        vec![],
    ] {
        let (_dir, client, server) = fixture(reply);
        assert!(client.request(Operation::Read).is_err());
        server.join().unwrap();
    }
    let dir = tempdir().unwrap();
    assert!(Client::new(dir.path().join("missing"), HOST)
        .unwrap()
        .request(Operation::Read)
        .is_err());
}

#[test]
fn tests_that_slow_trickle_spends_one_deadline_and_never_retries_a_set() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        for _ in 0..30 {
            if stream.write_all(b" ").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        listener.set_nonblocking(true).unwrap();
        assert!(
            listener.accept().is_err(),
            "client retried an uncertain set"
        );
    });
    let start = Instant::now();
    assert!(Client::new(path, HOST)
        .unwrap()
        .request(Operation::Set(true))
        .is_err());
    assert!(start.elapsed() >= Duration::from_millis(1800));
    assert!(start.elapsed() < Duration::from_millis(2500));
    server.join().unwrap();
}

// Discovery is exercised in a fresh process so parallel tests never mutate the
// process-wide environment or accidentally connect to the owner's live daemon.
#[test]
fn discovery_child() {
    if std::env::var_os("MENU_DISCOVERY_CHILD").is_none() {
        return;
    }
    let result = Client::from_environment().and_then(|client| client.request(Operation::Read));
    if std::env::var_os("MENU_EXPECT_DISCOVERY_ERROR").is_some() {
        assert!(result.is_err());
    } else {
        assert!(!result.unwrap().show_excluded);
    }
}

fn discovery_command(root: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "discovery_child", "--nocapture"])
        .env_clear()
        .env("MENU_DISCOVERY_CHILD", "1")
        .env("HOME", root)
        .env("HERDR_SOCKET_PATH", HOST)
        .env("HERDR_PLUGIN_ID", "shadowfax.menu")
        .env("HERDR_PLUGIN_STATE_DIR", "/wrong/menu/state")
        .env("HERDR_PLUGIN_CONFIG_DIR", "/wrong/menu/config");
    command
}

#[test]
fn tests_that_discovery_uses_agents_root_and_empty_xdg_falls_back_to_home() {
    for mode in ["unset", "empty", "xdg", "override", "empty_override"] {
        let dir = tempdir().unwrap();
        let root = match mode {
            "xdg" => dir.path().join("xdg/herdr/plugins/shadowfax.agents"),
            "override" => dir.path().join("explicit"),
            "empty_override" => dir.path().to_path_buf(),
            _ => dir
                .path()
                .join(".local/state/herdr/plugins/shadowfax.agents"),
        };
        fs::create_dir_all(&root).unwrap();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            stream.write_all(&wire(snapshot(false))).unwrap();
        });
        let mut command = discovery_command(dir.path());
        command.current_dir(dir.path());
        match mode {
            "empty" => {
                command.env("XDG_STATE_HOME", "");
            }
            "xdg" => {
                command.env("XDG_STATE_HOME", dir.path().join("xdg"));
            }
            "override" => {
                command
                    .env("HERDR_AGENTS_STATE", &root)
                    .env("XDG_STATE_HOME", "/ignored");
            }
            "empty_override" => {
                command.env("HERDR_AGENTS_STATE", "");
            }
            _ => {}
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        server.join().unwrap();
    }
    let dir = tempdir().unwrap();
    for host in [None, Some(""), Some("relative.sock")] {
        let mut command = discovery_command(dir.path());
        command
            .env("MENU_EXPECT_DISCOVERY_ERROR", "1")
            .env_remove("HERDR_SOCKET_PATH");
        if let Some(host) = host {
            command.env("HERDR_SOCKET_PATH", host);
        }
        assert!(command.output().unwrap().status.success());
    }
}

use herdr_menu::agents_policy::{PendingRequest, Snapshot};
use herdr_menu::app::{InputOutcome, MenuState};
use herdr_menu::config::{Config, MenuItem};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

fn menu() -> MenuState {
    MenuState::new(Config::parse("[[items]]\ntype='workspace_visibility'\nkey='V'\nlabel='delegated panes'\n[[items]]\nkey='x'\nlabel='custom'\ncommand='custom'\n").unwrap())
}
fn press(key: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)
}
fn typed_snapshot(show: bool) -> Snapshot {
    serde_json::from_value(snapshot(show)).unwrap()
}
fn render_menu(state: &MenuState) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
    terminal
        .draw(|frame| herdr_menu::ui::render(state, frame))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn tests_that_loading_and_setting_disable_only_visibility_and_preserve_custom_noun() {
    let mut state = menu();
    assert!(state.has_visibility_action());
    assert!(render_menu(&state).contains("delegated panes (loading…)"));
    assert_eq!(state.handle_key(press('V')), InputOutcome::Continue);
    assert!(matches!(
        state.handle_key(press('x')),
        InputOutcome::Execute(MenuItem::Shell(_))
    ));
    state.complete_visibility(Ok(typed_snapshot(false)));
    assert!(render_menu(&state).contains("show delegated panes"));
    state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        state.handle_key(press('V')),
        InputOutcome::SetVisibility(true)
    );
    assert!(render_menu(&state).contains("show delegated panes (saving…)"));
    assert_eq!(state.handle_key(press('V')), InputOutcome::Continue);
    assert!(matches!(
        state.handle_key(press('x')),
        InputOutcome::Execute(MenuItem::Shell(_))
    ));
    state.complete_visibility(Ok(typed_snapshot(true)));
    assert!(render_menu(&state).contains("hide delegated panes"));
    let mut repeat = press('V');
    repeat.kind = KeyEventKind::Repeat;
    assert_eq!(state.handle_key(repeat), InputOutcome::Continue);
    state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        state.handle_key(press('V')),
        InputOutcome::SetVisibility(false)
    );
}

#[test]
fn tests_that_timeout_and_pending_application_leave_other_actions_available_without_flipping() {
    for error in [
        PolicyError::Unavailable("two-second request deadline exceeded".into()),
        PolicyError::Remote {
            code: ErrorCode::ApplyPending,
            error: "host rejected filter".into(),
            show_excluded: Some(true),
        },
    ] {
        let mut state = menu();
        state.complete_visibility(Ok(typed_snapshot(false)));
        assert_eq!(
            state.handle_key(press('V')),
            InputOutcome::SetVisibility(true)
        );
        state.complete_visibility(Err(error.clone()));
        let text = render_menu(&state);
        assert!(text.contains("delegated panes (unavailable)"));
        assert!(text.contains("Reopen"));
        assert!(state.visibility_failure().is_some());
        assert_eq!(state.handle_key(press('V')), InputOutcome::Continue);
        assert!(matches!(
            state.handle_key(press('x')),
            InputOutcome::Execute(MenuItem::Shell(_))
        ));
        // A fresh successful read is required before the model offers a new target.
        state.complete_visibility(Ok(typed_snapshot(true)));
        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            state.handle_key(press('V')),
            InputOutcome::SetVisibility(false)
        );
    }
}

#[test]
fn tests_that_background_policy_read_leaves_navigation_responsive() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (release, waiting) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        stream.write_all(&wire(snapshot(false))).unwrap();
    });
    let start = Instant::now();
    let task = PendingRequest::spawn(Client::new(path, HOST), Operation::Read).unwrap();
    let mut state = menu();
    assert!(task.try_complete().is_none());
    assert!(matches!(
        state.handle_key(press('x')),
        InputOutcome::Execute(MenuItem::Shell(_))
    ));
    assert!(start.elapsed() < Duration::from_millis(250));
    release.send(()).unwrap();
    loop {
        if let Some(reply) = task.try_complete() {
            state.complete_visibility(reply);
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        state.handle_key(press('V')),
        InputOutcome::SetVisibility(true)
    );
    server.join().unwrap();
}

#[test]
fn tests_that_default_config_offers_visibility_with_v_and_no_policy_item_does_no_work() {
    let config = Config::parse(herdr_menu::config::DEFAULT_CONFIG).unwrap();
    assert!(config.items.iter().any(|item| matches!(item, MenuItem::WorkspaceVisibility(action) if action.key == 'v' && action.label == "excluded workspaces")));
    let state = MenuState::new(
        Config::parse("[[items]]\nkey='x'\nlabel='custom'\ncommand='custom'\n").unwrap(),
    );
    assert!(!state.has_visibility_action());
}

use herdr_menu::install::upgrade_visibility_config;
use std::os::unix::fs::{symlink, PermissionsExt};

#[test]
fn tests_that_visibility_upgrade_appends_once_preserving_custom_items_and_exact_backup() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = "# owner menu\ntitle='mine'\nwidth=81\n[[items]]\nkey='x' # keep\nlabel='custom'\ncommand='echo custom'\n";
    fs::write(&path, original).unwrap();
    let backup = upgrade_visibility_config(&path).unwrap().unwrap();
    assert_eq!(fs::read_to_string(backup).unwrap(), original);
    let upgraded = fs::read_to_string(&path).unwrap();
    assert!(upgraded.starts_with(original));
    let config = Config::parse(&upgraded).unwrap();
    assert_eq!(config.width, 81);
    assert!(
        matches!(&config.items[1], MenuItem::WorkspaceVisibility(action) if action.key == 'v' && action.label == "excluded workspaces")
    );
    assert!(upgrade_visibility_config(&path).unwrap().is_none());
    assert_eq!(fs::read_to_string(path).unwrap(), upgraded);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn tests_that_custom_visibility_action_is_present_by_type_and_key_conflict_explains_manual_shape() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let custom = "[[items]]\nkey='v'\nlabel='custom'\ncommand='custom'\n[[items]]\ntype='workspace_visibility'\nkey='W'\nlabel='owner workspaces'\n";
    fs::write(&path, custom).unwrap();
    assert!(upgrade_visibility_config(&path).unwrap().is_none());
    assert_eq!(fs::read_to_string(&path).unwrap(), custom);
    let conflict = "# keep\n[[items]]\nkey='v'\nlabel='custom'\ncommand='custom'\n";
    fs::write(&path, conflict).unwrap();
    let error = upgrade_visibility_config(&path).unwrap_err().to_string();
    assert!(error.contains("already bound"));
    assert!(
        error.contains("[[items]]")
            && error.contains("workspace_visibility")
            && error.contains("label =")
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), conflict);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn tests_that_malformed_or_invalid_upgrade_input_never_changes_bytes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    for source in [
        "[[items]\n",
        "items='bad'\n",
        "[[items]]\ntype='workspace_visibility'\nkey='v'\n",
        "[[items]]\nkey='x'\nlabel='missing command'\n",
    ] {
        fs::write(&path, source).unwrap();
        assert!(
            upgrade_visibility_config(&path).is_err(),
            "accepted {source}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), source);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn upgrade_child() {
    let Some(path) = std::env::var_os("MENU_UPGRADE_FIXTURE") else {
        return;
    };
    upgrade_visibility_config(Path::new(&path))
        .unwrap()
        .unwrap();
}

#[test]
fn tests_that_upgrade_preserves_symlink_and_permissions_even_under_restrictive_umask() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("real.toml");
    let link = dir.path().join("config.toml");
    fs::write(&target, "# unchanged comment\ntitle='mine'\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o660)).unwrap();
    symlink(&target, &link).unwrap();
    let output = Command::new("sh")
        .args(["-c", "umask 077; exec \"$@\"", "upgrade-test"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "upgrade_child"])
        .env("MENU_UPGRADE_FIXTURE", &link)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o660
    );
    let backup = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().contains("menu-backup"))
        .unwrap();
    assert_eq!(
        fs::metadata(backup).unwrap().permissions().mode() & 0o777,
        0o660
    );
    fs::remove_file(&target).unwrap();
    assert!(upgrade_visibility_config(&link).is_err());
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn tests_that_upgrade_cli_preserves_global_config_and_existing_installer_modes() {
    let dir = tempdir().unwrap();
    let menu_dir = dir.path().join("menu");
    fs::create_dir(&menu_dir).unwrap();
    let path = menu_dir.join("config.toml");
    fs::write(&path, "# owner\ntitle='mine'\n").unwrap();
    let global = dir.path().join("herdr.toml");
    fs::write(&global, "# global untouched\n").unwrap();
    let fake = dir.path().join("herdr");
    fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$MENU_TEST_LOG\"\nprintf '{}\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-menu"))
        .args(["install", "--upgrade-workspace-visibility"])
        .env("HERDR_PLUGIN_CONFIG_DIR", &menu_dir)
        .env("HERDR_CONFIG_PATH", &global)
        .env("HERDR_BIN_PATH", &fake)
        .env("MENU_TEST_LOG", dir.path().join("calls"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(global).unwrap(), "# global untouched\n");
    assert!(Config::load(&path)
        .unwrap()
        .items
        .iter()
        .any(|i| matches!(i, MenuItem::WorkspaceVisibility(_))));
    for old_flag in ["--add-handoff", "--add-redistribute"] {
        assert!(!Command::new(env!("CARGO_BIN_EXE_herdr-menu"))
            .args(["install", "--upgrade-workspace-visibility", old_flag])
            .output()
            .unwrap()
            .status
            .success());
    }
}

#[test]
fn tests_that_disabled_visibility_is_skipped_by_initial_navigation_and_reenabled_after_read() {
    let mut state = menu();
    assert_eq!(state.selected(), Some(1));
    state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(state.selected(), Some(1));
    let mut only = MenuState::new(
        Config::parse("[[items]]\ntype='workspace_visibility'\nkey='v'\nlabel='workspaces'\n")
            .unwrap(),
    );
    assert_eq!(only.selected(), None);
    only.complete_visibility(Ok(typed_snapshot(false)));
    assert_eq!(only.selected(), Some(0));
    assert_eq!(
        only.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        InputOutcome::SetVisibility(true)
    );
}

#[test]
fn tests_that_failed_set_never_redirects_enter_to_an_unrelated_action() {
    let mut state = MenuState::new(Config::parse(herdr_menu::config::DEFAULT_CONFIG).unwrap());
    state.complete_visibility(Ok(typed_snapshot(false)));
    assert_eq!(
        state.handle_key(press('v')),
        InputOutcome::SetVisibility(true)
    );
    let selected = state.selected();
    state.complete_visibility(Err(PolicyError::Unavailable("timeout".into())));
    assert_eq!(state.selected(), selected);
    for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
        let mut key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        key.kind = kind;
        assert_eq!(state.handle_key(key), InputOutcome::Continue);
    }
}

#[test]
fn tests_that_legacy_press_repeats_cannot_reverse_a_completed_set_without_navigation() {
    for key in [
        press('V'),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    ] {
        let mut state = menu();
        state.complete_visibility(Ok(typed_snapshot(false)));
        assert_eq!(
            state.handle_key(press('V')),
            InputOutcome::SetVisibility(true)
        );
        state.complete_visibility(Ok(typed_snapshot(true)));
        for _ in 0..5 {
            assert_eq!(state.handle_key(key), InputOutcome::Continue);
        }
        assert!(render_menu(&state).contains("hide delegated panes"));
        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            state.handle_key(press('V')),
            InputOutcome::SetVisibility(false)
        );
    }
}

#[test]
fn tests_that_press_during_loading_needs_navigation_after_fresh_reply() {
    let mut state = menu();
    assert_eq!(state.handle_key(press('V')), InputOutcome::Continue);
    state.complete_visibility(Ok(typed_snapshot(false)));
    assert_eq!(state.handle_key(press('V')), InputOutcome::Continue);
    state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        state.handle_key(press('V')),
        InputOutcome::SetVisibility(true)
    );
}

#[test]
fn tests_that_pending_intent_recovery_and_long_diagnostics_are_accessible_at_narrow_widths() {
    for width in [40, 64, 68] {
        let mut state = menu();
        state.complete_visibility(Err(PolicyError::Remote {
            code: ErrorCode::ApplyPending,
            error: format!("{} END_DIAGNOSTIC", "long failure ".repeat(40)),
            show_excluded: Some(true),
        }));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 14)).unwrap();
        let mut frames = String::new();
        for _ in 0..30 {
            terminal
                .draw(|frame| herdr_menu::ui::render(&state, frame))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            frames.push_str(&text);
            state.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        }
        assert!(
            frames.contains("show_excluded=true"),
            "width {width}: missing persisted intent"
        );
        assert!(
            frames.contains("application pending"),
            "width {width}: missing application status"
        );
        assert!(
            frames.contains("Reopen menu to refresh"),
            "width {width}: missing recovery"
        );
        assert!(
            frames.contains("END_DIAGNOSTIC"),
            "width {width}: diagnostic clipped"
        );
        assert!(matches!(
            state.handle_key(press('x')),
            InputOutcome::Execute(MenuItem::Shell(_))
        ));
    }
}

#[test]
fn tests_that_enter_on_an_only_loading_action_cannot_activate_after_reply() {
    let mut state = MenuState::new(
        Config::parse("[[items]]\ntype='workspace_visibility'\nkey='v'\nlabel='workspaces'\n")
            .unwrap(),
    );
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(state.handle_key(enter), InputOutcome::Continue);
    state.complete_visibility(Ok(typed_snapshot(false)));
    assert_eq!(state.handle_key(enter), InputOutcome::Continue);
    state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(state.handle_key(enter), InputOutcome::SetVisibility(true));
}
