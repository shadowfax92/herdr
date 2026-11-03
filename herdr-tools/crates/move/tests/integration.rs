use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tempfile::tempdir;
use tools_core::testing::{fake_host, ok, write_executable};
use tools_core::{Config, Host};
use tools_move::app::{
    MergeWorkspaceRequest, MovePaneRequest, MoveTabRequest, PaneDestination, PaneSource,
    TabDestination,
};
use tools_move::herdr::Herdr;
use tools_move::layout::SplitDirection;
use tools_move::move_ops::Mover;

struct FakeHerdr {
    _directory: tempfile::TempDir,
    binary: PathBuf,
}

impl FakeHerdr {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let binary = directory.path().join("herdr");
        write_executable(
            &binary,
            r#"#!/bin/sh
echo "$*" >> "$0.log"
case "$1 $2" in
  "pane layout")
    case "$*" in
      *"w1:p1"*|*"w1:p2"*)
        echo '{"result":{"layout":{"workspace_id":"w1","tab_id":"w1:t1","zoomed":false,"area":{"x":0,"y":0,"width":100,"height":20},"focused_pane_id":"w1:p1","panes":[{"pane_id":"w1:p1","focused":true,"rect":{"x":0,"y":0,"width":50,"height":20}},{"pane_id":"w1:p2","focused":false,"rect":{"x":50,"y":0,"width":50,"height":20}}],"splits":[{"direction":"right","ratio":0.5,"rect":{"x":0,"y":0,"width":100,"height":20}}]}}}'
        ;;
      *"w1:p3"*)
        echo '{"result":{"layout":{"workspace_id":"w1","tab_id":"w1:t2","zoomed":false,"area":{"x":0,"y":0,"width":100,"height":20},"focused_pane_id":"w1:p3","panes":[{"pane_id":"w1:p3","focused":false,"rect":{"x":0,"y":0,"width":100,"height":20}}],"splits":[]}}}'
        ;;
      *"w2:p1"*)
        echo '{"result":{"layout":{"workspace_id":"w2","tab_id":"w2:t1","zoomed":false,"area":{"x":0,"y":0,"width":100,"height":20},"focused_pane_id":"w2:p1","panes":[{"pane_id":"w2:p1","focused":false,"rect":{"x":0,"y":0,"width":100,"height":20}}],"splits":[]}}}'
        ;;
      *)
        exit 3
        ;;
    esac
    ;;
  "workspace list")
    echo '{"result":{"workspaces":[{"workspace_id":"w1","label":"source"},{"workspace_id":"w2","label":"target"}]}}'
    ;;
  "tab list")
    echo '{"result":{"tabs":[{"tab_id":"w1:t1","workspace_id":"w1","label":"main","pane_count":2},{"tab_id":"w1:t2","workspace_id":"w1","label":"logs","pane_count":1},{"tab_id":"w2:t1","workspace_id":"w2","label":"target","pane_count":1}]}}'
    ;;
  "pane list")
    echo '{"result":{"panes":[{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","focused":true},{"pane_id":"w1:p2","tab_id":"w1:t1","workspace_id":"w1"},{"pane_id":"w1:p3","tab_id":"w1:t2","workspace_id":"w1"},{"pane_id":"w2:p1","tab_id":"w2:t1","workspace_id":"w2"}]}}'
    ;;
  "pane move")
    case "$*" in
      *"w1:p1 --new-workspace"*)
        echo '{"result":{"move_result":{"changed":true,"pane":{"pane_id":"w3:p1","tab_id":"w3:t1","workspace_id":"w3"}}}}'
        ;;
      *"w1:p2 --tab w3:t1"*)
        echo '{"result":{"move_result":{"changed":true,"pane":{"pane_id":"w3:p2","tab_id":"w3:t1","workspace_id":"w3"}}}}'
        ;;
      *"w1:p3 --new-tab --workspace w3"*)
        echo '{"result":{"move_result":{"changed":true,"pane":{"pane_id":"w3:p3","tab_id":"w3:t2","workspace_id":"w3"}}}}'
        ;;
      *"w1:p1 --tab w2:t1"*)
        echo '{"result":{"move_result":{"changed":true,"pane":{"pane_id":"w2:p9","tab_id":"w2:t1","workspace_id":"w2"}}}}'
        ;;
      *"w1:p3 --tab w2:t1"*)
        echo '{"result":{"move_result":{"changed":true,"pane":{"pane_id":"w2:p8","tab_id":"w2:t1","workspace_id":"w2"}}}}'
        ;;
      *"w1:p3 --new-tab --workspace w2"*)
        if [ -f "$0.fail-second-tab" ]; then
          echo 'injected second-tab failure' >&2
          exit 4
        fi
        echo '{"result":{"move_result":{"changed":true,"pane":{"pane_id":"w2:p7","tab_id":"w2:t8","workspace_id":"w2"}}}}'
        ;;
      *"--new-tab"*)
        echo '{"result":{"move_result":{"changed":true,"pane":{"pane_id":"w2:p9","tab_id":"w2:t9","workspace_id":"w2"}}}}'
        ;;
      *"--tab w2:t9"*)
        echo '{"result":{"move_result":{"changed":true,"pane":{"pane_id":"w2:p8","tab_id":"w2:t9","workspace_id":"w2"}}}}'
        ;;
      *)
        exit 3
        ;;
    esac
    ;;
  "workspace focus"|"tab focus"|"plugin pane")
    echo '{"result":{"type":"ok"}}'
    ;;
  *)
    echo "unsupported: $*" >&2
    exit 2
    ;;
esac
"#,
        );
        Self {
            _directory: directory,
            binary,
        }
    }

    fn client(&self) -> Herdr {
        Herdr::new(Host::new(&self.binary, None))
    }

    fn log(&self) -> String {
        fs::read_to_string(self.binary.with_extension("log")).unwrap_or_default()
    }

    fn fail_second_tab(&self) {
        fs::write(self.binary.with_extension("fail-second-tab"), "").unwrap();
    }
}

/// A fake socket whose client is `client_width` cells wide (pane area at x = 0); it records
/// every request and accepts the popup open.
fn popup_host(client_width: u16) -> (Arc<Mutex<Vec<Value>>>, tempfile::TempDir, Herdr) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let (socket, host) = fake_host(move |request: Value| {
        log.lock().unwrap().push(request.clone());
        match request["method"].as_str() {
            Some("pane.layout") => ok(
                &request,
                json!({"layout": {"area": {"x": 0, "y": 0, "width": client_width, "height": 49}}}),
            ),
            _ => ok(&request, json!({"type": "ok"})),
        }
    });
    (seen, socket, Herdr::new(host))
}

fn popup_open(seen: &Mutex<Vec<Value>>) -> Value {
    let seen = seen.lock().unwrap();
    let opens = seen
        .iter()
        .filter(|request| request["method"] == "plugin.pane.open")
        .collect::<Vec<_>>();
    assert_eq!(opens.len(), 1, "{seen:?}");
    opens[0]["params"].clone()
}

#[test]
fn launch_opens_the_native_popup_with_source_context() {
    let (seen, _socket, herdr) = popup_host(174);

    herdr.launch_picker(&Config::default(), "w1:p1").unwrap();

    // No configured size: Herdr applies the manifest pane's 76×24, the size ferry passed.
    assert_eq!(
        popup_open(&seen),
        json!({
            "plugin_id": "shadowfax.tools",
            "entrypoint": "move",
            "placement": "popup",
            "focus": true,
            "env": {"HERDR_FERRY_SOURCE_PANE_ID": "w1:p1"},
        })
    );
}

#[test]
fn tests_that_the_popup_takes_its_size_from_config_and_profiles() {
    let config = Config::parse(
        r#"
[popup.size]
move = { width = 60, height = 20 }

[[popup.profile]]
name = "narrow"
max_client_width = 180
size.move = { width = "50%", height = "40%" }
"#,
    )
    .unwrap();

    let (seen, _socket, herdr) = popup_host(300);
    herdr.launch_picker(&config, "w1:p1").unwrap();
    let wide = popup_open(&seen);
    assert_eq!((&wide["width"], &wide["height"]), (&json!(60), &json!(20)));

    let (seen, _socket, herdr) = popup_host(174);
    herdr.launch_picker(&config, "w1:p1").unwrap();
    let narrow = popup_open(&seen);
    assert_eq!(
        (&narrow["width"], &narrow["height"]),
        (&json!("50%"), &json!("40%"))
    );
    assert_eq!(
        narrow["env"],
        json!({"HERDR_FERRY_SOURCE_PANE_ID": "w1:p1"})
    );
}

#[test]
fn whole_tab_move_replays_the_reported_split_with_returned_pane_ids() {
    let fake = FakeHerdr::new();
    let mover = Mover::new(fake.client());

    let summary = mover
        .move_tab(&MoveTabRequest {
            tab_ids: vec!["w1:t1".into()],
            destination: TabDestination::Workspace {
                workspace_id: "w2".into(),
            },
        })
        .unwrap();

    assert_eq!(summary.message, "Tab “main” moved to w2");
    assert_eq!(summary.moved_panes, 2);
    assert_eq!(summary.moved_tabs, 1);
    let log = fake.log();
    assert!(log.contains("pane move w1:p1 --new-tab --workspace w2 --label main --no-focus"));
    assert!(log.contains(
        "pane move w1:p2 --tab w2:t9 --split right --target-pane w2:p9 --ratio 0.5 --no-focus"
    ));
    assert!(log.ends_with("workspace focus w2\ntab focus w2:t9\n"));
}

#[test]
fn multiple_panes_append_in_selection_order_and_focus_once() {
    let fake = FakeHerdr::new();
    let mover = Mover::new(fake.client());

    let summary = mover
        .move_pane(&MovePaneRequest {
            sources: vec![
                PaneSource {
                    pane_id: "w1:p1".into(),
                    expected_tab_id: "w1:t1".into(),
                },
                PaneSource {
                    pane_id: "w1:p3".into(),
                    expected_tab_id: "w1:t2".into(),
                },
            ],
            direction: SplitDirection::Down,
            destination: PaneDestination::ExistingTab {
                tab_id: "w2:t1".into(),
            },
        })
        .unwrap();

    assert_eq!(summary.message, "2 panes moved to w2:t1");
    let log = fake.log();
    assert!(log.contains("pane move w1:p1 --tab w2:t1 --split down --no-focus"));
    assert!(log.contains("pane move w1:p3 --tab w2:t1 --split down --target-pane w2:p9 --no-focus"));
    assert_eq!(log.matches("workspace focus").count(), 1);
    assert_eq!(log.matches("tab focus").count(), 1);
}

#[test]
fn multiple_tabs_append_to_one_new_workspace() {
    let fake = FakeHerdr::new();
    let mover = Mover::new(fake.client());

    let summary = mover
        .move_tab(&MoveTabRequest {
            tab_ids: vec!["w1:t1".into(), "w1:t2".into()],
            destination: TabDestination::NewWorkspace {
                label: Some("combined".into()),
            },
        })
        .unwrap();

    assert_eq!(summary.message, "2 tabs moved to w3");
    assert_eq!(summary.moved_panes, 3);
    assert_eq!(summary.moved_tabs, 2);
    let log = fake.log();
    let first = log
        .find("pane move w1:p1 --new-workspace --label combined --tab-label main --no-focus")
        .unwrap();
    let second = log
        .find("pane move w1:p3 --new-tab --workspace w3 --label logs --no-focus")
        .unwrap();
    assert!(first < second);
    assert!(log.contains(
        "pane move w1:p2 --tab w3:t1 --split right --target-pane w3:p1 --ratio 0.5 --no-focus"
    ));
    assert!(log.ends_with("workspace focus w3\ntab focus w3:t1\n"));
}

#[test]
fn workspace_merge_appends_every_tab_in_source_order() {
    let fake = FakeHerdr::new();
    let mover = Mover::new(fake.client());

    let summary = mover
        .merge_workspace(&MergeWorkspaceRequest {
            source_workspace_id: "w1".into(),
            destination_workspace_id: "w2".into(),
        })
        .unwrap();

    assert_eq!(
        summary.message,
        "Workspace “source” merged into “target” · 2 tabs"
    );
    assert_eq!(summary.moved_tabs, 2);
    let log = fake.log();
    let first = log
        .find("pane move w1:p1 --new-tab --workspace w2 --label main --no-focus")
        .unwrap();
    let second = log
        .find("pane move w1:p3 --new-tab --workspace w2 --label logs --no-focus")
        .unwrap();
    assert!(first < second);
}

#[test]
fn invalid_batch_is_rejected_before_any_move() {
    let fake = FakeHerdr::new();
    let mover = Mover::new(fake.client());

    let error = mover
        .move_tab(&MoveTabRequest {
            tab_ids: vec!["w1:t1".into(), "missing".into()],
            destination: TabDestination::Workspace {
                workspace_id: "w2".into(),
            },
        })
        .unwrap_err();

    assert!(error.to_string().contains("selected tab no longer exists"));
    assert!(!fake.log().contains("pane move"));
}

#[test]
fn partial_batch_failure_reports_completed_work() {
    let fake = FakeHerdr::new();
    fake.fail_second_tab();
    let mover = Mover::new(fake.client());

    let error = mover
        .move_tab(&MoveTabRequest {
            tab_ids: vec!["w1:t1".into(), "w1:t2".into()],
            destination: TabDestination::Workspace {
                workspace_id: "w2".into(),
            },
        })
        .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("after 1/2 tabs and 2/3 panes"));
    assert!(message.contains("all moved panes are still live"));
    assert!(!fake.log().contains("workspace focus"));
}
