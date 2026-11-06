//! `[agent_key_rules]`: per-agent rules for keys herdr would forward to the
//! focused pane, as written in config.toml and compiled for the client.
//!
//! A rule names an agent herdr detects, a key, and what herdr does instead of
//! forwarding that key to the agent: drop it, or send another key. Only the
//! client applies rules, as the last step of Terminal-mode key routing, after
//! herdr's own bindings and the prefix. It reads them from its own local
//! config at startup and on reload, like `[copy_hints]`, under `--remote` and
//! with either `--remote-keybindings` value. Rules have no server half, unlike
//! `[keys]`, whose custom commands the server defines and runs. A server
//! compiles the table only to report problems.
//!
//! A bad rule must not cost the rest of the config, at either entry point:
//! - at startup `Config::load` deserializes the whole file at once, and a
//!   serde error anywhere would reset every section to its default. The
//!   section therefore stays a raw TOML value, which no input can fail to
//!   deserialize into, until `AgentKeyRules::compile` walks it;
//! - a live reload loads each top-level section on its own. This one accepts
//!   any value, so it always loads, and the same `compile` decides rule by
//!   rule what survives.
//!
//! `compile` turns each problem into one diagnostic and skips only the rule or
//! agent table it is in.
//!
//! The section is a tuple struct on purpose: `scripts/config_reference_check.py`
//! then treats `agent_key_rules` as one documented key instead of an
//! open-ended table of tables.

use serde::Deserialize;

use super::io::{format_config_key_path, ConfigKeyPathSegment};
use super::keybinds::{
    format_key_combo, is_unmodified_printable, parse_key_combo, terminal_key_matches_combo,
    KeyCombo,
};
use crate::detect::{agent_label, parse_agent_label, parse_canonical_agent_label, Agent};
use crate::input::TerminalKey;

const SECTION: &str = "agent_key_rules";

/// The two action forms, as diagnostics name them.
const ACTION_FORMS: &str = r#""drop" or { remap = "<key>" }"#;

/// `[agent_key_rules]` as written: any TOML value at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentKeyRulesConfig(Option<toml::Value>);

impl<'de> Deserialize<'de> for AgentKeyRulesConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        toml::Value::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

/// What herdr does with a forwarded key that a rule matches. Herdr owns the
/// matched key either way, as it owns a key bound to one of its actions, so
/// the pane never sees the user's press, repeats or release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyAction {
    /// The pane receives nothing.
    Drop,
    /// The pane receives one whole keystroke (press and release) of this key
    /// for the press and for each repeat.
    Remap(KeyCombo),
}

/// One usable rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyRule {
    pub(crate) agent: Agent,
    /// Normalized by `parse_key_combo` and matched with herdr's binding
    /// matcher, so Shift and shifted punctuation match the way `[keys]` does.
    pub(crate) key: KeyCombo,
    pub(crate) action: KeyAction,
}

/// `[agent_key_rules]` validated and compiled, once per config load.
///
/// Only `compile` builds one, so it never holds an invalid rule and nothing
/// at key time handles a config error.
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentKeyRules {
    /// In compile order: agent tables by name, then keys by string, both
    /// sorted because the toml crate is built without `preserve_order`. No two
    /// rules share an agent and a key. Two rules can still match one key only
    /// for a shifted punctuation chord written both ways (`ctrl+?` and
    /// `ctrl+shift+/`); the first wins.
    rules: Vec<KeyRule>,
}

impl AgentKeyRules {
    /// Validates and compiles `[agent_key_rules]`. It never fails: each
    /// problem becomes one diagnostic and costs only the rule or agent table
    /// it is in. Startup diagnostics (`Config::collect_diagnostics`),
    /// live-reload diagnostics and the client's rules all come from this one
    /// function.
    pub(crate) fn compile(config: &AgentKeyRulesConfig) -> (Self, Vec<String>) {
        let mut diagnostics = Vec::new();
        let mut rules: Vec<KeyRule> = Vec::new();
        let Some(value) = config.0.as_ref() else {
            return (Self { rules }, diagnostics);
        };
        let Some(agents) = value.as_table() else {
            diagnostics.push(format!(
                "{SECTION} must be a table of [{SECTION}.<agent>] tables; ignoring it"
            ));
            return (Self { rules }, diagnostics);
        };
        for (name, entries) in agents {
            // Agent names are read the way detection reads labels, so case and
            // the detector's aliases (`claude-code`) name the same agent.
            let Some(agent) = parse_agent_label(name) else {
                diagnostics.push(format!(
                    "{} is not an agent herdr detects (known: {}); skipping its rules",
                    key_path(&[name]),
                    known_agent_labels()
                ));
                continue;
            };
            let Some(entries) = entries.as_table() else {
                diagnostics.push(format!(
                    "{} must be a table of \"<key>\" = <action> entries; skipping its rules",
                    key_path(&[name])
                ));
                continue;
            };
            for (key_name, value) in entries {
                match compile_rule(agent, key_name, value, &rules) {
                    Ok(rule) => rules.push(rule),
                    Err(problem) => diagnostics.push(format!(
                        "{} {problem}; skipping rule",
                        key_path(&[name, key_name])
                    )),
                }
            }
        }
        (Self { rules }, diagnostics)
    }

    /// Whether any rule exists. Key routing checks this before it reads the
    /// snapshot, so a config without rules costs one branch per forwarded key.
    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rule for `key` on its way to a pane whose agent `herdr agent list`
    /// calls `label`. The label is read the way detection reads labels, so
    /// case and the detector's aliases name the same agent, and a label herdr
    /// does not know has no rules. Process detection reports canonical labels,
    /// which resolve without allocating; only a hook that reports some other
    /// spelling pays for the normalizing parse.
    pub(crate) fn find(&self, label: &str, key: &TerminalKey) -> Option<KeyRule> {
        let agent = parse_canonical_agent_label(label).or_else(|| parse_agent_label(label))?;
        self.rules
            .iter()
            .find(|rule| rule.agent == agent && terminal_key_matches_combo(key, rule.key))
            .copied()
    }
}

/// One entry of an agent's table, or the problem that skips it, worded to
/// follow the entry's path in a diagnostic.
fn compile_rule(
    agent: Agent,
    key_name: &str,
    value: &toml::Value,
    rules: &[KeyRule],
) -> Result<KeyRule, String> {
    let key = parse_key_combo(key_name).ok_or_else(|| "is not a key".to_owned())?;
    // `[keys]` refuses these as direct bindings for the same reason: a host in
    // kitty report-all mode sends them as text, which never passes key
    // routing, so such a rule would hold on some hosts and not on others.
    if is_unmodified_printable(key) {
        return Err("is a typing key, which can reach the pane as text".to_owned());
    }
    let action = parse_action(value)?;
    if rules
        .iter()
        .any(|rule| rule.agent == agent && rule.key == key)
    {
        return Err(format!(
            "repeats the {} rule for {}",
            agent_label(agent),
            format_key_combo(key)
        ));
    }
    Ok(KeyRule { agent, key, action })
}

/// An entry's action in serde's externally tagged form: a string names an
/// action without a parameter, and a one-entry table names an action with its
/// parameter. A future action is a new name, and a herdr that does not know it
/// reports and skips only that rule.
fn parse_action(value: &toml::Value) -> Result<KeyAction, String> {
    let wrong_form = || format!("must be {ACTION_FORMS}");
    let unknown = |name: &str| format!("has unknown action {name:?} (expected {ACTION_FORMS})");
    match value {
        toml::Value::String(name) => match name.as_str() {
            "drop" => Ok(KeyAction::Drop),
            "remap" => Err(wrong_form()),
            _ => Err(unknown(name)),
        },
        toml::Value::Table(table) => {
            let mut entries = table.iter();
            match (entries.next(), entries.next()) {
                (Some((name, parameter)), None) => match (name.as_str(), parameter) {
                    ("remap", toml::Value::String(target)) => parse_key_combo(target)
                        .map(KeyAction::Remap)
                        .ok_or_else(|| format!("remaps to {target:?}, which is not a key")),
                    ("drop" | "remap", _) => Err(wrong_form()),
                    (name, _) => Err(unknown(name)),
                },
                _ => Err(wrong_form()),
            }
        }
        _ => Err(wrong_form()),
    }
}

/// `agent_key_rules` and `names` as one dotted path, rendered the way
/// "unknown config key" diagnostics render paths: plain names bare, anything
/// else TOML-quoted, as in `agent_key_rules.claude."ctrl+d"`.
fn key_path(names: &[&str]) -> String {
    let segments: Vec<_> = std::iter::once(SECTION)
        .chain(names.iter().copied())
        .map(|name| ConfigKeyPathSegment::Key(name.to_owned()))
        .collect();
    format_config_key_path(&segments)
}

/// Every label `herdr agent list` can report for a detected agent, so the
/// list grows with the detector instead of going stale.
fn known_agent_labels() -> String {
    Agent::ALL
        .iter()
        .map(|agent| agent_label(*agent))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::config::{is_keybinding_config_diagnostic, Config};

    fn compile(toml: &str) -> (AgentKeyRules, Vec<String>) {
        let config: Config = toml::from_str(toml).expect("config parses");
        AgentKeyRules::compile(&config.agent_key_rules)
    }

    fn ctrl(ch: char) -> TerminalKey {
        TerminalKey::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn drop_rule(agent: Agent, ch: char) -> Option<KeyRule> {
        Some(KeyRule {
            agent,
            key: (KeyCode::Char(ch), KeyModifiers::CONTROL),
            action: KeyAction::Drop,
        })
    }

    /// One config with every rule-level and table-level problem beside the
    /// rules that survive them.
    const EVERY_PROBLEM: &str = r#"
[agent_key_rules]
codex = "drop"

[agent_key_rules.cluade]
"ctrl+d" = "drop"

[agent_key_rules.claude]
"control+d" = "drop"
"ctrl+d" = "drop"
"ctrl+dd" = "drop"
q = "drop"
"shift+a" = "drop"
"ctrl+b" = "block"
"ctrl+e" = { send = "x" }
"ctrl+f" = 5
"ctrl+g" = { remap = "pagedown" }
"ctrl+h" = { remap = 5 }
"ctrl+j" = { drop = true }
"ctrl+k" = {}
"ctrl+l" = { remap = "up", drop = "x" }
"ctrl+n" = "remap"
"ctrl+p" = { remap = "up" }

[agent_key_rules.claude-code]
"ctrl+d" = "drop"
"ctrl+t" = "drop"
"#;

    #[test]
    fn the_owners_rules_compile() {
        let (rules, diagnostics) = compile(
            r#"
[agent_key_rules.claude]
"ctrl+d" = "drop"
"ctrl+t" = "drop"
"#,
        );

        assert_eq!(diagnostics, Vec::<String>::new());
        assert_eq!(
            rules.find("claude", &ctrl('d')),
            drop_rule(Agent::Claude, 'd')
        );
        assert_eq!(
            rules.find("claude", &ctrl('t')),
            drop_rule(Agent::Claude, 't')
        );
        assert_eq!(rules.find("codex", &ctrl('d')), None);
        assert_eq!(rules.find("claude", &ctrl('u')), None);
    }

    #[test]
    fn no_table_means_no_rules_and_no_diagnostics() {
        let (rules, diagnostics) = compile("");
        assert!(rules.is_empty());
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn a_remap_names_the_key_herdr_sends_instead() {
        let (rules, diagnostics) = compile(
            r#"
[agent_key_rules.codex]
"ctrl+p" = { remap = "up" }
"ctrl+n" = { remap = "ctrl+shift+tab" }
"#,
        );

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            rules.find("codex", &ctrl('p')).map(|rule| rule.action),
            Some(KeyAction::Remap((KeyCode::Up, KeyModifiers::empty())))
        );
        assert_eq!(
            rules.find("codex", &ctrl('n')).map(|rule| rule.action),
            Some(KeyAction::Remap((KeyCode::BackTab, KeyModifiers::CONTROL)))
        );
    }

    #[test]
    fn agent_names_and_pane_labels_fold_like_detection() {
        let (rules, diagnostics) = compile(
            r#"
[agent_key_rules.claude-code]
"ctrl+d" = "drop"

[agent_key_rules.Codex]
"ctrl+t" = "drop"
"#,
        );

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for label in ["claude", "Claude", "claude-code"] {
            assert_eq!(
                rules.find(label, &ctrl('d')),
                drop_rule(Agent::Claude, 'd'),
                "{label}"
            );
        }
        assert_eq!(
            rules.find("codex", &ctrl('t')),
            drop_rule(Agent::Codex, 't')
        );
        assert_eq!(rules.find("my-hook-agent", &ctrl('d')), None);
    }

    #[test]
    fn rule_keys_match_the_way_bindings_match() {
        let (rules, _) = compile(
            r#"
[agent_key_rules.claude]
"ctrl+D" = "drop"
"ctrl+?" = "drop"
"#,
        );

        let ctrl_shift_d = TerminalKey::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(rules.find("claude", &ctrl_shift_d).is_some());
        assert_eq!(
            rules.find("claude", &ctrl('d')),
            None,
            "shift is part of the key"
        );
        let mut ctrl_question = TerminalKey::new(
            KeyCode::Char('/'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        ctrl_question.shifted_codepoint = Some(u32::from('?'));
        assert!(rules.find("claude", &ctrl_question).is_some());
    }

    #[test]
    fn each_bad_entry_costs_only_itself() {
        let (rules, diagnostics) = compile(EVERY_PROBLEM);

        let known = known_agent_labels();
        assert!(known.starts_with("pi, claude, codex,") && known.contains("agy"));
        let action_forms = r#""drop" or { remap = "<key>" }"#;
        assert_eq!(
            diagnostics,
            vec![
                r#"agent_key_rules.claude."ctrl+b" has unknown action "block" (expected "drop" or { remap = "<key>" }); skipping rule"#.to_owned(),
                r#"agent_key_rules.claude."ctrl+d" repeats the claude rule for ctrl+d; skipping rule"#.to_owned(),
                r#"agent_key_rules.claude."ctrl+dd" is not a key; skipping rule"#.to_owned(),
                format!(r#"agent_key_rules.claude."ctrl+e" has unknown action "send" (expected {action_forms}); skipping rule"#),
                format!(r#"agent_key_rules.claude."ctrl+f" must be {action_forms}; skipping rule"#),
                r#"agent_key_rules.claude."ctrl+g" remaps to "pagedown", which is not a key; skipping rule"#.to_owned(),
                format!(r#"agent_key_rules.claude."ctrl+h" must be {action_forms}; skipping rule"#),
                format!(r#"agent_key_rules.claude."ctrl+j" must be {action_forms}; skipping rule"#),
                format!(r#"agent_key_rules.claude."ctrl+k" must be {action_forms}; skipping rule"#),
                format!(r#"agent_key_rules.claude."ctrl+l" must be {action_forms}; skipping rule"#),
                format!(r#"agent_key_rules.claude."ctrl+n" must be {action_forms}; skipping rule"#),
                "agent_key_rules.claude.q is a typing key, which can reach the pane as text; skipping rule".to_owned(),
                r#"agent_key_rules.claude."shift+a" is a typing key, which can reach the pane as text; skipping rule"#.to_owned(),
                r#"agent_key_rules.claude-code."ctrl+d" repeats the claude rule for ctrl+d; skipping rule"#.to_owned(),
                format!("agent_key_rules.cluade is not an agent herdr detects (known: {known}); skipping its rules"),
                r#"agent_key_rules.codex must be a table of "<key>" = <action> entries; skipping its rules"#.to_owned(),
            ]
        );

        // The rules beside the problems survive, whichever spelling or alias
        // table they were written under.
        assert_eq!(
            rules.find("claude", &ctrl('d')),
            drop_rule(Agent::Claude, 'd')
        );
        assert_eq!(
            rules.find("claude", &ctrl('t')),
            drop_rule(Agent::Claude, 't')
        );
        assert_eq!(
            rules.find("claude", &ctrl('p')).map(|rule| rule.action),
            Some(KeyAction::Remap((KeyCode::Up, KeyModifiers::empty())))
        );
        for skipped in ['b', 'e', 'f', 'g', 'h', 'j', 'k', 'l', 'n'] {
            assert_eq!(rules.find("claude", &ctrl(skipped)), None, "ctrl+{skipped}");
        }
        assert_eq!(rules.find("codex", &ctrl('d')), None);
    }

    #[test]
    fn a_section_that_is_not_a_table_is_ignored_whole() {
        let (rules, diagnostics) = compile("agent_key_rules = 5\n");
        assert!(rules.is_empty());
        assert_eq!(
            diagnostics,
            vec![
                "agent_key_rules must be a table of [agent_key_rules.<agent>] tables; ignoring it"
            ]
        );
    }

    #[test]
    fn diagnostics_stay_visible_when_the_keymap_comes_from_the_server() {
        let (_, mut diagnostics) = compile(EVERY_PROBLEM);
        diagnostics.extend(compile("agent_key_rules = 5\n").1);

        assert_eq!(diagnostics.len(), 17);
        for diagnostic in &diagnostics {
            assert!(
                !is_keybinding_config_diagnostic(diagnostic),
                "hidden under --remote-keybindings server: {diagnostic}"
            );
        }
    }
}
