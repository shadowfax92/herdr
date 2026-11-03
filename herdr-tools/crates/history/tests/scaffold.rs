//! The plugin contract `history` owns: its subcommands as `herdr-tools history`
//! mounts them, and its block of the shared `herdr-plugin.toml`.
//!
//! Talon's scaffold tests ran its own binary. The `herdr-tools` binary belongs to
//! the root package, which this crate's tests cannot build, so the CLI checks
//! parse through clap exactly as `src/main.rs` mounts [`Command`];
//! `tests/real_history.py` runs the real binary.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tools_history::Command;

/// `herdr-tools history <command>`, mounted the way `src/main.rs` does it.
#[derive(Debug, Parser)]
#[command(name = "herdr-tools")]
struct Cli {
    #[command(subcommand)]
    top: Top,
}

#[derive(Debug, Subcommand)]
enum Top {
    #[command(subcommand)]
    History(Command),
}

fn parse(args: &[&str]) -> Result<Command, clap::Error> {
    let argv = ["herdr-tools", "history"].iter().chain(args);
    Cli::try_parse_from(argv).map(|cli| match cli.top {
        Top::History(command) => command,
    })
}

#[test]
fn cli_requires_a_subcommand() {
    assert!(parse(&[]).is_err());
}

#[test]
fn cli_rejects_an_unknown_subcommand() {
    assert!(parse(&["unknown"]).is_err());
    // Talon's installer is gone, and `launch` is now `open`.
    assert!(parse(&["install-keybindings"]).is_err());
    assert!(parse(&["launch"]).is_err());
    assert!(matches!(parse(&["open"]), Ok(Command::Open)));
    assert!(matches!(parse(&["picker"]), Ok(Command::Picker)));
}

#[test]
fn manifest_declares_the_complete_plugin_contract() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../herdr-plugin.toml");
    let manifest: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();

    assert_eq!(manifest["id"].as_str(), Some(tools_core::PLUGIN_ID));
    // Herdr reads the plugin version from the manifest while Cargo stamps the
    // binary, so a release must bump both together.
    assert_eq!(
        manifest["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    let platforms: Vec<_> = manifest["platforms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|platform| platform.as_str().unwrap())
        .collect();
    assert_eq!(platforms, ["macos", "linux"]);
    assert_eq!(manifest["build"].as_array().unwrap().len(), 1);

    let mine = |kind: &str| {
        manifest[kind]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| {
                entry["command"].as_array().is_some_and(|argv| {
                    argv.get(1).and_then(toml::Value::as_str) == Some("history")
                })
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let actions = mine("actions");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["id"].as_str(), Some("history"));
    assert_eq!(
        actions[0]["description"].as_str(),
        Some("Browse recent focused-pane output and copy a target or selection.")
    );
    assert_eq!(
        actions[0]["contexts"].as_array().unwrap(),
        &[toml::Value::from("pane")]
    );
    assert_eq!(
        actions[0]["command"].as_array().unwrap(),
        &["./bin/herdr-tools", "history", "open"].map(toml::Value::from)
    );

    let panes = mine("panes");
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0]["id"].as_str(), Some(tools_history::POPUP_KEY));
    assert_eq!(panes[0]["placement"].as_str(), Some("popup"));
    assert_eq!(panes[0]["width"].as_str(), Some("90%"));
    assert_eq!(panes[0]["height"].as_str(), Some("90%"));
    assert_eq!(
        panes[0]["command"].as_array().unwrap(),
        &["./bin/herdr-tools", "history", "picker"].map(toml::Value::from)
    );
    for kind in ["startup", "events"] {
        assert!(mine(kind).is_empty(), "history registers no {kind} hooks");
    }
}
