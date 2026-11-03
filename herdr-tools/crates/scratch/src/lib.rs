//! The `scratch` mode of herdr-tools: `herdr-tools scratch <command>`.
//!
//! Per-pane persistent scratch popups (nvim, shell, or any configured command)
//! backed by private tmux servers, with a resident cleanup worker. Runtime Neovim
//! assets live in the plugin dir at `nvim/scratch/` (`Paths::root()`).
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-scratch/`
//! (plugin `shadowfax.scratch`, HEAD 10d9ad2) with unchanged behavior; Herdr
//! access, config, popup sizing and invocation parsing now come from tools-core.
//!
//! ```text
//!  scratch-<name> action ─▶ toggle ── plugin.pane.open (size: popup key scratch-<name>)
//!                                        │ env: name, source pane, cwd, tmux prefix
//!                                        ▼
//!  `scratch` popup pane  ─▶ run-popup ── herdr pane get (root: Grove worktree or cwd)
//!                                        │ publish context/<session>.json
//!                                        ▼
//!                           tmux attach to hs/<name>/<server>/<pane> on a private
//!                           server; the hide key detaches, which closes the popup
//!  [[startup]], pane.closed ─▶ cleanup-start ─▶ resident cleanup-worker (see cleanup)
//! ```

pub mod cleanup;
mod config;
mod context;
mod herdr;
mod legacy;
mod prefix;
mod tmux;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;
use tools_core::{Host, Invocation, Paths};

use config::{LoadedConfig, MANIFEST_POPUP};
use context::SourcePane;
use herdr::PopupRequest;

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "scratch";

/// Scratch's private state: `<HERDR_PLUGIN_STATE_DIR>/scratch/`. Every mode
/// keeps its state under its own subdir of the shared plugin state dir, so the
/// cleanup worker's locks, context files and tmux sockets (`tmux-<euid>/…`,
/// via `TMUX_TMPDIR`) can never meet another mode's files.
pub(crate) fn state_dir() -> PathBuf {
    Paths::from_env().state_dir().join(MODE)
}

/// `herdr-tools scratch …`. Manifest actions, panes and hooks call these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open or hide the persistent popup for scratch `<scratch>` over the focused pane.
    Toggle {
        #[arg(long, visible_alias = "kind")]
        scratch: String,
    },
    /// Run the popup terminal (normally launched by Herdr).
    RunPopup,
    /// Print the resolved config and popup sizes.
    Config,
    /// Preview reclaimable sessions; --apply performs one bounded sweep.
    Cleanup {
        #[arg(long)]
        apply: bool,
    },
    /// Reap stale sessions using the cleanup policy and notify the Herdr client.
    Reap,
    /// Ensure the independent background cleanup worker is running.
    CleanupStart,
    /// Ask the worker to stop; existing scratches remain intact.
    CleanupStop,
    /// The resident worker itself, started as `herdr-tools scratch cleanup-worker`.
    #[command(hide = true)]
    CleanupWorker,
}

/// Runs one `scratch` command.
pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Toggle { scratch } => toggle(&scratch),
        Command::RunPopup => run_popup(),
        Command::Config => show_config(),
        Command::Cleanup { apply } => cleanup::inspect(apply),
        Command::Reap => cleanup::reap(),
        Command::CleanupStart => cleanup::start(),
        Command::CleanupStop => cleanup::stop(),
        Command::CleanupWorker => cleanup::worker(),
    }
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: `shadowfax.scratch/config.yaml` (default_popup, scratches, profiles,
/// cleanup). `cleanup` and `scratches` become `[scratch]`; each scratch's popup
/// (its own `popup`, else `default_popup`) becomes `[popup.size].scratch-<name>`,
/// and `profiles` become `[[popup.profile]]` entries sizing every scratch, so each
/// client width resolves to the same size and profile as before (see `legacy`).
///
/// State: nothing is copied, because none of it can be. New state lives in
/// `<state>/scratch/`. The old state dir
/// (`~/.local/state/herdr/plugins/shadowfax.scratch/`) holds two live private
/// tmux servers (`tmux-<euid>/shadowfax-herdr-{scratch,workspace}`) running the
/// owner's editors and shells, the context files those editors watch, and a
/// cleanup cache keyed by those servers' pids. After the cutover, toggles start
/// fresh sessions under `shadowfax.tools`; old sessions keep running until their
/// server exits and can be reached with
/// `tmux -S ~/.local/state/herdr/plugins/shadowfax.scratch/tmux-$(id -u)/shadowfax-herdr-workspace attach`.
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    legacy::migrate(legacy_config_root)
}

pub fn toggle(name: &str) -> Result<()> {
    let host = Host::from_env();
    let source = match SourcePane::from_invocation(Invocation::current()?)? {
        Some(source) => source,
        None => herdr::current_pane(&host)?,
    };
    let config = LoadedConfig::load()?;
    let scratch = config.scratch(name)?;
    // Measured here, not through Host::open_popup, because a toggle without a
    // plugin context still knows its source pane from `herdr pane current`.
    let popup = config.popup(name, host.client_width(&source.pane_id).ok())?;
    let tmux_prefix = match scratch.tmux_prefix {
        Some(prefix) => prefix,
        None => prefix::tmux_prefix_from_user_config()?,
    };
    herdr::open_popup(
        &host,
        &PopupRequest {
            scratch_name: name.into(),
            source_pane_id: source.pane_id,
            source_cwd: source.cwd,
            tmux_prefix,
            size: popup.map(|popup| popup.dims),
        },
    )
}

pub fn run_popup() -> Result<()> {
    let scratch_name =
        std::env::var("HERDR_SCRATCH_NAME").context("HERDR_SCRATCH_NAME is missing")?;
    let source_pane_id = std::env::var("HERDR_SCRATCH_SOURCE_PANE")
        .context("HERDR_SCRATCH_SOURCE_PANE is missing")?;
    let context = herdr::scratch_context(&Host::from_env(), &source_pane_id)?;
    let state_dir = state_dir();
    let herdr_environment = tmux::HerdrEnvironment::from_process();
    let tmux_prefix = std::env::var("HERDR_SCRATCH_PREFIX").unwrap_or_else(|_| "C-b".into());
    let config = LoadedConfig::load()?;
    let scratch = config.scratch(&scratch_name)?;
    let hide_keys = config
        .keys()
        .map(prefix::normalize_tmux_key)
        .collect::<Result<Vec<_>>>()?;

    tmux::run(
        &scratch,
        &context,
        &herdr_environment,
        &state_dir,
        &tmux_prefix,
        &hide_keys,
    )
}

pub fn show_config() -> Result<()> {
    let config = LoadedConfig::load()?;
    let host = Host::from_env();
    let client_width = herdr::current_pane(&host)
        .and_then(|pane| host.client_width(&pane.pane_id))
        .ok();

    println!("config: {}", config.path.display());
    println!(
        "cleanup: {} ({} hours since last use, {} second interval)",
        if config.cleanup().enabled {
            "enabled"
        } else {
            "disabled"
        },
        config.cleanup().ttl_hours,
        config.cleanup().interval_seconds
    );
    println!(
        "client_width: {}",
        client_width
            .map(|width| width.to_string())
            .unwrap_or_else(|| "unavailable".into())
    );
    for name in config.names() {
        let scratch = config.scratch(name)?;
        let popup = config.popup(name, client_width)?;
        let size = popup.as_ref().map_or(MANIFEST_POPUP, |popup| popup.dims);
        let profile = popup
            .as_ref()
            .and_then(|popup| popup.profile.as_deref())
            .unwrap_or("default");
        let key = scratch.key.as_deref().unwrap_or("unbound");
        println!(
            "{name}: {} x {} ({profile}, {key})",
            size.width, size.height
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    fn manifest() -> toml::Table {
        include_str!("../../../herdr-plugin.toml").parse().unwrap()
    }

    fn command(entry: &toml::Value) -> Vec<&str> {
        entry["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect()
    }

    #[test]
    fn manifest_exposes_toggles_and_owned_reaping() {
        let manifest = manifest();
        let actions = manifest["actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|action| action["id"].as_str().unwrap().starts_with("scratch-"))
            .collect::<Vec<_>>();
        let ids = actions
            .iter()
            .map(|action| action["id"].as_str().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(ids, ["scratch-nvim", "scratch-shell", "scratch-reap"]);
        assert_eq!(
            command(actions[0]),
            [
                "./bin/herdr-tools",
                "scratch",
                "toggle",
                "--scratch",
                "nvim"
            ]
        );
        assert_eq!(
            command(actions[1]),
            [
                "./bin/herdr-tools",
                "scratch",
                "toggle",
                "--scratch",
                "shell"
            ]
        );
        // Cross-plugin callers reach reaping through Herdr, which supplies this
        // plugin's state and config dirs; so it takes no pane context.
        assert!(actions[2].get("contexts").is_none());
        assert_eq!(
            command(actions[2]),
            ["./bin/herdr-tools", "scratch", "reap"]
        );
    }

    #[test]
    fn manifest_runs_the_popup_and_keeps_the_worker_alive() {
        let manifest = manifest();
        let pane = manifest["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pane| pane["id"].as_str() == Some("scratch"))
            .unwrap();
        assert_eq!(pane["placement"].as_str(), Some("popup"));
        assert_eq!(command(pane), ["./bin/herdr-tools", "scratch", "run-popup"]);
        let start = ["./bin/herdr-tools", "scratch", "cleanup-start"];
        assert!(manifest["startup"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hook| command(hook) == start));
        assert!(manifest["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hook| hook["on"].as_str() == Some("pane.closed") && command(hook) == start));
    }
}
