use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use serde_json::Value;
use tempfile::tempdir;
use tools_core::testing::write_executable;
use tools_core::Invocation;
use tools_menu::agent_panes::AgentPaneController;
use tools_menu::config::ForkPlacement;
use tools_menu::herdr::Herdr;

struct FakeHerdr {
    _directory: tempfile::TempDir,
    binary: PathBuf,
    fallback_cwd: PathBuf,
    socket_worker: Option<std::thread::JoinHandle<()>>,
    stop_socket: Arc<AtomicBool>,
}

impl FakeHerdr {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let fallback_cwd = directory.path().join("fallback-project");
        fs::create_dir(&fallback_cwd).unwrap();
        let binary = directory.path().join("herdr");
        write_executable(
            &binary,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "$0.log"
if [ -f "$0.fail-$1-$2" ]; then
  printf 'simulated %s %s failure\n' "$1" "$2" >&2
  exit 9
fi
if [ "$1 $2" = "pane split" ] && [ -f "$0.block-state" ]; then
  mkdir -p "$(sed -n '1p' "$0.block-state")"
fi
if [ "$1 $2" = "pane get" ]; then
  printf '{{"result":{{"pane":{{"pane_id":"%s","cwd":"{}","foreground_cwd":"{}","agent":"codex","agent_session":{{"source":"herdr:codex","agent":"codex","kind":"id","value":"native-session-id"}}}}}}}}\n' "$3"
elif [ "$1 $2" = "pane split" ]; then
  printf '%s' "$6" > "$0.split"
  printf '%s\n' '{{"result":{{"pane":{{"pane_id":"w1:pF"}}}}}}'
elif [ "$1 $2" = "pane layout" ]; then
  printf '%s\n' '{{"result":{{"layout":{{"workspace_id":"w1","tab_id":"w1:t1","zoomed":false,"focused_pane_id":"w1:p1","area":{{"x":0,"y":0,"width":240,"height":60}},"panes":[{{"pane_id":"w1:p1","rect":{{"x":0,"y":0,"width":240,"height":60}}}}]}}}}}}'
elif [ "$1 $2" = "pane zoom" ]; then
  printf '%s\n' '{{"result":{{"type":"pane_zoom"}}}}'
elif [ "$1 $2" = "pane run" ]; then
  printf '%s\n' '{{"result":{{"type":"input_sent"}}}}'
elif [ "$1 $2" = "pane swap" ]; then
  printf '%s' "$6" > "$0.swap"
  printf '%s\n' '{{"result":{{"swap":{{"changed":true}}}}}}'
elif [ "$1 $2" = "pane close" ]; then
  printf '%s\n' '{{"result":{{"type":"pane_closed"}}}}'
elif [ "$1 $2" = "notification show" ]; then
  printf '%s\n' '{{"result":{{"type":"ok"}}}}'
else
  printf '%s\n' 'unsupported fake command' >&2
  exit 2
fi
"#,
                fallback_cwd.display(),
                fallback_cwd.display()
            ),
        );
        let listener = UnixListener::bind(directory.path().join("api.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop_socket = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stop_socket);
        let fake_binary = binary.clone();
        // The socket double reads the same mutation markers as the CLI double,
        // so fork topology checks cross the real transport seam in these tests.
        // It also records every request: the popup now opens over the socket
        // (`plugin.pane.open`) rather than `herdr plugin pane open`.
        let socket_worker = std::thread::spawn(move || {
            use serde_json::json;
            while !stop.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                };
                // macOS inherits O_NONBLOCK from the listener. Request reads
                // must wait for the client write instead of racing with it.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let mut requests = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(fake_binary.with_extension("socket.jsonl"))
                    .unwrap();
                writeln!(requests, "{request}").unwrap();
                match request["method"].as_str() {
                    Some("plugin.pane.open") => {
                        writeln!(
                            stream,
                            "{}",
                            json!({"id":request["id"],"result":{"type":"ok"}})
                        )
                        .unwrap();
                        continue;
                    }
                    Some("pane.layout") => {
                        let area = json!({"x":26,"y":0,"width":174,"height":49});
                        let response =
                            json!({"id":request["id"],"result":{"layout":{"area":area}}});
                        writeln!(stream, "{response}").unwrap();
                        continue;
                    }
                    _ => {}
                }
                let original = json!({"type":"pane","pane_id":"w1:p1"});
                let mut root = original.clone();
                if let Ok(direction) = fs::read_to_string(fake_binary.with_extension("split")) {
                    let fork = json!({"type":"pane","pane_id":"w1:pF"});
                    let swap =
                        fs::read_to_string(fake_binary.with_extension("swap")).unwrap_or_default();
                    let (first, second) = if swap == "left" || swap == "up" {
                        (fork, original)
                    } else {
                        (original, fork)
                    };
                    root = json!({"type":"split","direction":direction,"ratio":0.5,"first":first,"second":second});
                }
                let response = json!({"id":request["id"],"result":{"layout":{"workspace_id":"w1","tab_id":"w1:t1","zoomed":false,"root":root}}});
                writeln!(stream, "{response}").unwrap();
            }
        });
        Self {
            _directory: directory,
            binary,
            fallback_cwd,
            socket_worker: Some(socket_worker),
            stop_socket,
        }
    }

    fn client(&self) -> Herdr {
        Herdr::new(&self.binary).with_socket(self.binary.parent().unwrap().join("api.sock"))
    }

    fn log(&self) -> String {
        fs::read_to_string(self.binary.with_extension("log")).unwrap_or_default()
    }

    /// Every socket request so far, in order.
    fn requests(&self) -> Vec<Value> {
        fs::read_to_string(self.binary.with_extension("socket.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The params of the one `plugin.pane.open` request, if any.
    fn popup_open(&self) -> Option<Value> {
        let opens = self
            .requests()
            .into_iter()
            .filter(|request| request["method"] == "plugin.pane.open")
            .collect::<Vec<_>>();
        assert!(opens.len() <= 1, "{opens:?}");
        opens
            .into_iter()
            .next()
            .map(|request| request["params"].clone())
    }

    fn fail_command(&self, command: &str) {
        fs::write(format!("{}.fail-{}", self.binary.display(), command), "").unwrap();
    }

    fn block_state_write_after_split(&self, state_path: &Path) {
        fs::write(
            format!("{}.block-state", self.binary.display()),
            format!("{}\n", state_path.display()),
        )
        .unwrap();
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        self.stop_socket.store(true, Ordering::Relaxed);
        let result = self.socket_worker.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

/// The shared config file as `launch` receives it from `Config::current()`.
fn write_config(path: &Path, source: &str) -> tools_core::Config {
    fs::write(path, source).unwrap();
    tools_core::Config::load(path).unwrap()
}

#[test]
fn launch_opens_the_native_popup_with_source_context_and_configured_size() {
    let fake = FakeHerdr::new();
    let source_cwd = fake.binary.parent().unwrap().join("source-project");
    fs::create_dir(&source_cwd).unwrap();
    let config = write_config(
        &fake.binary.parent().unwrap().join("config.toml"),
        r#"
[popup.size]
menu = { width = 72, height = 32 }

[menu]
max_height = 18
[[menu.items]]
key = "a"
label = "alpha"
command = "alpha"
[[menu.items]]
key = "b"
label = "beta"
command = "beta"
"#,
    );
    let invocation = Invocation::from_context_json(&format!(
        r#"{{"focused_pane_id":"w1:p1","focused_pane_cwd":"{}"}}"#,
        source_cwd.display()
    ))
    .unwrap();

    fake.client().launch(&invocation, Some(&config)).unwrap();

    let open = fake.popup_open().expect("popup opened");
    assert_eq!(open["plugin_id"], "shadowfax.tools");
    assert_eq!(open["entrypoint"], "menu");
    assert_eq!(open["placement"], "popup");
    assert_eq!(
        (open["width"].as_u64(), open["height"].as_u64()),
        (Some(72), Some(32))
    );
    assert_eq!(
        open["env"]["HERDR_MENU_SOURCE_CWD"],
        source_cwd.display().to_string()
    );
    assert_eq!(open["env"]["HERDR_MENU_SOURCE_PANE_ID"], "w1:p1");
    // Nothing else crossed the CLI: no profile needed the client width either.
    assert!(fake.log().is_empty(), "{}", fake.log());
    assert_eq!(fake.requests().len(), 1);
}

#[test]
fn launch_without_a_size_uses_the_menu_width_and_item_count_height() {
    let fake = FakeHerdr::new();
    let config = write_config(
        &fake.binary.parent().unwrap().join("config.toml"),
        "[menu]\nmax_height = 18\n[[menu.items]]\nkey = 'a'\nlabel = 'alpha'\ncommand = 'alpha'\n",
    );
    let invocation = Invocation::from_context_json(&format!(
        r#"{{"focused_pane_id":"w1:p1","focused_pane_cwd":"{}"}}"#,
        fake.fallback_cwd.display()
    ))
    .unwrap();

    fake.client().launch(&invocation, Some(&config)).unwrap();

    let open = fake.popup_open().unwrap();
    assert_eq!(
        (open["width"].as_u64(), open["height"].as_u64()),
        (Some(64), Some(6))
    );
}

#[test]
fn launch_matches_popup_profiles_on_the_source_client_width() {
    let fake = FakeHerdr::new();
    let config = write_config(
        &fake.binary.parent().unwrap().join("config.toml"),
        r#"
[popup.size]
menu = { width = 80, height = 32 }
[[popup.profile]]
name = "laptop"
max_client_width = 310
size.menu = { width = "60%", height = "70%" }
"#,
    );
    let invocation = Invocation::from_context_json(&format!(
        r#"{{"focused_pane_id":"w1:p1","focused_pane_cwd":"{}"}}"#,
        fake.fallback_cwd.display()
    ))
    .unwrap();

    fake.client().launch(&invocation, Some(&config)).unwrap();

    let requests = fake.requests();
    assert_eq!(requests[0]["method"], "pane.layout");
    assert_eq!(requests[0]["params"]["pane_id"], "w1:p1");
    let open = fake.popup_open().unwrap();
    assert_eq!(
        (open["width"].as_str(), open["height"].as_str()),
        (Some("60%"), Some("70%"))
    );
}

#[test]
fn launch_resolves_missing_source_cwd_from_the_focused_pane() {
    let fake = FakeHerdr::new();
    let config = write_config(&fake.binary.parent().unwrap().join("config.toml"), "");
    let invocation = Invocation::from_context_json(r#"{"focused_pane_id":"w1:p9"}"#).unwrap();

    fake.client().launch(&invocation, Some(&config)).unwrap();

    let log = fake.log();
    assert!(log.lines().any(|line| line == "pane get w1:p9"));
    assert!(!log.contains("--cwd"));
    let open = fake.popup_open().unwrap();
    assert_eq!(
        open["env"]["HERDR_MENU_SOURCE_CWD"],
        fake.fallback_cwd.display().to_string()
    );
    assert_eq!(open["env"]["HERDR_MENU_SOURCE_PANE_ID"], "w1:p9");
    // The built-in menu with its "no [menu] table" warning: the old 68 × 16.
    assert_eq!(
        (open["width"].as_u64(), open["height"].as_u64()),
        (Some(68), Some(16))
    );
}

#[test]
fn invalid_config_uses_fallback_dimensions_but_still_opens() {
    let fake = FakeHerdr::new();
    let path = fake.binary.parent().unwrap().join("config.toml");
    fs::write(&path, "[[menu.items]\n").unwrap();
    assert!(tools_core::Config::load(&path).is_err());
    let invocation = Invocation::from_context_json(&format!(
        r#"{{"focused_pane_id":"w1:p1","focused_pane_cwd":"{}"}}"#,
        fake.fallback_cwd.display()
    ))
    .unwrap();

    // `launch_from_environment` passes None when `Config::current()` fails.
    fake.client().launch(&invocation, None).unwrap();

    let open = fake.popup_open().unwrap();
    assert_eq!(
        (open["width"].as_u64(), open["height"].as_u64()),
        (Some(64), Some(16))
    );
}

#[test]
fn missing_pane_context_notifies_without_opening() {
    let fake = FakeHerdr::new();
    let config = write_config(&fake.binary.parent().unwrap().join("config.toml"), "");
    let invocation = Invocation::from_context_json("{}").unwrap();

    let error = fake
        .client()
        .launch_with_reporting(&invocation, Some(&config))
        .unwrap_err()
        .to_string();

    assert!(error.contains("source pane ID"));
    let log = fake.log();
    assert!(log.contains("notification show Herdr Menu --body"));
    assert!(fake.popup_open().is_none());
}

#[test]
fn agent_fork_splits_runs_exact_rtr_session_and_rotates_the_tracked_pane() {
    let fake = FakeHerdr::new();
    let state = fake.binary.parent().unwrap().join("state/last-fork.json");
    let controller = AgentPaneController::new(
        fake.client(),
        "w1:p1",
        &fake.fallback_cwd,
        &state,
        "/opt/RTR tools/rtr",
    );

    controller.fork(ForkPlacement::Left).unwrap();
    // Splitting focuses the fork, so the next popup is owned by that pane.
    // Rotation must accept either member of the persisted pane pair.
    let focused_fork_controller = AgentPaneController::new(
        fake.client(),
        "w1:pF",
        &fake.fallback_cwd,
        &state,
        "/opt/RTR tools/rtr",
    );
    focused_fork_controller.rotate().unwrap();

    let log = fake.log();
    assert!(log.contains("pane get w1:p1"), "{log}");
    assert!(
        log.contains(&format!(
            "pane split --pane w1:p1 --direction right --cwd {} --focus",
            fake.fallback_cwd.display()
        )),
        "{log}"
    );
    assert!(
        log.contains("pane swap --pane w1:pF --direction left"),
        "{log}"
    );
    assert!(
        log.contains("pane run w1:pF '/opt/RTR tools/rtr' fork native-session-id --tool codex"),
        "{log}"
    );
    assert!(
        log.contains("pane swap --pane w1:pF --direction right"),
        "{log}"
    );
    let state_json: serde_json::Value = serde_json::from_slice(&fs::read(state).unwrap()).unwrap();
    assert_eq!(state_json["schema_version"], 1);
    assert_eq!(state_json["source_pane_id"], "w1:p1");
    assert_eq!(state_json["fork_pane_id"], "w1:pF");
}

#[test]
fn top_fork_splits_down_then_swaps_the_new_pane_above_the_source() {
    let fake = FakeHerdr::new();
    let state = fake.binary.parent().unwrap().join("state/last-fork.json");
    let controller =
        AgentPaneController::new(fake.client(), "w1:p1", &fake.fallback_cwd, state, "rtr");

    controller.fork(ForkPlacement::Top).unwrap();

    let log = fake.log();
    assert!(
        log.contains(&format!(
            "pane split --pane w1:p1 --direction down --cwd {} --focus",
            fake.fallback_cwd.display()
        )),
        "{log}"
    );
    assert!(
        log.contains("pane swap --pane w1:pF --direction up"),
        "{log}"
    );
    assert!(
        log.contains("pane run w1:pF rtr fork native-session-id --tool codex"),
        "{log}"
    );
}

#[test]
fn fork_reports_both_left_placement_and_cleanup_failures() {
    let fake = FakeHerdr::new();
    fake.fail_command("pane-swap");
    fake.fail_command("pane-close");
    let state = fake.binary.parent().unwrap().join("state/last-fork.json");
    let controller =
        AgentPaneController::new(fake.client(), "w1:p1", &fake.fallback_cwd, state, "rtr");

    let error = controller
        .fork(ForkPlacement::Left)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("placing the fork pane on the left"),
        "{error}"
    );
    assert!(error.contains("cleanup failed"), "{error}");
    assert!(error.contains("w1:pF"), "{error}");
    assert!(fake.log().contains("pane close w1:pF"));
}

#[test]
fn state_write_failure_closes_the_empty_pane_before_rtr_starts() {
    let fake = FakeHerdr::new();
    let state = fake.binary.parent().unwrap().join("state/last-fork.json");
    fake.block_state_write_after_split(&state);
    let controller =
        AgentPaneController::new(fake.client(), "w1:p1", &fake.fallback_cwd, &state, "rtr");

    let error = controller
        .fork(ForkPlacement::Right)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("remembering the fork pane for rotation"),
        "{error}"
    );
    let log = fake.log();
    assert!(log.contains("pane close w1:pF"), "{log}");
    assert!(!log.contains("pane run"), "{log}");
}

#[test]
fn run_failure_closes_the_pane_and_restores_the_previous_rotation_target() {
    let fake = FakeHerdr::new();
    fake.fail_command("pane-run");
    let state = fake.binary.parent().unwrap().join("state/last-fork.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    let previous = b"{\"schema_version\":1,\"source_pane_id\":\"old-source\",\"fork_pane_id\":\"old-fork\",\"next_direction\":2}\n";
    fs::write(&state, previous).unwrap();
    let controller =
        AgentPaneController::new(fake.client(), "w1:p1", &fake.fallback_cwd, &state, "rtr");

    let error = controller
        .fork(ForkPlacement::Right)
        .unwrap_err()
        .to_string();

    assert!(error.contains("starting RTR in the fork pane"), "{error}");
    assert!(fake.log().contains("pane close w1:pF"));
    assert_eq!(fs::read(state).unwrap(), previous);
}

#[test]
fn failed_run_keeps_new_state_when_the_orphan_pane_cannot_be_closed() {
    let fake = FakeHerdr::new();
    fake.fail_command("pane-run");
    fake.fail_command("pane-close");
    let state = fake.binary.parent().unwrap().join("state/last-fork.json");
    let controller =
        AgentPaneController::new(fake.client(), "w1:p1", &fake.fallback_cwd, &state, "rtr");

    let error = controller
        .fork(ForkPlacement::Right)
        .unwrap_err()
        .to_string();

    assert!(error.contains("cleanup failed"), "{error}");
    assert!(error.contains("w1:pF"), "{error}");
    let state_json: serde_json::Value = serde_json::from_slice(&fs::read(state).unwrap()).unwrap();
    assert_eq!(state_json["fork_pane_id"], "w1:pF");
}
