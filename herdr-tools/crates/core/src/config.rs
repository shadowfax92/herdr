//! The one user config file, `$HERDR_PLUGIN_CONFIG_DIR/config.toml`.
//!
//! The file holds one table per mode plus `[popup]`. Core validates only the
//! file's shape and `[popup]`; each mode owns its table's schema and reads it with
//! [`Config::section`]. A missing file means every default. An unknown top-level
//! table is an error naming the valid ones, so a typo cannot silently fall back.
//! A removed mode's table (`RETIRED_SECTIONS`) is ignored instead.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;

use crate::paths::Paths;
use crate::popup::{PopupConfig, ResolvedPopup};

/// Every top-level table `config.toml` may contain: `popup`, then one per mode.
pub const SECTIONS: &[&str] = &[
    "popup", "find", "move", "menu", "comment", "layout", "scratch",
];

/// Tables of removed modes, accepted and ignored. Every mode loads this file,
/// so rejecting one would break them all, and a config synced from another
/// machine can still hold one. `[history]` belonged to the History popup,
/// which Herdr's native copy hints (`[copy_hints]` in Herdr's config) replaced.
const RETIRED_SECTIONS: &[&str] = &["history"];

/// A loaded and shape-checked `config.toml`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    path: Option<PathBuf>,
    tables: toml::Table,
    popup: PopupConfig,
}

impl Config {
    /// The process-wide config from [`Paths::config_file`], loaded on first use.
    /// A broken file is reported on every call, never replaced by defaults.
    pub fn current() -> Result<&'static Config> {
        static CURRENT: OnceLock<Result<Config, String>> = OnceLock::new();
        CURRENT
            .get_or_init(|| {
                Self::load(&Paths::from_env().config_file()).map_err(|error| format!("{error:#}"))
            })
            .as_ref()
            .map_err(|error| anyhow!("{error}"))
    }

    /// Loads `path`; a missing file is the all-defaults config.
    pub fn load(path: &Path) -> Result<Self> {
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path: Some(path.to_owned()),
                    ..Self::default()
                })
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        let mut config =
            Self::parse(&source).with_context(|| format!("invalid {}", path.display()))?;
        config.path = Some(path.to_owned());
        Ok(config)
    }

    /// Parses and shape-checks config text that has no file behind it.
    pub fn parse(source: &str) -> Result<Self> {
        let tables: toml::Table = toml::from_str(source)?;
        for (key, value) in &tables {
            if RETIRED_SECTIONS.contains(&key.as_str()) {
                continue;
            }
            if !SECTIONS.contains(&key.as_str()) {
                bail!(
                    "unknown table [{key}]; valid tables are {}",
                    SECTIONS
                        .iter()
                        .map(|name| format!("[{name}]"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if !value.is_table() {
                bail!("`{key}` must be a table, written [{key}]");
            }
        }
        let popup: PopupConfig = match tables.get("popup") {
            Some(value) => value.clone().try_into().context("invalid [popup]")?,
            None => PopupConfig::default(),
        };
        popup.validate()?;
        Ok(Self {
            path: None,
            tables,
            popup,
        })
    }

    /// The file this config came from (None for [`Config::parse`]).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Deserializes a mode's table, or `T::default()` when the table is absent.
    /// `name` must be one of [`SECTIONS`]. Modes choose their own strictness
    /// (`#[serde(deny_unknown_fields)]`, `#[serde(default)]`).
    pub fn section<T: DeserializeOwned + Default>(&self, name: &str) -> Result<T> {
        let Some(table) = self.section_table(name)? else {
            return Ok(T::default());
        };
        toml::Value::Table(table.clone())
            .try_into()
            .with_context(|| match self.path() {
                Some(path) => format!("invalid [{name}] in {}", path.display()),
                None => format!("invalid [{name}]"),
            })
    }

    /// A mode's raw table, for modes that read it field by field.
    pub fn section_table(&self, name: &str) -> Result<Option<&toml::Table>> {
        if !SECTIONS.contains(&name) {
            bail!("`{name}` is not a herdr-tools config section");
        }
        Ok(self.tables.get(name).and_then(toml::Value::as_table))
    }

    /// The parsed `[popup]` table.
    pub fn popup_config(&self) -> &PopupConfig {
        &self.popup
    }

    /// The configured size for popup `key` on a client `client_width` cells
    /// wide; None leaves it to the manifest default. See [`crate::popup`].
    pub fn popup(&self, key: &str, client_width: Option<u16>) -> Option<ResolvedPopup> {
        self.popup.resolve(key, client_width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::popup::PopupSize;
    use serde::Deserialize;

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(default, deny_unknown_fields)]
    struct Find {
        patterns: Vec<String>,
    }

    #[test]
    fn missing_file_means_every_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.path(), Some(path.as_path()));
        assert_eq!(config.section::<Find>("find").unwrap(), Find::default());
        assert_eq!(config.popup("find", Some(300)), None);
    }

    #[test]
    fn sections_deserialize_their_own_table() {
        let config = Config::parse(
            r#"
[find]
patterns = ["a", "b"]

[popup.size]
find = { width = 110, height = "90%" }
"#,
        )
        .unwrap();
        assert_eq!(config.section::<Find>("find").unwrap().patterns, ["a", "b"]);
        assert_eq!(
            config.popup("find", None).unwrap().dims.width,
            PopupSize::Cells(110)
        );
        assert!(config.section_table("menu").unwrap().is_none());
        assert!(config.section::<Find>("fnid").is_err());
    }

    #[test]
    fn unknown_tables_are_named_with_the_valid_ones() {
        let error = Config::parse("[jump]\n").unwrap_err().to_string();
        assert!(error.contains("[jump]"), "{error}");
        assert!(
            error.contains("[find]") && error.contains("[scratch]"),
            "{error}"
        );
        assert!(Config::parse("find = 3\n").is_err());
    }

    #[test]
    fn a_retired_history_table_is_ignored() {
        let config = Config::parse(
            r#"
[history]
alphabet = "asdf"

[[history.patterns]]
name = "git-ssh"
regex = 'git@[a-zA-Z0-9.-]+:[a-zA-Z0-9/.-]+\.git'

[find]
"#,
        )
        .unwrap();
        assert!(config.section_table("find").unwrap().is_some());
        assert!(config.section_table("history").is_err());
    }

    #[test]
    fn invalid_sections_and_popups_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[find]\npatterns = 3\n").unwrap();
        let config = Config::load(&path).unwrap();
        let error = format!("{:#}", config.section::<Find>("find").unwrap_err());
        assert!(
            error.contains("[find]") && error.contains("config.toml"),
            "{error}"
        );

        std::fs::write(
            &path,
            "[popup.size]\nfind = { width = \"120%\", height = 3 }\n",
        )
        .unwrap();
        let error = format!("{:#}", Config::load(&path).unwrap_err());
        assert!(
            error.contains("config.toml") && error.contains("120%"),
            "{error}"
        );
    }

    #[test]
    fn the_design_example_parses() {
        let config = Config::parse(
            r#"
[popup.size]
find = { width = 110, height = "90%" }
comment-capture = { width = "90%", height = "90%" }

[[popup.profile]]
name = "laptop"
max_client_width = 310
size.comment-capture = { width = "95%", height = "90%" }
size.scratch-nvim = { width = "95%", height = "95%" }

[find]
[move]
[menu]
[comment]
[scratch]
"#,
        )
        .unwrap();
        let laptop = config.popup("scratch-nvim", Some(300)).unwrap();
        assert_eq!(laptop.profile.as_deref(), Some("laptop"));
        assert_eq!(laptop.dims.width, PopupSize::Percent(95));
        assert_eq!(config.popup("scratch-nvim", Some(400)), None);
    }
}
