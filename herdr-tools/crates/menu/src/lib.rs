//! The `menu` mode of herdr-tools: `herdr-tools menu <command>`.
//!
//! A configurable command menu: fork or rotate agent conversations, redistribute
//! panes, copy pane context, and run the user's own shell items.
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-menu/`
//! (plugin `shadowfax.menu`, HEAD `abc628a`); that tree is read-only.
//!
//! ```text
//!  action shadowfax.tools.menu ─▶ menu open   (herdr::launch_from_environment)
//!     captures the source pane + cwd, sizes the popup from [popup.size].menu or
//!     the item count, opens plugin pane `menu` with HERDR_MENU_SOURCE_* env
//!                                    │
//!  popup pane `menu`  ─────────────▶ menu menu   (herdr::run_menu_from_environment)
//!     reads [menu] (config), runs the TUI (app + ui), and executes the chosen
//!     item: agent_panes (fork/rotate) · redistribute · handoff (copy context)
//!     · scope (focus/hide picker, focus switch) · execute (shell items)
//!
//!  any Herdr pane ─────────────────▶ menu copy-context [--stdout]
//!  the menu, detached ─────────────▶ menu scope-pick <focus|hide>  (scope.rs)
//! ```
//!
//! Every Herdr call goes through [`herdr::Herdr`], a thin adapter over
//! `tools_core::Host` that keeps the old method names; the mode-specific calls
//! (pane split/swap/move, layout export) live there and in the feature modules.

use std::path::Path;

use anyhow::Result;
use clap::Subcommand;

pub mod agent_panes;
pub mod app;
pub mod clipboard;
pub mod config;
pub mod execute;
mod failure_ui;
mod fork_layout;
pub mod handoff;
pub mod herdr;
mod legacy;
pub mod redistribute;
pub mod redistribute_ui;
pub mod scope;
pub mod ui;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "menu";

/// The manifest pane the menu opens as a popup; it doubles as the
/// `[popup.size]` / `[[popup.profile]]` key.
pub const POPUP: &str = "menu";

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
    /// Open Herdr's native focus/hide picker once the menu popup has gone.
    /// The menu starts this itself; it reads stdin to EOF first.
    #[command(hide = true)]
    ScopePick {
        /// `focus` or `hide`: what the picker's space toggles.
        mode: config::ScopePickMode,
    },
}

/// Runs one `menu` command.
pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Open => herdr::launch_from_environment(),
        Command::Menu => herdr::run_menu_from_environment(),
        Command::CopyContext {
            source_pane,
            stdout,
        } => herdr::copy_context_from_environment(source_pane, stdout),
        Command::ScopePick { mode } => scope::run_pick_helper(mode),
    }
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: `shadowfax.menu/config.toml`. Its `title`, `max_height` and
/// `items` become `[menu]`, its `width`/`height` become `[popup.size].menu`, and
/// old action ids inside item commands are rewritten (see [`legacy`]).
///
/// No state needs to survive the cutover: copy nothing from the old
/// `~/.local/state/herdr/plugins/shadowfax.menu/`. It holds only `last-fork.json`
/// (the pane pair `rotate_fork` swaps, keyed by live pane ids) and two advisory
/// lock files. The new state lives in `<plugin state dir>/menu/`; until the
/// first fork there, rotate reports that no fork has been recorded, exactly as
/// on a fresh install.
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    legacy::migrate(legacy_config_root)
}
