//! The old herdr-scratch config, `shadowfax.scratch/config.yaml`, and its
//! translation into this mode's fragment of the new `config.toml`.
//!
//! ```text
//!  config.yaml                         config.toml fragment
//!  cleanup                     ──▶     [scratch.cleanup]
//!  scratches.<name> (no popup) ──▶     [scratch.scratches.<name>]
//!  scratches.<name>.popup
//!    or default_popup          ──▶     [popup.size] scratch-<name>
//!  profiles[i].popups.<name>   ──▶     [[popup.profile]] size.scratch-<name>
//! ```
//!
//! Old resolution took the first width-matching profile and fell back to the
//! scratch's base size when that profile lacked the scratch. Core instead skips
//! a matching profile that lacks the key, so each migrated profile sizes every
//! scratch, filling gaps with the base size; then both rules pick the same
//! profile and size for every width.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tools_core::{PopupConfig, PopupDims, PopupProfile, PopupSize};

use crate::config::{CleanupConfig, ScratchDefinition, TmuxMode};

/// The old plugin's config dir under the legacy root.
pub(crate) const LEGACY_DIR: &str = "shadowfax.scratch";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyConfig {
    #[serde(default)]
    cleanup: CleanupConfig,
    default_popup: LegacyPopup,
    scratches: BTreeMap<String, LegacyScratch>,
    #[serde(default)]
    profiles: Vec<LegacyProfile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyScratch {
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    shell: bool,
    #[serde(default)]
    clear_tmux_env: bool,
    #[serde(default)]
    tmux_mode: TmuxMode,
    tmux_prefix: Option<String>,
    tmx_type: Option<String>,
    key: Option<String>,
    popup: Option<LegacyPopup>,
}

/// The old YAML's popup: two strings, `"80"` (cells) or `"80%"`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPopup {
    width: String,
    height: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyProfile {
    name: String,
    #[serde(rename = "match")]
    selector: LegacyMatch,
    #[serde(default)]
    popups: BTreeMap<String, LegacyPopup>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyMatch {
    min_client_width: Option<u16>,
    max_client_width: Option<u16>,
}

/// `migrate_legacy`: the fragment for `<legacy_root>/shadowfax.scratch/config.yaml`,
/// or an empty table when the old plugin never wrote one.
pub(crate) fn migrate(legacy_config_root: &Path) -> Result<toml::Table> {
    let path = legacy_config_root.join(LEGACY_DIR).join("config.yaml");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(toml::Table::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    migrate_yaml(&source).with_context(|| format!("invalid scratch config {}", path.display()))
}

/// Translates old YAML text; an old config the old plugin would have rejected
/// is rejected here too, and the result is checked as the new mode reads it.
pub(crate) fn migrate_yaml(source: &str) -> Result<toml::Table> {
    let old: LegacyConfig = noyalib::from_str(source)?;
    old.validate()?;

    let scratches = old
        .scratches
        .iter()
        .map(|(name, scratch)| {
            let definition = ScratchDefinition {
                command: scratch.command.clone(),
                shell: scratch.shell,
                clear_tmux_env: scratch.clear_tmux_env,
                tmux_mode: scratch.tmux_mode,
                tmux_prefix: scratch.tmux_prefix.clone(),
                tmx_type: scratch.tmx_type.clone(),
                key: scratch.key.clone(),
            };
            (name.clone(), definition)
        })
        .collect::<BTreeMap<_, _>>();

    let base = |name: &str| -> Result<PopupDims> {
        old.scratches[name]
            .popup
            .as_ref()
            .unwrap_or(&old.default_popup)
            .dims()
    };
    let mut popup = PopupConfig::default();
    for name in old.scratches.keys() {
        popup
            .size
            .insert(crate::config::popup_key(name), base(name)?);
    }
    for profile in &old.profiles {
        let mut size = BTreeMap::new();
        for name in old.scratches.keys() {
            let dims = match profile.popups.get(name) {
                Some(popup) => popup.dims()?,
                None => base(name)?,
            };
            size.insert(crate::config::popup_key(name), dims);
        }
        popup.profile.push(PopupProfile {
            name: profile.name.clone(),
            min_client_width: profile.selector.min_client_width,
            max_client_width: profile.selector.max_client_width,
            size,
        });
    }

    let mut scratch = toml::Table::new();
    scratch.insert("cleanup".into(), toml::Value::try_from(&old.cleanup)?);
    scratch.insert("scratches".into(), toml::Value::try_from(&scratches)?);
    let mut fragment = toml::Table::new();
    fragment.insert("popup".into(), toml::Value::try_from(&popup)?);
    fragment.insert(crate::MODE.into(), toml::Value::Table(scratch));

    let rendered = toml::to_string(&fragment)?;
    crate::config::LoadedConfig::from_config(tools_core::Config::parse(&rendered)?)
        .context("the migrated scratch config does not validate")?;
    Ok(fragment)
}

impl LegacyPopup {
    fn dims(&self) -> Result<PopupDims> {
        let parse = |name: &str, value: &str| {
            value.parse::<PopupSize>().map_err(|_| {
                anyhow::anyhow!("popup {name} `{value}` must be positive cells or 1%-100%")
            })
        };
        Ok(PopupDims {
            width: parse("width", &self.width)?,
            height: parse("height", &self.height)?,
        })
    }
}

impl LegacyConfig {
    /// The old plugin's own validation, minus the per-scratch checks the new
    /// mode repeats on the migrated result.
    fn validate(&self) -> Result<()> {
        crate::config::validate_cleanup(&self.cleanup)?;
        self.default_popup.dims()?;
        if self.scratches.is_empty() {
            bail!("at least one scratch definition is required");
        }
        for scratch in self.scratches.values() {
            if let Some(popup) = &scratch.popup {
                popup.dims()?;
            }
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
                if !self.scratches.contains_key(name) {
                    bail!(
                        "profile `{}` references unknown scratch `{name}`",
                        profile.name
                    );
                }
                popup.dims()?;
            }
        }
        Ok(())
    }

    /// The old plugin's popup resolution, kept to prove migrated parity.
    #[cfg(test)]
    fn popup(&self, name: &str, client_width: Option<u16>) -> (LegacyPopup, Option<String>) {
        let base = self.scratches[name]
            .popup
            .clone()
            .unwrap_or_else(|| self.default_popup.clone());
        let Some(width) = client_width else {
            return (base, None);
        };
        let matches = |selector: &LegacyMatch| {
            selector.min_client_width.is_none_or(|min| width >= min)
                && selector.max_client_width.is_none_or(|max| width <= max)
        };
        match self.profiles.iter().find(|p| matches(&p.selector)) {
            Some(profile) => (
                profile.popups.get(name).cloned().unwrap_or(base),
                Some(profile.name.clone()),
            ),
            None => (base, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LoadedConfig, MANIFEST_POPUP};
    use tools_core::Config;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn loaded(fragment: &toml::Table) -> LoadedConfig {
        LoadedConfig::from_config(Config::parse(&toml::to_string(fragment).unwrap()).unwrap())
            .unwrap()
    }

    /// Every width the old and the migrated config are asked about must give
    /// the same size and profile, including the base-size fallbacks.
    fn assert_parity(yaml: &str) {
        let old: LegacyConfig = noyalib::from_str(yaml).unwrap();
        let new = loaded(&migrate_yaml(yaml).unwrap());
        let widths = [
            None,
            Some(1),
            Some(174),
            Some(310),
            Some(311),
            Some(350),
            Some(351),
            Some(399),
            Some(400),
            Some(512),
            Some(u16::MAX),
        ];
        for name in old.scratches.keys() {
            for width in widths {
                let (size, profile) = old.popup(name, width);
                let resolved = new.popup(name, width).unwrap().unwrap();
                assert_eq!(resolved.dims, size.dims().unwrap(), "{name} at {width:?}");
                assert_eq!(resolved.profile, profile, "{name} at {width:?}");
            }
        }
    }

    #[test]
    fn owners_config_migrates_exactly() {
        let root = Path::new(FIXTURES).join("legacy-owner");
        let fragment = migrate(&root).unwrap();
        let expected: toml::Table = std::fs::read_to_string(root.join("expected.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            fragment,
            expected,
            "{}",
            toml::to_string(&fragment).unwrap()
        );
        assert_parity(&std::fs::read_to_string(root.join(LEGACY_DIR).join("config.yaml")).unwrap());
        let new = loaded(&fragment);
        let nvim = new.scratch("nvim").unwrap();
        assert_eq!(
            (nvim.command, nvim.tmx_type, nvim.tmux_prefix, nvim.key),
            (
                vec!["nvim".to_owned()],
                "vim".to_owned(),
                Some("C-a".to_owned()),
                Some("alt+i".to_owned())
            )
        );
        assert_eq!(new.cleanup(), &CleanupConfig::default());
    }

    #[test]
    fn default_config_migrates_with_parity() {
        assert_parity(include_str!(
            "../tests/fixtures/legacy-default/shadowfax.scratch/config.yaml"
        ));
    }

    #[test]
    fn profiles_missing_a_scratch_keep_the_old_base_fallback() {
        // Old: `laptop` matches 300 but lacks `shell`, so shell used its own
        // base size under `laptop`, even though `wide` sizes shell too.
        let yaml = r#"
default_popup: { width: "90%", height: "95%" }
scratches:
  nvim: { command: ["nvim"], popup: { width: "120", height: "40" } }
  shell: { shell: true }
profiles:
  - name: laptop
    match: { max_client_width: 310 }
    popups:
      nvim: { width: "95%", height: "95%" }
  - name: wide
    match: {}
    popups:
      shell: { width: "50%", height: "50%" }
"#;
        assert_parity(yaml);
        let new = loaded(&migrate_yaml(yaml).unwrap());
        let shell = new.popup("shell", Some(300)).unwrap().unwrap();
        assert_eq!(shell.profile.as_deref(), Some("laptop"));
        assert_eq!(shell.dims, MANIFEST_POPUP);
        let nvim = new.popup("nvim", Some(400)).unwrap().unwrap();
        assert_eq!(nvim.dims.width, PopupSize::Cells(120));
    }

    #[test]
    fn missing_old_config_migrates_to_nothing() {
        let empty = tempfile_dir();
        assert!(migrate(&empty).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn invalid_old_configs_are_refused() {
        for yaml in [
            "invalid: [",
            "default_popup: { width: \"90%\", height: \"120%\" }\nscratches:\n  s: { shell: true }\n",
            "default_popup: { width: \"90%\", height: \"95%\" }\nscratches: {}\n",
            "default_popup: { width: \"90%\", height: \"95%\" }\nscratches:\n  s: { shell: true }\nprofiles:\n  - name: p\n    match: {}\n    popups:\n      t: { width: \"1\", height: \"1\" }\n",
            "default_popup: { width: \"90%\", height: \"95%\" }\nscratches:\n  s: { tmux_mode: workspace, shell: true }\n",
        ] {
            assert!(migrate_yaml(yaml).is_err(), "{yaml}");
        }
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "scratch-legacy-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
