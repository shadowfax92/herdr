//! Scratch's view of the one herdr-tools `config.toml`: the `[scratch]` table
//! (`cleanup`, `scratches`) plus the `scratch-<name>` keys of core's `[popup]`.
//!
//! Every scratch shares the manifest's `scratch` popup pane; its size comes from
//! popup key `scratch-<name>` (`[[popup.profile]]`, then `[popup.size]`, then the
//! manifest pane's 90% x 95%). The old plugin kept sizes in its own YAML
//! (`default_popup`, per-scratch `popup`, `profiles`); `crate::legacy` maps those
//! onto the same keys, so a migrated file resolves every width exactly as before.
//!
//! Config is read fresh by [`LoadedConfig::load`] rather than through
//! `Config::current`: the cleanup worker is long-lived and must see retention
//! edits on its next sweep, as it did when it re-read `config.yaml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tools_core::{Config, Paths, PopupDims, PopupSize, ResolvedPopup};

use crate::MODE;

/// The built-in `[scratch]` table, used when config sets no `scratches`.
pub(crate) const DEFAULT_CONFIG: &str = include_str!("../config.default.toml");

/// The manifest `scratch` pane's own size, which Herdr applies when config
/// sizes no `scratch-<name>` key. Pinned to `herdr-plugin.toml` by a test.
pub(crate) const MANIFEST_POPUP: PopupDims = PopupDims {
    width: PopupSize::Percent(90),
    height: PopupSize::Percent(95),
};

/// The popup key that sizes scratch `name`'s popup.
pub fn popup_key(name: &str) -> String {
    format!("{MODE}-{name}")
}

/// `[scratch]` exactly as written. `scratches: None` means the built-in set.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Section {
    cleanup: CleanupConfig,
    scratches: Option<BTreeMap<String, ScratchDefinition>>,
}

/// The validated scratch config and the core config it came from (for popups).
#[derive(Debug)]
pub struct LoadedConfig {
    pub path: PathBuf,
    config: Config,
    cleanup: CleanupConfig,
    scratches: BTreeMap<String, ScratchDefinition>,
}

/// Retention is read by the background worker on each sweep. Old user configs
/// inherit the policy without rewriting popup definitions or keybindings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanupConfig {
    pub enabled: bool,
    pub ttl_hours: u64,
    pub interval_seconds: u64,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_hours: 24,
            interval_seconds: 60,
        }
    }
}

/// One `[scratch.scratches.<name>]`. Its popup size is popup key `scratch-<name>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchDefinition {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shell: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_tmux_env: bool,
    #[serde(default)]
    pub tmux_mode: TmuxMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmx_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TmuxMode {
    #[default]
    Minimal,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedScratch {
    pub name: String,
    pub command: Vec<String>,
    pub tmx_type: String,
    pub clear_tmux_env: bool,
    pub tmux_mode: TmuxMode,
    pub tmux_prefix: Option<String>,
    pub key: Option<String>,
}

impl LoadedConfig {
    /// Reads `config.toml` from the plugin config dir, uncached.
    pub fn load() -> Result<Self> {
        Self::load_from(&Paths::from_env().config_file())
    }

    pub(crate) fn load_from(path: &Path) -> Result<Self> {
        // Core's errors already name the file; scratch's own checks add it.
        let config = Config::load(path).context("invalid scratch config")?;
        Self::from_config(config)
    }

    pub(crate) fn from_config(config: Config) -> Result<Self> {
        let path = config.path().map(Path::to_path_buf).unwrap_or_default();
        let section: Section = config.section(MODE).context("invalid scratch config")?;
        let scratches = match section.scratches {
            Some(scratches) => scratches,
            None => default_scratches()?,
        };
        let loaded = Self {
            path,
            config,
            cleanup: section.cleanup,
            scratches,
        };
        loaded
            .validate()
            .with_context(|| format!("invalid scratch config {}", loaded.path.display()))?;
        Ok(loaded)
    }

    pub fn cleanup(&self) -> &CleanupConfig {
        &self.cleanup
    }

    pub fn scratch(&self, name: &str) -> Result<ResolvedScratch> {
        let scratch = self
            .scratches
            .get(name)
            .with_context(|| format!("unknown scratch `{name}`"))?;
        let mut command = if scratch.shell {
            vec![
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
                "-l".into(),
            ]
        } else {
            scratch.command.clone()
        };
        if scratch.clear_tmux_env {
            let mut wrapper = vec![
                "env".into(),
                "-u".into(),
                "TMUX".into(),
                "-u".into(),
                "TMUX_PANE".into(),
                "-u".into(),
                "TMUX_TMPDIR".into(),
            ];
            wrapper.append(&mut command);
            command = wrapper;
        }
        Ok(ResolvedScratch {
            name: name.into(),
            command,
            tmx_type: scratch.tmx_type.clone().unwrap_or_else(|| name.into()),
            clear_tmux_env: scratch.clear_tmux_env,
            tmux_mode: scratch.tmux_mode,
            tmux_prefix: scratch
                .tmux_prefix
                .as_deref()
                .map(crate::prefix::normalize_tmux_key)
                .transpose()?,
            key: scratch.key.clone(),
        })
    }

    /// The configured size of scratch `name`'s popup on a client
    /// `client_width` cells wide; None leaves it to the manifest default.
    pub fn popup(&self, name: &str, client_width: Option<u16>) -> Result<Option<ResolvedPopup>> {
        if !self.scratches.contains_key(name) {
            bail!("unknown scratch `{name}`");
        }
        Ok(self.config.popup(&popup_key(name), client_width))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.scratches.keys().map(String::as_str)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.scratches
            .values()
            .filter_map(|scratch| scratch.key.as_deref())
    }

    fn validate(&self) -> Result<()> {
        validate_cleanup(&self.cleanup)?;
        validate_scratches(&self.scratches)?;
        // The old plugin rejected a profile naming an unknown scratch; popup
        // keys are that reference now, so a typo still fails loudly.
        let popup = self.config.popup_config();
        let sized = popup
            .size
            .keys()
            .map(|key| ("[popup.size]".to_owned(), key))
            .chain(popup.profile.iter().flat_map(|profile| {
                profile
                    .size
                    .keys()
                    .map(|key| (format!("popup profile `{}`", profile.name), key))
            }));
        for (owner, key) in sized {
            if let Some(name) = key.strip_prefix(&format!("{MODE}-")) {
                if !self.scratches.contains_key(name) {
                    bail!("{owner} sizes `{key}`, but there is no scratch `{name}`");
                }
            }
        }
        Ok(())
    }
}

/// The scratches of [`DEFAULT_CONFIG`].
fn default_scratches() -> Result<BTreeMap<String, ScratchDefinition>> {
    Config::parse(DEFAULT_CONFIG)?
        .section::<Section>(MODE)?
        .scratches
        .context("the built-in scratch config has no scratches")
}

pub(crate) fn validate_cleanup(cleanup: &CleanupConfig) -> Result<()> {
    if cleanup.ttl_hours == 0 || cleanup.ttl_hours.checked_mul(3600).is_none() {
        bail!("cleanup.ttl_hours must be positive and fit in seconds");
    }
    if !(5..=86400).contains(&cleanup.interval_seconds) {
        bail!("cleanup.interval_seconds must be between 5 and 86400");
    }
    Ok(())
}

pub(crate) fn validate_scratches(scratches: &BTreeMap<String, ScratchDefinition>) -> Result<()> {
    if scratches.is_empty() {
        bail!("at least one scratch definition is required");
    }
    for (name, scratch) in scratches {
        if !valid_name(name) {
            bail!("scratch name `{name}` may contain only letters, numbers, `_`, and `-`");
        }
        match (scratch.shell, scratch.command.is_empty()) {
            (true, false) => bail!("scratch `{name}` cannot set both `shell` and `command`"),
            (false, true) => bail!("scratch `{name}` must set `shell = true` or `command`"),
            _ => {}
        }
        if scratch.command.iter().any(|part| part.is_empty()) {
            bail!("scratch `{name}` contains an empty command argument");
        }
        if scratch.tmux_mode == TmuxMode::Workspace && scratch.tmux_prefix.is_none() {
            bail!("scratch `{name}` workspace mode requires `tmux_prefix`");
        }
        if let Some(prefix) = &scratch.tmux_prefix {
            crate::prefix::normalize_tmux_key(prefix)
                .with_context(|| format!("scratch `{name}` has invalid `tmux_prefix`"))?;
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Result<LoadedConfig> {
        LoadedConfig::from_config(Config::parse(source)?)
    }

    /// No `[scratch]` at all: the built-in scratches and retention.
    fn config() -> LoadedConfig {
        parse("").unwrap()
    }

    /// The old default YAML, migrated: its popup profiles now live in `[popup]`.
    fn migrated_default() -> LoadedConfig {
        let yaml = include_str!("../tests/fixtures/legacy-default/shadowfax.scratch/config.yaml");
        let fragment = crate::legacy::migrate_yaml(yaml).unwrap();
        parse(&toml::to_string(&fragment).unwrap()).unwrap()
    }

    #[test]
    fn existing_configs_get_one_day_retention_and_invalid_policy_is_rejected() {
        let old = "[scratch.scratches.shell]\nshell = true\n";
        let config = parse(old).unwrap();
        assert!(config.cleanup().enabled);
        assert_eq!(config.cleanup().ttl_hours, 24);
        assert_eq!(config.cleanup().interval_seconds, 60);
        assert!(parse(&format!("{old}[scratch.cleanup]\nttl_hours = 0\n")).is_err());
        assert!(parse(&format!("{old}[scratch.cleanup]\ninterval_seconds = 0\n")).is_err());
        assert_eq!(
            parse(&format!("{old}[scratch.cleanup]\nttl_hours = 48\n"))
                .unwrap()
                .cleanup()
                .ttl_hours,
            48
        );
    }

    #[test]
    fn selects_profiles_in_order() {
        let config = migrated_default();
        let width = |client| {
            config
                .popup("nvim", Some(client))
                .unwrap()
                .unwrap()
                .dims
                .width
        };

        assert_eq!(width(300), PopupSize::Percent(95));
        assert_eq!(width(330), PopupSize::Percent(90));
        assert_eq!(width(380), PopupSize::Percent(90));
        let full = config.popup("nvim", Some(512)).unwrap().unwrap();
        assert_eq!(
            full.dims,
            PopupDims {
                width: PopupSize::Percent(70),
                height: PopupSize::Percent(95),
            }
        );
        assert_eq!(full.profile.as_deref(), Some("full-ultrawide"));
    }

    #[test]
    fn resolves_commands_and_shell() {
        let config = config();

        assert_eq!(config.scratch("nvim").unwrap().command, ["nvim"]);
        let shell = config.scratch("shell").unwrap();
        assert_eq!(shell.command.last().map(String::as_str), Some("-l"));
        assert!(!shell.clear_tmux_env);
    }

    #[test]
    fn resolves_workspace_mode_and_prefix() {
        let config = config();

        let nvim = config.scratch("nvim").unwrap();
        assert_eq!(nvim.tmux_mode, TmuxMode::Workspace);
        assert_eq!(nvim.tmux_prefix.as_deref(), Some("C-a"));

        let shell = config.scratch("shell").unwrap();
        assert_eq!(shell.tmux_mode, TmuxMode::Workspace);
        assert_eq!(shell.tmux_prefix.as_deref(), Some("C-a"));
    }

    #[test]
    fn default_scratches_are_nvim_and_shell() {
        let config = config();

        assert_eq!(config.names().collect::<Vec<_>>(), ["nvim", "shell"]);
        assert_eq!(config.keys().collect::<Vec<_>>(), ["alt+i", "alt+0"]);
        assert!(config.scratch("tmux").is_err());
        assert_eq!(config.cleanup(), &CleanupConfig::default());
        // The shipped default file is the built-in config, retention included.
        let explicit = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(explicit.scratches, config.scratches);
        assert_eq!(explicit.cleanup(), config.cleanup());
        // Without popup config every scratch takes the manifest's size.
        assert_eq!(config.popup("nvim", Some(512)).unwrap(), None);
    }

    #[test]
    fn rejects_invalid_explicit_tmux_prefix() {
        let source = DEFAULT_CONFIG.replace("tmux_prefix = \"ctrl+a\"", "tmux_prefix = \"cmd+a\"");

        assert!(parse(&source).is_err());
    }

    #[test]
    fn rejects_invalid_popup_size() {
        let source = format!(
            "{DEFAULT_CONFIG}\n[popup.size]\nscratch-nvim = {{ width = \"90%\", height = \"120%\" }}\n"
        );

        assert!(parse(&source).is_err());
    }

    #[test]
    fn popup_keys_must_name_a_configured_scratch() {
        let error = parse("[popup.size]\nscratch-nvmi = { width = 80, height = 20 }\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid scratch config"), "{error}");
        let error = format!(
            "{:#}",
            parse("[[popup.profile]]\nname = \"laptop\"\nsize.scratch-tmux = { width = 80, height = 20 }\n")
                .unwrap_err()
        );
        assert!(
            error.contains("laptop") && error.contains("no scratch `tmux`"),
            "{error}"
        );
        // Other modes' keys are not scratch's business.
        assert!(parse("[popup.size]\nfind = { width = 80, height = 20 }\n").is_ok());
    }

    #[test]
    fn written_scratches_replace_the_built_in_set_and_must_not_be_empty() {
        let config = parse("[scratch.scratches.pad]\ncommand = [\"vi\"]\n").unwrap();
        assert_eq!(config.names().collect::<Vec<_>>(), ["pad"]);
        assert_eq!(config.scratch("pad").unwrap().tmx_type, "pad");
        assert!(parse("[scratch]\nscratches = {}\n").is_err());
        assert!(parse("[scratch]\nunknown = 1\n").is_err());
        // A per-scratch popup is now popup key `scratch-<name>`, not a field.
        assert!(parse(
            "[scratch.scratches.pad]\ncommand = [\"vi\"]\npopup = { width = 80, height = 20 }\n"
        )
        .is_err());
    }

    #[test]
    fn manifest_popup_matches_the_scratch_pane() {
        let manifest: toml::Table = include_str!("../../../herdr-plugin.toml").parse().unwrap();
        let pane = manifest["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pane| pane["id"].as_str() == Some("scratch"))
            .unwrap();
        let dims: PopupDims = toml::Value::Table(toml::Table::from_iter([
            ("width".to_owned(), pane["width"].clone()),
            ("height".to_owned(), pane["height"].clone()),
        ]))
        .try_into()
        .unwrap();
        assert_eq!(dims, MANIFEST_POPUP);
    }
}
