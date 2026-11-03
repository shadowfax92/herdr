//! The comment mode's contracts with Herdr: what it reads from the plugin
//! context, which CLI and socket calls it makes, what popups receive, how
//! refusals are worded, and what the manifest declares.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};
use tempfile::tempdir;
use tools_comment::context::ActionContext;
use tools_comment::herdr::{
    classify_failure, inline_annotation_popup, pane_read_args, pane_send_input_request,
    FailureKind, HerdrClient, HostHerdr,
};
use tools_core::testing::{fake_cli, fake_host, ok};
use tools_core::PopupDims;

/// A fake Herdr socket that records every request and answers with `reply`.
fn recording_host(
    reply: impl Fn(&Value) -> Value + Send + 'static,
) -> (tempfile::TempDir, HostHerdr, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let (dir, host) = fake_host(move |request| {
        log.lock().unwrap().push(request.clone());
        reply(&request)
    });
    let herdr = HostHerdr::new("/unused/herdr", host.socket_path().unwrap());
    (dir, herdr, seen)
}

fn size(width: &str, height: &str) -> Option<PopupDims> {
    Some(PopupDims {
        width: width.parse().unwrap(),
        height: height.parse().unwrap(),
    })
}

#[test]
fn action_context_reads_the_originating_pane_and_runtime_paths() {
    let context = ActionContext::from_values(
        r#"{"focused_pane_id":"w2:p3","focused_pane_cwd":"/tmp/project","selected_text":"first\nsecond"}"#,
        "/tmp/comments-state",
        "/opt/bin/herdr",
        "/tmp/herdr.sock",
    )
    .unwrap();

    assert_eq!(context.pane_id, "w2:p3");
    assert_eq!(context.pane_cwd, Some(PathBuf::from("/tmp/project")));
    assert_eq!(context.selected_text.as_deref(), Some("first\nsecond"));
    assert_eq!(context.state_dir, PathBuf::from("/tmp/comments-state"));
    assert_eq!(context.herdr_bin, PathBuf::from("/opt/bin/herdr"));
    assert_eq!(context.session_identity, "/tmp/herdr.sock");
}

#[test]
fn action_context_rejects_missing_required_values() {
    assert!(ActionContext::from_values("{}", "/tmp/state", "herdr", "/tmp/socket").is_err());
    assert!(ActionContext::from_values(
        r#"{"focused_pane_id":"w1:p1"}"#,
        "",
        "herdr",
        "/tmp/socket",
    )
    .is_err());
}

#[test]
fn plugin_panes_expose_only_opaque_state_ids() {
    let (_dir, herdr, seen) = recording_host(|request| ok(request, json!({})));

    herdr.open_annotation("abc123", size("70%", "90%")).unwrap();
    herdr.open_review("def456", size("70%", "85%")).unwrap();
    herdr.open_review("ghi789", None).unwrap();

    let seen = seen.lock().unwrap();
    assert!(seen
        .iter()
        .all(|request| request["method"] == "plugin.pane.open"));
    assert_eq!(
        seen[0]["params"],
        json!({"plugin_id": "shadowfax.tools", "entrypoint": "comment-capture",
               "placement": "popup", "focus": true, "width": "70%", "height": "90%",
               "env": {"HERDR_COMMENTS_RUN_ID": "abc123"}})
    );
    assert_eq!(
        seen[1]["params"],
        json!({"plugin_id": "shadowfax.tools", "entrypoint": "comment-review",
               "placement": "popup", "focus": true, "width": "70%", "height": "85%",
               "env": {"HERDR_COMMENTS_REVIEW_ID": "def456"}})
    );
    // An unconfigured popup sends no size, so Herdr uses the manifest pane's.
    assert!(seen[2]["params"].get("width").is_none());
    assert!(seen[2]["params"].get("height").is_none());
}

#[test]
fn inline_annotation_popup_marks_the_existing_capture_entrypoint() {
    let popup = inline_annotation_popup("abc123");

    assert_eq!(popup.entrypoint, "comment-capture");
    assert_eq!(
        popup.env,
        [
            ("HERDR_COMMENTS_RUN_ID", "abc123".to_owned()),
            ("HERDR_COMMENTS_INLINE", "1".to_owned()),
        ]
    );
}

#[test]
fn client_width_uses_the_full_layout_for_the_focused_pane() {
    let (_dir, herdr, seen) = recording_host(|request| {
        ok(
            request,
            json!({"layout": {"area": {"x": 30, "y": 0, "width": 482, "height": 60}}}),
        )
    });

    herdr.client_width("w1:p2").unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0]["method"], "pane.layout");
    assert_eq!(seen[0]["params"], json!({"pane_id": "w1:p2"}));
}

#[test]
fn client_width_reads_the_layout_right_edge_from_herdr() {
    let (_dir, herdr, _seen) = recording_host(|request| {
        ok(
            request,
            json!({"layout": {"area": {"x": 30, "y": 0, "width": 482, "height": 60}}}),
        )
    });

    assert_eq!(herdr.client_width("w1:p2").unwrap(), 512);
}

#[test]
fn stale_handoff_binary_uses_the_installed_sibling() {
    let temp = tempdir().unwrap();
    let args = temp.path().join("args");
    let installed = fake_cli(
        temp.path(),
        &format!("printf '%s\\n' \"$@\" > '{}'", args.display()),
    );
    let stale = temp.path().join("herdr.fast-scroll-staging");

    let herdr = HostHerdr::new(stale, "/unused");
    herdr.notify("Comments", "saved").unwrap();

    assert_eq!(herdr.host().binary(), installed);
    assert_eq!(
        std::fs::read_to_string(args).unwrap(),
        "notification\nshow\nComments\n--body\nsaved\n--sound\nnone\n"
    );
}

#[test]
fn pane_capture_requests_rendered_history_without_unwrapping() {
    assert_eq!(
        pane_read_args("w1:p2", "recent", "ansi", Some(1_000)),
        ["pane", "read", "w1:p2", "--source", "recent", "--format", "ansi", "--lines", "1000"]
    );
}

#[test]
fn pane_input_uses_the_bracketed_paste_endpoint_without_enter() {
    let request: Value =
        serde_json::from_str(&pane_send_input_request("req-1", "w1:p2", "first\nsecond").unwrap())
            .unwrap();

    assert_eq!(request["method"], "pane.send_input");
    assert_eq!(request["params"]["pane_id"], "w1:p2");
    assert_eq!(request["params"]["text"], "first\nsecond");
    assert_eq!(request["params"]["keys"], Value::Array(Vec::new()));
}

#[test]
fn pane_input_rejects_a_request_larger_than_herdr_accepts() {
    let text = "a".repeat(1024 * 1024);

    assert!(pane_send_input_request("req-1", "w1:p2", &text).is_err());
    let error = HostHerdr::new("/unused/herdr", "/nonexistent/herdr.sock")
        .send_input("w1:p2", &text)
        .unwrap_err();
    assert!(
        error.to_string().contains("socket request limit"),
        "{error}"
    );
}

#[test]
fn pane_input_uses_one_request_and_one_response_on_the_runtime_socket() {
    let temp = tempdir().unwrap();
    let socket_path = temp.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        let response = serde_json::json!({
            "id": request["id"],
            "result": {"type": "ok"}
        });
        writeln!(reader.get_mut(), "{response}").unwrap();
        (line, request)
    });

    HostHerdr::new("/unused/herdr", &socket_path)
        .send_input("w1:p2", "first\nsecond")
        .unwrap();
    let (line, request) = server.join().unwrap();

    assert_eq!(line.matches('\n').count(), 1);
    assert_eq!(request["method"], "pane.send_input");
    assert_eq!(request["params"]["keys"], Value::Array(Vec::new()));
}

#[test]
fn herdr_failures_are_actionable() {
    assert_eq!(classify_failure("error: ui_busy"), FailureKind::UiBusy);
    assert_eq!(classify_failure("pane_not_found"), FailureKind::PaneMissing);
    assert_eq!(classify_failure("connection refused"), FailureKind::Command);
}

/// Refusals over the socket keep the old CLI path's wording, which is what the
/// action's notification shows.
#[test]
fn socket_refusals_are_actionable() {
    let refuse = |code: &'static str| {
        recording_host(
            move |request| json!({"id": request["id"], "error": {"code": code, "message": "refused"}}),
        )
    };

    let (_dir, herdr, _) = refuse("ui_busy");
    assert_eq!(
        herdr
            .open_annotation("abc123", None)
            .unwrap_err()
            .to_string(),
        "could not open the annotation popup: close the active Herdr popup or modal"
    );
    let (_dir, herdr, _) = refuse("pane_not_found");
    assert_eq!(
        herdr.send_input("w1:p2", "text").unwrap_err().to_string(),
        "could not insert comments: the originating pane no longer exists"
    );
    let (_dir, herdr, _) = refuse("plugin_pane_open_failed");
    assert_eq!(
        herdr.open_review("def456", None).unwrap_err().to_string(),
        "could not open the review popup: Herdr rejected the request"
    );
    let unreachable = HostHerdr::new("/unused/herdr", "/nonexistent/herdr.sock");
    assert_eq!(
        unreachable
            .open_annotation("abc123", None)
            .unwrap_err()
            .to_string(),
        "could not open the annotation popup: Herdr rejected the request"
    );
}

#[test]
fn manifest_declares_the_public_contract() {
    let manifest: toml::Table = include_str!("../../../herdr-plugin.toml").parse().unwrap();
    let entries = |kind: &str| manifest[kind].as_array().unwrap().clone();
    let entry = |kind: &str, id: &str| {
        entries(kind)
            .into_iter()
            .find(|entry| entry["id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("no {kind} `{id}`"))
    };
    let strings = |value: &toml::Value| {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };

    assert_eq!(manifest["id"].as_str(), Some(tools_core::PLUGIN_ID));
    let min_version = manifest["min_herdr_version"].as_str().unwrap();
    let version = |text: &str| {
        text.split('.')
            .map(|part| part.parse::<u32>().unwrap())
            .collect::<Vec<_>>()
    };
    assert!(version(min_version) >= version("0.7.5"), "{min_version}");
    assert_eq!(strings(&manifest["platforms"]), ["macos", "linux"]);

    for (id, contexts, command) in [
        ("comment-capture", &["pane", "selection"][..], "capture"),
        ("comment-review", &["pane"][..], "review"),
        ("comment-paste", &["pane"][..], "paste"),
    ] {
        let action = entry("actions", id);
        assert_eq!(strings(&action["contexts"]), contexts, "{id}");
        assert_eq!(
            strings(&action["command"]),
            ["./bin/herdr-tools", "comment", command],
            "{id}"
        );
    }
    for (id, command) in [
        ("comment-capture", "capture-popup"),
        ("comment-review", "review-popup"),
    ] {
        let pane = entry("panes", id);
        assert_eq!(pane["placement"].as_str(), Some("popup"), "{id}");
        assert_eq!(
            strings(&pane["command"]),
            ["./bin/herdr-tools", "comment", command],
            "{id}"
        );
    }
}
