//! The `history` mode of herdr-tools: `herdr-tools history <command>`.
//!
//! Browse the focused pane's recent output and copy a target or selection with
//! keyboard hints.
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-talon/`
//! (plugin `shadowfax.talon`, HEAD `ce1f7c9`); that tree is read-only. Behavior is
//! unchanged. What moved to tools-core: the Herdr client ([`tools_core::Host`]),
//! invocation parsing ([`tools_core::Invocation`]), config loading (the `[history]`
//! table of the one `config.toml`), and popup sizing (popup key [`POPUP_KEY`]).
//!
//! ```text
//!  action shadowfax.tools.history ─▶ `history open`   (snapshot.rs)
//!     read the source pane's history (herdr.rs), match targets (matcher.rs),
//!     write a private one-shot handoff (RunStore), open the `history` popup
//!     with HERDR_HISTORY_RUN_ID
//!                                        │
//!  popup pane `history` ──────────────▶ `history picker` (picker.rs)
//!     claim the handoff, run the TUI (app.rs + document.rs + ui.rs, hints.rs),
//!     copy the completion through the clipboard utility (actions.rs), exit
//! ```

use std::path::Path;

use anyhow::Result;
use clap::Subcommand;

pub mod actions;
pub mod app;
mod cells;
pub mod config;
pub mod document;
pub mod herdr;
pub mod hints;
pub mod matcher;
mod migrate;
pub mod picker;
pub mod snapshot;
pub mod ui;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "history";

/// The popup key for `[popup.size]` / `[[popup.profile]]`, and the manifest pane id.
pub const POPUP_KEY: &str = "history";

/// The title of every notification this mode shows (talon's said "Talon").
pub const NOTIFICATION_TITLE: &str = "History";

/// `herdr-tools history …`. Manifest actions and panes call these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Snapshot the invoking pane and open the picker popup over it (talon's `launch`).
    Open,
    /// Run the popup terminal (normally launched by Herdr).
    Picker,
}

/// Runs one `history` command.
pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Open => {
            snapshot::launch_from_environment()?;
            Ok(())
        }
        Command::Picker => picker::run_from_environment(),
    }
}

/// This mode's fragment of the new `config.toml`, built from talon's old config
/// under `legacy_config_root` (`~/.config/herdr/plugins/config`): its
/// `shadowfax.talon/config.toml` hint settings become `[history]`, and its popup
/// sizes become `[popup.size].history` and `[[popup.profile]]` entries. Empty
/// when talon has no config dir. `src/migrate.rs` has the rules.
///
/// Talon kept only transient run handoffs in its state dir, so nothing there
/// needs to survive the cutover.
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    migrate::migrate_legacy(legacy_config_root)
}
