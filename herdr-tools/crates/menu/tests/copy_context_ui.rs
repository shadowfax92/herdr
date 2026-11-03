use clap::{Parser, Subcommand};
use tools_menu::app::{InputOutcome, MenuState};
use tools_menu::config::{Config, MenuItem, DEFAULT_CONFIG};
use tools_menu::Command;

/// `herdr-tools menu …`, as `src/main.rs` mounts this mode.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    top: Top,
}

#[derive(Subcommand)]
enum Top {
    #[command(subcommand)]
    Menu(Command),
}

impl Cli {
    fn try_parse_menu(args: &[&str]) -> Result<Command, clap::Error> {
        let argv = ["herdr-tools", "menu"].iter().chain(args);
        Self::try_parse_from(argv).map(|cli| match cli.top {
            Top::Menu(command) => command,
        })
    }
}
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn menu() -> MenuState {
    MenuState::new(Config::parse(DEFAULT_CONFIG).unwrap())
}

fn rendered(menu: &MenuState, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| tools_menu::ui::render(menu, frame))
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
fn copy_context_accelerator_executes_directly_and_enter_uses_the_same_action() {
    let mut menu = menu();
    // The default menu's retired visibility row keeps its six-row diagnostics
    // panel open (as it did once Agents' read failed), so give the ten items room.
    assert!(rendered(&menu, 68, 20).contains("copy pane context"));
    assert!(matches!(
        menu.handle_key(key(KeyCode::Char('h'))),
        InputOutcome::Execute(MenuItem::CopyContext(_))
    ));
    assert!(matches!(
        menu.handle_key(key(KeyCode::Enter)),
        InputOutcome::Execute(MenuItem::CopyContext(_))
    ));
}

#[test]
fn copy_failure_preserves_source_action_and_allows_explicit_retry_or_cancel() {
    let mut menu = menu();
    menu.handle_key(key(KeyCode::Char('h')));
    menu.set_copy_failure(format!(
        "Clipboard provider failed\n{}\nprovider exit status 1",
        "A long diagnostic. ".repeat(100)
    ));
    assert!(rendered(&menu, 48, 10).contains("Clipboard provider failed"));
    assert_eq!(
        menu.handle_key(key(KeyCode::Char('l'))),
        InputOutcome::Continue
    );
    menu.handle_key(key(KeyCode::End));
    assert!(rendered(&menu, 48, 10).contains("provider exit status 1"));
    let mut held_enter = key(KeyCode::Enter);
    held_enter.kind = KeyEventKind::Repeat;
    assert_eq!(menu.handle_key(held_enter), InputOutcome::Continue);
    assert_eq!(
        menu.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
        InputOutcome::Continue
    );
    assert!(matches!(
        menu.handle_key(key(KeyCode::Enter)),
        InputOutcome::Execute(MenuItem::CopyContext(_))
    ));
    assert!(menu.copy_failure().is_none());
    menu.set_copy_failure("Still unavailable".into());
    assert_eq!(menu.handle_key(key(KeyCode::Esc)), InputOutcome::Continue);
    assert!(menu.copy_failure().is_none());
    assert!(matches!(
        menu.handle_key(key(KeyCode::Char('l'))),
        InputOutcome::Execute(MenuItem::Fork(_))
    ));
    menu.handle_key(key(KeyCode::Char('h')));
    menu.set_copy_failure("Unavailable".into());
    assert_eq!(
        menu.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT)),
        InputOutcome::Cancel
    );
    for (width, height) in [(20, 6), (1, 1)] {
        rendered(&menu, width, height);
    }
}

#[test]
fn copy_cli_has_explicit_source_and_read_only_output_without_scope_selection() {
    let command =
        Cli::try_parse_menu(&["copy-context", "--source-pane", "w1:p17", "--stdout"]).unwrap();
    assert!(matches!(
        command,
        Command::CopyContext { source_pane: Some(source), stdout: true } if source == "w1:p17"
    ));
    assert!(Cli::try_parse_menu(&["copy-context", "--scope", "session"]).is_err());
    // The installers are gone: `[[keys.command]]` in Herdr's config binds keys.
    for args in [
        &["install"][..],
        &["install", "--add-handoff"],
        &["install", "--add-redistribute"],
    ] {
        assert!(Cli::try_parse_menu(args).is_err(), "{args:?}");
    }
}
