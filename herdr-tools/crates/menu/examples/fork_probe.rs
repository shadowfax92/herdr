//! Headless acceptance entrypoint: exercise the same controller as the popup
//! against an explicitly supplied private server, without starting a TUI.
use anyhow::{bail, Result};
use tools_menu::agent_panes::AgentPaneController;
use tools_menu::config::ForkPlacement;
use tools_menu::herdr::Herdr;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 8 {
        bail!("usage: fork_probe HERDR SOCKET SOURCE CWD STATE RTR left|right|top|bottom");
    }
    let placement = match args[7].as_str() {
        "left" => ForkPlacement::Left,
        "right" => ForkPlacement::Right,
        "top" => ForkPlacement::Top,
        "bottom" => ForkPlacement::Bottom,
        _ => bail!("unknown placement"),
    };
    AgentPaneController::new(
        Herdr::new(&args[1]).with_socket(&args[2]),
        &args[3],
        &args[4],
        &args[5],
        &args[6],
    )
    .fork(placement)
}
