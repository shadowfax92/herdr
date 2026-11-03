//! `herdr-tools`: the one program behind the `shadowfax.tools` Herdr plugin.
//!
//! Herdr runs `./bin/herdr-tools <mode> <command…>` for every manifest action,
//! popup pane, startup hook and event; this file only dispatches to the mode
//! crate (`crates/<mode>`), which owns everything below its `run`. The one
//! command that is not a mode is `migrate-config`, which builds the merged
//! `config.toml` from the old plugins' configs.

mod migrate;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "herdr-tools",
    version,
    about = "Find, move, history, menu, comment, layout and scratch tools for Herdr"
)]
struct Cli {
    #[command(subcommand)]
    command: Top,
}

#[derive(Debug, Subcommand)]
enum Top {
    /// Search panes and jump to one, or paste a zoxide path (formerly herdr-jump).
    #[command(subcommand)]
    Find(tools_find::Command),
    /// Move, merge, or kill panes, tabs and workspaces (formerly herdr-ferry).
    #[command(subcommand)]
    Move(tools_move::Command),
    /// Browse pane history and copy values with hints (formerly herdr-talon).
    #[command(subcommand)]
    History(tools_history::Command),
    /// The configurable command and agent-fork menu (formerly herdr-menu).
    #[command(subcommand)]
    Menu(tools_menu::Command),
    /// Annotate, review and paste comments on pane history (formerly herdr-comments).
    #[command(subcommand)]
    Comment(tools_comment::Command),
    /// Narrow splits, equalize, and grid layouts (formerly herdr-layouts).
    #[command(subcommand)]
    Layout(tools_layout::Command),
    /// Persistent per-pane scratch popups (formerly herdr-scratch).
    #[command(subcommand)]
    Scratch(tools_scratch::Command),
    /// Merge the old plugins' configs into one shadowfax.tools config.toml.
    MigrateConfig {
        /// The directory holding each old plugin's `shadowfax.<old>/` config dir
        /// [default: ~/.config/herdr/plugins/config].
        #[arg(long, value_name = "DIR")]
        legacy_root: Option<PathBuf>,
        /// Replace an existing config.toml.
        #[arg(long)]
        force: bool,
        /// Print the merged config instead of writing it.
        #[arg(long)]
        stdout: bool,
    },
}

impl Top {
    /// The name errors are reported under, matching the first argument.
    fn name(&self) -> &'static str {
        match self {
            Self::Find(_) => tools_find::MODE,
            Self::Move(_) => tools_move::MODE,
            Self::History(_) => tools_history::MODE,
            Self::Menu(_) => tools_menu::MODE,
            Self::Comment(_) => tools_comment::MODE,
            Self::Layout(_) => tools_layout::MODE,
            Self::Scratch(_) => tools_scratch::MODE,
            Self::MigrateConfig { .. } => "migrate-config",
        }
    }

    fn run(self) -> anyhow::Result<()> {
        match self {
            Self::Find(command) => tools_find::run(command),
            Self::Move(command) => tools_move::run(command),
            Self::History(command) => tools_history::run(command),
            Self::Menu(command) => tools_menu::run(command),
            Self::Comment(command) => tools_comment::run(command),
            Self::Layout(command) => tools_layout::run(command),
            Self::Scratch(command) => tools_scratch::run(command),
            Self::MigrateConfig {
                legacy_root,
                force,
                stdout,
            } => migrate::run(legacy_root, force, stdout),
        }
    }
}

fn main() {
    let command = Cli::parse().command;
    let name = command.name();
    // Herdr records an action's stderr in the plugin log (`herdr plugin log`);
    // modes that should also surface errors as notifications do so themselves.
    if let Err(error) = command.run() {
        eprintln!("herdr-tools {name}: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    /// Every command the manifest gives Herdr must parse, so a port cannot
    /// rename a subcommand out from under its action, pane, startup or event.
    #[test]
    fn every_manifest_command_parses() {
        let manifest: toml::Table = include_str!("../herdr-plugin.toml").parse().unwrap();
        let mut commands = Vec::new();
        for kind in ["actions", "panes", "startup", "events"] {
            for entry in manifest[kind].as_array().unwrap() {
                commands.push(entry["command"].as_array().unwrap().clone());
            }
        }
        assert!(commands.len() >= 25, "{commands:?}");
        for command in commands {
            let argv = command
                .iter()
                .map(|arg| arg.as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(argv[0], "./bin/herdr-tools", "{argv:?}");
            if let Err(error) = Cli::try_parse_from(&argv) {
                panic!("{argv:?} does not parse: {error}");
            }
        }
    }

    /// The manifest carries exactly DESIGN.md's actions and popup panes.
    #[test]
    fn manifest_ids_match_the_design() {
        let manifest: toml::Table = include_str!("../herdr-plugin.toml").parse().unwrap();
        assert_eq!(manifest["id"].as_str(), Some(tools_core::PLUGIN_ID));
        let ids = |kind: &str| {
            manifest[kind]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let mut expected = tools_core::legacy::ACTION_IDS
            .iter()
            .map(|(_, new)| new.strip_prefix("shadowfax.tools.").unwrap().to_owned())
            .collect::<Vec<_>>();
        let mut actions = ids("actions");
        expected.sort();
        actions.sort();
        assert_eq!(actions, expected);
        assert_eq!(
            ids("panes"),
            [
                "find",
                "move",
                "history",
                "menu",
                "comment-capture",
                "comment-review",
                "layout-grid",
                "scratch"
            ]
        );
        for pane in manifest["panes"].as_array().unwrap() {
            assert_eq!(pane["placement"].as_str(), Some("popup"));
            assert!(pane.get("width").is_some() && pane.get("height").is_some());
        }
    }
}
