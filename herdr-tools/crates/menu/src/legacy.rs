//! The old plugin's config, `shadowfax.menu/config.toml`, as a fragment of the
//! shared `config.toml` (`herdr-tools migrate-config`).
//!
//! The old file held the menu at its top level together with its popup size:
//! `width` (default 64) and `height` (default: sized to the items, capped by
//! `max_height`). `title`, `max_height` and `items` move to `[menu]` unchanged
//! apart from item commands, which can name the old plugins two ways:
//!
//! - old action ids (`plugin action invoke shadowfax.scratch.reap`) become their
//!   `shadowfax.tools.*` ids, through `tools_core::legacy`;
//! - old plugin binaries run by path (`exec '…/herdr-layouts/target/release/
//!   herdr-layouts' grid-dialog …`) become `"$HERDR_PLUGIN_ROOT/bin/herdr-tools"
//!   layout grid-dialog …`. Herdr sets `HERDR_PLUGIN_ROOT` for every plugin pane
//!   and shell items inherit the menu popup's environment, so the command follows
//!   the installed plugin rather than a checkout that the cutover retires.
//!
//! The size moves to `[popup.size].menu`, whose entries always fix both
//! dimensions:
//!
//! | old `width` / `height` | new `[popup.size].menu`                            |
//! |------------------------|----------------------------------------------------|
//! | both                   | `{ width, height }`                                |
//! | `height` only          | `{ width = 64, height }`; 64 was the default width |
//! | `width` only           | `{ width, height = <today's automatic height> }`   |
//! | neither                | none: an unconfigured menu is 64 wide, auto height |
//!
//! Only "`width` only" is inexact: its height is computed once from the items
//! being migrated and no longer follows later edits to them.

use std::borrow::Cow;
use std::path::Path;

use anyhow::{Context, Result};
use toml::{Table, Value};
use tools_core::legacy::rewrite_action_ids;

use crate::config::{optional_dimension_value, Config, DEFAULT_WIDTH};

/// The old plugin's config directory under the legacy root.
const LEGACY_DIR: &str = "shadowfax.menu";

/// Top-level fields of the old file that are the `[menu]` table now.
const MENU_FIELDS: &[&str] = &["title", "max_height", "items"];

/// Each old plugin binary and the `herdr-tools` mode that replaced it.
const OLD_BINARIES: &[(&str, &str)] = &[
    ("herdr-jump", "find"),
    ("herdr-ferry", "move"),
    ("herdr-talon", "history"),
    ("herdr-menu", "menu"),
    ("herdr-comments", "comment"),
    ("herdr-layouts", "layout"),
    ("herdr-scratch", "scratch"),
];

/// The merged binary as a migrated shell item names it (see the module docs).
const HERDR_TOOLS: &str = r#""$HERDR_PLUGIN_ROOT/bin/herdr-tools""#;

pub(crate) fn migrate(legacy_root: &Path) -> Result<Table> {
    let path = legacy_root.join(LEGACY_DIR).join("config.toml");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Table::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    let old: Table =
        toml::from_str(&source).with_context(|| format!("failed to parse {}", path.display()))?;

    let mut menu = Table::new();
    for field in MENU_FIELDS {
        if let Some(value) = old.get(*field) {
            menu.insert((*field).to_owned(), value.clone());
        }
    }
    if let Some(Value::Array(items)) = menu.get_mut("items") {
        for item in items {
            if let Some(Value::String(command)) = item.get_mut("command") {
                if let Cow::Owned(rewritten) = rewrite_action_ids(command) {
                    *command = rewritten;
                }
                if let Cow::Owned(rewritten) = rewrite_old_binaries(command) {
                    *command = rewritten;
                }
            }
        }
    }

    // The old plugin rejected the whole file over a bad size, so do the same
    // here rather than silently dropping or inventing one.
    let invalid = || format!("invalid popup size in {}", path.display());
    let width = optional_dimension_value(&old, "width", 20).with_context(invalid)?;
    let height = optional_dimension_value(&old, "height", 6).with_context(invalid)?;
    let size = match (width, height) {
        (Some(width), Some(height)) => Some((width, height)),
        (None, Some(height)) => Some((DEFAULT_WIDTH, height)),
        // A broken menu kept the old launcher's fallback size, which an
        // unconfigured broken `[menu]` still gets; only a valid one is measured.
        (Some(width), None) => Config::from_table(&menu)
            .ok()
            .map(|config| (width, config.popup_height())),
        (None, None) => None,
    };

    // An existing old file always yields `[menu]`, even an empty one: without
    // it the new menu would fall back to the built-in items instead.
    let mut fragment = Table::new();
    fragment.insert(crate::MODE.to_owned(), Value::Table(menu));
    if let Some((width, height)) = size {
        let dims = Table::from_iter([
            ("width".to_owned(), Value::Integer(width.into())),
            ("height".to_owned(), Value::Integer(height.into())),
        ]);
        let sizes = Table::from_iter([(crate::POPUP.to_owned(), Value::Table(dims))]);
        let popup = Table::from_iter([("size".to_owned(), Value::Table(sizes))]);
        fragment.insert("popup".to_owned(), Value::Table(popup));
    }
    Ok(fragment)
}

/// Replaces every old plugin binary run by path in a shell command with
/// [`HERDR_TOOLS`] and its mode, keeping the arguments that follow.
///
/// A match is one shell word, bare or wholly inside `'…'` / `"…"`, that ends
/// in `herdr-<old>/target/{release,debug}/herdr-<old>`: the old plugin's own
/// cargo output, the only way its binary was reachable outside Herdr. Anything
/// else, such as the same name inside a longer word, is left alone.
fn rewrite_old_binaries(command: &str) -> Cow<'_, str> {
    let boundary = |ch: char| ch.is_whitespace() || matches!(ch, ';' | '&' | '|' | '(' | ')');
    let mut output = String::new();
    let mut copied = 0;
    let mut search = 0;
    while let Some(offset) = command[search..].find("/target/") {
        let marker = search + offset;
        search = marker + 1;
        let after_target = &command[marker + "/target/".len()..];
        let Some(profile) = ["release/", "debug/"]
            .into_iter()
            .find(|profile| after_target.starts_with(profile))
        else {
            continue;
        };
        let name_start = marker + "/target/".len() + profile.len();
        let name_end = command[name_start..]
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')))
            .map_or(command.len(), |length| name_start + length);
        let name = &command[name_start..name_end];
        let Some((_, mode)) = OLD_BINARIES.iter().find(|(old, _)| *old == name) else {
            continue;
        };
        let quote = command[name_end..]
            .chars()
            .next()
            .filter(|ch| matches!(ch, '\'' | '"'));
        let (start, end) = match quote {
            Some(quote) => match command[..marker].rfind(quote) {
                Some(open) => (open, name_end + 1),
                None => continue,
            },
            None if command[name_end..].starts_with(|ch: char| !boundary(ch)) => continue,
            None => (
                command[..marker].rfind(boundary).map_or(0, |at| at + 1),
                name_end,
            ),
        };
        let word = &command[start..end];
        let path = quote.map_or(word, |_| &word[1..word.len() - 1]);
        let suffix = format!("{name}/target/{profile}{name}");
        if path != suffix && !path.ends_with(&format!("/{suffix}")) {
            continue;
        }
        output.push_str(&command[copied..start]);
        output.push_str(HERDR_TOOLS);
        output.push(' ');
        output.push_str(mode);
        copied = end;
        search = end;
    }
    if copied == 0 {
        return Cow::Borrowed(command);
    }
    output.push_str(&command[copied..]);
    Cow::Owned(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MenuItem;
    use tools_core::{PopupDims, PopupSize};

    /// A copy of the owner's live `shadowfax.menu/config.toml`.
    const OWNER_CONFIG: &str = include_str!("../tests/fixtures/legacy/shadowfax.menu/config.toml");

    fn legacy(source: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(LEGACY_DIR)).unwrap();
        std::fs::write(root.path().join(LEGACY_DIR).join("config.toml"), source).unwrap();
        root
    }

    /// Renders the fragment and reads it back the way every mode will.
    fn reload(fragment: &Table) -> tools_core::Config {
        tools_core::Config::parse(&toml::to_string_pretty(fragment).unwrap()).unwrap()
    }

    fn cells(width: u16, height: u16) -> Option<PopupDims> {
        Some(PopupDims {
            width: PopupSize::Cells(width),
            height: PopupSize::Cells(height),
        })
    }

    #[test]
    fn no_old_config_is_an_empty_fragment() {
        let root = tempfile::tempdir().unwrap();
        assert!(migrate(root.path()).unwrap().is_empty());
    }

    #[test]
    fn owner_config_keeps_its_menu_size_and_rewrites_the_scratch_action() {
        let root = legacy(OWNER_CONFIG);
        let fragment = migrate(root.path()).unwrap();
        let config = reload(&fragment);

        assert_eq!(
            config.popup("menu", None).map(|popup| popup.dims),
            cells(80, 32)
        );
        let menu = Config::from_core(&config).unwrap();
        assert_eq!(menu.title, "commands");
        assert_eq!(menu.max_height, 80);
        assert!(menu.warnings.is_empty(), "{:?}", menu.warnings);

        // Same items in the same order as the old file, with one command rewritten.
        let old = Config::parse(OWNER_CONFIG).unwrap();
        assert_eq!(menu.items.len(), old.items.len());
        assert_eq!(menu.items.len(), 16);
        let reap = |config: &Config| {
            config
                .shell_items()
                .find(|item| item.key == 'x')
                .unwrap()
                .clone()
        };
        assert_eq!(
            reap(&old).command,
            r#""$HERDR_BIN_PATH" plugin action invoke shadowfax.scratch.reap"#
        );
        assert_eq!(
            reap(&menu).command,
            r#""$HERDR_BIN_PATH" plugin action invoke shadowfax.tools.scratch-reap"#
        );
        let layout = |config: &Config| {
            config
                .shell_items()
                .find(|item| item.key == 'g')
                .unwrap()
                .command
                .clone()
        };
        assert_eq!(
            layout(&old),
            r#"exec '/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-layouts/target/release/herdr-layouts' grid-dialog --pane "$HERDR_MENU_SOURCE_PANE_ID""#
        );
        assert_eq!(
            layout(&menu),
            r#"exec "$HERDR_PLUGIN_ROOT/bin/herdr-tools" layout grid-dialog --pane "$HERDR_MENU_SOURCE_PANE_ID""#
        );
        for (new, old) in menu.items.iter().zip(&old.items) {
            if !matches!(new, MenuItem::Shell(item) if matches!(item.key, 'x' | 'g')) {
                assert_eq!(new, old);
            }
        }
    }

    #[test]
    fn old_plugin_binaries_become_herdr_tools_modes() {
        // The owner's "create layout…" item, verbatim.
        assert_eq!(
            rewrite_old_binaries(
                r#"exec '/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-layouts/target/release/herdr-layouts' grid-dialog --pane "$HERDR_MENU_SOURCE_PANE_ID""#
            ),
            r#"exec "$HERDR_PLUGIN_ROOT/bin/herdr-tools" layout grid-dialog --pane "$HERDR_MENU_SOURCE_PANE_ID""#
        );
        for (old, new) in [
            (
                "/src/herdr-jump/target/release/herdr-jump open paths",
                r#""$HERDR_PLUGIN_ROOT/bin/herdr-tools" find open paths"#,
            ),
            (
                r#"cd /x && "/my dir/herdr-ferry/target/debug/herdr-ferry" open; echo ok"#,
                r#"cd /x && "$HERDR_PLUGIN_ROOT/bin/herdr-tools" move open; echo ok"#,
            ),
            (
                "herdr-scratch/target/release/herdr-scratch reap|cat",
                r#""$HERDR_PLUGIN_ROOT/bin/herdr-tools" scratch reap|cat"#,
            ),
            (
                "a/herdr-talon/target/release/herdr-talon x; b/herdr-comments/target/release/herdr-comments y; c/herdr-menu/target/release/herdr-menu z",
                r#""$HERDR_PLUGIN_ROOT/bin/herdr-tools" history x; "$HERDR_PLUGIN_ROOT/bin/herdr-tools" comment y; "$HERDR_PLUGIN_ROOT/bin/herdr-tools" menu z"#,
            ),
        ] {
            assert_eq!(rewrite_old_binaries(old), new);
        }
        for untouched in [
            "mac-notify clear",
            // Another crate's output, a renamed binary, a longer word, a non-cargo path.
            "/src/herdr-jump/target/release/herdr-ferry open",
            "/src/herdr-jump/target/release/herdr-jumper open",
            "/src/herdr-jump/target/release/herdr-jump.sh",
            "/src/herdr-jump/target/profiling/herdr-jump open",
            "/opt/bin/herdr-jump open",
        ] {
            assert!(
                matches!(rewrite_old_binaries(untouched), Cow::Borrowed(_)),
                "{untouched}"
            );
        }
    }

    #[test]
    fn partial_and_absent_sizes_map_onto_full_popup_entries() {
        let items = "[[items]]\nkey='a'\nlabel='a'\ncommand='a'\n[[items]]\nkey='b'\nlabel='b'\ncommand='b'\n";
        let size = |source: &str| {
            let root = legacy(source);
            reload(&migrate(root.path()).unwrap())
                .popup("menu", None)
                .map(|popup| popup.dims)
        };
        assert_eq!(size(&format!("height = 20\n{items}")), cells(64, 20));
        // Two items + frame = 6, the automatic height at migration time.
        assert_eq!(size(&format!("width = 90\n{items}")), cells(90, 6));
        assert_eq!(size(items), None);
    }

    #[test]
    fn an_old_file_without_items_stays_a_user_menu() {
        let root = legacy("title = \"mine\"\n");
        let fragment = migrate(root.path()).unwrap();
        let menu = Config::from_core(&reload(&fragment)).unwrap();
        assert_eq!(menu.title, "mine");
        assert!(menu.items.is_empty());
        assert!(!menu.used_default);

        let root = legacy("");
        let text = toml::to_string_pretty(&migrate(root.path()).unwrap()).unwrap();
        assert!(text.contains("[menu]"), "{text}");
        assert!(
            !Config::from_core(&tools_core::Config::parse(&text).unwrap())
                .unwrap()
                .used_default
        );
    }

    #[test]
    fn invalid_old_files_are_refused_with_their_path() {
        for source in ["width = 5\n", "height = \"tall\"\n", "[[items]\n"] {
            let root = legacy(source);
            let error = format!("{:#}", migrate(root.path()).unwrap_err());
            assert!(error.contains("shadowfax.menu"), "{error}");
        }
    }
}
