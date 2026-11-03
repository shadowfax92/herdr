use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use serde_json::{json, Value};
use tempfile::tempdir;

/// `herdr-tools menu …`, through this crate's test stand-in for the root binary.
fn herdr_tools_menu() -> Command {
    Command::new(env!("CARGO_BIN_EXE_herdr-tools-menu"))
}

#[test]
fn stdout_uses_explicit_then_captured_then_caller_source_without_clipboard_or_notifications() {
    let directory = tempdir().unwrap();
    let binary = directory.path().join("herdr");
    fs::write(
        &binary,
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
with (root / "calls.jsonl").open("a") as log:
    log.write(json.dumps({"args": sys.argv[1:], "socket": os.getenv("HERDR_SOCKET_PATH")}) + "\n")
if pathlib.Path(__file__).name != "herdr":
    (root / "clipboard-called").touch()
    raise SystemExit(9)
if sys.argv[1:] == ["session", "list", "--json"]:
    print(json.dumps({"sessions": [{"name": "test", "socket_path": os.environ["HERDR_SOCKET_PATH"]}]}))
elif sys.argv[1:] == ["api", "snapshot"]:
    print((root / "snapshot.json").read_text())
else:
    raise SystemExit(7)
"#,
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    // Catch an accidental clipboard call without touching the user's clipboard.
    for provider in ["pbcopy", "wl-copy", "xclip", "xsel"] {
        fs::copy(&binary, directory.path().join(provider)).unwrap();
    }
    let panes = ["caller", "captured", "explicit"].map(|label| {
        json!({
            "pane_id": format!("w1:p-{label}"), "terminal_id": format!("terminal-{label}"),
            "workspace_id": "w1", "tab_id": "w1:t1", "label": label, "agent_status": "unknown"
        })
    });
    fs::write(
        directory.path().join("snapshot.json"),
        json!({"result": {"snapshot": {
            "panes": panes, "agents": [],
            "workspaces": [{"workspace_id": "w1", "label": "project"}],
            "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1", "label": "work"}]
        }}})
        .to_string(),
    )
    .unwrap();
    let socket = directory.path().join("api.sock");
    let test_path = std::env::join_paths(std::iter::once(directory.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    for (explicit, captured, expected) in [
        (true, true, "explicit"),
        (false, true, "captured"),
        (false, false, "caller"),
    ] {
        let mut command = herdr_tools_menu();
        command
            .args(["menu", "copy-context", "--stdout"])
            .env("HERDR_ENV", "1")
            .env("HERDR_BIN_PATH", &binary)
            .env("HERDR_SOCKET_PATH", &socket)
            .env("HERDR_PANE_ID", "w1:p-caller")
            .env("PATH", &test_path)
            .env_remove("HERDR_MENU_SOURCE_PANE_ID");
        if explicit {
            command.args(["--source-pane", "w1:p-explicit"]);
        }
        if captured {
            command.env("HERDR_MENU_SOURCE_PANE_ID", "w1:p-captured");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains(&format!("\"pane_id\": \"w1:p-{expected}\"")),
            "{text}"
        );
        for other in ["caller", "captured", "explicit"] {
            if other != expected {
                assert!(!text.contains(&format!("terminal-{other}")));
            }
        }
    }
    assert!(!directory.path().join("clipboard-called").exists());
    let calls = fs::read_to_string(directory.path().join("calls.jsonl")).unwrap();
    let calls = calls
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 6);
    assert!(calls
        .iter()
        .all(|call| call["socket"] == socket.to_str().unwrap()));
    assert!(calls
        .iter()
        .all(|call| call["args"][0] == "session" || call["args"][0] == "api"));
}

#[test]
fn cli_requires_managed_environment_and_never_falls_back_to_focus() {
    for (managed, expected) in [(false, "HERDR_ENV=1"), (true, "no source pane")] {
        let mut command = herdr_tools_menu();
        command
            .args(["menu", "copy-context", "--stdout"])
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_MENU_SOURCE_PANE_ID")
            .env("HERDR_BIN_PATH", "/must-not-be-executed");
        if managed {
            command.env("HERDR_ENV", "1");
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn copy_notification_identifies_source_and_delivery_failure_keeps_copy_successful() {
    let directory = tempdir().unwrap();
    let binary = directory.path().join("herdr");
    fs::write(
        &binary,
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
args = sys.argv[1:]
name = pathlib.Path(__file__).name
with (root / "calls.jsonl").open("a") as log:
    log.write(json.dumps({"program": name, "args": args}) + "\n")
if name != "herdr":
    (root / "clipboard.txt").write_text(sys.stdin.read())
elif args == ["session", "list", "--json"]:
    print(json.dumps({"sessions": []}))
elif args == ["api", "snapshot"]:
    print(json.dumps({"result": {"snapshot": {
        "panes": [{"pane_id": "w1:p-source", "terminal_id": "term-source",
            "workspace_id": "w1", "tab_id": "w1:t1", "agent_status": "unknown"}],
        "agents": [], "workspaces": [{"workspace_id": "w1", "label": "project"}],
        "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1", "label": "work"}]
    }}}))
elif args[:2] == ["notification", "show"]:
    if os.getenv("FAIL_NOTIFICATION") == "1":
        print("notification unavailable", file=sys.stderr)
        raise SystemExit(8)
else:
    raise SystemExit(7)
"#,
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    for provider in ["pbcopy", "wl-copy", "xclip", "xsel"] {
        fs::copy(&binary, directory.path().join(provider)).unwrap();
    }
    let test_path = std::env::join_paths(std::iter::once(directory.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    for notification_fails in [false, true] {
        fs::write(directory.path().join("calls.jsonl"), "").unwrap();
        let output = herdr_tools_menu()
            .args(["menu", "copy-context"])
            .env("HERDR_ENV", "1")
            .env("HERDR_BIN_PATH", &binary)
            .env("HERDR_SOCKET_PATH", directory.path().join("api.sock"))
            .env("HERDR_PANE_ID", "w1:p-popup")
            .env("HERDR_MENU_SOURCE_PANE_ID", "w1:p-source")
            .env("PATH", &test_path)
            .env(
                "FAIL_NOTIFICATION",
                if notification_fails { "1" } else { "0" },
            )
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert_eq!(stderr.contains("notification failed"), notification_fails);
        let copied = fs::read_to_string(directory.path().join("clipboard.txt")).unwrap();
        assert!(copied.contains("\"pane_id\": \"w1:p-source\""));
        let calls = fs::read_to_string(directory.path().join("calls.jsonl")).unwrap();
        let calls: Vec<Value> = calls
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(calls.len(), 4);
        assert_ne!(
            calls[2]["program"], "herdr",
            "clipboard must succeed before notification"
        );
        let notification = calls[3]["args"].as_array().unwrap();
        assert_eq!(notification[0], "notification");
        let body = notification.last().unwrap().as_str().unwrap();
        assert!(body.contains("w1:p-source"), "{body}");
        assert!(!body.contains("w1:p-popup"));
    }
}
