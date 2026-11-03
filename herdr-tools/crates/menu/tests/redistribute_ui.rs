use herdr_menu::redistribute::LayoutSnapshot;
use herdr_menu::redistribute_ui::{RedistributionInput, RedistributionState};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

fn state() -> RedistributionState {
    let snapshot: LayoutSnapshot = serde_json::from_value(json!({
        "workspace_id": "w1", "tab_id": "w1:t1", "tab_label": "bos-fixes", "zoomed": false,
        "focused_pane_id": "w1:p1", "area": {"x": 0, "y": 0, "width": 240, "height": 60},
        "panes": (1..=7).map(|i| json!({"pane_id": format!("w1:p{i}"),
            "rect": {"x": i * 20, "y": 0, "width": 20, "height": 60}})).collect::<Vec<_>>()
    }))
    .unwrap();
    RedistributionState::new(snapshot)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn fork_recovery_details_scroll_and_cannot_repeat_the_failed_action() {
    use herdr_menu::app::{InputOutcome, MenuState};
    use herdr_menu::config::Config;
    let mut menu = MenuState::new(
        Config::parse("[[items]]\ntype='fork'\nkey='l'\nlabel='left'\nplacement='left'\n").unwrap(),
    );
    menu.set_fork_failure(format!(
        "Source w1:t1\nHolding w1:t2\n{}\nunderlying failure",
        "Long diagnostic path. ".repeat(100)
    ));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
    let mut render = |menu: &MenuState| {
        terminal
            .draw(|frame| herdr_menu::ui::render(menu, frame))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    let first = render(&menu);
    assert!(
        first.contains("Source w1:t1") && first.contains("Holding w1:t2"),
        "{first}"
    );
    for code in [KeyCode::Enter, KeyCode::Char('l')] {
        assert_eq!(menu.handle_key(key(code)), InputOutcome::Continue);
        assert!(menu.fork_failure().is_some());
    }
    menu.handle_key(key(KeyCode::End));
    assert!(render(&menu).contains("underlying failure"));
    menu.handle_key(key(KeyCode::Home));
    assert_eq!(render(&menu), first);
    menu.handle_key(key(KeyCode::Esc));
    assert!(menu.fork_failure().is_none());
}

#[test]
fn grid_requires_preview_then_explicit_confirmation() {
    let mut state = state();
    assert_eq!(
        state.handle_key(key(KeyCode::Char('g'))),
        RedistributionInput::Continue
    );
    let RedistributionInput::Apply(plan) = state.handle_key(key(KeyCode::Enter)) else {
        panic!("expected apply");
    };
    assert_eq!(
        plan.rows().iter().map(Vec::len).collect::<Vec<_>>(),
        vec![4, 3]
    );
}

fn rendered(state: &RedistributionState, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| state.render(frame, frame.area()))
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
fn menu_offers_only_two_modes_and_side_by_side_previews_one_row() {
    let mut state = state();
    let text = rendered(&state, 68, 16);
    assert!(text.contains("Side by side"));
    assert!(text.contains("Grid"));
    assert!(!text.contains("Stacked"));
    let small = rendered(&state, 20, 6);
    assert!(small.contains("Side by side"));
    assert!(small.contains("Grid"));
    assert!(small.contains("Enter / Esc"));
    assert_eq!(
        state.handle_key(key(KeyCode::Char('s'))),
        RedistributionInput::Continue
    );
    let RedistributionInput::Apply(plan) = state.handle_key(key(KeyCode::Enter)) else {
        panic!("expected apply");
    };
    assert_eq!(plan.rows().len(), 1);
    assert_eq!(plan.rows()[0].len(), 7);
}

#[test]
fn grid_numbers_replace_the_default_and_arrows_adjust_rows() {
    let mut state = state();
    state.handle_key(key(KeyCode::Char('g')));
    state.handle_key(key(KeyCode::Char('3')));
    assert_eq!(state.plan().unwrap().rows().len(), 3);
    state.handle_key(key(KeyCode::Left));
    assert_eq!(state.plan().unwrap().rows().len(), 2);
    state.handle_key(key(KeyCode::Right));
    assert_eq!(state.plan().unwrap().rows().len(), 3);
    state.handle_key(key(KeyCode::Char('0')));
    assert_eq!(
        state.handle_key(key(KeyCode::Enter)),
        RedistributionInput::Continue
    );
    assert!(rendered(&state, 68, 16).contains("Rows must be"));
    state.handle_key(key(KeyCode::Backspace));
    assert!(state.plan().is_err());
    state.handle_key(key(KeyCode::Char('2')));
    assert_eq!(state.plan().unwrap().rows().len(), 2);
}

#[test]
fn escape_backs_out_and_alt_q_closes_without_applying() {
    let mut state = state();
    state.handle_key(key(KeyCode::Char('g')));
    assert_eq!(
        state.handle_key(key(KeyCode::Esc)),
        RedistributionInput::Continue
    );
    assert_eq!(
        state.handle_key(key(KeyCode::Esc)),
        RedistributionInput::Back
    );
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT)),
        RedistributionInput::Close
    );
}

#[test]
fn modified_keys_and_held_enter_cannot_accidentally_confirm() {
    let mut state = state();
    state.handle_key(key(KeyCode::Char('g')));
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
        RedistributionInput::Continue
    );
    let mut repeated = key(KeyCode::Enter);
    repeated.kind = ratatui::crossterm::event::KeyEventKind::Repeat;
    assert_eq!(state.handle_key(repeated), RedistributionInput::Continue);
}

#[test]
fn preview_uses_source_label_and_survives_small_popups() {
    let mut state = state();
    state.handle_key(key(KeyCode::Char('g')));
    let normal = rendered(&state, 68, 16);
    assert!(normal.contains("bos-fixes"));
    assert!(normal.contains("7 panes"));
    assert!(normal.contains("Rows: 2"));
    assert!(normal.contains("Enter apply"));
    let compact = rendered(&state, 40, 10);
    assert!(compact.contains("4 + 3"));
    for (width, height) in [(20, 6), (1, 1), (68, 12)] {
        rendered(&state, width, height);
    }
}

#[test]
fn stale_preview_requires_another_press_and_failure_cannot_be_retried() {
    let mut state = state();
    state.handle_key(key(KeyCode::Char('g')));
    let mut changed = state.plan().unwrap().snapshot().clone();
    changed.area.width = 200;
    changed.zoomed = true;
    state.refresh(changed);
    assert!(rendered(&state, 68, 16).contains("Layout changed"));
    assert!(rendered(&state, 68, 16).contains("Applying will unzoom"));
    let RedistributionInput::Apply(plan) = state.handle_key(key(KeyCode::Enter)) else {
        panic!("expected confirmation");
    };
    assert_eq!(plan.snapshot().area.width, 200);
    state.fail("Inspect source tab w1:t1 and holding tab w1:t2".into());
    assert_eq!(
        state.handle_key(key(KeyCode::Enter)),
        RedistributionInput::Continue
    );
    assert_eq!(
        state.handle_key(key(KeyCode::Char('g'))),
        RedistributionInput::Continue
    );
    assert!(rendered(&state, 68, 16).contains("holding tab w1:t2"));
    assert_eq!(
        state.handle_key(key(KeyCode::Esc)),
        RedistributionInput::Back
    );
}

#[test]
fn default_menu_dispatches_redistribute_and_returns_to_existing_actions() {
    use herdr_menu::app::{InputOutcome, MenuState};
    use herdr_menu::config::{Config, MenuItem, DEFAULT_CONFIG};
    let mut menu = MenuState::new(Config::parse(DEFAULT_CONFIG).unwrap());
    assert!(matches!(
        menu.handle_key(key(KeyCode::Char('d'))),
        InputOutcome::Execute(MenuItem::Redistribute(_))
    ));
    menu.open_redistribution(state().plan().unwrap().snapshot().clone());
    assert_eq!(menu.handle_key(key(KeyCode::Esc)), InputOutcome::Continue);
    assert!(matches!(
        menu.handle_key(key(KeyCode::Char('l'))),
        InputOutcome::Execute(MenuItem::Fork(_))
    ));
}
