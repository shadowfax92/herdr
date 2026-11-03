//! The `history` mode's settings: the `[history]` table of the one herdr-tools
//! `config.toml` (hint alphabet and target patterns), plus the shared `[popup]`
//! table, from which the picker's size is resolved for popup key `history`.
//!
//! Talon kept these in its own `shadowfax.talon/config.toml` with top-level
//! `alphabet`, `enabled_builtin_patterns`, `[[patterns]]`, `[popup]` and
//! `[[profiles]]`. The first three moved under `[history]` unchanged; the popup
//! sizes moved to core's `[popup.size].history` / `[[popup.profile]]` (see
//! `migrate.rs`, which carries an old file over).

use std::path::Path;

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tools_core::{PopupConfig, ResolvedPopup};

use crate::{MODE, POPUP_KEY};

const DEFAULT_ALPHABET: &str = "asdfwerzxcuioptbm";
const LEGACY_DEFAULT_ALPHABET: &str = "asdfwerzxvjkluopghtyb";
const RESERVED_HINT_KEYS: &str = "ghjklnqvy";

const BUILTIN_PATTERNS: &[(&str, &str)] = &[
    ("ip", r"\d{1,3}(?:\.\d{1,3}){3}"),
    (
        "uuid",
        r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
    ),
    ("sha", r"[0-9a-f]{7,128}"),
    ("digit", r"[0-9]{4,}"),
    (
        "url",
        r#"(?:https?://|git@|git://|ssh://|ftp://|file:///)[^\s()"']+"#,
    ),
    ("path", r"(?:[.\w\-~$@]+)?(?:/[.\w\-@]+)+/?"),
    ("hex", r"0x[0-9a-fA-F]+"),
    (
        "kubernetes",
        r"(?:binding|componentstatuses|configmap|endpoints?|events?|limitrange|namespaces?|nodes?|persistentvolumeclaims?|persistentvolumes?|pods?|podtemplates?|replicationcontrollers?|resourcequotas?|secrets?|serviceaccounts?|services?|daemonset\.apps|deployment\.apps|replicaset\.apps|statefulset\.apps|cronjob\.batch|job\.batch|ingress\.networking\.k8s\.io|networkpolicy\.networking\.k8s\.io|clusterrolebindings?\.rbac\.authorization\.k8s\.io|roles?\.rbac\.authorization\.k8s\.io|customresourcedefinitions?\.apiextensions\.k8s\.io)[[:alnum:]_#$%&+=/@.-]+",
    ),
    (
        "git-status",
        r"(?:modified|deleted|deleted by us|new file): +(?<match>.+)",
    ),
    (
        "git-status-branch",
        r"Your branch is up to date with '(?<match>.*)'\.",
    ),
    ("diff", r"(?:---|\+\+\+) [ab]/(?<match>.*)"),
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PatternDefinition {
    pub name: String,
    pub regex: String,
}

/// The validated settings one `history open` run uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub alphabet: Vec<char>,
    pub patterns: Vec<PatternDefinition>,
    /// The whole shared `[popup]` table; [`Config::popup`] reads key `history`.
    pub popup: PopupConfig,
}

impl Default for Config {
    /// No `[popup]` sizes: the picker opens at the manifest pane's 90% x 90%.
    /// Talon's built-in laptop / ultrawide profiles are not defaults any more;
    /// `migrate-config` writes them out for anyone who relied on them.
    fn default() -> Self {
        Self {
            alphabet: DEFAULT_ALPHABET.chars().collect(),
            patterns: definitions(BUILTIN_PATTERNS).collect(),
            popup: PopupConfig::default(),
        }
    }
}

impl Config {
    /// Loads a herdr-tools `config.toml`; a missing file is every default.
    pub fn load(path: &Path) -> Result<Self> {
        Self::from_tools(&tools_core::Config::load(path)?)
    }

    /// The `[history]` table (all defaults when absent) and `[popup]` of an
    /// already-loaded herdr-tools config.
    pub fn from_tools(tools: &tools_core::Config) -> Result<Self> {
        let file: FileConfig = tools.section(MODE)?;
        let defaults = Self::default();
        let configured_alphabet = file
            .alphabet
            .unwrap_or_else(|| defaults.alphabet.iter().collect());
        let alphabet = if configured_alphabet == LEGACY_DEFAULT_ALPHABET {
            DEFAULT_ALPHABET
        } else {
            &configured_alphabet
        }
        .chars()
        .collect::<Vec<_>>();
        validate_alphabet(&alphabet)?;

        let enabled = file.enabled_builtin_patterns.unwrap_or_else(|| {
            BUILTIN_PATTERNS
                .iter()
                .map(|(name, _)| (*name).to_string())
                .collect()
        });
        let mut patterns = file.patterns;
        for name in enabled {
            let Some((_, source)) = BUILTIN_PATTERNS
                .iter()
                .find(|(candidate, _)| *candidate == name)
            else {
                bail!("unknown built-in pattern '{name}'");
            };
            patterns.push(PatternDefinition {
                name,
                regex: (*source).to_string(),
            });
        }
        validate_patterns(&patterns)?;

        Ok(Self {
            alphabet,
            patterns,
            popup: tools.popup_config().clone(),
        })
    }

    /// The picker's configured size on a client `client_width` cells wide: the
    /// first matching profile that sizes `history`, else `[popup.size].history`,
    /// else None (the manifest default). An unknown width skips profiles.
    pub fn popup(&self, client_width: Option<u16>) -> Option<ResolvedPopup> {
        self.popup.resolve(POPUP_KEY, client_width)
    }
}

/// `[history]` as written. Unknown keys are refused: one file now holds every
/// mode, and talon's old `popup` / `profiles` keys belong under `[popup]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    alphabet: Option<String>,
    enabled_builtin_patterns: Option<Vec<String>>,
    #[serde(default)]
    patterns: Vec<PatternDefinition>,
}

fn definitions(
    patterns: &'static [(&'static str, &'static str)],
) -> impl Iterator<Item = PatternDefinition> {
    patterns.iter().map(|(name, regex)| PatternDefinition {
        name: (*name).to_string(),
        regex: (*regex).to_string(),
    })
}

fn validate_alphabet(alphabet: &[char]) -> Result<()> {
    if alphabet.len() < 2 {
        bail!("alphabet must contain at least two keys");
    }
    let unique = alphabet.iter().copied().collect::<HashSet<_>>();
    if unique.len() != alphabet.len() {
        bail!("alphabet keys must be unique");
    }
    if alphabet.iter().any(|key| !key.is_ascii_lowercase()) {
        bail!("alphabet must use lower-case ASCII keys");
    }
    let conflicts = alphabet
        .iter()
        .copied()
        .filter(|key| RESERVED_HINT_KEYS.contains(*key))
        .collect::<String>();
    if !conflicts.is_empty() {
        bail!(
            "alphabet contains reserved normal-mode keys: {conflicts}; remove them or use the default '{DEFAULT_ALPHABET}'"
        );
    }
    Ok(())
}

fn validate_patterns(patterns: &[PatternDefinition]) -> Result<()> {
    let mut names = HashSet::new();
    for pattern in patterns {
        if pattern.name.trim().is_empty() {
            bail!("pattern names cannot be empty");
        }
        if !names.insert(pattern.name.as_str()) {
            bail!("duplicate pattern name '{}'", pattern.name);
        }
        regex::Regex::new(&pattern.regex)
            .with_context(|| format!("invalid regex for pattern '{}'", pattern.name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn defaults_include_every_approved_pattern() {
        let config = Config::default();
        let names = config
            .patterns
            .iter()
            .map(|pattern| pattern.name.as_str())
            .collect::<HashSet<_>>();

        for expected in [
            "ip",
            "uuid",
            "sha",
            "digit",
            "url",
            "path",
            "hex",
            "kubernetes",
            "git-status",
            "git-status-branch",
            "diff",
        ] {
            assert!(names.contains(expected), "missing {expected}");
        }
        assert!(config.alphabet.len() >= 10);
        assert!(!config.alphabet.contains(&'q'));
    }

    #[test]
    fn custom_patterns_are_ordered_before_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[history]
alphabet = "abc"
enabled_builtin_patterns = ["url"]

[[history.patterns]]
name = "ticket"
regex = "TKT-[0-9]+"
"#,
        )
        .unwrap();

        let config = Config::load(&path).unwrap();

        assert_eq!(config.alphabet, vec!['a', 'b', 'c']);
        assert_eq!(config.patterns[0].name, "ticket");
        assert!(config.patterns.iter().any(|pattern| pattern.name == "url"));
        assert!(!config.patterns.iter().any(|pattern| pattern.name == "ip"));
    }

    #[test]
    fn invalid_regex_and_alphabet_fail_during_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[history]
alphabet = "aa"

[[history.patterns]]
name = "broken"
regex = "("
"#,
        )
        .unwrap();

        let error = Config::load(&path).unwrap_err().to_string();

        assert!(error.contains("alphabet") || error.contains("broken"));
    }

    /// Talon's built-in profiles now arrive as `[[popup.profile]]` entries
    /// (written by `migrate-config`); resolution must pick the same sizes.
    #[test]
    fn popup_profiles_are_responsive_and_first_match_wins() {
        let tools = tools_core::Config::parse(&crate::migrate::builtin_popup_toml()).unwrap();
        let config = Config::from_tools(&tools).unwrap();
        let width = |client| config.popup(Some(client)).unwrap().dims.width.to_string();

        assert_eq!(width(300), "95%");
        assert_eq!(width(330), "90%");
        assert_eq!(width(380), "90%");
        assert_eq!(width(512), "70%");
        assert_eq!(
            config.popup(Some(512)).unwrap().dims.height.to_string(),
            "90%"
        );
        // No config at all: no size, so Herdr uses the manifest's 90% x 90%.
        assert_eq!(Config::default().popup(Some(512)), None);
    }

    #[test]
    fn hint_alphabet_rejects_normal_mode_commands() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[history]\nalphabet = \"asj\"\n").unwrap();

        let error = Config::load(&path).unwrap_err().to_string();

        assert!(error.contains("reserved"));
        assert!(error.contains('j'));
        assert!(error.contains(DEFAULT_ALPHABET));
    }

    #[test]
    fn legacy_default_alphabet_migrates_to_the_safe_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[history]\nalphabet = \"asdfwerzxvjkluopghtyb\"\n").unwrap();

        let config = Config::load(&path).unwrap();

        assert_eq!(
            config.alphabet,
            DEFAULT_ALPHABET.chars().collect::<Vec<_>>()
        );
    }

    #[test]
    fn talons_old_top_level_keys_are_refused_inside_history() {
        let error = Config::from_tools(
            &tools_core::Config::parse("[history]\npopup = { width = \"90%\" }\n").unwrap(),
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("unknown field `popup`"),
            "{error:#}"
        );
    }
}
