//! Local SSH process ownership: pane transport and noninteractive side channels.
//! The skeleton launches nothing, including when the feature is enabled.

use std::{collections::BTreeMap, io};

use super::config::SshPanesConfig;

#[derive(Debug, Clone, Default)]
pub(crate) struct ConnectOptions {
    pub host: String,
    pub cwd: Option<String>,
    pub no_sync: bool,
    pub ssh_args: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RunOptions {
    pub host: String,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub no_tty: bool,
    pub argv: Vec<String>,
}

pub fn connect(_opts: ConnectOptions, cfg: &SshPanesConfig) -> io::Result<i32> {
    unavailable(cfg)
}

pub fn run_remote(_opts: RunOptions, cfg: &SshPanesConfig) -> io::Result<i32> {
    unavailable(cfg)
}

fn unavailable(cfg: &SshPanesConfig) -> io::Result<i32> {
    let (message, code) = if cfg.enabled {
        (super::NOT_IMPLEMENTED_MESSAGE.to_string(), 1)
    } else {
        (super::disabled_message(), 2)
    };
    eprintln!("{message}");
    Ok(code)
}
