//! Shared scrollable diagnostics for failed menu actions. The
//! owner decides how to leave the screen; scrolling never retries a mutation.
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::cell::Cell;

/// Owns diagnostic text and viewport state shared by action recovery screens.
#[derive(Clone, Debug)]
pub struct FailureView {
    message: String,
    scroll: usize,
    viewport: Cell<(usize, usize)>,
}

impl FailureView {
    pub fn new(message: String) -> Self {
        Self {
            message,
            scroll: 0,
            viewport: Cell::new((0, 0)),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let (maximum, page) = self.viewport.get();
        let current = self.scroll.min(maximum);
        self.scroll = match key.code {
            KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => current.saturating_add(1).min(maximum),
            KeyCode::PageUp => current.saturating_sub(page),
            KeyCode::PageDown => current.saturating_add(page).min(maximum),
            KeyCode::Home => 0,
            KeyCode::End => maximum,
            _ => current,
        };
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let lines = error_lines(&self.message, usize::from(area.width));
        let height = usize::from(area.height);
        let maximum = lines.len().saturating_sub(height);
        self.viewport.set((maximum, height));
        let visible = lines
            .into_iter()
            .skip(self.scroll.min(maximum))
            .take(height)
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(visible).style(Style::default().fg(Color::Red)),
            area,
        );
    }
}

/// Wrap diagnostics by terminal-cell width before slicing the viewport. Using
/// the same lines for rendering and scroll bounds keeps every part of long CLI
/// errors accessible, including unbroken IDs/JSON, without scrolling past EOF.
fn error_lines(message: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for source in message.lines() {
        let mut line = String::new();
        let mut used = 0;
        for character in source.chars() {
            let cells = Line::raw(character.to_string()).width();
            if used + cells > width && !line.is_empty() {
                lines.push(Line::raw(std::mem::take(&mut line)));
                used = 0;
            }
            line.push(character);
            used += cells;
        }
        lines.push(Line::raw(line));
    }
    lines
}
