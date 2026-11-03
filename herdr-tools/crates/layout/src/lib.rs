//! The `layout` mode of herdr-tools: `herdr-tools layout <command>`.
//!
//! Tmux-style layout shortcuts: 30% splits, focused-pane equalization, and an
//! exact columns x rows grid dialog.
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-layouts/` (plugin `shadowfax.layouts`); that tree is read-only.
//! Until the port lands every command fails with "layout is not ported yet".

use std::path::Path;

use anyhow::{bail, Result};
use clap::Subcommand;
use clap::ValueEnum;

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
        /// `<columns>x<rows>`, e.g. `3x2` (the port restores the typed `GridSize`).
        size: String,
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
    let _ = command;
    bail!("{MODE} is not ported yet")
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: none.
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    let _ = legacy_config_root;
    Ok(toml::Table::new())
}
