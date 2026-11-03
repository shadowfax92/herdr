//! The picker registry: every popup Herdr Jump can open, declared in the
//! embedded `pickers.toml`. A picker pairs a source with what Enter does; the
//! built-in `panes` source keeps its own navigator, and every other picker is
//! configuration on the line engine (`lines`). Herdr can only bind keys to
//! manifest actions, so each picker also has an `[[actions]]` entry in
//! `herdr-plugin.toml`; a unit test keeps the two files in agreement.
use anyhow::{bail, Context, Result};
use serde::Deserialize;

const PICKERS: &str = include_str!("../pickers.toml");

/// The picker `herdr-jump open` shows when no id is given (the Alt-p navigator).
pub(crate) const DEFAULT: &str = "panes";

/// Popup width in terminal columns for a picker that declares none.
const DEFAULT_WIDTH: u16 = 110;

/// One declared popup: immutable launch configuration read from the embedded
/// registry. `open` validates the id before asking Herdr for a popup, the popup
/// process looks it up again by the id in its environment, and
/// `install-keybindings` binds its `key` to `action`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Picker {
    pub id: String,
    /// Manifest action that opens this picker; keybindings target it.
    pub action: String,
    pub title: String,
    pub key: Option<String>,
    /// How the key reads in Herdr's key help.
    pub description: String,
    /// Outer popup width in terminal columns on a terminal wide enough for it;
    /// `Host::open` narrows it to fit smaller ones.
    pub width: u16,
    pub source: Source,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Every open Herdr pane, with views, kill mode and preview; Enter jumps.
    Panes,
    Lines(LineSource),
}

/// A command whose output lines are the items, and what Enter does with them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineSource {
    pub command: Vec<String>,
    pub kind: Kind,
    pub enter: Vec<Deliver>,
    pub empty: Option<String>,
    /// Tab and Shift-Tab mark items; Enter then delivers every mark, in the
    /// order marked, as one space-separated text.
    pub multi: bool,
}

/// How items are shown, matched and delivered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    #[default]
    Text,
    /// Filesystem paths: `~` display, leaf-first matching, absolute and
    /// shell-quoted when typed.
    Path,
}

/// One Enter effect. Order in the spec is execution order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Deliver {
    /// Type into the pane the popup was opened from, never followed by Enter.
    Paste,
    /// Put on the system clipboard.
    Copy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    picker: Vec<Raw>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    id: String,
    action: Option<String>,
    title: String,
    key: Option<String>,
    description: Option<String>,
    source: Option<String>,
    command: Option<Vec<String>>,
    kind: Option<Kind>,
    enter: Option<Vec<Deliver>>,
    empty: Option<String>,
    multi: Option<bool>,
    width: Option<u16>,
}

/// Every declared picker, in file order.
pub(crate) fn all() -> Result<Vec<Picker>> {
    parse(PICKERS)
}

pub(crate) fn find(id: &str) -> Result<Picker> {
    all()?
        .into_iter()
        .find(|picker| picker.id == id)
        .with_context(|| format!("unknown picker {id:?}; pickers are declared in pickers.toml"))
}

fn parse(text: &str) -> Result<Vec<Picker>> {
    let file: File = toml_edit::de::from_str(text).context("invalid pickers.toml")?;
    let mut pickers: Vec<Picker> = Vec::new();
    for raw in file.picker {
        let picker = validate(raw)?;
        if pickers
            .iter()
            .any(|p| p.id == picker.id || p.action == picker.action)
        {
            bail!("picker {:?} repeats an id or action", picker.id);
        }
        pickers.push(picker);
    }
    Ok(pickers)
}

fn validate(raw: Raw) -> Result<Picker> {
    let id = raw.id;
    let action = raw.action.unwrap_or_else(|| id.clone());
    // Herdr action ids allow letters, digits, `:`, `_` and `-`; the id also
    // travels through an environment variable, so keep it to that alphabet.
    for name in [&id, &action] {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))
        {
            bail!("picker id/action {name:?} must use letters, digits, ':', '_' or '-'");
        }
    }
    let source = match (raw.source.as_deref(), raw.command) {
        (Some("panes"), None) => {
            if raw.kind.is_some()
                || raw.enter.is_some()
                || raw.empty.is_some()
                || raw.multi.is_some()
            {
                bail!("picker {id:?}: the panes source takes no kind, enter, empty or multi");
            }
            Source::Panes
        }
        (Some(other), None) => bail!("picker {id:?}: unknown source {other:?}"),
        (None, Some(command)) => {
            if command.first().is_none_or(|program| program.is_empty()) {
                bail!("picker {id:?}: command needs a program");
            }
            let enter = raw.enter.unwrap_or_default();
            if enter.is_empty() {
                bail!("picker {id:?}: enter needs \"paste\", \"copy\" or both");
            }
            Source::Lines(LineSource {
                command,
                kind: raw.kind.unwrap_or_default(),
                enter,
                empty: raw.empty,
                multi: raw.multi.unwrap_or_default(),
            })
        }
        _ => bail!("picker {id:?} needs exactly one of source or command"),
    };
    let width = raw.width.unwrap_or(DEFAULT_WIDTH);
    if !(40..=400).contains(&width) {
        bail!("picker {id:?}: width must be 40 to 400 columns");
    }
    Ok(Picker {
        id,
        action,
        description: raw.description.unwrap_or_else(|| raw.title.clone()),
        title: raw.title,
        key: raw.key,
        width,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml_edit::DocumentMut;

    /// Built-ins must exist; further pickers are configuration, so nothing
    /// here may depend on how many the file declares.
    #[test]
    fn embedded_registry_declares_panes_and_paths() {
        let panes = find(DEFAULT).unwrap();
        assert_eq!(panes.action, "open");
        assert_eq!(panes.key.as_deref(), Some("alt+p"));
        assert_eq!(panes.source, Source::Panes);
        // 20% wider than the paths popup, for the Name column.
        assert_eq!(panes.width, 132);
        let paths = find("paths").unwrap();
        assert_eq!(paths.action, "paths");
        assert_eq!(paths.key.as_deref(), Some("alt+n"));
        assert_eq!(paths.width, 110);
        assert_eq!(
            paths.source,
            Source::Lines(LineSource {
                command: vec!["zoxide".into(), "query".into(), "--list".into()],
                kind: Kind::Path,
                enter: vec![Deliver::Paste, Deliver::Copy],
                empty: Some(
                    "zoxide has no directories yet. cd around and they appear here.".into()
                ),
                multi: true,
            })
        );
        assert!(find("missing").is_err());
    }

    /// Herdr binds keys to manifest actions, so a picker without its action is
    /// unreachable and an `open` action without a picker fails at runtime.
    #[test]
    fn every_picker_has_its_manifest_action_and_back() {
        let manifest: DocumentMut = include_str!("../herdr-plugin.toml").parse().unwrap();
        let actions = manifest["actions"].as_array_of_tables().unwrap();
        let command = |table: &toml_edit::Table| -> Vec<String> {
            table["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect()
        };
        let pickers = all().unwrap();
        for picker in &pickers {
            let action = actions
                .iter()
                .find(|a| a["id"].as_str() == Some(picker.action.as_str()))
                .unwrap_or_else(|| panic!("no [[actions]] id = {:?}", picker.action));
            let mut expected = vec!["./target/release/herdr-jump".to_owned(), "open".into()];
            if picker.id != DEFAULT {
                expected.push(picker.id.clone());
            }
            assert_eq!(command(action), expected, "{}", picker.id);
        }
        for action in actions.iter() {
            let argv = command(action);
            if argv.get(1).map(String::as_str) == Some("open") {
                let id = argv.get(2).map_or(DEFAULT, String::as_str);
                assert!(pickers.iter().any(|p| p.id == id), "{argv:?}");
            }
        }
    }

    #[test]
    fn invalid_specs_are_rejected_with_the_picker_named() {
        for (spec, needle) in [
            ("id = \"x\"\ntitle = \"X\"", "exactly one"),
            (
                "id = \"x\"\ntitle = \"X\"\nsource = \"panes\"\ncommand = [\"ls\"]",
                "exactly one",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\ncommand = []\nenter = [\"copy\"]",
                "program",
            ),
            ("id = \"x\"\ntitle = \"X\"\ncommand = [\"ls\"]", "enter"),
            (
                "id = \"x\"\ntitle = \"X\"\ncommand = [\"ls\"]\nenter = []",
                "enter",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\ncommand = [\"ls\"]\nenter = [\"run\"]",
                "pickers.toml",
            ),
            (
                "id = \"x y\"\ntitle = \"X\"\ncommand = [\"ls\"]\nenter = [\"copy\"]",
                "letters",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\nsource = \"tabs\"",
                "unknown source",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\nsource = \"panes\"\nkind = \"path\"",
                "takes no",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\nsource = \"panes\"\nmulti = true",
                "takes no",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\ncommand = [\"ls\"]\nenter = [\"copy\"]\nmulti = \"yes\"",
                "pickers.toml",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\ncommand = [\"ls\"]\nenter = [\"copy\"]\ncolor = 1",
                "pickers.toml",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\nsource = \"panes\"\nwidth = 20",
                "width must be",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\nsource = \"panes\"\nwidth = \"80%\"",
                "pickers.toml",
            ),
        ] {
            let error = parse(&format!("[[picker]]\n{spec}\n")).unwrap_err();
            assert!(format!("{error:#}").contains(needle), "{spec}: {error:#}");
        }
        let twice =
            "[[picker]]\nid = \"x\"\ntitle = \"X\"\ncommand = [\"ls\"]\nenter = [\"copy\"]\n";
        assert!(parse(&twice.repeat(2)).is_err());
    }
}
