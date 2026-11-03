//! The `move` mode of herdr-tools: `herdr-tools move <command>`.
//!
//! Move or merge live panes, tabs and workspaces, or kill them, from a native
//! popup.
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-ferry/` (plugin `shadowfax.ferry`); that tree is read-only.
//! Until the port lands every command fails with "move is not ported yet".

use std::path::Path;

use anyhow::{bail, Result};
use clap::Subcommand;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "move";

/// `herdr-tools move …`. Manifest actions, panes and hooks call these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the picker popup over the invoking pane.
    Open,
    /// Run the popup terminal (normally launched by Herdr).
    Picker,
    /// Runs a kill the picker confirmed and handed off; not meant to be run by hand.
    /// The picker starts it as `herdr-tools move execute-kill` (see `tools_core::self_command`).
    #[command(hide = true)]
    ExecuteKill,
}

/// Runs one `move` command.
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
