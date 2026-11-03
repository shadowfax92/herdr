//! tools-core: the small, generic seam every herdr-tools mode builds on.
//!
//! Herdr starts `./bin/herdr-tools <mode> <command…>` for each manifest action,
//! popup pane, startup hook, and event. Each process reads the same few things:
//!
//! ```text
//!  herdr ──env──▶ Invocation (context, action, entrypoint, event)  parsed once
//!        ──env──▶ Paths      (plugin config / state / root dirs)
//!                 Config     ($HERDR_PLUGIN_CONFIG_DIR/config.toml) loaded once
//!                   └─ [popup] size + profiles ─▶ ResolvedPopup
//!  mode ──────▶ Host  ── CLI (herdr <args>)  ─┐
//!                     ── socket (method, params) ─┴─▶ herdr server
//! ```
//!
//! Core holds no mode logic: a mode's own herdr calls live in its crate on top of
//! [`Host::cli`], [`Host::cli_json`] and [`Host::request`]. Matchers and TUIs stay
//! per mode for now. The one shared read is [`scope`]: Herdr's focus/hide verdict
//! per local tab, which every pane list filters by.

pub mod config;
pub mod host;
pub mod invocation;
pub mod legacy;
pub mod paths;
pub mod popup;
pub mod scope;

#[cfg(any(test, feature = "test-util"))]
pub use host::testing;

pub use config::Config;
pub use host::{Envelope, Host, HostError, PaneArea};
pub use invocation::{Invocation, PluginContext};
pub use paths::Paths;
pub use popup::{PopupConfig, PopupDims, PopupProfile, PopupSize, ResolvedPopup};
pub use scope::{ScopeFilter, ScopeReport, ScopeTab};

/// The plugin id in `herdr-plugin.toml`. Popups, actions and config dirs all hang off it.
pub const PLUGIN_ID: &str = "shadowfax.tools";

/// A [`std::process::Command`] that runs this same executable as `herdr-tools <mode>`.
///
/// Modes that hand work to a detached copy of themselves (a kill executor, a
/// cleanup worker, an editor callback) must prefix the mode: the old plugins ran
/// `current_exe() <command>`, but the merged binary needs `current_exe() <mode> <command>`.
pub fn self_command(mode: &str) -> anyhow::Result<std::process::Command> {
    use anyhow::Context as _;
    let exe = std::env::current_exe().context("the herdr-tools executable path is unavailable")?;
    let mut command = std::process::Command::new(exe);
    command.arg(mode);
    Ok(command)
}
