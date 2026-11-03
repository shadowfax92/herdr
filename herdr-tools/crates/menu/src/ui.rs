use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::MenuState;
use crate::config::MenuItem;

pub fn render(state: &MenuState, frame: &mut Frame) {
    let area = frame.area();
    if let Some(failure) = state.copy_failure() {
        let block = Block::bordered()
            .title(" Could not copy pane context ")
            .border_style(Style::default().fg(Color::Red));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(inner);
        failure.render(frame, body);
        frame.render_widget(
            Paragraph::new("Enter retry · ↑/↓ scroll · Home/End\nEsc back / Alt+q close"),
            footer,
        );
        return;
    }
    if let Some(failure) = state.fork_failure() {
        let block = Block::bordered()
            .title(" Fork stopped ")
            .border_style(Style::default().fg(Color::Red));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(inner);
        failure.render(frame, body);
        frame.render_widget(
            Paragraph::new("↑/↓ scroll · Home/End · PgUp/PgDn\nEsc back / Alt+q close"),
            footer,
        );
        return;
    }
    if let Some(dialog) = state.redistribution() {
        dialog.render(frame, area);
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(format!(" {} ", state.title())).centered());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(error) = state.fatal_error() {
        let message = vec![
            Line::from(Span::styled(
                error,
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            Line::from(Span::styled(
                "esc / alt+q  close",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: true }), inner);
        return;
    }

    let mut status = Vec::new();
    if !state.warnings().is_empty() {
        let noun = if state.warnings().len() == 1 {
            "warning"
        } else {
            "warnings"
        };
        status.push(Line::from(Span::styled(
            format!("{} config {noun}", state.warnings().len()),
            Style::default().fg(Color::Yellow),
        )));
        status.push(Line::from(Span::styled(
            state.warnings()[0].as_str(),
            Style::default().fg(Color::Yellow),
        )));
    }
    if let Some(failure) = state.failure() {
        for line in failure.lines() {
            status.push(Line::from(Span::styled(
                line,
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
        }
    }
    if let Some(index) = state.confirming() {
        if let Some(item) = state.items().get(index) {
            status.push(Line::from(vec![
                Span::styled(
                    item.confirmation().unwrap_or("Run command?"),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    "  y/enter run  n/esc cancel",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    if state.scope_failure().is_some() {
        status.push(Line::from(Span::styled(
            "PgUp/PgDn/Home/End: details",
            Style::default().fg(Color::DarkGray),
        )));
    }
    status.push(Line::from(Span::styled(
        "↑/↓ select  enter run  esc / alt+q close",
        Style::default().fg(Color::DarkGray),
    )));

    // Reuse the existing cell-width-aware diagnostics view in a bounded region.
    // Its scroll keys never run an action or steal accelerators from one.
    let diagnostic_height = if state.scope_failure().is_some() {
        (inner.height / 2).min(6)
    } else {
        0
    };
    let [menu_area, diagnostic_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(diagnostic_height),
        Constraint::Length(u16::try_from(status.len()).unwrap_or(u16::MAX)),
    ])
    .areas(inner);

    if state.items().is_empty() {
        frame.render_widget(Paragraph::new("No actions configured"), menu_area);
    } else {
        let rows = state
            .items()
            .iter()
            .map(|item| match item {
                MenuItem::Shell(_)
                | MenuItem::Fork(_)
                | MenuItem::RotateFork(_)
                | MenuItem::Redistribute(_)
                | MenuItem::CopyContext(_)
                | MenuItem::ScopePick(_)
                | MenuItem::FocusToggle(_) => ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{} ", item.key().expect("action item has an accelerator")),
                        Style::default()
                            .fg(if state.item_enabled(item) {
                                Color::Cyan
                            } else {
                                Color::DarkGray
                            })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {}", state.item_label(item)),
                        if state.item_enabled(item) {
                            Style::default()
                        } else {
                            Style::default().fg(Color::DarkGray)
                        },
                    ),
                ])),
                MenuItem::Separator => ListItem::new(Line::from(Span::styled(
                    "─".repeat(menu_area.width as usize),
                    Style::default().fg(Color::DarkGray),
                ))),
            })
            .collect::<Vec<_>>();
        let list = List::new(rows)
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(" ");
        let mut list_state = ListState::default().with_selected(state.selected());
        frame.render_stateful_widget(list, menu_area, &mut list_state);
    }
    if let Some(failure) = state.scope_failure() {
        failure.render(frame, diagnostic_area);
    }
    frame.render_widget(Paragraph::new(status), status_area);
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::config::Config;

    fn render_text(state: &MenuState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(state, frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn menu_renders_title_commands_separator_and_warning() {
        let config = Config::parse(
            r#"
title = "commands"
[[items]]
key = "c"
label = "clear notifications"
command = "clear"
[[items]]
type = "separator"
[[items]]
key = "c"
label = "duplicate"
command = "duplicate"
[[items]]
key = "r"
label = "reload config"
command = "reload"
"#,
        )
        .unwrap();
        let rendered = render_text(&MenuState::new(config), 50, 12);

        assert!(rendered.contains("commands"));
        assert!(rendered.contains("c  clear notifications"));
        assert!(rendered.contains("r  reload config"));
        assert!(rendered.contains("config warning"));
        assert!(rendered.contains("duplicate accelerator"));
    }

    #[test]
    fn confirmation_and_failure_are_visible() {
        let config = Config::parse(
            r#"
[[items]]
key = "x"
label = "reap"
command = "reap"
confirm = "Reap scratch sessions?"
"#,
        )
        .unwrap();
        let mut state = MenuState::new(config);
        state.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('x'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        state.set_failure("command exited with status 2");
        let rendered = render_text(&state, 50, 10);

        assert!(rendered.contains("Reap scratch sessions?"));
        assert!(rendered.contains("command exited with status 2"));
        assert!(rendered.contains("y/enter"));
    }

    #[test]
    fn fatal_error_screen_explains_how_to_close() {
        let rendered = render_text(&MenuState::fatal("failed to parse config"), 44, 8);

        assert!(rendered.contains("failed to parse config"));
        assert!(rendered.contains("esc / alt+q"));
    }

    #[test]
    fn long_menu_keeps_the_selected_row_visible() {
        let mut source = String::new();
        for index in 0..10 {
            source.push_str(&format!(
                "[[items]]\nkey = \"{index}\"\nlabel = \"item {index}\"\ncommand = \"item {index}\"\n"
            ));
        }
        let mut state = MenuState::new(Config::parse(&source).unwrap());
        for _ in 0..8 {
            state.handle_key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Down,
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }
        let rendered = render_text(&state, 32, 8);

        assert!(rendered.contains("8  item 8"));
    }
}
