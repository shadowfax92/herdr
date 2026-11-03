use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event};

use crate::app::{App, InputOutcome};
use crate::herdr::{Herdr, SOURCE_PANE_ENV};
use crate::kill_ops;
use crate::move_ops::Mover;
use crate::{ui, MODE};

/// Entry point of `herdr-tools move picker`, the popup pane `move open` started.
pub fn run_from_environment() -> Result<()> {
    let herdr = Herdr::from_environment();
    let result = (|| {
        let source_pane_id = std::env::var(SOURCE_PANE_ENV)
            .with_context(|| format!("{SOURCE_PANE_ENV} is not set"))?;
        let topology = herdr.topology()?;
        let mut app = App::new(topology, source_pane_id)?;
        let mover = Mover::new(herdr.clone());
        ratatui::run(|terminal| run_picker(terminal, &mut app, &mover, &herdr))
            .context("Move picker failed")
    })();
    if let Err(error) = &result {
        let _ = herdr.notify(&format!("Move failed: {error:#}"));
    }
    result
}

fn run_picker(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    mover: &Mover,
    herdr: &Herdr,
) -> Result<()> {
    loop {
        terminal.draw(|frame| ui::render(app, frame))?;
        let event = event::read()?;
        if matches!(event, Event::Resize(_, _)) {
            terminal.autoresize()?;
            continue;
        }
        match app.handle_event(event) {
            InputOutcome::Continue => {}
            InputOutcome::Cancel => return Ok(()),
            InputOutcome::MovePane(request) => {
                let noun = if request.sources.len() == 1 {
                    "pane"
                } else {
                    "panes"
                };
                app.set_working(format!("Moving {} {noun}…", request.sources.len()));
                terminal.draw(|frame| ui::render(app, frame))?;
                match mover.move_pane(&request) {
                    Ok(summary) => {
                        let _ = herdr.notify(&summary.message);
                        return Ok(());
                    }
                    Err(error) => app.set_failure(format!("{error:#}")),
                }
            }
            InputOutcome::MoveTab(request) => {
                let subject = if request.tab_ids.len() == 1 {
                    "tab with its"
                } else {
                    "tabs with their"
                };
                app.set_working(format!(
                    "Moving {} {subject} live panes…",
                    request.tab_ids.len()
                ));
                terminal.draw(|frame| ui::render(app, frame))?;
                match mover.move_tab(&request) {
                    Ok(summary) => {
                        let _ = herdr.notify(&summary.message);
                        return Ok(());
                    }
                    Err(error) => app.set_failure(format!("{error:#}")),
                }
            }
            InputOutcome::MergeWorkspace(request) => {
                app.set_working("Appending every source tab to the destination…");
                terminal.draw(|frame| ui::render(app, frame))?;
                match mover.merge_workspace(&request) {
                    Ok(summary) => {
                        let _ = herdr.notify(&summary.message);
                        return Ok(());
                    }
                    Err(error) => app.set_failure(format!("{error:#}")),
                }
            }
            InputOutcome::Kill(plan) => {
                // Hand off and exit at once: the plan may close this popup's own tab, and the
                // detached executor reports the outcome as a notification.
                let started = tools_core::self_command(MODE)
                    .and_then(|executor| kill_ops::spawn_executor(executor, herdr, &plan));
                match started {
                    Ok(_) => return Ok(()),
                    Err(error) => app.set_failure(format!("{error:#}")),
                }
            }
        }
    }
}
