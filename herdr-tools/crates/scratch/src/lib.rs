//! The `scratch` mode of herdr-tools: `herdr-tools scratch <command>`.
//!
//! Per-pane persistent scratch popups (nvim, shell, or any configured command)
//! backed by private tmux servers, with a resident cleanup worker. Runtime Neovim
//! assets live in the plugin dir at `nvim/scratch/` (`Paths::root()`).
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-scratch/` (plugin `shadowfax.scratch`); that tree is read-only.
//! Until the port lands every command fails with "scratch is not ported yet".

use std::path::Path;

use anyhow::{bail, Result};
use clap::Subcommand;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "scratch";

/// `herdr-tools scratch …`. Manifest actions, panes and hooks call these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open or hide the persistent popup for scratch `<scratch>` over the focused pane.
    Toggle {
        #[arg(long, visible_alias = "kind")]
        scratch: String,
    },
    /// Run the popup terminal (normally launched by Herdr).
    RunPopup,
    /// Print the resolved config and popup sizes.
    Config,
    /// Preview reclaimable sessions; --apply performs one bounded sweep.
    Cleanup {
        #[arg(long)]
        apply: bool,
    },
    /// Reap stale sessions using the cleanup policy and notify the Herdr client.
    Reap,
    /// Ensure the independent background cleanup worker is running.
    CleanupStart,
    /// Ask the worker to stop; existing scratches remain intact.
    CleanupStop,
    /// The resident worker itself, started as `herdr-tools scratch cleanup-worker`.
    #[command(hide = true)]
    CleanupWorker,
}

/// Runs one `scratch` command.
pub fn run(command: Command) -> Result<()> {
    let _ = command;
    bail!("{MODE} is not ported yet")
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: `shadowfax.scratch/config.yaml` (default_popup, scratches, profiles,
/// cleanup); each scratch's popup maps onto popup key `scratch-<name>`. Scratch
/// state (tmux sockets, context files) lives under the old plugin's state dir;
/// if any must survive the cutover, say so here so the cutover copies it.
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    let _ = legacy_config_root;
    Ok(toml::Table::new())
}
