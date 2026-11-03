//! `herdr-tools menu …` for this crate's CLI tests.
//!
//! The real `herdr-tools` binary belongs to the root package, and cargo exposes
//! `CARGO_BIN_EXE_<name>` only to a package's own integration tests. This stand-in
//! mounts the same `tools_menu::Command` under a `menu` subcommand and reports
//! errors with the same `herdr-tools menu: …` prefix as `src/main.rs`.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "herdr-tools")]
struct Cli {
    #[command(subcommand)]
    command: Top,
}

#[derive(Subcommand)]
enum Top {
    #[command(subcommand)]
    Menu(tools_menu::Command),
}

fn main() {
    let Top::Menu(command) = Cli::parse().command;
    if let Err(error) = tools_menu::run(command) {
        eprintln!("herdr-tools {}: {error:#}", tools_menu::MODE);
        std::process::exit(1);
    }
}
