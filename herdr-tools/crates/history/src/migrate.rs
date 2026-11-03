//! `migrate_legacy`: talon's `shadowfax.talon/config.toml` as a fragment of the
//! one herdr-tools `config.toml`.
//!
//! ```text
//!  shadowfax.talon/config.toml           fragment
//!    alphabet                        ─▶  [history] alphabet
//!    enabled_builtin_patterns        ─▶  [history] enabled_builtin_patterns
//!    [[patterns]]                    ─▶  [[history.patterns]]
//!    [popup] width/height            ─▶  [popup.size] history = { width, height }
//!    [[profiles]] name/min/max/w/h   ─▶  [[popup.profile]] name/min/max, size.history
//! ```
//!
//! Talon applied built-in sizes whenever its file set no `popup` or `profiles`
//! (the owner's case: the file holds only patterns). Core has no such defaults,
//! so those built-ins are written out explicitly and the picker keeps its size on
//! every client width. Old sizes were strings even for cells (`"120"`); they
//! become core's spelling (`120` cells, `"90%"`). The fragment is validated the
//! way `history open` will read it, so a broken old file fails the migration
//! rather than the first launch.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use toml::{Table, Value};
use tools_core::{PopupDims, PopupSize};

use crate::config::{Config, PatternDefinition};
use crate::POPUP_KEY;

/// Talon's plugin id, which named its config dir.
const LEGACY_DIR: &str = "shadowfax.talon";

/// Talon's built-in `popup`, used when its file set none.
const BUILTIN_POPUP: LegacyPopup<&str> = LegacyPopup {
    width: "90%",
    height: "90%",
};

/// Talon's built-in `profiles`, in order, used when its file set none. Client
/// widths 351–399 match none and use [`BUILTIN_POPUP`].
const BUILTIN_PROFILES: &[LegacyProfile<&str>] = &[
    LegacyProfile {
        name: "laptop",
        min_client_width: None,
        max_client_width: Some(310),
        width: "95%",
        height: "90%",
    },
    LegacyProfile {
        name: "partial-ultrawide",
        min_client_width: None,
        max_client_width: Some(350),
        width: "90%",
        height: "90%",
    },
    LegacyProfile {
        name: "full-ultrawide",
        min_client_width: Some(400),
        max_client_width: None,
        width: "70%",
        height: "90%",
    },
];

/// Talon's whole config file, as talon 0.2.1 read it.
#[derive(Debug, Default, Deserialize)]
struct LegacyFile {
    alphabet: Option<String>,
    enabled_builtin_patterns: Option<Vec<String>>,
    #[serde(default)]
    patterns: Vec<PatternDefinition>,
    popup: Option<LegacyPopup<String>>,
    profiles: Option<Vec<LegacyProfile<String>>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPopup<S> {
    width: S,
    height: S,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyProfile<S> {
    name: S,
    min_client_width: Option<u16>,
    max_client_width: Option<u16>,
    width: S,
    height: S,
}

pub(crate) fn migrate_legacy(legacy_config_root: &Path) -> Result<Table> {
    let directory = legacy_config_root.join(LEGACY_DIR);
    // No talon dir: talon never ran here, so there is nothing to carry over.
    // Herdr creates a plugin's config dir, so an installed talon without a file
    // still gets its built-in sizes below.
    if !directory.is_dir() {
        return Ok(Table::new());
    }
    let path = directory.join("config.toml");
    let legacy = match std::fs::read_to_string(&path) {
        Ok(source) => toml::from_str::<LegacyFile>(&source)
            .with_context(|| format!("failed to parse {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LegacyFile::default(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    let fragment = fragment(legacy)?;
    let text = toml::to_string(&fragment)?;
    tools_core::Config::parse(&text)
        .and_then(|tools| Config::from_tools(&tools))
        .with_context(|| format!("{} does not carry over", path.display()))?;
    Ok(fragment)
}

fn fragment(legacy: LegacyFile) -> Result<Table> {
    let mut history = Table::new();
    if let Some(alphabet) = legacy.alphabet {
        history.insert("alphabet".into(), Value::String(alphabet));
    }
    if let Some(enabled) = legacy.enabled_builtin_patterns {
        history.insert(
            "enabled_builtin_patterns".into(),
            Value::Array(enabled.into_iter().map(Value::String).collect()),
        );
    }
    if !legacy.patterns.is_empty() {
        history.insert(
            "patterns".into(),
            Value::try_from(&legacy.patterns).context("could not carry over patterns")?,
        );
    }

    let base = match &legacy.popup {
        Some(popup) => dims(&popup.width, &popup.height)?,
        None => dims(BUILTIN_POPUP.width, BUILTIN_POPUP.height)?,
    };
    let profiles = match &legacy.profiles {
        Some(profiles) => profiles
            .iter()
            .map(|profile| profile_entry(&profile.as_str()))
            .collect::<Result<Vec<_>>>()?,
        None => BUILTIN_PROFILES
            .iter()
            .map(profile_entry)
            .collect::<Result<Vec<_>>>()?,
    };

    let mut size = Table::new();
    size.insert(POPUP_KEY.into(), base);
    let mut popup = Table::new();
    popup.insert("size".into(), Value::Table(size));
    if !profiles.is_empty() {
        popup.insert("profile".into(), Value::Array(profiles));
    }

    let mut fragment = Table::new();
    if !history.is_empty() {
        fragment.insert(crate::MODE.into(), Value::Table(history));
    }
    fragment.insert("popup".into(), Value::Table(popup));
    Ok(fragment)
}

impl LegacyProfile<String> {
    fn as_str(&self) -> LegacyProfile<&str> {
        LegacyProfile {
            name: &self.name,
            min_client_width: self.min_client_width,
            max_client_width: self.max_client_width,
            width: &self.width,
            height: &self.height,
        }
    }
}

/// One `[[popup.profile]]` that sizes only `history`.
fn profile_entry(profile: &LegacyProfile<&str>) -> Result<Value> {
    let mut entry = Table::new();
    entry.insert("name".into(), Value::String(profile.name.into()));
    if let Some(min) = profile.min_client_width {
        entry.insert("min_client_width".into(), Value::Integer(min.into()));
    }
    if let Some(max) = profile.max_client_width {
        entry.insert("max_client_width".into(), Value::Integer(max.into()));
    }
    let mut size = Table::new();
    size.insert(
        POPUP_KEY.into(),
        dims(profile.width, profile.height)
            .with_context(|| format!("popup profile '{}'", profile.name))?,
    );
    entry.insert("size".into(), Value::Table(size));
    Ok(Value::Table(entry))
}

/// Talon's string sizes (`"90%"`, `"120"`) in core's spelling.
fn dims(width: &str, height: &str) -> Result<Value> {
    let parse = |name: &str, value: &str| {
        value
            .parse::<PopupSize>()
            .map_err(|error| anyhow::anyhow!("popup {name} '{value}': {error}"))
    };
    let dims = PopupDims {
        width: parse("width", width)?,
        height: parse("height", height)?,
    };
    Value::try_from(dims).context("could not render a popup size")
}

/// Talon's built-in sizes as a herdr-tools config, for tests.
#[cfg(test)]
pub(crate) fn builtin_popup_toml() -> String {
    toml::to_string(&fragment(LegacyFile::default()).unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn legacy_root(config: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(LEGACY_DIR);
        std::fs::create_dir(&directory).unwrap();
        if let Some(config) = config {
            std::fs::write(directory.join("config.toml"), config).unwrap();
        }
        root
    }

    fn load(fragment: &Table) -> (tools_core::Config, Config) {
        let tools = tools_core::Config::parse(&toml::to_string(fragment).unwrap()).unwrap();
        let config = Config::from_tools(&tools).unwrap();
        (tools, config)
    }

    #[test]
    fn no_talon_dir_migrates_to_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert!(migrate_legacy(root.path()).unwrap().is_empty());
    }

    /// The owner's real file (copied into the fixture): five custom patterns and
    /// no popup settings, so talon's built-in profiles must be spelled out.
    #[test]
    fn owners_patterns_carry_over_with_talons_builtin_sizes() {
        let fragment = migrate_legacy(Path::new(OWNER_FIXTURE)).unwrap();
        let (tools, config) = load(&fragment);

        let custom = ["git-ssh", "user-uuid", "user-sha", "file-line", "hostname"];
        let names = config
            .patterns
            .iter()
            .map(|pattern| pattern.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names[..5], custom, "custom patterns stay first, in order");
        assert_eq!(names.len(), 5 + 11, "every built-in stays enabled");
        assert_eq!(config.alphabet, Config::default().alphabet);
        assert!(fragment["history"].get("alphabet").is_none());

        let popup = tools.popup_config();
        assert_eq!(
            popup
                .profile
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["laptop", "partial-ultrawide", "full-ultrawide"]
        );
        for (client, width, profile) in [
            (174, "95%", Some("laptop")),
            (310, "95%", Some("laptop")),
            (330, "90%", Some("partial-ultrawide")),
            (380, "90%", None),
            (512, "70%", Some("full-ultrawide")),
        ] {
            let resolved = config.popup(Some(client)).unwrap();
            assert_eq!(resolved.dims.width.to_string(), width, "client {client}");
            assert_eq!(resolved.dims.height.to_string(), "90%", "client {client}");
            assert_eq!(resolved.profile.as_deref(), profile, "client {client}");
        }
    }

    #[test]
    fn an_installed_talon_without_a_file_still_gets_its_builtin_sizes() {
        let root = legacy_root(None);
        let fragment = migrate_legacy(root.path()).unwrap();

        assert!(fragment.get("history").is_none());
        assert_eq!(
            toml::to_string(&fragment).unwrap(),
            builtin_popup_toml(),
            "same as the built-ins"
        );
    }

    #[test]
    fn configured_sizes_replace_the_builtins_and_cells_become_numbers() {
        let root = legacy_root(Some(
            r#"
alphabet = "abc"
enabled_builtin_patterns = ["url"]

[popup]
width = "120"
height = "80%"

[[profiles]]
name = "tiny"
min_client_width = 10
max_client_width = 100
width = "50%"
height = "40"
"#,
        ));
        let fragment = migrate_legacy(root.path()).unwrap();
        let (_, config) = load(&fragment);

        assert_eq!(
            fragment["popup"]["size"]["history"]["width"].as_integer(),
            Some(120)
        );
        assert_eq!(
            fragment["popup"]["profile"][0]["size"]["history"]["height"].as_integer(),
            Some(40)
        );
        assert_eq!(fragment["popup"]["profile"].as_array().unwrap().len(), 1);
        assert_eq!(config.alphabet, ['a', 'b', 'c']);
        assert_eq!(config.patterns.len(), 1);
        let small = config.popup(Some(90)).unwrap();
        assert_eq!(small.profile.as_deref(), Some("tiny"));
        assert_eq!(
            config.popup(Some(200)).unwrap().dims.width.to_string(),
            "120"
        );
    }

    #[test]
    fn an_empty_profiles_list_is_kept_empty() {
        let root = legacy_root(Some("profiles = []\n"));
        let fragment = migrate_legacy(root.path()).unwrap();
        let (_, config) = load(&fragment);

        assert!(fragment["popup"].get("profile").is_none());
        assert!(config.popup(Some(512)).unwrap().profile.is_none());
    }

    #[test]
    fn a_broken_old_config_fails_the_migration() {
        for (source, expected) in [
            ("alphabet = \"asj\"\n", "reserved"),
            (
                "[[patterns]]\nname = \"x\"\nregex = \"(\"\n",
                "invalid regex",
            ),
            ("[popup]\nwidth = \"0\"\nheight = \"90%\"\n", "popup width"),
            ("[[patterns]\n", "failed to parse"),
        ] {
            let root = legacy_root(Some(source));
            let error = format!("{:#}", migrate_legacy(root.path()).unwrap_err());
            assert!(error.contains(expected), "{source:?}: {error}");
        }
    }
}
