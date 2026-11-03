//! Help and completion spec for `herdr scope` (parsing lives in `cli/scope.rs`).
use clap::{Arg, Command};

use super::{flag, json_flag};

const TARGET_HELP: &str =
    "[machine:]workspace[/tab]; the machine defaults to local, `studio:` names a whole machine";

pub(super) fn command() -> Command {
    Command::new("scope")
        .about("Focus on or hide workspaces and tabs across machines")
        .subcommand(
            Command::new("get")
                .about("Show what is focused and hidden")
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("focus")
                .about("Show only these targets, or turn the kept focus back on")
                .arg(targets(false))
                .arg(flag("add").help("Add to the focused targets instead of replacing them"))
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("unfocus")
                .about("Turn focus off and keep its targets")
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("toggle")
                .about("Turn focus on or off")
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("hide")
                .about("Hide targets from the panels and notifications")
                .arg(targets(true))
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("unhide")
                .about("Show hidden targets again")
                .arg(targets(true))
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("clear")
                .about("Forget focus and runtime hides; restore the config's hides")
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("pick")
                .about("Open the focus or hide picker in the attached client")
                .arg(
                    Arg::new("mode")
                        .value_name("MODE")
                        .value_parser(["focus", "hide"])
                        .help("Which mark space toggles (default: focus)"),
                ),
        )
}

fn targets(required: bool) -> Arg {
    Arg::new("targets")
        .value_name("TARGET")
        .num_args(if required { 1.. } else { 0.. })
        .required(required)
        .help(TARGET_HELP)
}
