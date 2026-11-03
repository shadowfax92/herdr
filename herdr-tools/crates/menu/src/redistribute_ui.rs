//! Non-mutating redistribution dialog. Only an explicit Apply outcome crosses
//! into pane execution; choosing modes, typing rows, and canceling are local UI.

use anyhow::{Context, Result};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::failure_ui::FailureView;
use crate::redistribute::{LayoutPlan, LayoutSnapshot};

/// Dialog outcomes separate navigation from the only mutating handoff, Apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedistributionInput {
    Continue,
    Back,
    Close,
    Apply(Box<LayoutPlan>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    SideBySide,
    Grid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Choose,
    Preview,
    Failed,
}

/// Keeps the confirmed source snapshot separate from editable preview controls.
#[derive(Clone, Debug)]
pub struct RedistributionState {
    snapshot: LayoutSnapshot,
    mode: Mode,
    step: Step,
    rows: String,
    replace_rows: bool,
    message: Option<String>,
    failure: Option<FailureView>,
}

impl RedistributionState {
    pub fn new(snapshot: LayoutSnapshot) -> Self {
        let rows = snapshot.panes.len().clamp(1, 2).to_string();
        Self {
            snapshot,
            mode: Mode::Grid,
            step: Step::Choose,
            rows,
            replace_rows: true,
            message: None,
            failure: None,
        }
    }

    pub fn refresh(&mut self, snapshot: LayoutSnapshot) {
        self.snapshot = snapshot;
        self.message = Some("Layout changed. Review the new preview, then Enter again.".into());
    }

    pub fn fail(&mut self, error: String) {
        // A failed multi-step move may leave panes in different tabs. Do not
        // permit a retry against the old inventory from this preview screen.
        self.step = Step::Failed;
        self.failure = Some(FailureView::new(error));
    }

    pub fn plan(&self) -> Result<LayoutPlan> {
        let rows = match self.mode {
            Mode::SideBySide => 1,
            Mode::Grid => self
                .rows
                .parse::<usize>()
                .context("Enter a whole number of rows")?,
        };
        LayoutPlan::new(self.snapshot.clone(), rows)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> RedistributionInput {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return RedistributionInput::Continue;
        }
        // A held Enter must not acknowledge a refreshed preview or skip past
        // the mode picker into an unintended execution.
        if key.code == KeyCode::Enter && key.kind == KeyEventKind::Repeat {
            return RedistributionInput::Continue;
        }
        if matches!(key.code, KeyCode::Char('q' | 'Q')) && key.modifiers.contains(KeyModifiers::ALT)
        {
            return RedistributionInput::Close;
        }
        if key.code == KeyCode::Esc {
            if self.step == Step::Preview {
                self.step = Step::Choose;
                self.message = None;
                return RedistributionInput::Continue;
            }
            return RedistributionInput::Back;
        }
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return RedistributionInput::Continue;
        }
        if self.step == Step::Failed {
            if let Some(failure) = &mut self.failure {
                failure.handle_key(key);
            }
        } else if self.step == Step::Choose {
            match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k') => {
                    self.mode = if self.mode == Mode::Grid {
                        Mode::SideBySide
                    } else {
                        Mode::Grid
                    };
                }
                KeyCode::Char('s') => {
                    self.mode = Mode::SideBySide;
                    self.preview();
                }
                KeyCode::Char('g') => {
                    self.mode = Mode::Grid;
                    self.preview();
                }
                KeyCode::Enter => self.preview(),
                _ => {}
            }
        } else if self.step == Step::Preview {
            if key.code == KeyCode::Enter {
                if let Ok(plan) = self.plan() {
                    return RedistributionInput::Apply(Box::new(plan));
                }
            }
            if self.mode == Mode::Grid {
                match key.code {
                    KeyCode::Char(digit) if digit.is_ascii_digit() => {
                        if self.replace_rows {
                            self.rows.clear();
                            self.replace_rows = false;
                        }
                        if self.rows.len() < 6 {
                            self.rows.push(digit);
                        }
                    }
                    KeyCode::Backspace => {
                        self.rows.pop();
                        self.replace_rows = false;
                    }
                    KeyCode::Left | KeyCode::Right => {
                        let rows = self.rows.parse::<usize>().unwrap_or(1);
                        let rows = if key.code == KeyCode::Left {
                            rows.saturating_sub(1)
                        } else {
                            rows.saturating_add(1)
                        };
                        self.rows = rows.clamp(1, self.snapshot.panes.len().max(1)).to_string();
                        self.replace_rows = true;
                    }
                    _ => {}
                }
            }
        }
        RedistributionInput::Continue
    }

    fn preview(&mut self) {
        self.step = Step::Preview;
        self.replace_rows = true;
        self.message = None;
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Redistribute panes ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let compact = inner.height < 6;
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(if compact { 1 } else { 2 }),
            Constraint::Min(0),
            Constraint::Length(if compact { 1 } else { 2 }),
        ])
        .areas(inner);
        let title = format!(
            "{} · {} panes",
            self.snapshot
                .tab_label
                .as_deref()
                .unwrap_or(&self.snapshot.tab_id),
            self.snapshot.panes.len()
        );
        let subtitle = match self.step {
            Step::Choose => "Choose an arrangement".into(),
            Step::Failed => "Stopped — no original panes closed".into(),
            Step::Preview => match self.mode {
                Mode::SideBySide => "Side by side · one row".into(),
                Mode::Grid => format!(
                    "Grid · Rows: {}   ←/→ or type a number",
                    if self.rows.is_empty() {
                        "_"
                    } else {
                        &self.rows
                    }
                ),
            },
        };
        let heading = if compact {
            vec![Line::from(if self.step == Step::Choose {
                title
            } else {
                subtitle
            })]
        } else {
            vec![Line::from(title), Line::from(subtitle)]
        };
        frame.render_widget(Paragraph::new(heading), header);
        let footer_text = match self.step {
            Step::Choose => "Enter preview\nEsc back / Alt+q close",
            Step::Preview => "Enter apply\nEsc back / Alt+q close",
            Step::Failed => "↑/↓ scroll · Home/End · PgUp/PgDn\nEsc back / Alt+q close",
        };
        let footer_text = if compact {
            if self.step == Step::Failed {
                "↑/↓ · Esc back"
            } else {
                "Enter / Esc"
            }
        } else {
            footer_text
        };
        frame.render_widget(
            Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray)),
            footer,
        );
        if self.step == Step::Failed {
            if let Some(failure) = &self.failure {
                failure.render(frame, body);
            }
            return;
        }
        if self.step == Step::Choose {
            let choices = [
                (Mode::SideBySide, "s  Side by side — equal-width columns"),
                (Mode::Grid, "g  Grid… — choose the number of rows"),
            ];
            let lines = choices
                .into_iter()
                .map(|(mode, label)| {
                    Line::from(Span::styled(
                        label,
                        if mode == self.mode {
                            Style::default()
                                .fg(Color::White)
                                .bg(Color::Blue)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ))
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), body);
            return;
        }
        let plan = match self.plan() {
            Ok(plan) => plan,
            Err(error) => {
                frame.render_widget(
                    Paragraph::new(error.to_string())
                        .style(Style::default().fg(Color::Yellow))
                        .wrap(Wrap { trim: true }),
                    body,
                );
                return;
            }
        };
        let mut notices = Vec::new();
        if let Some(message) = &self.message {
            notices.push(message.as_str());
        }
        if self.snapshot.zoomed {
            notices.push("Applying will unzoom this tab.");
        }
        let [preview, notice_area] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(match notices.len() {
                0 => 0,
                1 => 2,
                _ => 3,
            }),
        ])
        .areas(body);
        if !notices.is_empty() {
            frame.render_widget(
                Paragraph::new(notices.join("\n"))
                    .style(Style::default().fg(Color::Yellow))
                    .wrap(Wrap { trim: true }),
                notice_area,
            );
        }
        render_preview(&plan, frame, preview);
    }
}

fn render_preview(plan: &LayoutPlan, frame: &mut Frame, area: Rect) {
    let rows = plan.rows();
    let columns = rows.iter().map(Vec::len).max().unwrap_or(1);
    if usize::from(area.height) < rows.len() * 3 || usize::from(area.width) < columns * 5 {
        let counts = rows
            .iter()
            .map(|row| row.len().to_string())
            .collect::<Vec<_>>()
            .join(" + ");
        frame.render_widget(
            Paragraph::new(format!(
                "Panes per row: {counts}\nShorter rows use wider panes."
            ))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let mut index = 1;
    for (row_index, row) in rows.iter().enumerate() {
        let y = usize::from(area.height) * row_index / rows.len();
        let bottom = usize::from(area.height) * (row_index + 1) / rows.len();
        for (column, pane) in row.iter().enumerate() {
            let x = usize::from(area.width) * column / row.len();
            let right = usize::from(area.width) * (column + 1) / row.len();
            let rect = Rect::new(
                area.x + x as u16,
                area.y + y as u16,
                (right - x) as u16,
                (bottom - y) as u16,
            );
            let focused = *pane == plan.snapshot().focused_pane_id;
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if focused {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }));
            frame.render_widget(
                Paragraph::new(index.to_string()).centered().block(block),
                rect,
            );
            index += 1;
        }
    }
}
