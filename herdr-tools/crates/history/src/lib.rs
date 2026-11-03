//! The `history` mode of herdr-tools: `herdr-tools history <command>`.
//!
//! Browse the focused pane's recent output and copy a target or selection with
//! keyboard hints.
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-talon/` (plugin `shadowfax.talon`); that tree is read-only.
//! Until the port lands every command fails with "history is not ported yet".

use std::path::Path;

use anyhow::{bail, Result};
use clap::Subcommand;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "history";

/// `herdr-tools history …`. Manifest actions, panes and hooks call these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Snapshot the invoking pane and open the picker popup over it.
    Launch,
    /// Run the popup terminal (normally launched by Herdr).
    Picker,
}

/// Runs one `history` command.
pub fn run(command: Command) -> Result<()> {
    let _ = command;
    bail!("{MODE} is not ported yet")
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: `shadowfax.talon/config.toml` (alphabet, patterns, popup, profiles); when it
/// sets no `popup`/`profiles`, talon's built-in default profiles applied, so the
/// migrated fragment must spell them out under `[[popup.profile]]` for key `history`.
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    let _ = legacy_config_root;
    Ok(toml::Table::new())
}
