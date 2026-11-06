//! Terminal ownership and asynchronous read handoffs for every picker. Only
//! these foreground loops may commit a jump, kill, paste or copy; read workers
//! cannot mutate focus, panes or the clipboard after Escape or teardown.
use crate::{
    age,
    app::{Action, App, Mode},
    catalog::{self, Candidate},
    command, deliver,
    host::{Host, KillExecutor},
    lines::{Batch, Item, LinePicker, Outcome},
    lines_ui,
    pickers::{LineSource, Lists},
    ui,
    worker::Worker,
};
use anyhow::Result;
use ratatui::{
    crossterm::{
        event::{self, DisableBracketedPaste, EnableBracketedPaste, Event},
        execute,
    },
    DefaultTerminal,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Runs `body` with the popup's terminal: raw mode and the alternate screen
/// (restored by Ratatui), plus bracketed paste so pasted text arrives as one
/// `Event::Paste` instead of keystrokes that could press Enter.
fn session(body: impl FnOnce(&mut DefaultTerminal) -> Result<()>) -> Result<()> {
    ratatui::run(|terminal| {
        execute!(std::io::stdout(), EnableBracketedPaste)?;
        let _paste = PasteGuard;
        body(terminal)
    })
}

pub(crate) fn run(host: Host, source: String) -> Result<()> {
    let own = std::env::var("HERDR_PANE_ID").unwrap_or_default();
    let mut app = App::with_context(source.clone(), own.clone());
    let killer = KillExecutor::new(host.clone(), source.clone(), own.clone());
    let catalog_host = host.clone();
    // The worker coalesces queued reads but may publish an older in-flight one.
    // Carry foreground revisions through the handoff so an old policy cannot
    // clear loading or arm a kill review while a newer refresh is pending.
    let catalog =
        Worker::new(move |revision: u64| (revision, catalog_host.candidates(&source, &own)));
    let preview_host = host.clone();
    let preview = Worker::new(move |(generation, target): (u64, Candidate)| {
        (generation, preview_host.preview(&target))
    });
    catalog.submit(app.request_catalog());
    session(|terminal| {
        let mut dirty = true;
        let mut generation = app.generation;
        let mut selected_at = Instant::now();
        let mut requested = None;
        let mut second = age::now_ms() / 1000;
        loop {
            // Rows show session ages from the wall clock; without input or
            // worker results nothing else redraws, so `45s` would freeze.
            let now = age::now_ms() / 1000;
            if now != second {
                second = now;
                dirty = true;
            }
            if dirty {
                terminal.draw(|frame| ui::render(&app, frame))?;
                dirty = false;
            }
            // Drain queued keyboard input before delivering asynchronous
            // results. Enter queued before a failed jump's refresh must not
            // commit a different selection that appeared while I/O completed.
            if event::poll(Duration::from_millis(16))? {
                for _ in 0..64 {
                    let event = event::read()?;
                    if matches!(event, Event::Resize(_, _)) {
                        terminal.autoresize()?;
                    }
                    let previous_mode = app.mode();
                    match app.handle(event) {
                        Action::Cancel => return Ok(()),
                        Action::Refresh => catalog.submit(app.request_catalog()),
                        Action::Jump => {
                            if let Some(target) = app.selected().cloned() {
                                app.message = Some("Jumping…".into());
                                terminal.draw(|frame| ui::render(&app, frame))?;
                                match host.jump(&target) {
                                    Ok(()) => return Ok(()),
                                    Err(error) => {
                                        app.message = Some(format!(
                                            "{} · Edit or move selection to retry",
                                            catalog::clean(&format!("{error:#}"))
                                        ));
                                        catalog.submit(app.request_catalog());
                                    }
                                }
                            }
                        }
                        Action::Kill(targets) => {
                            app.message = Some(format!("Killing {} panes…", targets.len()));
                            terminal.draw(|frame| ui::render(&app, frame))?;
                            app.killed(killer.kill(&targets).summary());
                            catalog.submit(app.request_catalog());
                        }
                        Action::None => {}
                    }
                    if previous_mode != Mode::Confirm
                        && app.mode() == Mode::Confirm
                        && !show_confirmation(terminal, &mut app)?
                    {
                        return Ok(());
                    }
                    dirty = true;
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            if let Some((revision, result)) = catalog.take() {
                if app.catalog_finished(revision, result.map_err(|error| format!("{error:#}"))) {
                    if app.mode() == Mode::Confirm && !show_confirmation(terminal, &mut app)? {
                        return Ok(());
                    }
                    dirty = true;
                }
            }
            if app.generation != generation {
                generation = app.generation;
                selected_at = Instant::now();
                requested = None;
            }
            if app.preview_visible
                && requested != Some(generation)
                && selected_at.elapsed() >= Duration::from_millis(60)
            {
                if let Some(target) = app.selected() {
                    preview.submit((generation, target.clone()));
                    requested = Some(generation);
                }
            }
            if let Some((generation, result)) = preview.take() {
                app.apply_preview(
                    generation,
                    result
                        .unwrap_or_else(|e| catalog::clean(&format!("Preview unavailable: {e:#}"))),
                );
                dirty = true;
            }
        }
    })
}

/// Initial and refreshed confirmations use the same foreground consent seam.
/// Flushing the revised target set and draining typed-ahead input must happen
/// before `y` can authorize it; a still-pending catalog cannot arm confirmation.
fn show_confirmation(terminal: &mut DefaultTerminal, app: &mut App) -> Result<bool> {
    terminal.draw(|frame| ui::render(app, frame))?;
    while event::poll(Duration::ZERO)? {
        let queued = event::read()?;
        if matches!(queued, Event::Resize(_, _)) {
            terminal.autoresize()?;
            terminal.draw(|frame| ui::render(app, frame))?;
        }
        if app.handle(queued) == Action::Cancel {
            return Ok(false);
        }
    }
    app.confirmation_shown();
    Ok(true)
}

/// Runs one list's command on its own worker, in the source pane's directory.
fn loader(host: &Host, source_pane: &str, spec: &LineSource) -> Worker<(), Result<Batch, String>> {
    let (host, pane, command, origin, kind) = (
        host.clone(),
        source_pane.to_owned(),
        spec.command.clone(),
        spec.origin.clone(),
        spec.kind,
    );
    Worker::new(move |()| -> Result<Batch, String> {
        let home = std::env::var("HOME").ok().filter(|home| !home.is_empty());
        let cwd = host
            .pane_cwd(&pane)
            .ok()
            .flatten()
            .or_else(|| home.as_deref().map(PathBuf::from).filter(|p| p.is_dir()))
            .unwrap_or_else(|| PathBuf::from("/"));
        let output = command::run(&command, &origin, &cwd, command::DEADLINE)?;
        Ok(Batch {
            items: output
                .lines
                .iter()
                .map(|line| Item::new(line, kind, home.as_deref(), &cwd))
                .collect(),
            skipped: output.skipped,
            truncated: output.truncated,
        })
    })
}

/// A line picker: each list's command runs on its own worker, and Enter
/// delivers the marks (or the selection) here, on the foreground loop.
/// The popup closes only after every Enter effect succeeded; otherwise it stays
/// open and says what did and didn't happen.
///
/// Every list of a group starts loading at open, each on its own thread, so
/// the heading can count them all and a slow list (recent-files) never delays
/// the one on screen (zoxide): results are applied per list as they land.
pub(crate) fn run_lines(host: Host, source_pane: String, lists: &Lists) -> Result<()> {
    // A group's members agree on these, so the first list speaks for all.
    let shared = &lists.sources[0];
    let mut state = LinePicker::with_lists(
        shared.kind,
        shared.enter.clone(),
        shared.multi,
        lists.sources.len(),
        lists.opened,
    );
    let chrome = lines_ui::Chrome { lists };
    let loaders: Vec<_> = lists
        .sources
        .iter()
        .map(|spec| loader(&host, &source_pane, spec))
        .collect();
    // The list on screen first, so its thread starts first.
    loaders[state.active()].submit(());
    for (index, loader) in loaders.iter().enumerate() {
        if index != state.active() {
            loader.submit(());
        }
    }
    session(|terminal| {
        let mut dirty = true;
        loop {
            if dirty {
                terminal.draw(|frame| lines_ui::render(&state, &chrome, frame))?;
                dirty = false;
            }
            // Input drains before load results, as in the navigator: typed-ahead
            // keys apply to the list the user was looking at.
            if event::poll(Duration::from_millis(16))? {
                for _ in 0..64 {
                    let event = event::read()?;
                    if matches!(event, Event::Resize(_, _)) {
                        terminal.autoresize()?;
                    }
                    match state.handle(event) {
                        Outcome::Cancel => return Ok(()),
                        Outcome::Reload => loaders[state.active()].submit(()),
                        Outcome::Deliver(delivery) => {
                            match deliver::deliver(
                                &host,
                                &source_pane,
                                &delivery.values,
                                state.kind(),
                                &delivery.effects,
                            ) {
                                Ok(()) => return Ok(()),
                                Err(failure) => state.delivery_failed(
                                    delivery.values,
                                    failure.done,
                                    failure.message,
                                ),
                            }
                        }
                        Outcome::None => {}
                    }
                    dirty = true;
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            for (index, loader) in loaders.iter().enumerate() {
                if let Some(result) = loader.take() {
                    state.loaded(index, result);
                    dirty = true;
                }
            }
        }
    })
}

struct PasteGuard;
impl Drop for PasteGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    }
}
