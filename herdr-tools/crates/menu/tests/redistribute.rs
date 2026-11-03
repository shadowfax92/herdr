use herdr_menu::herdr::Herdr;
use herdr_menu::redistribute::{ApplyOutcome, Redistributor};
use herdr_menu::redistribute::{LayoutPlan, LayoutSnapshot};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

struct Runtime {
    directory: tempfile::TempDir,
    binary: PathBuf,
}

impl Runtime {
    fn new(count: usize) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("herdr");
        fs::write(&binary, include_str!("fixtures/herdr-layout.py")).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            directory.path().join("state.json"),
            json!({
                "layout": snapshot(count), "log": [], "moves": 0,
            })
            .to_string(),
        )
        .unwrap();
        Self { directory, binary }
    }

    fn controller(&self, pane: &str) -> Redistributor {
        Redistributor::new(
            Herdr::new(&self.binary),
            pane,
            self.directory.path(),
            self.directory.path(),
        )
    }

    fn state(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.directory.path().join("state.json")).unwrap())
            .unwrap()
    }

    fn update(&self, key: &str, value: serde_json::Value) {
        let mut state = self.state();
        state[key] = value;
        fs::write(self.directory.path().join("state.json"), state.to_string()).unwrap();
    }
}

fn snapshot(count: usize) -> LayoutSnapshot {
    serde_json::from_value(json!({
        "workspace_id": "w1", "tab_id": "w1:t1", "zoomed": false,
        "focused_pane_id": "w1:p1",
        "area": {"x": 0, "y": 0, "width": 240, "height": 60},
        "panes": (1..=count).rev().map(|index| json!({
            "pane_id": format!("w1:p{index}"),
            "rect": {"x": (index - 1) * 12, "y": 0, "width": 12, "height": 60}
        })).collect::<Vec<_>>()
    }))
    .unwrap()
}

#[test]
fn redistribute_seven_panes_into_two_rows_without_filler_panes() {
    let plan = LayoutPlan::new(snapshot(7), 2).unwrap();
    assert_eq!(
        plan.rows(),
        &[
            vec!["w1:p1", "w1:p2", "w1:p3", "w1:p4"],
            vec!["w1:p5", "w1:p6", "w1:p7"],
        ]
    );
    assert_eq!(plan.pane_count(), 7);
}

#[test]
fn redistribute_live_panes_preserves_ids_and_restores_source_focus() {
    let runtime = Runtime::new(7);
    let controller = runtime.controller("w1:p1");
    let plan = LayoutPlan::new(controller.capture().unwrap(), 2).unwrap();
    assert_eq!(controller.apply(&plan).unwrap(), ApplyOutcome::Applied);
    let state = runtime.state();
    let tabs = state["tabs"].as_object().unwrap();
    assert_eq!(tabs.len(), 1);
    let after: LayoutSnapshot = serde_json::from_value(state["tabs"]["w1:t1"].clone()).unwrap();
    assert_eq!(after.focused_pane_id, "w1:p1");
    assert_eq!(after.panes.len(), 7);
    let mut sizes = std::collections::BTreeMap::new();
    for pane in &after.panes {
        sizes
            .entry(pane.rect.y)
            .or_insert_with(Vec::new)
            .push(pane.rect.width);
    }
    assert_eq!(sizes.values().map(Vec::len).collect::<Vec<_>>(), vec![4, 3]);
    assert!(sizes
        .values()
        .all(|row| row.iter().max().unwrap() - row.iter().min().unwrap() <= 1));
    let log = state["log"].as_array().unwrap();
    assert!(!log
        .iter()
        .any(|line| line.as_str().unwrap().contains("layout apply")));
    let closes = log
        .iter()
        .filter_map(|line| line.as_str())
        .filter(|line| line.starts_with("pane close"))
        .collect::<Vec<_>>();
    assert_eq!(closes, vec!["pane close w1:temporary"]);
}

#[test]
fn failed_moves_show_recovery_ids_first_and_allow_reading_the_entire_error() {
    use herdr_menu::redistribute_ui::{RedistributionInput, RedistributionState};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let runtime = Runtime::new(7);
    let controller = runtime.controller("w1:p1");
    let plan = LayoutPlan::new(controller.capture().unwrap(), 2).unwrap();
    runtime.update("reject_move", json!(1));
    let error = format!("{:#}", controller.apply(&plan).unwrap_err());
    for width in [20, 40] {
        let mut state = RedistributionState::new(plan.snapshot().clone());
        state.fail(error.clone());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 6)).unwrap();
        let mut render = |state: &RedistributionState| {
            terminal
                .draw(|frame| state.render(frame, frame.area()))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let first = render(&state);
        assert!(first.contains("Source w1:t1"), "{first}");
        assert!(first.contains("Holding w1:holding"), "{first}");
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut seen = first.clone();
        for _ in 0..error.len() {
            assert_eq!(
                state.handle_key(key(KeyCode::Down)),
                RedistributionInput::Continue
            );
            seen.push_str(&render(&state));
        }
        assert!(seen.contains("did not move"), "{seen}");
        let last = render(&state);
        state.handle_key(key(KeyCode::End));
        assert_eq!(
            last,
            render(&state),
            "scrolling must stop at the final line"
        );
        state.handle_key(key(KeyCode::Home));
        assert_eq!(first, render(&state));
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            RedistributionInput::Continue
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Esc)),
            RedistributionInput::Back
        );
    }
}

#[test]
fn redistribute_stale_preview_refreshes_without_mutating_panes() {
    let runtime = Runtime::new(7);
    let controller = runtime.controller("w1:p3");
    let plan = LayoutPlan::new(controller.capture().unwrap(), 2).unwrap();
    let mut tabs = runtime.state()["tabs"].clone();
    tabs["w1:t1"]["area"]["width"] = json!(220);
    runtime.update("tabs", tabs);
    let ApplyOutcome::Refresh(snapshot) = controller.apply(&plan).unwrap() else {
        panic!("must refresh");
    };
    assert_eq!(snapshot.area.width, 220);
    assert_eq!(snapshot.focused_pane_id, "w1:p3");
    assert_eq!(runtime.state()["moves"], 0);
    assert!(!runtime.state()["log"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry.as_str().unwrap().starts_with("tab create")));
}

#[test]
fn redistribute_failures_leave_all_original_panes_alive() {
    for rejected_move in [1, 4, 7, 11] {
        let runtime = Runtime::new(7);
        let controller = runtime.controller("w1:p4");
        let plan = LayoutPlan::new(controller.capture().unwrap(), 2).unwrap();
        runtime.update("reject_move", json!(rejected_move));
        let error = format!("{:#}", controller.apply(&plan).unwrap_err());
        assert!(error.contains("Holding w1:holding"), "{error}");
        assert!(error.contains("did not move"), "{error}");
        let state = runtime.state();
        let ids = state["tabs"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|tab| tab["panes"].as_array().unwrap())
            .map(|pane| pane["pane_id"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>();
        for id in 1..=7 {
            assert!(ids.contains(format!("w1:p{id}").as_str()));
        }
        assert!(ids.contains("w1:temporary"));
        assert!(!state["log"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry.as_str().unwrap().starts_with("pane close")));
    }
}

#[test]
fn redistribute_unzooms_only_on_apply_and_restores_a_moved_source() {
    let runtime = Runtime::new(3);
    let mut layout = runtime.state()["layout"].clone();
    layout["zoomed"] = json!(true);
    runtime.update("layout", layout);
    let controller = runtime.controller("w1:p3");
    let plan = LayoutPlan::new(controller.capture().unwrap(), 2).unwrap();
    assert!(runtime.state()["tabs"]["w1:t1"]["zoomed"]
        .as_bool()
        .unwrap());
    assert_eq!(controller.apply(&plan).unwrap(), ApplyOutcome::Applied);
    let state = runtime.state();
    assert_eq!(state["tabs"]["w1:t1"]["zoomed"], false);
    assert_eq!(state["tabs"]["w1:t1"]["focused_pane_id"], "w1:p3");
    assert_eq!(state["focused_tab"], "w1:t1");
}

#[test]
fn redistribute_transport_and_cleanup_errors_never_close_originals() {
    for failure in ["pane move", "pane close"] {
        let runtime = Runtime::new(3);
        let controller = runtime.controller("w1:p1");
        let plan = LayoutPlan::new(controller.capture().unwrap(), 1).unwrap();
        runtime.update("fail", json!(failure));
        let error = format!("{:#}", controller.apply(&plan).unwrap_err());
        assert!(error.contains("Original panes were not closed"), "{error}");
        let state = runtime.state();
        assert_eq!(
            state["tabs"]
                .as_object()
                .unwrap()
                .values()
                .map(|tab| tab["panes"].as_array().unwrap().len())
                .sum::<usize>(),
            4
        );
    }
}

#[test]
fn redistribute_concurrent_invocations_fail_before_mutating_layout() {
    let runtime = Runtime::new(3);
    let controller = runtime.controller("w1:p1");
    let plan = LayoutPlan::new(controller.capture().unwrap(), 2).unwrap();
    let lock = fs::File::create(runtime.directory.path().join("redistribute.lock")).unwrap();
    fs4::FileExt::lock(&lock).unwrap();
    assert!(controller
        .apply(&plan)
        .unwrap_err()
        .to_string()
        .contains("another redistribution"));
    assert_eq!(runtime.state()["moves"], 0);
}

#[test]
fn redistribute_rejects_a_source_that_moved_tabs_since_preview() {
    let runtime = Runtime::new(3);
    let controller = runtime.controller("w1:p1");
    let plan = LayoutPlan::new(controller.capture().unwrap(), 1).unwrap();
    let mut tabs = runtime.state()["tabs"].clone();
    tabs["w1:t1"]["tab_id"] = json!("w1:another-tab");
    runtime.update("tabs", tabs);
    assert!(controller
        .apply(&plan)
        .unwrap_err()
        .to_string()
        .contains("moved to another tab"));
    assert_eq!(runtime.state()["moves"], 0);
}

#[test]
fn redistribute_unexpected_move_identity_stops_without_cleanup() {
    let runtime = Runtime::new(3);
    let controller = runtime.controller("w1:p1");
    let plan = LayoutPlan::new(controller.capture().unwrap(), 1).unwrap();
    runtime.update("malformed_move", json!(1));
    let error = format!("{:#}", controller.apply(&plan).unwrap_err());
    assert!(error.contains("unexpected identity"));
    let state = runtime.state();
    assert_eq!(state["moves"], 1);
    assert!(!state["log"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry.as_str().unwrap().starts_with("pane close")));
}

#[test]
fn redistribute_even_grid_and_side_by_side_keep_exact_pane_counts() {
    let grid = LayoutPlan::new(snapshot(6), 2).unwrap();
    assert_eq!(
        grid.rows(),
        &[
            vec!["w1:p1", "w1:p2", "w1:p3"],
            vec!["w1:p4", "w1:p5", "w1:p6"],
        ]
    );
    let columns = LayoutPlan::new(snapshot(6), 1).unwrap();
    assert_eq!(columns.rows().len(), 1);
    assert_eq!(columns.rows()[0].len(), 6);
    assert_eq!(
        LayoutPlan::new(snapshot(1), 1).unwrap().rows(),
        &[vec!["w1:p1"]]
    );
}

#[test]
fn redistribute_orders_by_visual_row_then_column() {
    let mut source = snapshot(4);
    source
        .panes
        .iter_mut()
        .find(|p| p.pane_id == "w1:p1")
        .unwrap()
        .rect
        .y = 30;
    let plan = LayoutPlan::new(source, 2).unwrap();
    assert_eq!(
        plan.rows(),
        &[vec!["w1:p2", "w1:p3"], vec!["w1:p4", "w1:p1"],]
    );
}

#[test]
fn redistribute_rejects_invalid_inventory_rows_and_unusable_sizes() {
    assert!(LayoutPlan::new(snapshot(0), 1).is_err());
    assert!(LayoutPlan::new(snapshot(7), 0).is_err());
    assert!(LayoutPlan::new(snapshot(7), 8).is_err());
    let mut source = snapshot(7);
    source.area.width = 30;
    assert!(LayoutPlan::new(source, 2)
        .unwrap_err()
        .to_string()
        .contains("too small"));
    let mut source = snapshot(7);
    source.area.height = 8;
    assert!(LayoutPlan::new(source, 2).is_err());
    let mut source = snapshot(7);
    source.panes[0].pane_id = source.panes[1].pane_id.clone();
    assert!(LayoutPlan::new(source, 2)
        .unwrap_err()
        .to_string()
        .contains("duplicate"));
}

#[test]
fn redistribute_uneven_grids_never_drop_duplicate_or_create_panes() {
    for count in 1usize..=40 {
        for rows in 1..=count.min(12) {
            if count.div_ceil(rows) > 20 {
                continue;
            }
            let plan = LayoutPlan::new(snapshot(count), rows).unwrap();
            let sizes = plan.rows().iter().map(Vec::len).collect::<Vec<_>>();
            assert!(sizes
                .windows(2)
                .all(|pair| pair[0] >= pair[1] && pair[0] - pair[1] <= 1));
            let ids = plan
                .rows()
                .iter()
                .flatten()
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(ids.len(), count);
            assert_eq!(sizes.iter().sum::<usize>(), count);
        }
    }
}
