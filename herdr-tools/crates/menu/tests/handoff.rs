use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{json, Value};
use tools_core::testing::write_executable;
use tools_menu::handoff::HandoffController;
use tools_menu::herdr::Herdr;

/// Each test gets a process adapter with its own fixtures and command log. The
/// adapter refuses unknown commands so a capture cannot accidentally mutate UI.
struct Runtime {
    directory: tempfile::TempDir,
    binary: PathBuf,
}

impl Runtime {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("herdr");
        write_executable(
            &binary,
            r#"#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
if pathlib.Path(__file__).name != 'herdr':
    (root / 'clipboard-called').touch()
    sys.exit(0)
state = json.loads((root / 'state.json').read_text())
args = sys.argv[1:]
socket = os.environ.get('HERDR_SOCKET_PATH')
with (root / 'calls.jsonl').open('a') as log:
    log.write(json.dumps({'args': args, 'socket': socket}) + '\n')
if state.get('fail') == args:
    print('snapshot unavailable', file=sys.stderr)
    sys.exit(4)
if args == ['session', 'list', '--json']:
    print(json.dumps({'sessions': state['sessions']}))
elif args == ['api', 'snapshot'] and socket in state['snapshots']:
    print(json.dumps({'result': {'snapshot': state['snapshots'][socket]}}))
elif args[:2] in [['pane', 'read'], ['pane', 'run'], ['agent', 'prompt'], ['agent', 'wait']]:
    print('{}')
else:
    print('unexpected command or session', file=sys.stderr)
    sys.exit(3)
"#,
        );
        let runtime = Self { directory, binary };
        runtime.save(json!({
            "sessions": [
                {"name": "default", "socket_path": "/fake/default.sock", "running": true},
                {"name": "other", "socket_path": "/fake/other.sock", "running": true}
            ],
            "snapshots": {
                "/fake/default.sock": snapshot("term_default"),
                "/fake/other.sock": snapshot("term_other")
            }
        }));
        runtime
    }

    fn save(&self, state: Value) {
        fs::write(self.directory.path().join("state.json"), state.to_string()).unwrap();
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.directory.path().join("state.json")).unwrap())
            .unwrap()
    }

    fn controller(&self, socket: &str, pane: &str) -> HandoffController {
        HandoffController::new(
            Herdr::new(&self.binary).with_socket(socket),
            pane.to_string(),
        )
        .with_search_path(Some(self.command_path()))
    }

    fn capture(&self, pane: &str) -> Value {
        let document = self
            .controller("/fake/default.sock", pane)
            .capture()
            .unwrap();
        assert_eq!(document.pane_count, 1);
        record(&document.text)
    }

    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.directory.path().join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn command_path(&self) -> std::ffi::OsString {
        std::env::join_paths(std::iter::once(self.directory.path().to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap()
    }
}

fn record(text: &str) -> Value {
    serde_json::from_str(
        text.split_once("```json\n")
            .unwrap()
            .1
            .split_once("\n```")
            .unwrap()
            .0,
    )
    .unwrap()
}

fn commands(text: &str) -> &str {
    text.split_once("```sh\n")
        .unwrap()
        .1
        .strip_suffix("\n```\n")
        .expect("the document ends after the command block")
}

fn snapshot(terminal: &str) -> Value {
    json!({
        "focused_pane_id": "w2:p2",
        "workspaces": [
            {"workspace_id": "w1", "label": "build"},
            {"workspace_id": "w2", "label": "other work"}
        ],
        "tabs": [
            {"workspace_id": "w1", "tab_id": "w1:t1", "label": "implementation"},
            {"workspace_id": "w2", "tab_id": "w2:t1", "label": "popup focus"}
        ],
        "panes": [
            {
                "pane_id": "w1:p1", "terminal_id": terminal,
                "workspace_id": "w1", "tab_id": "w1:t1", "label": "pane annotation",
                "terminal_title": "raw title", "terminal_title_stripped": "clean title",
                "cwd": "/repo", "foreground_cwd": "/repo/feature",
                "agent": "codex", "agent_status": "working",
                "agent_session": {"source": "herdr:codex", "agent": "codex", "kind": "id", "value": "native-id"},
                "tokens": {"secret": "NEVER_COPY_RENDERING_TOKENS"},
                "scroll": {"text": "NEVER_COPY_TRANSCRIPTS"}
            },
            {
                "pane_id": "w1:p2", "terminal_id": "term_shell",
                "workspace_id": "w1", "tab_id": "w1:t1", "label": "shell label",
                "terminal_title": "shell", "cwd": "/repo", "agent_status": "unknown"
            },
            {
                "pane_id": "w2:p2", "terminal_id": "term_popup",
                "workspace_id": "w2", "tab_id": "w2:t1", "agent_status": "unknown",
                "terminal_title": "MENU_POPUP_DO_NOT_COPY"
            }
        ],
        "agents": [{"pane_id": "w1:p1", "terminal_id": terminal, "name": "real-agent-name"}]
    })
}

#[test]
fn capture_uses_source_pane_instead_of_focused_popup() {
    let runtime = Runtime::new();
    let document = runtime
        .controller("/fake/default.sock", "w1:p1")
        .capture()
        .unwrap();
    assert_eq!(document.pane_count, 1);
    assert!(document.text.starts_with("# Herdr pane context\n"));
    let json = document
        .text
        .split_once("```json\n")
        .unwrap()
        .1
        .split_once("\n```")
        .unwrap()
        .0;
    assert_eq!(
        json,
        r#"{
  "pane_id": "w1:p1",
  "session": "default",
  "agent": "codex",
  "label": "pane annotation",
  "tab": "implementation",
  "workspace": "build",
  "cwd": "/repo/feature"
}"#
    );
    assert_eq!(
        commands(&document.text),
        "herdr --session default pane read w1:p1 --lines 120\n\
         herdr --session default agent prompt w1:p1 \"<message>\" --wait\n\
         herdr --session default agent wait w1:p1"
    );
    assert!(!document.text.contains("MENU_POPUP_DO_NOT_COPY"));
    assert!(!document.text.contains("term_shell"));
    assert!(!document.text.contains("NEVER_COPY"));
    assert_eq!(runtime.calls().len(), 2);
}

#[test]
fn identical_pane_ids_in_other_sessions_use_that_sessions_data_and_commands() {
    let runtime = Runtime::new();
    let mut state = runtime.state();
    state["snapshots"]["/fake/other.sock"]["panes"][0]["label"] = json!("other session pane");
    runtime.save(state);
    let document = runtime
        .controller("/fake/other.sock", "w1:p1")
        .capture()
        .unwrap();
    let data = record(&document.text);
    assert_eq!(data["session"], "other");
    assert_eq!(data["label"], "other session pane");
    assert!(!document.text.contains("default"));
    assert!(!document.text.contains("pane annotation"));
    assert_eq!(
        commands(&document.text),
        "herdr --session other pane read w1:p1 --lines 120\n\
         herdr --session other agent prompt w1:p1 \"<message>\" --wait\n\
         herdr --session other agent wait w1:p1"
    );
    assert!(runtime
        .calls()
        .iter()
        .all(|call| call["socket"] == "/fake/other.sock"));
}

#[test]
fn label_prefers_pane_annotation_then_agent_name_for_the_same_terminal_only() {
    let runtime = Runtime::new();
    let data = runtime.capture("w1:p1");
    assert_eq!(data["label"], "pane annotation");
    let mut state = runtime.state();
    state["snapshots"]["/fake/default.sock"]["panes"][0]["label"] = Value::Null;
    runtime.save(state.clone());
    assert_eq!(runtime.capture("w1:p1")["label"], "real-agent-name");
    for (pane_id, terminal_id) in [("w1:p1", "stale-terminal"), ("w1:p2", "term_default")] {
        let agent = &mut state["snapshots"]["/fake/default.sock"]["agents"][0];
        agent["pane_id"] = json!(pane_id);
        agent["terminal_id"] = json!(terminal_id);
        runtime.save(state.clone());
        assert!(runtime.capture("w1:p1").get("label").is_none());
    }
}

#[test]
fn shells_get_only_read_and_run_commands_and_no_agent_key() {
    let runtime = Runtime::new();
    let document = runtime
        .controller("/fake/default.sock", "w1:p2")
        .capture()
        .unwrap();
    assert_eq!(
        record(&document.text),
        json!({
            "pane_id": "w1:p2", "session": "default", "label": "shell label",
            "tab": "implementation", "workspace": "build", "cwd": "/repo"
        })
    );
    assert_eq!(
        commands(&document.text),
        "herdr --session default pane read w1:p2 --lines 120\n\
         herdr --session default pane run w1:p2 \"<command>\""
    );
}

#[test]
fn a_different_first_herdr_on_path_uses_socket_routing() {
    let runtime = Runtime::new();
    let other = tempfile::tempdir().unwrap();
    write_executable(
        &other.path().join("herdr"),
        "#!/usr/bin/python3\nimport pathlib\npathlib.Path(__file__).with_suffix('.called').touch()\nraise SystemExit(99)\n",
    );
    // The real capturing binary is later on PATH. Only the first herdr can
    // determine where an emitted command would go; no probe may execute it.
    let path = std::env::join_paths([other.path(), runtime.directory.path()]).unwrap();
    let document = runtime
        .controller("/fake/default.sock", "w1:p1")
        .with_search_path(Some(path))
        .capture()
        .unwrap();
    let data = record(&document.text);
    assert_eq!(data["socket"], "/fake/default.sock");
    assert!(data.get("session").is_none());
    assert!(commands(&document.text)
        .lines()
        .all(|line| line.starts_with("HERDR_SOCKET_PATH='/fake/default.sock' herdr ")));
    assert_eq!(runtime.calls().len(), 2);
    assert!(!other.path().join("herdr.called").exists());
}

#[test]
fn the_same_herdr_on_path_keeps_session_routing() {
    let runtime = Runtime::new();
    let document = runtime
        .controller("/fake/default.sock", "w1:p1")
        .capture()
        .unwrap();
    assert_eq!(record(&document.text)["session"], "default");
    assert!(record(&document.text).get("socket").is_none());
    assert!(commands(&document.text)
        .lines()
        .all(|line| line.starts_with("herdr --session default ")));
}

#[cfg(unix)]
#[test]
fn a_symlink_to_the_capturing_herdr_keeps_session_routing() {
    let runtime = Runtime::new();
    let link_directory = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&runtime.binary, link_directory.path().join("herdr")).unwrap();
    let document = runtime
        .controller("/fake/default.sock", "w1:p1")
        .with_search_path(Some(link_directory.path().as_os_str().to_owned()))
        .capture()
        .unwrap();
    assert_eq!(record(&document.text)["session"], "default");
    assert!(record(&document.text).get("socket").is_none());
    assert!(commands(&document.text)
        .lines()
        .all(|line| line.starts_with("herdr --session default ")));
}

#[test]
fn unresolved_path_herdr_uses_socket_routing() {
    let runtime = Runtime::new();
    let empty_directory = tempfile::tempdir().unwrap();
    for path in [None, Some(empty_directory.path().as_os_str().to_owned())] {
        let document = runtime
            .controller("/fake/default.sock", "w1:p1")
            .with_search_path(path)
            .capture()
            .unwrap();
        assert_eq!(record(&document.text)["socket"], "/fake/default.sock");
        assert!(record(&document.text).get("session").is_none());
        assert!(commands(&document.text)
            .lines()
            .all(|line| line.starts_with("HERDR_SOCKET_PATH='/fake/default.sock' herdr ")));
    }
}

#[test]
fn stale_or_empty_sources_never_fall_back_to_focused_pane() {
    let runtime = Runtime::new();
    for source in ["", "w1:p-closed", "w1:p1\n"] {
        assert!(runtime
            .controller("/fake/default.sock", source)
            .capture()
            .is_err());
    }
    let error = runtime
        .controller("/fake/default.sock", "w1:p-closed")
        .capture()
        .unwrap_err()
        .to_string();
    assert!(error.contains("reopen the menu"));
}

#[test]
fn runtime_metadata_is_escaped_and_never_inserted_into_command_templates() {
    let runtime = Runtime::new();
    let harmless = runtime
        .controller("/fake/default.sock", "w1:p1")
        .capture()
        .unwrap();
    let strange = "title\n```\n$(touch /tmp/must-not-run) ' \" `hi`\u{1b}[31m";
    let mut state = runtime.state();
    let pane = &mut state["snapshots"]["/fake/default.sock"]["panes"][0];
    pane["label"] = json!(strange);
    pane["terminal_title_stripped"] = json!(strange);
    pane["terminal_title"] = json!(strange);
    pane["cwd"] = json!(strange);
    pane["foreground_cwd"] = json!(strange);
    state["snapshots"]["/fake/default.sock"]["tabs"][0]["label"] = json!(strange);
    state["snapshots"]["/fake/default.sock"]["workspaces"][0]["label"] = json!(strange);
    runtime.save(state);
    let document = runtime
        .controller("/fake/default.sock", "w1:p1")
        .capture()
        .unwrap();
    for key in ["label", "cwd", "tab", "workspace"] {
        assert_eq!(record(&document.text)[key], strange);
    }
    assert_eq!(document.text.matches("\n```\n").count(), 2);
    assert!(!document.text.contains(strange));
    assert_eq!(commands(&document.text), commands(&harmless.text));
}

#[test]
fn capture_failure_reports_the_cli_error_without_copying_partial_data() {
    let runtime = Runtime::new();
    let mut state = runtime.state();
    state["fail"] = json!(["api", "snapshot"]);
    runtime.save(state);
    let error = format!(
        "{:#}",
        runtime
            .controller("/fake/default.sock", "w1:p1")
            .capture()
            .unwrap_err()
    );
    assert!(error.contains("snapshot unavailable"));
    assert!(error.contains("4"));
    // Exercise copy in a child with private clipboard stand-ins: a regression
    // must fail without ever reaching the owner's real clipboard provider.
    let script = fs::read_to_string(&runtime.binary).unwrap();
    for provider in ["pbcopy", "wl-copy", "xclip", "xsel"] {
        write_executable(&runtime.directory.path().join(provider), &script);
    }
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-tools-menu"))
        .args(["menu", "copy-context", "--source-pane", "w1:p1"])
        .env("HERDR_ENV", "1")
        .env("HERDR_BIN_PATH", &runtime.binary)
        .env("HERDR_SOCKET_PATH", "/fake/default.sock")
        .env("PATH", runtime.command_path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("snapshot unavailable"));
    assert!(!runtime.directory.path().join("clipboard-called").exists());
}

#[test]
fn missing_or_failed_session_discovery_keeps_socket_routing_with_shell_quoting() {
    let runtime = Runtime::new();
    let socket = "/fake/owner's session.sock";
    let original = runtime.state();
    // Empty discovery, a list that contains only other sockets, and a failed
    // request must all retain the exact socket rather than guess a session.
    for mode in ["empty", "unmatched", "failed"] {
        let mut state = original.clone();
        state["snapshots"][socket] = snapshot("term_quoted");
        if mode == "empty" {
            state["sessions"] = json!([]);
        } else if mode == "failed" {
            state["fail"] = json!(["session", "list", "--json"]);
        }
        runtime.save(state);
        for pane in ["w1:p1", "w1:p2"] {
            let document = runtime.controller(socket, pane).capture().unwrap();
            let data = record(&document.text);
            assert_eq!(data["socket"], socket);
            assert!(data.get("session").is_none());
            let expected = if pane == "w1:p1" {
                "HERDR_SOCKET_PATH='/fake/owner'\\''s session.sock' herdr pane read w1:p1 --lines 120\n\
                 HERDR_SOCKET_PATH='/fake/owner'\\''s session.sock' herdr agent prompt w1:p1 \"<message>\" --wait\n\
                 HERDR_SOCKET_PATH='/fake/owner'\\''s session.sock' herdr agent wait w1:p1"
            } else {
                "HERDR_SOCKET_PATH='/fake/owner'\\''s session.sock' herdr pane read w1:p2 --lines 120\n\
                 HERDR_SOCKET_PATH='/fake/owner'\\''s session.sock' herdr pane run w1:p2 \"<command>\""
            };
            assert_eq!(commands(&document.text), expected);
            let output = Command::new("sh")
                .args(["-c", commands(&document.text)])
                .env("PATH", runtime.command_path())
                .env("HERDR_SOCKET_PATH", "/wrong/inherited.sock")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let calls = runtime.calls();
            let emitted = if pane == "w1:p1" {
                vec![
                    json!(["pane", "read", "w1:p1", "--lines", "120"]),
                    json!(["agent", "prompt", "w1:p1", "<message>", "--wait"]),
                    json!(["agent", "wait", "w1:p1"]),
                ]
            } else {
                vec![
                    json!(["pane", "read", "w1:p2", "--lines", "120"]),
                    json!(["pane", "run", "w1:p2", "<command>"]),
                ]
            };
            let received = &calls[calls.len() - emitted.len()..];
            for (call, args) in received.iter().zip(emitted) {
                assert_eq!(call["socket"], socket);
                assert_eq!(call["args"], args);
            }
        }
    }
}

#[test]
fn empty_socket_never_falls_back_to_another_session() {
    let runtime = Runtime::new();
    let error = runtime
        .controller("", "w1:p1")
        .capture()
        .unwrap_err()
        .to_string();
    assert!(error.contains("socket path must be absolute"));
    assert!(!runtime.directory.path().join("calls.jsonl").exists());
}

#[test]
fn unusable_session_names_fall_back_to_socket_routing() {
    let runtime = Runtime::new();
    for name in [
        "",
        "-option",
        ".",
        "..",
        "../other",
        "two words",
        "a:b",
        "a;b",
        "$(id)",
        "a\n```",
        "a'b",
        "é",
        &"a".repeat(65),
    ] {
        let mut state = runtime.state();
        state["sessions"][0]["name"] = json!(name);
        runtime.save(state);
        let document = runtime
            .controller("/fake/default.sock", "w1:p1")
            .capture()
            .unwrap();
        let data = record(&document.text);
        assert_eq!(data["socket"], "/fake/default.sock", "{name:?}");
        assert!(data.get("session").is_none());
        assert!(commands(&document.text)
            .lines()
            .all(|line| line.starts_with("HERDR_SOCKET_PATH='/fake/default.sock' herdr ")));
    }
}

#[test]
fn pane_ids_must_be_safe_shell_arguments_even_if_present_in_the_snapshot() {
    let runtime = Runtime::new();
    for id in [
        "",
        " ",
        "-option",
        ".hidden",
        "_hidden",
        "w1:p1;id",
        "w1:p1$(id)",
        "w1:p1'",
        "w1:p1\n",
        "w1:p1\u{1b}",
        "w1:é",
    ] {
        let mut state = runtime.state();
        state["snapshots"]["/fake/default.sock"]["panes"][0]["pane_id"] = json!(id);
        runtime.save(state);
        let error = runtime
            .controller("/fake/default.sock", id)
            .capture()
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid source pane ID"), "{error}");
    }
    assert!(!runtime.directory.path().join("calls.jsonl").exists());
    let mut state = runtime.state();
    state["snapshots"]["/fake/default.sock"]["panes"][0]["pane_id"] = json!("A1:p.2_x-y");
    runtime.save(state);
    assert_eq!(runtime.capture("A1:p.2_x-y")["pane_id"], "A1:p.2_x-y");
}

#[test]
fn cwd_prefers_foreground_and_missing_metadata_is_omitted() {
    let runtime = Runtime::new();
    assert_eq!(runtime.capture("w1:p1")["cwd"], "/repo/feature");
    let mut state = runtime.state();
    state["snapshots"]["/fake/default.sock"]["panes"][0]["foreground_cwd"] = Value::Null;
    runtime.save(state.clone());
    assert_eq!(runtime.capture("w1:p1")["cwd"], "/repo");
    let snapshot = &mut state["snapshots"]["/fake/default.sock"];
    for field in ["label", "agent", "cwd", "foreground_cwd"] {
        snapshot["panes"][0].as_object_mut().unwrap().remove(field);
    }
    snapshot["agents"] = json!([]);
    snapshot["workspaces"][0]["label"] = Value::Null;
    snapshot["tabs"][0].as_object_mut().unwrap().remove("label");
    runtime.save(state);
    assert_eq!(
        runtime.capture("w1:p1"),
        json!({"pane_id": "w1:p1", "session": "default"})
    );
}

#[test]
fn discarded_runtime_values_never_appear_in_the_document() {
    let runtime = Runtime::new();
    let document = runtime
        .controller("/fake/default.sock", "w1:p1")
        .capture()
        .unwrap();
    for value in [
        "term_default",
        "raw title",
        "clean title",
        "native-id",
        "working",
        "herdr:codex",
        "real-agent-name",
        "NEVER_COPY",
    ] {
        assert!(!document.text.contains(value), "unexpected {value:?}");
    }
    for key in [
        "captured_at_unix_seconds",
        "terminal_id",
        "terminal_title",
        "tab_id",
        "workspace_id",
        "agent_name",
        "agent_status",
        "agent_session",
        "foreground_cwd",
    ] {
        assert!(!document.text.contains(key), "unexpected {key:?}");
    }
}
