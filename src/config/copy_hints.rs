//! `[copy_hints]`: the label alphabet and the patterns behind copy-mode hints,
//! exactly as written in config.toml.
//!
//! Only the client uses these settings, and it reads them from its own local
//! config at startup and on reload, like `[ui] copy_on_select`. A server
//! compiles the table only to report problems. Validation lives in
//! `crate::copy_hints::HintRules::compile`, which knows the built-in pattern
//! names and the alphabet rules.
//!
//! A bad pattern must not cost the rest of the config. That holds at three
//! levels:
//! - a regex that does not compile, an unknown built-in name or a bad alphabet
//!   is a string problem, never a serde error, so `compile` reports it and
//!   drops only that item;
//! - each `[[copy_hints.patterns]]` entry stays a raw TOML value until
//!   `CopyHintPatternEntries::sources` checks it, so even `regex = 5` drops
//!   only its own entry. At startup `Config::load` deserializes the whole
//!   file at once, and a serde error anywhere would reset every section;
//! - `[copy_hints]` is its own top-level section. A live reload loads each
//!   section separately, so a type error in it (say `alphabet = 5`) keeps the
//!   previous copy-hint settings and nothing else.
//!
//! The entries are a tuple struct on purpose: `scripts/config_reference_check.py`
//! then treats `copy_hints.patterns` as one documented key instead of an
//! open-ended array of tables.

use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct CopyHintsConfig {
    /// Keys that make up hint labels, most comfortable first: at least two
    /// distinct lowercase letters or digits. Default: "asdfwerzxcuioptbm".
    pub alphabet: Option<String>,
    /// Built-in patterns matched after the custom ones, in this priority
    /// order. Default: every built-in.
    pub enabled_builtin_patterns: Option<Vec<String>>,
    /// Custom `[[copy_hints.patterns]]` tables with `name` and `regex`, tried
    /// before the built-ins in the order written.
    pub patterns: CopyHintPatternEntries,
}

/// `copy_hints.patterns` as written, any TOML value at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CopyHintPatternEntries(Option<toml::Value>);

/// One usable `[[copy_hints.patterns]]` entry: its position in the list, the
/// name diagnostics call it by, and its regex source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CopyHintPatternSource<'a> {
    pub(crate) index: usize,
    pub(crate) name: Option<&'a str>,
    pub(crate) regex: &'a str,
}

impl<'de> Deserialize<'de> for CopyHintPatternEntries {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        toml::Value::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

impl CopyHintPatternEntries {
    /// The entries that have a `regex` string, in config order. Every entry
    /// or key that is not usable adds one diagnostic and is skipped; the
    /// regexes themselves are checked by `HintRules::compile`.
    pub(crate) fn sources(&self, diagnostics: &mut Vec<String>) -> Vec<CopyHintPatternSource<'_>> {
        let Some(value) = self.0.as_ref() else {
            return Vec::new();
        };
        let Some(entries) = value.as_array() else {
            diagnostics.push(
                "copy_hints.patterns must be an array of [[copy_hints.patterns]] tables; ignoring it"
                    .to_owned(),
            );
            return Vec::new();
        };
        let mut sources = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let Some(table) = entry.as_table() else {
                diagnostics.push(format!(
                    "copy_hints.patterns[{index}] is not a table; skipping pattern"
                ));
                continue;
            };
            for key in table
                .keys()
                .filter(|key| !matches!(key.as_str(), "name" | "regex"))
            {
                diagnostics.push(format!(
                    "unknown config key copy_hints.patterns.{index}.{key}; ignoring key"
                ));
            }
            let name = match table.get("name") {
                None => None,
                Some(toml::Value::String(name)) => Some(name.as_str()),
                Some(_) => {
                    diagnostics.push(format!(
                        "copy_hints.patterns[{index}].name must be a string; skipping pattern"
                    ));
                    continue;
                }
            };
            match table.get("regex") {
                Some(toml::Value::String(regex)) => {
                    sources.push(CopyHintPatternSource { index, name, regex })
                }
                Some(_) => diagnostics.push(format!(
                    "copy_hints.patterns[{index}].regex must be a string; skipping pattern"
                )),
                None => diagnostics.push(format!(
                    "copy_hints.patterns[{index}] has no regex; skipping pattern"
                )),
            }
        }
        sources
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    fn sources(toml: &str) -> (Vec<(Option<String>, String)>, Vec<String>) {
        let config: Config = toml::from_str(toml).expect("config parses");
        let mut diagnostics = Vec::new();
        let sources = config
            .copy_hints
            .patterns
            .sources(&mut diagnostics)
            .into_iter()
            .map(|source| (source.name.map(str::to_owned), source.regex.to_owned()))
            .collect();
        (sources, diagnostics)
    }

    #[test]
    fn the_owners_patterns_parse_unchanged_under_copy_hints() {
        // The owner's five custom patterns, byte for byte.
        let (parsed, diagnostics) = sources(
            r#"
[[copy_hints.patterns]]
name = "git-ssh"
regex = 'git@[a-zA-Z0-9.-]+:[a-zA-Z0-9/.-]+\.git'

[[copy_hints.patterns]]
name = "user-uuid"
regex = "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"

[[copy_hints.patterns]]
name = "user-sha"
regex = "[0-9a-f]{7,40}"

[[copy_hints.patterns]]
name = "file-line"
regex = '/[^ \t\r\n]+:[0-9]+'

[[copy_hints.patterns]]
name = "hostname"
regex = '(?i)\b(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}\b'
"#,
        );

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            parsed
                .iter()
                .map(|(name, _)| name.as_deref())
                .collect::<Vec<_>>(),
            [
                Some("git-ssh"),
                Some("user-uuid"),
                Some("user-sha"),
                Some("file-line"),
                Some("hostname")
            ]
        );
        assert_eq!(parsed[3].1, r"/[^ \t\r\n]+:[0-9]+");
    }

    #[test]
    fn a_malformed_entry_costs_only_itself() {
        let (parsed, diagnostics) = sources(
            r#"
[[copy_hints.patterns]]
name = "number"
regex = 5

[[copy_hints.patterns]]
regex = "TKT-[0-9]+"
note = "no name is fine"

[[copy_hints.patterns]]
name = "missing"

[[copy_hints.patterns]]
name = ["not", "a", "string"]
regex = "x"

[[copy_hints.patterns]]
name = "kept"
regex = "PR-[0-9]+"
"#,
        );

        assert_eq!(
            parsed,
            [
                (None, "TKT-[0-9]+".to_owned()),
                (Some("kept".to_owned()), "PR-[0-9]+".to_owned()),
            ]
        );
        assert_eq!(
            diagnostics,
            [
                "copy_hints.patterns[0].regex must be a string; skipping pattern",
                "unknown config key copy_hints.patterns.1.note; ignoring key",
                "copy_hints.patterns[2] has no regex; skipping pattern",
                "copy_hints.patterns[3].name must be a string; skipping pattern",
            ]
        );
    }

    #[test]
    fn patterns_that_are_not_a_list_of_tables_are_reported() {
        let (parsed, diagnostics) = sources("[copy_hints]\npatterns = \"TKT-[0-9]+\"\n");
        assert!(parsed.is_empty());
        assert_eq!(
            diagnostics,
            ["copy_hints.patterns must be an array of [[copy_hints.patterns]] tables; ignoring it"]
        );

        let (parsed, diagnostics) = sources("[copy_hints]\npatterns = [\"TKT-[0-9]+\"]\n");
        assert!(parsed.is_empty());
        assert_eq!(
            diagnostics,
            ["copy_hints.patterns[0] is not a table; skipping pattern"]
        );
    }
}
