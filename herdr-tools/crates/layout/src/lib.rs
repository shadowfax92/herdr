//! The `layout` mode of herdr-tools: `herdr-tools layout <command>`.
//!
//! Tmux-style layout shortcuts: 30% splits, focused-pane equalization, and an
//! exact columns x rows grid dialog.
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-layouts/`
//! (plugin `shadowfax.layouts`, HEAD `d2eb162`); that tree is read-only. Its own
//! Herdr client, socket client and context parsing became tools-core's [`Host`]
//! and [`tools_core::Invocation`]; the layout logic is unchanged. The old
//! `install-menu` action is gone: a menu item runs
//! `herdr-tools layout grid-dialog --pane "$HERDR_MENU_SOURCE_PANE_ID"` instead.
//!
//! ```text
//!  split left|right ─▶ herdr pane split (+ pane swap)              herdr.rs (CLI)
//!  equalize         ─▶ layout.export ─▶ layout.rs ─▶ layout.set_split_ratio …
//!                                                                  socket.rs (socket API)
//!  open-grid        ─▶ popup `layout-grid` (sized by [popup]) ─▶ grid-dialog
//!  grid-dialog      ─▶ grid_ui.rs preview ─Enter─▶ grid.rs GridController
//!  grid <CxR>       ─────────────────────────────▶ grid.rs GridController
//! ```
//!
//! Beyond `cargo test`, two real-herdr checks run in an isolated server (never
//! the user's session), from `herdr-tools/`: `python3 crates/layout/tests/real_grid.py
//! [path/to/herdr-menu]` and `python3 crates/layout/tests/real_actions.py`.

mod context;
pub mod grid;
pub mod grid_ui;
mod herdr;
mod layout;
mod socket;

use std::path::Path;

use anyhow::{bail, Result};
use clap::Subcommand;
use clap::ValueEnum;
use tools_core::Host;

use context::FocusedPane;
use grid::{ApplyOutcome, GridController, GridSize};
use layout::equalize_updates;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "layout";

/// `herdr-tools layout …`. Manifest actions, panes and hooks call these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create and focus a pane occupying `percent` of the width on one side.
    Split {
        #[arg(value_enum)]
        side: Side,
        #[arg(long, default_value_t = 30)]
        percent: u8,
    },
    /// Equalize the focused pane and adjacent panes in the same direction.
    Equalize,
    /// Arrange existing panes and add empty shells to fill columns x rows.
    Grid {
        /// `<columns>x<rows>`, e.g. `5x2`.
        size: GridSize,
        #[arg(long)]
        pane: Option<String>,
    },
    /// Open the native Create layout popup.
    OpenGrid,
    /// Run the layout dialog in the current terminal (also used by the menu).
    GridDialog {
        #[arg(long)]
        pane: Option<String>,
    },
}

/// Which side a [`Command::Split`] puts the new pane on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Side {
    Left,
    Right,
}

/// Runs one `layout` command.
pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Split { side, percent } => split(side, percent),
        Command::Equalize => equalize(),
        Command::OpenGrid => grid_ui::open(),
        Command::GridDialog { pane } => grid_ui::run(pane),
        Command::Grid { size, pane } => {
            let controller = GridController::from_environment(pane)?;
            let snapshot = controller.capture()?;
            match controller.apply(&snapshot, size)? {
                ApplyOutcome::Applied { created } => {
                    println!(
                        "Created {}x{} layout: kept {} panes, added {created} empty shells.",
                        size.columns,
                        size.rows,
                        snapshot.panes.len()
                    );
                    Ok(())
                }
                ApplyOutcome::Refresh(_) => {
                    bail!("Layout changed before apply; run the command again")
                }
            }
        }
    }
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: none. `shadowfax.layouts` read no config file (its config dir is
/// empty), and its popup used the manifest's fixed 80x24, which the new
/// `layout-grid` pane keeps as its default. It kept no state that must survive
/// the cutover: the grid lock is `<server socket>.layouts.lock` beside Herdr's
/// socket, which this mode still uses, so old and new grids exclude each other.
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    let _ = legacy_config_root;
    Ok(toml::Table::new())
}

/// Splits the focused pane so the new, focused pane takes `percent` of the
/// width on `side`. Herdr splits only right or down, so a left split splits
/// right and then swaps the pair.
pub fn split(side: Side, percent: u8) -> Result<()> {
    if !(10..=90).contains(&percent) {
        bail!("split percent must be between 10 and 90");
    }
    let pane = FocusedPane::from_env()?;
    let size = f32::from(percent) / 100.0;
    let first_child_ratio = match side {
        Side::Left => size,
        Side::Right => 1.0 - size,
    };
    let host = Host::from_env();
    let new_pane_id = herdr::split_right(&host, &pane.pane_id, &pane.cwd, first_child_ratio)?;
    if side == Side::Left {
        herdr::swap(&host, &new_pane_id, &pane.pane_id)?;
    }
    Ok(())
}

/// tmux `select-layout -E`: evens out the contiguous run of same-direction
/// splits nearest the focused pane, leaving perpendicular splits alone.
pub fn equalize() -> Result<()> {
    let pane = FocusedPane::from_env()?;
    let host = Host::from_env();
    let layout = socket::export_layout(&host, &pane.pane_id)?;
    let updates = equalize_updates(&layout.root, &pane.pane_id)?;
    for update in updates {
        socket::set_split_ratio(&host, &layout.tab_id, &update.path, update.ratio)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_plugin_had_no_config_to_migrate() {
        let root = tempfile::tempdir().unwrap();
        assert!(migrate_legacy(root.path()).unwrap().is_empty());
        // The owner's real `shadowfax.layouts/` config dir is empty; a copy of it.
        std::fs::create_dir(root.path().join("shadowfax.layouts")).unwrap();
        assert!(migrate_legacy(root.path()).unwrap().is_empty());
    }
}
