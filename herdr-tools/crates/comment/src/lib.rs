//! The `comment` mode of herdr-tools: `herdr-tools comment <command>`.
//!
//! Select and comment on frozen pane history, review the comments in Neovim, and
//! paste the reviewed draft into the pane. Runtime Neovim assets live in the
//! plugin dir at `nvim/comment/` (`Paths::root()`).
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-comments/` (plugin `shadowfax.comments`); that tree is read-only.
//! Until the port lands every command fails with "comment is not ported yet".

use std::path::Path;

use anyhow::{bail, Result};
use clap::Subcommand;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "comment";

/// `herdr-tools comment …`. Manifest actions, panes and hooks call these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Snapshot the invoking pane and open the annotation popup.
    Capture,
    /// Run the annotation popup (normally launched by Herdr).
    CapturePopup,
    /// Open the review popup for this pane's collected comments.
    Review,
    /// Run the review popup (normally launched by Herdr).
    ReviewPopup,
    /// Paste this pane's reviewed draft or collected comments without submitting it.
    Paste,
    /// Mark a review ready; called back by the Neovim review script.
    ConfirmReview {
        #[arg(long)]
        id: String,
    },
}

/// Runs one `comment` command.
pub fn run(command: Command) -> Result<()> {
    let _ = command;
    bail!("{MODE} is not ported yet")
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: `shadowfax.comments/config.yaml` (inline_comments, popups, profiles); popups
/// `capture`/`review` map onto popup keys `comment-capture`/`comment-review`.
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    let _ = legacy_config_root;
    Ok(toml::Table::new())
}
