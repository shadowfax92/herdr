//! CLI entry point, kept outside the upstream parser so the fork owns SSH syntax.

use std::io;

pub fn run(_args: &[String]) -> io::Result<i32> {
    let cfg = crate::config::Config::load().config.ssh_panes;
    if !cfg.enabled {
        eprintln!("{}", super::disabled_message());
        return Ok(2);
    }
    run_enabled()
}

#[cfg(unix)]
fn run_enabled() -> io::Result<i32> {
    eprintln!("{}", super::NOT_IMPLEMENTED_MESSAGE);
    Ok(1)
}

#[cfg(not(unix))]
fn run_enabled() -> io::Result<i32> {
    eprintln!("ssh-panes: not supported on this platform");
    Ok(1)
}
