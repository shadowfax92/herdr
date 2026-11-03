//! Comment settings: the `[comment]` table of the shared `config.toml`, the popup
//! keys this mode sizes, and the reader for the old plugin's `config.yaml`.
//!
//! Popup sizes no longer live in a comments-only file. The capture and review
//! popups are popup keys `comment-capture` and `comment-review` in core's
//! `[popup.size]` / `[[popup.profile]]`. [`LoadedConfig::popup`] resolves them
//! against the client width, and an unconfigured key opens at the manifest
//! pane's default. Only [`legacy`], which `migrate_legacy` uses, still parses
//! the old YAML format.

use anyhow::Result;
use serde::Deserialize;
use tools_core::{Config, PopupConfig, PopupDims};

use crate::MODE;

/// Popup key (and manifest pane id) of the annotation popup.
pub const CAPTURE_POPUP: &str = "comment-capture";
/// Popup key (and manifest pane id) of the Neovim review popup.
pub const REVIEW_POPUP: &str = "comment-review";

/// The `[comment]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommentConfig {
    /// Capture comments on the Copy-mode selection instead of opening the
    /// snapshot browser.
    pub inline_comments: bool,
}

/// Inline comments are on by default, as they were for every fresh install of
/// the old plugin: its first run wrote a default `config.yaml` with
/// `inline_comments: true`. Only an old YAML that omitted the key meant `false`,
/// and `migrate_legacy` writes that case out explicitly.
impl Default for CommentConfig {
    fn default() -> Self {
        Self {
            inline_comments: true,
        }
    }
}

/// The settings one comment command needs, read once from the shared config.
#[derive(Debug)]
pub struct LoadedConfig {
    comment: CommentConfig,
    popup: PopupConfig,
}

impl LoadedConfig {
    /// Reads `[comment]` and `[popup]` from [`Config::current`]. A broken file is
    /// an error, never silently the defaults; a missing one is all defaults.
    pub fn load() -> Result<Self> {
        Self::from_config(Config::current()?)
    }

    /// The same settings from an already loaded config.
    pub fn from_config(config: &Config) -> Result<Self> {
        Ok(Self {
            comment: config.section(MODE)?,
            popup: config.popup_config().clone(),
        })
    }

    /// The configured size of popup `name` on a client `client_width` cells wide,
    /// or None to open at the manifest pane's default.
    pub fn popup(&self, name: &str, client_width: Option<u16>) -> Option<PopupDims> {
        self.popup
            .resolve(name, client_width)
            .map(|resolved| resolved.dims)
    }

    pub fn inline_comments(&self) -> bool {
        self.comment.inline_comments
    }
}

/// The old plugin's `config.yaml` (`shadowfax.comments`), read only to migrate it.
///
/// The old file held `inline_comments`, base `popups` (`capture`, `review`) and
/// width `profiles`. Its resolution was: the first profile whose width range
/// matches, then that profile's size for the popup, else the base size. Core's
/// profiles skip a matching profile that does not size a key, so the fragment
/// gives every profile both keys (falling back to the base size) to keep the
/// old answer for every client width.
pub mod legacy {
    use std::collections::BTreeMap;
    use std::path::Path;

    use anyhow::{bail, Context, Result};
    use serde::Deserialize;
    use tools_core::{PopupDims, PopupSize as Size};

    use super::{CAPTURE_POPUP, REVIEW_POPUP};

    /// The old plugin's config dir under the legacy root.
    pub const DIR: &str = "shadowfax.comments";
    /// The old plugin's config file inside [`DIR`].
    pub const FILE: &str = "config.yaml";
    /// Old popup names, paired with the popup keys that replace them.
    const POPUPS: [(&str, &str); 2] = [("capture", CAPTURE_POPUP), ("review", REVIEW_POPUP)];

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct Config {
        #[serde(default)]
        pub(crate) inline_comments: bool,
        popups: BTreeMap<String, PopupSize>,
        #[serde(default)]
        profiles: Vec<Profile>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PopupSize {
        width: String,
        height: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Profile {
        name: String,
        #[serde(rename = "match")]
        selector: ProfileMatch,
        #[serde(default)]
        popups: BTreeMap<String, PopupSize>,
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ProfileMatch {
        min_client_width: Option<u16>,
        max_client_width: Option<u16>,
    }

    /// `<legacy_root>/shadowfax.comments/config.yaml` as a fragment of the new
    /// `config.toml`, or an empty table when the old plugin never wrote one.
    pub fn fragment(legacy_root: &Path) -> Result<toml::Table> {
        let path = legacy_root.join(DIR).join(FILE);
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(toml::Table::new())
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        Config::parse(&source)
            .and_then(|config| config.to_fragment())
            .with_context(|| format!("invalid comments config {}", path.display()))
    }

    impl Config {
        pub(crate) fn parse(source: &str) -> Result<Self> {
            let config: Self = noyalib::from_str(source)?;
            config.validate()?;
            Ok(config)
        }

        fn validate(&self) -> Result<()> {
            for (required, _) in POPUPS {
                if !self.popups.contains_key(required) {
                    bail!("popup `{required}` is required");
                }
            }
            for popup in self.popups.values() {
                validate_popup(popup)?;
            }
            for profile in &self.profiles {
                if profile.name.trim().is_empty() {
                    bail!("profile name cannot be empty");
                }
                if profile
                    .selector
                    .min_client_width
                    .zip(profile.selector.max_client_width)
                    .is_some_and(|(min, max)| min > max)
                {
                    bail!("profile `{}` has min width above max width", profile.name);
                }
                for (name, popup) in &profile.popups {
                    if !self.popups.contains_key(name) {
                        bail!(
                            "profile `{}` references unknown popup `{name}`",
                            profile.name
                        );
                    }
                    validate_popup(popup)?;
                }
            }
            Ok(())
        }

        /// `[comment]`, `[popup.size]` and one `[[popup.profile]]` per old
        /// profile, in the old order. `inline_comments` is always written: the
        /// old default for a file without it was `false`, the new one is `true`.
        pub(crate) fn to_fragment(&self) -> Result<toml::Table> {
            let mut comment = toml::Table::new();
            comment.insert(
                "inline_comments".into(),
                toml::Value::Boolean(self.inline_comments),
            );

            let mut size = toml::Table::new();
            for (old, key) in POPUPS {
                size.insert(key.into(), dims(&self.popups[old])?);
            }

            let mut profiles = Vec::new();
            for profile in &self.profiles {
                let mut table = toml::Table::new();
                table.insert("name".into(), toml::Value::String(profile.name.clone()));
                for (field, width) in [
                    ("min_client_width", profile.selector.min_client_width),
                    ("max_client_width", profile.selector.max_client_width),
                ] {
                    if let Some(width) = width {
                        table.insert(field.into(), toml::Value::Integer(width.into()));
                    }
                }
                let mut sizes = toml::Table::new();
                for (old, key) in POPUPS {
                    let popup = profile.popups.get(old).unwrap_or(&self.popups[old]);
                    sizes.insert(key.into(), dims(popup)?);
                }
                table.insert("size".into(), toml::Value::Table(sizes));
                profiles.push(toml::Value::Table(table));
            }

            let mut popup = toml::Table::new();
            popup.insert("size".into(), toml::Value::Table(size));
            if !profiles.is_empty() {
                popup.insert("profile".into(), toml::Value::Array(profiles));
            }
            let mut fragment = toml::Table::new();
            fragment.insert(crate::MODE.into(), toml::Value::Table(comment));
            fragment.insert("popup".into(), toml::Value::Table(popup));
            Ok(fragment)
        }
    }

    fn validate_popup(popup: &PopupSize) -> Result<()> {
        for (name, value) in [("width", &popup.width), ("height", &popup.height)] {
            let valid = if let Some(percent) = value.strip_suffix('%') {
                percent
                    .parse::<u16>()
                    .is_ok_and(|number| (1..=100).contains(&number))
            } else {
                value.parse::<u16>().is_ok_and(|number| number > 0)
            };
            if !valid {
                bail!("popup {name} `{value}` must be positive cells or 1%-100%");
            }
        }
        Ok(())
    }

    /// An old `{ width: "90%", height: "40" }` as a new `{ width = "90%", height = 40 }`.
    fn dims(popup: &PopupSize) -> Result<toml::Value> {
        let parse = |value: &str| value.parse::<Size>().map_err(anyhow::Error::msg);
        let dims = PopupDims {
            width: parse(&popup.width)?,
            height: parse(&popup.height)?,
        };
        Ok(toml::Value::try_from(dims)?)
    }
}

#[cfg(test)]
mod tests {
    use super::legacy::Config as LegacyConfig;
    use super::*;

    /// The old plugin's `config.default.yaml`, which its first run wrote. The
    /// owner's live `config.yaml` is byte-identical to it.
    const DEFAULT_CONFIG: &str = include_str!("../tests/fixtures/config.default.yaml");

    /// The old default config, migrated and loaded the way every command loads it.
    fn config() -> LoadedConfig {
        migrated(DEFAULT_CONFIG)
    }

    fn migrated(source: &str) -> LoadedConfig {
        let fragment = LegacyConfig::parse(source).unwrap().to_fragment().unwrap();
        let text = toml::to_string(&fragment).unwrap();
        LoadedConfig::from_config(&Config::parse(&text).unwrap()).unwrap()
    }

    fn width(config: &LoadedConfig, name: &str, client_width: u16) -> String {
        config
            .popup(name, Some(client_width))
            .unwrap()
            .width
            .to_string()
    }

    #[test]
    fn inline_comments_are_enabled_by_default_and_legacy_configs_stay_disabled() {
        assert!(config().inline_comments());
        assert!(LoadedConfig::from_config(&Config::default())
            .unwrap()
            .inline_comments());

        let legacy = DEFAULT_CONFIG.replacen("inline_comments: true\n\n", "", 1);
        assert!(!migrated(&legacy).inline_comments());

        let disabled =
            DEFAULT_CONFIG.replacen("inline_comments: true", "inline_comments: false", 1);
        assert!(!migrated(&disabled).inline_comments());
    }

    #[test]
    fn selects_responsive_popup_sizes() {
        let config = config();

        assert_eq!(width(&config, CAPTURE_POPUP, 300), "95%");
        assert_eq!(width(&config, CAPTURE_POPUP, 330), "90%");
        assert_eq!(width(&config, CAPTURE_POPUP, 380), "90%");
        assert_eq!(width(&config, CAPTURE_POPUP, 512), "50%");
        assert_eq!(
            config
                .popup(CAPTURE_POPUP, Some(512))
                .unwrap()
                .height
                .to_string(),
            "70%"
        );
        assert_eq!(width(&config, REVIEW_POPUP, 512), "70%");
        assert_eq!(
            config
                .popup(REVIEW_POPUP, Some(512))
                .unwrap()
                .height
                .to_string(),
            "85%"
        );
    }

    #[test]
    fn rejects_invalid_popup_sizes() {
        let source = DEFAULT_CONFIG.replace("height: \"90%\"", "height: \"120%\"");

        assert!(LegacyConfig::parse(&source).is_err());
    }
}
