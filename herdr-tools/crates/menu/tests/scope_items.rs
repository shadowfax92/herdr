//! The focus, hide and focus-switch rows, and the retired `workspace_visibility`
//! row that now opens the hide picker.
//!
//! Each row reaches execution as its own outcome (the event loop turns a
//! `ScopePick` into `popup.close` + a detached `scope.pick`, and a
//! `FocusToggle` into `scope.get` + `scope.set`; `src/scope.rs` tests those
//! calls against a fake socket). On a Herdr without scope the rows read
//! "<label> (unavailable)" above a scrollable panel, never activate, and every
//! other row stays usable: what the retired row did when Agents was not running.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tools_menu::app::{InputOutcome, MenuState};
use tools_menu::config::{ActionItem, Config, MenuItem, ScopePickItem, ScopePickMode};
use tools_menu::scope::ScopeSupport;

const MENU: &str = "\
[[items]]\ntype='focus'\nkey='f'\nlabel='focus…'\n\
[[items]]\ntype='hide'\nkey='x'\nlabel='hide…'\n\
[[items]]\ntype='focus_toggle'\nkey='F'\nlabel='focus mode'\n\
[[items]]\ntype='workspace_visibility'\nkey='V'\nlabel='delegated panes'\n\
[[items]]\nkey='c'\nlabel='custom'\ncommand='custom'\n";

fn menu(scope: ScopeSupport) -> MenuState {
    MenuState::with_scope(Config::parse(MENU).unwrap(), scope)
}

fn press(key: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)
}

fn code(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn picked(key: char, label: &str, mode: ScopePickMode) -> InputOutcome {
    InputOutcome::Execute(MenuItem::ScopePick(ScopePickItem {
        key,
        label: label.into(),
        mode,
    }))
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
fn each_scope_row_dispatches_its_own_action() {
    for scope in [
        ScopeSupport::Unknown,
        ScopeSupport::Available {
            focus_active: false,
        },
    ] {
        let mut state = menu(scope);
        assert_eq!(
            state.handle_key(press('f')),
            picked('f', "focus…", ScopePickMode::Focus)
        );
        assert_eq!(
            state.handle_key(press('x')),
            picked('x', "hide…", ScopePickMode::Hide)
        );
        assert_eq!(
            state.handle_key(press('F')),
            InputOutcome::Execute(MenuItem::FocusToggle(ActionItem {
                key: 'F',
                label: "focus mode".into()
            }))
        );
        // The retired row behaves exactly as `hide`.
        assert_eq!(
            state.handle_key(press('V')),
            picked('V', "delegated panes", ScopePickMode::Hide)
        );
        assert!(state.scope_failure().is_none());
    }
}

#[test]
fn enter_runs_the_selected_scope_row() {
    let mut state = menu(ScopeSupport::Available { focus_active: true });
    assert_eq!(state.selected(), Some(0));
    assert_eq!(
        state.handle_key(code(KeyCode::Enter)),
        picked('f', "focus…", ScopePickMode::Focus)
    );
    state.handle_key(code(KeyCode::Down));
    state.handle_key(code(KeyCode::Down));
    assert!(matches!(
        state.handle_key(code(KeyCode::Enter)),
        InputOutcome::Execute(MenuItem::FocusToggle(_))
    ));
}

#[test]
fn the_focus_switch_shows_the_current_state() {
    let on = menu(ScopeSupport::Available { focus_active: true });
    assert!(render_menu(&on, 80, 14).contains("F  focus mode · on"));
    let off = menu(ScopeSupport::Available {
        focus_active: false,
    });
    assert!(render_menu(&off, 80, 14).contains("F  focus mode · off"));
    // Unread: the plain label, no guess.
    let unknown = render_menu(&menu(ScopeSupport::Unknown), 80, 14);
    assert!(unknown.contains("F  focus mode"));
    assert!(!unknown.contains("focus mode ·"));
    // The picker rows never carry a state.
    assert!(render_menu(&on, 80, 14).contains("x  hide…"));
}

#[test]
fn without_scope_the_rows_are_unavailable_and_never_activate() {
    let mut state = menu(ScopeSupport::Missing);
    let screen = render_menu(&state, 100, 24);
    for label in [
        "focus… (unavailable)",
        "hide… (unavailable)",
        "focus mode (unavailable)",
        "delegated panes (unavailable)",
    ] {
        assert!(screen.contains(label), "missing {label}: {screen}");
    }
    let mut repeat = press('V');
    repeat.kind = KeyEventKind::Repeat;
    for key in [press('f'), press('x'), press('F'), press('V'), repeat] {
        assert_eq!(state.handle_key(key), InputOutcome::Continue);
    }
    assert!(matches!(
        state.handle_key(press('c')),
        InputOutcome::Execute(MenuItem::Shell(_))
    ));
}

#[test]
fn disabled_scope_rows_are_skipped_by_navigation_and_enter() {
    let mut state = menu(ScopeSupport::Missing);
    assert_eq!(state.selected(), Some(4));
    state.handle_key(code(KeyCode::Down));
    assert_eq!(state.selected(), Some(4));
    state.handle_key(code(KeyCode::Up));
    assert_eq!(state.selected(), Some(4));

    let mut only = MenuState::with_scope(
        Config::parse("[[items]]\ntype='workspace_visibility'\nkey='v'\nlabel='workspaces'\n")
            .unwrap(),
        ScopeSupport::Missing,
    );
    assert_eq!(only.selected(), None);
    assert_eq!(
        only.handle_key(code(KeyCode::Enter)),
        InputOutcome::Continue
    );
    only.handle_key(code(KeyCode::Down));
    assert_eq!(only.selected(), None);
    assert_eq!(only.handle_key(press('v')), InputOutcome::Continue);
}

#[test]
fn learning_scope_is_missing_moves_the_selection_off_a_disabled_row() {
    let mut state = menu(ScopeSupport::Unknown);
    assert_eq!(state.selected(), Some(0));
    state.set_scope(ScopeSupport::Missing);
    assert_eq!(state.selected(), Some(4));
    assert!(state.scope_failure().is_some());
    state.set_scope(ScopeSupport::Available {
        focus_active: false,
    });
    assert_eq!(state.selected(), Some(4));
    assert!(state.scope_failure().is_none());
}

#[test]
fn the_built_in_v_row_opens_the_hide_picker() {
    let config = Config::builtin().unwrap();
    assert!(config.items.iter().any(|item| matches!(
        item,
        MenuItem::ScopePick(ScopePickItem {
            key: 'v',
            mode: ScopePickMode::Hide,
            ..
        })
    )));
    let mut state = MenuState::with_scope(
        config,
        ScopeSupport::Available {
            focus_active: false,
        },
    );
    assert!(matches!(
        state.handle_key(press('v')),
        InputOutcome::Execute(MenuItem::ScopePick(ScopePickItem {
            mode: ScopePickMode::Hide,
            ..
        }))
    ));
    assert!(state.scope_failure().is_none());

    let state = MenuState::with_scope(
        Config::parse("[[items]]\nkey='x'\nlabel='custom'\ncommand='custom'\n").unwrap(),
        ScopeSupport::Missing,
    );
    assert!(!state.has_scope_action());
    assert!(state.scope_failure().is_none());
    assert!(!render_menu(&state, 100, 24).contains("PgUp/PgDn"));
}

#[test]
fn unavailable_diagnostics_scroll_and_are_readable_at_narrow_widths() {
    for width in [40, 64, 68] {
        let mut state = menu(ScopeSupport::Missing);
        let mut frames = String::new();
        for _ in 0..30 {
            frames.push_str(&render_inner(&state, width, 14));
            state.handle_key(code(KeyCode::PageDown));
        }
        assert!(
            frames.contains("PgUp/PgDn/Home/End: details"),
            "width {width}: missing scroll hint"
        );
        assert!(
            frames.contains("no scope.get / scope.pick"),
            "width {width}: missing reason"
        );
        assert!(
            frames.contains("update Herdr to use them"),
            "width {width}: diagnostic clipped"
        );
        state.handle_key(code(KeyCode::Home));
        assert!(render_inner(&state, width, 14).contains("Focus and hide unavailable"));
        assert!(matches!(
            state.handle_key(press('c')),
            InputOutcome::Execute(MenuItem::Shell(_))
        ));
    }
}
