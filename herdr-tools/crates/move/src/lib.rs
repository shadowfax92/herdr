//! The `move` mode of herdr-tools: `herdr-tools move <command>`.
//!
//! Move or merge live panes, tabs and workspaces, or kill them, from a native
//! popup.
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-ferry/`
//! (plugin `shadowfax.ferry`, HEAD `bc0c6b9`); that tree is read-only. The port keeps
//! ferry's modules and behavior. What changed is the plumbing: Herdr calls go through
//! [`tools_core::Host`], the source pane comes from [`tools_core::Invocation`], and the
//! popup size comes from the shared `config.toml` (popup key [`POPUP_KEY`]).
//!
//! ```text
//!  action `shadowfax.tools.move`      popup pane `move`              detached, own session
//!  ─────────────────────────────      ─────────────────────────      ─────────────────────
//!  herdr-tools move open ──open──▶    herdr-tools move picker  ──y──▶ herdr-tools move
//!  (herdr::launch_from_environment)   (picker: App + ui + Mover)      execute-kill (kill_ops)
//! ```
//!
//! Moves and merges run inside the popup ([`move_ops`]); a confirmed kill is handed to a
//! detached executor ([`kill_ops`]) because it may close the popup's own tab.
//!
//! Every list the picker offers (sources, destinations, kill targets) follows Herdr's
//! focus/hide scope, read once per popup as the server's per-tab verdict
//! ([`tools_core::ScopeFilter`]). The tab the popup was opened from and its workspace stay
//! listed whatever the scope says, since the user is looking at them. Validation, kill
//! plans and the review's "what else dies" count use the full topology: scope narrows what
//! can be picked, never what a picked target contains.

use std::path::Path;

use anyhow::Result;
use clap::Subcommand;

pub mod app;
pub mod fuzzy;
pub mod herdr;
pub mod kill;
pub mod kill_ops;
pub mod layout;
pub mod move_ops;
pub mod picker;
pub mod ui;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "move";

/// The manifest's `[[panes]]` id for the picker popup.
pub const PICKER_ENTRYPOINT: &str = "move";

/// The key that sizes the picker in `[popup.size]` and `[[popup.profile]]`. Unconfigured, Herdr
/// uses the manifest pane's 76×24, the size ferry always passed.
pub const POPUP_KEY: &str = "move";

/// The title of every notification this mode shows (ferry's was "Ferry").
pub const NOTIFICATION_TITLE: &str = "Move";

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
    match command {
        Command::Open => herdr::launch_from_environment(),
        Command::Picker => picker::run_from_environment(),
        Command::ExecuteKill => kill_ops::run_from_environment(),
    }
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: none. Ferry read no config file (its `shadowfax.ferry/` config dir exists but
/// stays empty) and always opened its popup at 76×24, which is the `move` manifest pane's
/// default, so no `[popup.size]` entry is needed to keep that size. It kept no state either, so
/// the cutover copies nothing.
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    let _ = legacy_config_root;
    Ok(toml::Table::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_that_migration_yields_an_empty_fragment_with_or_without_the_old_dir() {
        let root = tempfile::tempdir().unwrap();
        assert!(migrate_legacy(root.path()).unwrap().is_empty());

        std::fs::create_dir(root.path().join("shadowfax.ferry")).unwrap();
        assert!(migrate_legacy(root.path()).unwrap().is_empty());
    }
}
