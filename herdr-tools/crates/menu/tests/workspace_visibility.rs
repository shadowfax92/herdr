//! The `workspace_visibility` item after the Agents plugin's retirement.
//!
//! herdr-menu toggled Agents' excluded-workspace rows through Agents' control
//! socket. With Agents disabled (no control socket, the owner's state today) the
//! policy read failed to connect and the row stayed disabled as
//! "<label> (unavailable)" above a scrollable diagnostics panel, while every
//! other action stayed usable. The port drops the socket adapter and keeps that
//! behavior: the item still parses, renders unavailable, and never activates.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tools_menu::app::{InputOutcome, MenuState};
use tools_menu::config::{Config, MenuItem};

fn menu() -> MenuState {
    MenuState::new(Config::parse("[[items]]\ntype='workspace_visibility'\nkey='V'\nlabel='delegated panes'\n[[items]]\nkey='x'\nlabel='custom'\ncommand='custom'\n").unwrap())
}

fn press(key: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)
}

fn render_menu(state: &MenuState, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| tools_menu::ui::render(state, frame))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

/// The frame's inside, rows joined without separators: the diagnostics panel
/// wraps by cell, so this rejoins its lines wherever the width split them.
fn render_inner(state: &MenuState, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| tools_menu::ui::render(state, frame))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (1..height - 1)
        .flat_map(|y| (1..width - 1).map(move |x| (x, y)))
        .map(|(x, y)| buffer[(x, y)].symbol().to_owned())
        .collect()
}

#[test]
fn retired_visibility_row_is_unavailable_keeps_its_noun_and_never_activates() {
    let mut state = menu();
    assert!(state.has_visibility_action());
    assert!(render_menu(&state, 100, 24).contains("delegated panes (unavailable)"));
    let mut repeat = press('V');
    repeat.kind = KeyEventKind::Repeat;
    for key in [press('V'), repeat, press('V')] {
        assert_eq!(state.handle_key(key), InputOutcome::Continue);
    }
    assert!(matches!(
        state.handle_key(press('x')),
        InputOutcome::Execute(MenuItem::Shell(_))
    ));
}

#[test]
fn disabled_visibility_is_skipped_by_navigation_and_enter() {
    let mut state = menu();
    assert_eq!(state.selected(), Some(1));
    state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(state.selected(), Some(1));
    state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(state.selected(), Some(1));

    let mut only = MenuState::new(
        Config::parse("[[items]]\ntype='workspace_visibility'\nkey='v'\nlabel='workspaces'\n")
            .unwrap(),
    );
    assert_eq!(only.selected(), None);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(only.handle_key(enter), InputOutcome::Continue);
    only.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(only.selected(), None);
    assert_eq!(only.handle_key(enter), InputOutcome::Continue);
    assert_eq!(only.handle_key(press('v')), InputOutcome::Continue);
}

#[test]
fn default_config_keeps_the_v_row_and_a_menu_without_it_has_no_panel() {
    let config = Config::builtin().unwrap();
    assert!(config.items.iter().any(|item| matches!(item, MenuItem::WorkspaceVisibility(action) if action.key == 'v' && action.label == "excluded workspaces")));
    let mut state = MenuState::new(config);
    assert!(state.visibility_failure().is_some());
    assert_eq!(state.handle_key(press('v')), InputOutcome::Continue);

    let state = MenuState::new(
        Config::parse("[[items]]\nkey='x'\nlabel='custom'\ncommand='custom'\n").unwrap(),
    );
    assert!(!state.has_visibility_action());
    assert!(state.visibility_failure().is_none());
    assert!(!render_menu(&state, 100, 24).contains("PgUp/PgDn"));
}

#[test]
fn retirement_diagnostics_scroll_and_are_readable_at_narrow_widths() {
    for width in [40, 64, 68] {
        let mut state = menu();
        let mut frames = String::new();
        for _ in 0..30 {
            frames.push_str(&render_inner(&state, width, 14));
            state.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        }
        assert!(
            frames.contains("PgUp/PgDn/Home/End: details"),
            "width {width}: missing scroll hint"
        );
        assert!(
            frames.contains("Agents plugin is retired"),
            "width {width}: missing reason"
        );
        assert!(
            frames.contains("hidden_workspaces"),
            "width {width}: diagnostic clipped"
        );
        state.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert!(render_inner(&state, width, 14).contains("Agents visibility unavailable"));
        assert!(matches!(
            state.handle_key(press('x')),
            InputOutcome::Execute(MenuItem::Shell(_))
        ));
    }
}
