//! The picker registry: every popup Find can open, declared in the embedded
//! `pickers.toml`. A picker pairs a source with what Enter does; the built-in
//! `panes` source keeps its own navigator, and every other picker is
//! configuration on the line engine (`lines`). Herdr can only bind keys to
//! manifest actions, so each picker also has an `[[actions]]` entry in
//! herdr-tools' `herdr-plugin.toml`; a unit test keeps the two files in
//! agreement. The action id doubles as the picker's popup key in `[popup]`.
//! Line pickers that share a `group` open as one popup with one list each
//! ([`lists`]); ctrl-o switches between them.
use anyhow::{bail, Context, Result};
use serde::Deserialize;

const PICKERS: &str = include_str!("../pickers.toml");

/// The picker `herdr-tools find open` shows when no id is given (the Alt-p navigator).
pub(crate) const DEFAULT: &str = "panes";

/// Popup width in terminal columns for a picker that declares none.
const DEFAULT_WIDTH: u16 = 110;

/// One declared popup: immutable launch configuration read from the embedded
/// registry. `open` validates the id before asking Herdr for a popup, and the
/// popup process looks it up again by the id in its environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Picker {
    pub id: String,
    /// Manifest action that opens this picker (`shadowfax.tools.<action>`);
    /// keybindings target it, and it names the picker's popup key in `[popup]`.
    pub action: String,
    pub title: String,
    /// Outer popup width in terminal columns on a terminal wide enough for it,
    /// unless `[popup]` sizes this picker's key; `Host::open` narrows it to fit
    /// smaller ones.
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
    /// The declaring picker's id, so group errors can name each member.
    pub id: String,
    pub command: Vec<String>,
    /// Where the list comes from, as the heading, the loading line and command
    /// errors name it: the spec's `origin`, else the program's file name. A
    /// wrapper such as `sh -c` sets it so the popup names the real tool.
    pub origin: String,
    pub kind: Kind,
    pub enter: Vec<Deliver>,
    pub empty: Option<String>,
    /// Tab and Shift-Tab mark items; Enter then delivers every mark, in the
    /// order marked, as one space-separated text.
    pub multi: bool,
    /// The list's name in a grouped popup's heading strip and ctrl-o hint
    /// (`Dirs`, `Files`): the spec's `label`, else its title.
    pub label: String,
    /// Pickers sharing a group are one popup; see [`lists`].
    pub group: Option<String>,
}

/// What one line-picker popup shows: the opened picker's list alone, or one
/// list per picker in its group, in pickers.toml order. The popup process
/// builds it from the picker id Herdr launched it with, so `find-paths` and
/// `find-recent-files` open the same popup, each on its own list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Lists {
    /// The popup's heading: the group's first picker's title, so switching
    /// lists never retitles (or reflows) the heading.
    pub title: String,
    /// Members agree on kind, enter and multi (`parse` enforces it), because
    /// marks and Enter span every list; command, origin, empty and label differ.
    pub sources: Vec<LineSource>,
    /// Index of the opened picker's list, shown first.
    pub opened: usize,
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
    source: Option<String>,
    command: Option<Vec<String>>,
    origin: Option<String>,
    kind: Option<Kind>,
    enter: Option<Vec<Deliver>>,
    empty: Option<String>,
    multi: Option<bool>,
    width: Option<u16>,
    label: Option<String>,
    group: Option<String>,
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

/// The lists the line picker `id` opens: its own, or its whole group's.
pub(crate) fn lists(id: &str) -> Result<Lists> {
    lists_in(&all()?, id)
}

fn lists_in(pickers: &[Picker], id: &str) -> Result<Lists> {
    let line = |picker: &Picker| match &picker.source {
        Source::Lines(source) => Some(source.clone()),
        Source::Panes => None,
    };
    let opened = pickers
        .iter()
        .find(|picker| picker.id == id)
        .with_context(|| format!("unknown picker {id:?}; pickers are declared in pickers.toml"))?;
    let Some(source) = line(opened) else {
        bail!("picker {id:?} is not a line picker");
    };
    let Some(group) = source.group.clone() else {
        return Ok(Lists {
            title: opened.title.clone(),
            sources: vec![source],
            opened: 0,
        });
    };
    let members: Vec<&Picker> = pickers
        .iter()
        .filter(
            |picker| matches!(&picker.source, Source::Lines(s) if s.group.as_ref() == Some(&group)),
        )
        .collect();
    Ok(Lists {
        // `parse` guarantees at least two members, the opened one among them.
        title: members.first().map_or(&opened.title, |p| &p.title).clone(),
        opened: members.iter().position(|p| p.id == id).unwrap_or_default(),
        sources: members.iter().filter_map(|p| line(p)).collect(),
    })
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
    validate_groups(&pickers)?;
    Ok(pickers)
}

/// A group is one popup whose marks and Enter span every list, so its members
/// must deliver alike; a lone member is almost surely a misspelled group.
fn validate_groups(pickers: &[Picker]) -> Result<()> {
    let grouped: Vec<&LineSource> = pickers
        .iter()
        .filter_map(|picker| match &picker.source {
            Source::Lines(source) if source.group.is_some() => Some(source),
            _ => None,
        })
        .collect();
    for source in &grouped {
        let members: Vec<&&LineSource> = grouped
            .iter()
            .filter(|other| other.group == source.group)
            .collect();
        let group = source.group.as_deref().unwrap_or_default();
        if members.len() < 2 {
            bail!(
                "picker {:?}: group {group:?} has no other picker; a group joins two or more",
                source.id
            );
        }
        let first = members[0];
        if (first.kind, &first.enter, first.multi) != (source.kind, &source.enter, source.multi) {
            bail!(
                "pickers {:?} and {:?} in group {group:?} must agree on kind, enter and multi",
                first.id,
                source.id
            );
        }
    }
    Ok(())
}

fn validate(raw: Raw) -> Result<Picker> {
    let id = raw.id;
    // Every find action shares the plugin's namespace with the other modes.
    let action = raw.action.unwrap_or_else(|| format!("find-{id}"));
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
            if raw.origin.is_some()
                || raw.kind.is_some()
                || raw.enter.is_some()
                || raw.empty.is_some()
                || raw.multi.is_some()
                || raw.label.is_some()
                || raw.group.is_some()
            {
                bail!(
                    "picker {id:?}: the panes source takes no origin, kind, enter, empty, multi, label or group"
                );
            }
            Source::Panes
        }
        (Some(other), None) => bail!("picker {id:?}: unknown source {other:?}"),
        (None, Some(command)) => {
            let Some(program) = command.first().filter(|program| !program.is_empty()) else {
                bail!("picker {id:?}: command needs a program");
            };
            let origin = match raw.origin {
                Some(origin) if origin.trim().is_empty() => {
                    bail!("picker {id:?}: origin must name the list's source")
                }
                Some(origin) => origin,
                None => std::path::Path::new(program)
                    .file_name()
                    .map_or_else(|| program.clone(), |n| n.to_string_lossy().into_owned()),
            };
            let enter = raw.enter.unwrap_or_default();
            if enter.is_empty() {
                bail!("picker {id:?}: enter needs \"paste\", \"copy\" or both");
            }
            for (field, value) in [("label", &raw.label), ("group", &raw.group)] {
                if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
                    bail!("picker {id:?}: {field} must not be blank");
                }
            }
            Source::Lines(LineSource {
                id: id.clone(),
                command,
                origin,
                kind: raw.kind.unwrap_or_default(),
                enter,
                empty: raw.empty,
                multi: raw.multi.unwrap_or_default(),
                label: raw.label.unwrap_or_else(|| raw.title.clone()),
                group: raw.group,
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
        title: raw.title,
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
        assert_eq!(panes.action, "find");
        assert_eq!(panes.source, Source::Panes);
        // 20% wider than the paths popup, for the Name column.
        assert_eq!(panes.width, 132);
        let paths = find("paths").unwrap();
        assert_eq!(paths.action, "find-paths");
        assert_eq!(paths.width, 110);
        assert_eq!(
            paths.source,
            Source::Lines(LineSource {
                id: "paths".into(),
                command: vec!["zoxide".into(), "query".into(), "--list".into()],
                origin: "zoxide".into(),
                kind: Kind::Path,
                enter: vec![Deliver::Paste, Deliver::Copy],
                empty: Some(
                    "zoxide has no directories yet. cd around and they appear here.".into()
                ),
                multi: true,
                label: "Dirs".into(),
                group: Some("paths".into()),
            })
        );
        assert!(find("missing").is_err());
    }

    /// alt+n (`paths`) and `find-recent-files` open one popup: the same lists
    /// in the same order under one title, each action on its own list.
    #[test]
    fn paths_and_recent_files_are_one_popup_opened_on_either_list() {
        let dirs = lists("paths").unwrap();
        let files = lists("recent-files").unwrap();
        assert_eq!((dirs.title.as_str(), dirs.opened), ("Paths", 0));
        assert_eq!((files.title.as_str(), files.opened), ("Paths", 1));
        assert_eq!(dirs.sources, files.sources);
        let named: Vec<_> = dirs
            .sources
            .iter()
            .map(|s| (s.id.as_str(), s.label.as_str(), s.origin.as_str()))
            .collect();
        assert_eq!(
            named,
            [
                ("paths", "Dirs", "zoxide"),
                ("recent-files", "Files", "recent-files")
            ]
        );
        assert!(lists(DEFAULT).is_err(), "the navigator has no lists");
        assert!(lists("missing").is_err());
    }

    #[test]
    fn groups_join_in_file_order_and_ungrouped_pickers_stand_alone() {
        let line = |id: &str, extra: &str| {
            format!("[[picker]]\nid = \"{id}\"\ntitle = \"T {id}\"\ncommand = [\"ls\"]\nenter = [\"copy\"]\n{extra}\n")
        };
        let text = [
            line("a", "group = \"g\""),
            line("solo", ""),
            line("b", "group = \"g\"\nlabel = \"Bees\""),
        ]
        .concat();
        let pickers = parse(&text).unwrap();
        let grouped = lists_in(&pickers, "b").unwrap();
        assert_eq!((grouped.title.as_str(), grouped.opened), ("T a", 1));
        let labels: Vec<_> = grouped.sources.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["T a", "Bees"], "label defaults to the title");
        let solo = lists_in(&pickers, "solo").unwrap();
        assert_eq!((solo.title.as_str(), solo.sources.len()), ("T solo", 1));
    }

    #[test]
    fn groups_must_have_two_members_that_deliver_alike() {
        let line = |id: &str, extra: &str| {
            format!("[[picker]]\nid = \"{id}\"\ntitle = \"T\"\ncommand = [\"ls\"]\nenter = [\"copy\"]\ngroup = \"g\"\n{extra}\n")
        };
        for (text, needle) in [
            (line("a", ""), "no other picker"),
            (
                line("a", "") + &line("b", "kind = \"path\""),
                "must agree on kind, enter and multi",
            ),
            (line("a", "") + &line("b", "multi = true"), "must agree"),
            (
                line("a", "").replace("[\"copy\"]", "[\"paste\"]") + &line("b", ""),
                "must agree",
            ),
            (
                line("a", "label = \" \"") + &line("b", ""),
                "label must not be blank",
            ),
            (
                line("a", "").replace("group = \"g\"", "group = \"\"") + &line("b", ""),
                "group must not be blank",
            ),
        ] {
            let error = parse(&text).unwrap_err();
            assert!(format!("{error:#}").contains(needle), "{text}: {error:#}");
        }
    }

    /// The recent-files picker is a paths twin over an `sh -c` wrapper. Its
    /// script runs here through the real runner under `env -i`, with a stub
    /// CLI, so the owner's PATH and recent-files log never take part.
    #[test]
    fn recent_files_resolves_the_cli_and_lists_existing_files_newest_first() {
        use std::os::unix::fs::PermissionsExt;
        let picker = find("recent-files").unwrap();
        assert_eq!(picker.action, "find-recent-files");
        assert_eq!(picker.width, DEFAULT_WIDTH, "sized like paths");
        let Source::Lines(spec) = picker.source else {
            panic!("recent-files is a line picker")
        };
        assert_eq!(spec.origin, "recent-files");
        assert_eq!(
            (spec.kind, spec.enter.as_slice(), spec.multi),
            (Kind::Path, [Deliver::Paste, Deliver::Copy].as_slice(), true)
        );

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        let spaced = home.join("dir with space");
        std::fs::create_dir_all(&spaced).unwrap();
        // `a*.md` would glob to two files if the script let the shell expand it.
        for file in ["a.txt", "b.txt"] {
            std::fs::write(home.join(file), "").unwrap();
        }
        for file in ["a*.md", "ab.md"] {
            std::fs::write(spaced.join(file), "").unwrap();
        }
        let stub = |dir: &std::path::Path, body: &str| {
            std::fs::create_dir_all(dir).unwrap();
            let path = dir.join("recent-files");
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\n[ \"$*\" = 'list --days 90' ] || {{ echo \"args: $*\" >&2; exit 2; }}\n{body}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let h = home.display();
        stub(
            &home.join("go/bin"),
            &format!(
                "printf '%s\\n' '{h}/b.txt' '{h}/gone.txt' '{h}/dir with space/a*.md' '{h}/a.txt'"
            ),
        );
        let run = |home: &std::path::Path, path: &str| {
            let mut argv = vec![
                "env".to_owned(),
                "-i".into(),
                format!("HOME={}", home.display()),
                format!("PATH={path}"),
            ];
            argv.extend(spec.command.iter().cloned());
            crate::command::run(
                &argv,
                &spec.origin,
                &root,
                std::time::Duration::from_secs(5),
            )
        };

        // ~/go/bin is found without being on PATH; the deleted file is dropped
        // and the rest keep the CLI's newest-first order.
        let listed = run(&home, "/usr/bin:/bin").unwrap();
        assert_eq!(
            listed.lines,
            [
                format!("{h}/b.txt"),
                format!("{h}/dir with space/a*.md"),
                format!("{h}/a.txt")
            ]
        );

        // A recent-files on PATH wins over the fallback dirs.
        let on_path = root.join("bin");
        stub(&on_path, &format!("echo '{h}/a.txt'"));
        let path = format!("{}:/usr/bin:/bin", on_path.display());
        assert_eq!(run(&home, &path).unwrap().lines, [format!("{h}/a.txt")]);

        // Its failure is reported under the list's name, with its own stderr.
        stub(
            &on_path,
            "echo 'history.tsv: permission denied' >&2; exit 3",
        );
        assert_eq!(
            run(&home, &path).unwrap_err(),
            "recent-files failed (exit 3): history.tsv: permission denied"
        );

        // No CLI anywhere lists nothing, which shows the picker's `empty`
        // message. Only provable where the fixed system dirs have none either.
        let system = [
            "/opt/homebrew/bin/recent-files",
            "/usr/local/bin/recent-files",
        ];
        if !system.iter().any(|p| std::path::Path::new(p).exists()) {
            let nothing = run(&root.join("empty-home"), "/usr/bin:/bin").unwrap();
            assert!(nothing.lines.is_empty(), "{nothing:?}");
            assert!(spec
                .empty
                .as_deref()
                .is_some_and(|e| e.contains("recent-files CLI")));
        }
    }

    /// Herdr binds keys to manifest actions, so a picker without its action is
    /// unreachable and an `open` action without a picker fails at runtime.
    #[test]
    fn every_picker_has_its_manifest_action_and_back() {
        let manifest: DocumentMut = include_str!("../../../herdr-plugin.toml").parse().unwrap();
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
            let mut expected = vec!["./bin/herdr-tools".to_owned(), "find".into(), "open".into()];
            if picker.id != DEFAULT {
                expected.push(picker.id.clone());
            }
            assert_eq!(command(action), expected, "{}", picker.id);
        }
        for action in actions.iter() {
            let argv = command(action);
            if argv.get(1..3) == Some(&["find".to_owned(), "open".to_owned()]) {
                let id = argv.get(3).map_or(DEFAULT, String::as_str);
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
                "id = \"x\"\ntitle = \"X\"\nsource = \"panes\"\norigin = \"y\"",
                "takes no",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\nsource = \"panes\"\ngroup = \"g\"",
                "takes no",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\nsource = \"panes\"\nlabel = \"L\"",
                "takes no",
            ),
            (
                "id = \"x\"\ntitle = \"X\"\ncommand = [\"ls\"]\nenter = [\"copy\"]\norigin = \" \"",
                "origin must",
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
