//! The `find` mode of herdr-tools: `herdr-tools find <command>`.
//!
//! Hosts transient picker popups (the pane navigator, zoxide paths with recent
//! files, and whatever else `pickers.toml` declares); Herdr owns panes and focus.
//!
//! ```text
//!  action find / find-paths ──▶ `find open [picker]`   validates the picker id,
//!                                    │                 sizes the popup (config, else
//!                                    │                 the picker's width), opens it
//!                                    ▼ HERDR_JUMP_SOURCE_PANE, HERDR_JUMP_PICKER
//!  popup pane `find` ─────────▶ `find picker`          owns the terminal: panes
//!                                                      navigator or a line picker
//! ```
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-jump/`
//! (plugin `shadowfax.jump`, HEAD `3afea65`); that tree is read-only. Herdr calls
//! go through [`tools_core::Host`]; the mode-specific ones (snapshot, focus, kill,
//! preview, paste) live in [`host`] on top of its socket primitive.
mod age;
mod app;
mod catalog;
mod command;
mod deliver;
mod host;
mod lines;
mod lines_ui;
mod matcher;
mod picker;
mod pickers;
mod prompt;
mod ui;
mod worker;

use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;
use tools_core::{Config, Invocation};

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "find";

/// `herdr-tools find …`. Manifest actions and the popup pane call these.
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

/// `[find]` in config.toml. Find has no settings of its own yet (its pickers are
/// the embedded `pickers.toml`; its popup sizes live in `[popup]`), so the table
/// must be empty: a typo there fails the action instead of being ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {}

/// Runs one `find` command.
pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Open { picker } => {
            // Validate before opening so a bad id fails in the action's log,
            // not as an empty popup.
            let picker = pickers::find(picker.as_deref().unwrap_or(pickers::DEFAULT))?;
            let host = host::Host::from_environment()?;
            let source = Invocation::current()?.source_pane_or_env()?;
            let config = Config::current()?;
            config.section::<Settings>(MODE)?;
            host.open(&source, &picker, config)
        }
        Command::Picker => {
            let source = std::env::var("HERDR_JUMP_SOURCE_PANE")
                .context("picker requires its launch context; invoke shadowfax.tools.find")?;
            // Popups opened by an older `open` carry no picker id: the navigator.
            let id = std::env::var("HERDR_JUMP_PICKER").unwrap_or_else(|_| pickers::DEFAULT.into());
            let picker = pickers::find(&id)?;
            let host = host::Host::from_environment()?;
            match picker.source {
                pickers::Source::Panes => picker::run(host, source),
                // A grouped picker opens its whole group, on its own list.
                pickers::Source::Lines(_) => {
                    picker::run_lines(host, source, &pickers::lists(&picker.id)?)
                }
            }
        }
    }
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Always
/// empty: herdr-jump read no config file, so `shadowfax.jump/` holds nothing to
/// carry over (the owner's is an empty directory). Its built-in popup sizes —
/// each picker's `width` in `pickers.toml`, 90% high — still apply whenever
/// `[popup]` leaves a picker's key unset, so they need no explicit entry.
///
/// State: none to carry over. Jump never wrote any (the owner's
/// `~/.local/state/herdr/plugins/shadowfax.jump/` is empty) and Find writes
/// none either, so it has no `<state>/find/` dir and the cutover copies nothing.
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    let _ = legacy_config_root;
    Ok(toml::Table::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_legacy_is_empty_for_a_missing_or_empty_jump_config() {
        let root = tempfile::tempdir().unwrap();
        assert!(migrate_legacy(root.path()).unwrap().is_empty());
        // The owner's real `shadowfax.jump/` is an empty directory; a stray file
        // there is still not jump config, because jump never read one.
        let jump = root.path().join("shadowfax.jump");
        std::fs::create_dir(&jump).unwrap();
        assert!(migrate_legacy(root.path()).unwrap().is_empty());
        std::fs::write(jump.join("notes.txt"), "not config").unwrap();
        assert!(migrate_legacy(root.path()).unwrap().is_empty());
    }

    #[test]
    fn the_find_table_has_no_settings_yet() {
        let config = Config::parse("[find]\n").unwrap();
        config.section::<Settings>(MODE).unwrap();
        let config = Config::parse("[find]\nwidth = 120\n").unwrap();
        let error = config.section::<Settings>(MODE).unwrap_err();
        assert!(format!("{error:#}").contains("width"), "{error:#}");
    }
}
