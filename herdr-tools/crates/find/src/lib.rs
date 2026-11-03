//! The `find` mode of herdr-tools: `herdr-tools find <command>`.
//!
//! Search every open pane by task, pane name, path, workspace or tab and jump
//! to it; or pick a zoxide path and type it into the invoking pane.
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-jump/` (plugin `shadowfax.jump`); that tree is read-only.
//! Until the port lands every command fails with "find is not ported yet".

use std::path::Path;

use anyhow::{bail, Result};
use clap::Subcommand;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "find";

/// `herdr-tools find …`. Manifest actions, panes and hooks call these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open a picker popup over the invoking pane (default: the pane navigator).
    Open {
        /// A picker id from pickers.toml, e.g. `paths`.
        picker: Option<String>,
    },
    /// Run the popup terminal (normally launched by Herdr).
    Picker,
}

/// Runs one `find` command.
pub fn run(command: Command) -> Result<()> {
    let _ = command;
    bail!("{MODE} is not ported yet")
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: none of its own (its pickers live in the embedded `pickers.toml`).
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    let _ = legacy_config_root;
    Ok(toml::Table::new())
}
