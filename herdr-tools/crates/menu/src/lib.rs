//! The `menu` mode of herdr-tools: `herdr-tools menu <command>`.
//!
//! A configurable command menu: fork or rotate agent conversations, redistribute
//! panes, copy pane context, and run the user's own shell items.
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-menu/` (plugin `shadowfax.menu`); that tree is read-only.
//! Until the port lands every command fails with "menu is not ported yet".

use std::path::Path;

use anyhow::{bail, Result};
use clap::Subcommand;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "menu";

/// `herdr-tools menu …`. Manifest actions, panes and hooks call these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the menu popup over the invoking pane.
    Open,
    /// Run the popup terminal (normally launched by Herdr).
    Menu,
    /// Copy the calling pane's identity and commands for another agent.
    CopyContext {
        /// Explicit source pane; defaults to the captured menu pane or HERDR_PANE_ID.
        #[arg(long)]
        source_pane: Option<String>,
        /// Print the handoff without changing the clipboard.
        #[arg(long)]
        stdout: bool,
    },
}

/// Runs one `menu` command.
pub fn run(command: Command) -> Result<()> {
    let _ = command;
    bail!("{MODE} is not ported yet")
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: `shadowfax.menu/config.toml` (title, items, width/height/max_height); its
/// `width`/`height` map onto `[popup.size].menu`, and item commands that invoke
/// old action ids or old plugin binaries need rewriting (`migrate-config` already
/// rewrites old action ids through `tools_core::legacy`).
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    let _ = legacy_config_root;
    Ok(toml::Table::new())
}
