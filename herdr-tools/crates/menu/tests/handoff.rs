use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use herdr_menu::handoff::HandoffController;
use herdr_menu::herdr::Herdr;
use serde_json::{json, Value};

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
        fs::write(
            &binary,
            r#"#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
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
else:
    print('unexpected command or session', file=sys.stderr)
    sys.exit(3)
"#,
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
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
    let data = record(&document.text);
    assert_eq!(document.pane_count, 1);
    assert_eq!(data["pane"]["pane_id"], "w1:p1");
    assert_eq!(data["workspace"]["label"], "build");
    assert_eq!(data["tab"]["label"], "implementation");
    assert!(!document.text.contains("MENU_POPUP_DO_NOT_COPY"));
    assert!(!document.text.contains("term_shell"));
    assert!(!document.text.contains("NEVER_COPY"));
    assert_eq!(runtime.calls().len(), 2);
}

#[test]
fn identical_pane_ids_in_other_sessions_keep_socket_and_terminal_identity() {
    let runtime = Runtime::new();
    let document = runtime
        .controller("/fake/other.sock", "w1:p1")
        .capture()
        .unwrap();
    let data = record(&document.text);
    assert_eq!(data["session"]["name"], "other");
    assert_eq!(data["session"]["socket_path"], "/fake/other.sock");
    assert_eq!(data["pane"]["terminal_id"], "term_other");
    assert!(runtime
        .calls()
        .iter()
        .all(|call| call["socket"] == "/fake/other.sock"));
}

#[test]
fn agent_name_comes_from_agent_inventory_not_pane_label() {
    let runtime = Runtime::new();
    let data = runtime.capture("w1:p1");
    assert_eq!(data["pane"]["label"], "pane annotation");
    assert_eq!(data["pane"]["agent_name"], "real-agent-name");
    assert_eq!(data["pane"]["agent"], "codex");
    assert_eq!(data["pane"]["agent_status"], "working");
    assert_eq!(data["pane"]["agent_session"]["value"], "native-id");
    assert_eq!(data["pane"]["terminal_title"], "clean title");
    assert_eq!(data["pane"]["cwd"], "/repo");
    assert_eq!(data["pane"]["foreground_cwd"], "/repo/feature");
    let mut state = runtime.state();
    state["snapshots"]["/fake/default.sock"]["agents"][0]["terminal_id"] = json!("stale-terminal");
    runtime.save(state);
    assert_eq!(runtime.capture("w1:p1")["pane"]["agent_name"], Value::Null);
}

#[test]
fn shells_are_valid_sources_and_use_the_raw_title_fallback() {
    let runtime = Runtime::new();
    let data = runtime.capture("w1:p2");
    assert_eq!(data["pane"]["terminal_id"], "term_shell");
    assert_eq!(data["pane"]["terminal_title"], "shell");
    assert_eq!(data["pane"]["agent"], Value::Null);
    assert_eq!(data["pane"]["agent_name"], Value::Null);
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
    let strange = "title\n```\n$(touch /tmp/must-not-run) ' \" `hi`\u{1b}[31m";
    let mut state = runtime.state();
    let pane = &mut state["snapshots"]["/fake/default.sock"]["panes"][0];
    pane["label"] = json!(strange);
    pane["terminal_title_stripped"] = json!(strange);
    runtime.save(state);
    let document = runtime
        .controller("/fake/default.sock", "w1:p1")
        .capture()
        .unwrap();
    assert_eq!(record(&document.text)["pane"]["label"], strange);
    assert_eq!(document.text.matches("\n```\n").count(), 2);
    let commands = document.text.split_once("```sh\n").unwrap().1;
    assert!(!commands.contains("must-not-run"));
    assert!(commands.contains("HERDR_SOCKET_PATH=\"${socket_path:?}\""));
    assert!(commands.contains("herdr agent prompt \"${pane_id:?}\""));
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
            .copy()
            .unwrap_err()
    );
    assert!(error.contains("snapshot unavailable"));
    assert!(error.contains("4"));
}

#[test]
fn undiscovered_session_keeps_the_authoritative_socket() {
    let runtime = Runtime::new();
    let mut state = runtime.state();
    state["sessions"] = json!([]);
    runtime.save(state);
    let data = runtime.capture("w1:p1");
    assert_eq!(data["session"]["name"], Value::Null);
    assert_eq!(data["session"]["socket_path"], "/fake/default.sock");
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
fn failed_session_name_discovery_still_exports_exact_socket_and_source() {
    let runtime = Runtime::new();
    let mut state = runtime.state();
    state["fail"] = json!(["session", "list", "--json"]);
    runtime.save(state);
    let data = runtime.capture("w1:p1");
    assert_eq!(data["session"]["name"], Value::Null);
    assert_eq!(data["session"]["socket_path"], "/fake/default.sock");
    assert_eq!(data["pane"]["pane_id"], "w1:p1");
    assert_eq!(data["pane"]["terminal_id"], "term_default");
    assert_eq!(runtime.calls().len(), 2);
    assert_eq!(runtime.calls()[1]["args"], json!(["api", "snapshot"]));
}
