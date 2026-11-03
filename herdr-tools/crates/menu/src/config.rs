//! The `[menu]` table of the shared `config.toml`: title, items and the cap on
//! automatic height.
//!
//! This was the whole of the old `shadowfax.menu/config.toml`; the same fields
//! now sit under `[menu]`, read field by field so one bad item is a warning, not
//! a broken menu. The popup size moved out: `[popup.size].menu` (or a
//! `[[popup.profile]]`) fixes both dimensions, and without one the launcher uses
//! [`Config::width`] and [`Config::popup_height`], exactly as the old
//! unconfigured `width`/`height` did.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use toml::value::Table;
use toml::Value;

/// The built-in `[menu]` table, used when `config.toml` has none.
pub const DEFAULT_CONFIG: &str = include_str!("../config.default.toml");

/// Popup width for a `[menu]` table when `[popup.size].menu` is unset: the old
/// default for a config file without `width`.
pub const DEFAULT_WIDTH: u16 = 64;

/// Popup width for the built-in menu: the old packaged default's `width = 68`.
pub const BUILTIN_WIDTH: u16 = 68;

/// One menu's parsed `[menu]` table. Invalid items are skipped with a warning;
/// only a structurally broken table (a bad title, items not an array) fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub title: String,
    /// The fallback popup width: [`BUILTIN_WIDTH`] for the built-in menu, else
    /// [`DEFAULT_WIDTH`]. `[popup.size].menu` overrides it.
    pub width: u16,
    pub max_height: u16,
    pub items: Vec<MenuItem>,
    pub warnings: Vec<String>,
    pub used_default: bool,
}

/// Typed action boundary between user TOML, menu navigation, and executors.
///
/// Native fork and layout actions stay structured, distinct from arbitrary
/// shell commands, so navigation cannot accidentally bypass their safety UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuItem {
    Shell(ShellItem),
    Fork(ForkItem),
    RotateFork(ActionItem),
    Redistribute(ActionItem),
    CopyContext(ActionItem),
    /// `type = "focus"` or `"hide"`: open Herdr's native scope picker in that
    /// mode. The retired `workspace_visibility` row parses as `hide`.
    ScopePick(ScopePickItem),
    /// `type = "focus_toggle"`: switch focus on or off, keeping its targets.
    FocusToggle(ActionItem),
    Separator,
}

/// Key and label shared by actions that require no additional parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionItem {
    pub key: char,
    pub label: String,
}

/// One native-conversation fork action and its requested pane placement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkItem {
    pub key: char,
    pub label: String,
    pub placement: ForkPlacement,
}

/// A row that opens Herdr's native focus/hide picker. The picker is the
/// fork's own overlay (it sees every machine; a plugin sees only Local), so the
/// menu only asks Herdr to open it, with `scope.pick {mode}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopePickItem {
    pub key: char,
    pub label: String,
    pub mode: ScopePickMode,
}

/// Which mark the picker's `space` toggles (`scope.pick`'s `mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopePickMode {
    Focus,
    Hide,
}

impl ScopePickMode {
    /// The wire and CLI spelling: `focus` or `hide`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Focus => "focus",
            Self::Hide => "hide",
        }
    }
}

impl std::str::FromStr for ScopePickMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "focus" => Ok(Self::Focus),
            "hide" => Ok(Self::Hide),
            other => bail!("scope picker mode must be 'focus' or 'hide', got '{other}'"),
        }
    }
}

/// Placements the plugin can synthesize from Herdr's right/down split API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkPlacement {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellItem {
    pub key: char,
    pub label: String,
    pub command: String,
    pub mode: ExecutionMode,
    pub confirm: Option<String>,
    pub cwd: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    #[default]
    Foreground,
    Detached,
}

impl Config {
    /// Parses the body of a `[menu]` table written as its own TOML document
    /// (`title = …`, `[[items]]`), the shape of [`DEFAULT_CONFIG`] and the old file.
    pub fn parse(source: &str) -> Result<Self> {
        let document: Value = toml::from_str(source).context("failed to parse config TOML")?;
        let table = document
            .as_table()
            .context("config root must be a TOML table")?;
        Self::from_table(table)
    }

    /// Parses a `[menu]` table.
    pub fn from_table(table: &Table) -> Result<Self> {
        parse_table(table, DEFAULT_WIDTH, false)
    }

    /// The built-in menu ([`DEFAULT_CONFIG`]), [`BUILTIN_WIDTH`] cells wide.
    pub fn builtin() -> Result<Self> {
        let document: Table = toml::from_str(DEFAULT_CONFIG).context("built-in menu config")?;
        parse_table(&document, BUILTIN_WIDTH, true)
    }

    /// The menu from the shared config: its `[menu]` table, else the built-in
    /// menu with a warning saying so (the old plugin warned about its missing
    /// file the same way, and the warning counts toward the automatic height).
    pub fn from_core(config: &tools_core::Config) -> Result<Self> {
        let location = config
            .path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "config.toml".into());
        match config.section_table(crate::MODE)? {
            Some(table) => {
                Self::from_table(table).with_context(|| format!("invalid [menu] in {location}"))
            }
            None => {
                let mut menu = Self::builtin()?;
                let exists = config.path().is_some_and(std::path::Path::exists);
                menu.warnings.insert(
                    0,
                    if exists {
                        format!("{location} has no [menu] table; using built-in defaults")
                    } else {
                        format!("{location} not found; using built-in defaults")
                    },
                );
                Ok(menu)
            }
        }
    }

    /// The automatic popup height: every item, two rows for config warnings and
    /// four for the frame and status, clamped to `6..=max_height`.
    pub fn popup_height(&self) -> u16 {
        let warning_rows = if self.warnings.is_empty() { 0 } else { 2 };
        let item_rows = u16::try_from(self.items.len()).unwrap_or(u16::MAX);
        item_rows
            .saturating_add(warning_rows)
            .saturating_add(4)
            .clamp(6, self.max_height)
    }

    pub fn shell_items(&self) -> impl Iterator<Item = &ShellItem> {
        self.items.iter().filter_map(|item| match item {
            MenuItem::Shell(item) => Some(item),
            MenuItem::Fork(_)
            | MenuItem::RotateFork(_)
            | MenuItem::Redistribute(_)
            | MenuItem::CopyContext(_)
            | MenuItem::ScopePick(_)
            | MenuItem::FocusToggle(_)
            | MenuItem::Separator => None,
        })
    }

    /// Whether any row needs Herdr's scope methods, so the menu reads
    /// `scope.get` before drawing only when one does.
    pub fn has_scope_items(&self) -> bool {
        self.items.iter().any(MenuItem::needs_scope)
    }
}

impl MenuItem {
    pub fn key(&self) -> Option<char> {
        match self {
            Self::Shell(item) => Some(item.key),
            Self::Fork(item) => Some(item.key),
            Self::ScopePick(item) => Some(item.key),
            Self::RotateFork(item)
            | Self::Redistribute(item)
            | Self::CopyContext(item)
            | Self::FocusToggle(item) => Some(item.key),
            Self::Separator => None,
        }
    }

    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Shell(item) => Some(&item.label),
            Self::Fork(item) => Some(&item.label),
            Self::ScopePick(item) => Some(&item.label),
            Self::RotateFork(item)
            | Self::Redistribute(item)
            | Self::CopyContext(item)
            | Self::FocusToggle(item) => Some(&item.label),
            Self::Separator => None,
        }
    }

    pub fn confirmation(&self) -> Option<&str> {
        match self {
            Self::Shell(item) => item.confirm.as_deref(),
            Self::Fork(_)
            | Self::RotateFork(_)
            | Self::Redistribute(_)
            | Self::CopyContext(_)
            | Self::ScopePick(_)
            | Self::FocusToggle(_)
            | Self::Separator => None,
        }
    }

    pub fn is_selectable(&self) -> bool {
        !matches!(self, Self::Separator)
    }

    /// Rows that only work on a Herdr with `scope.get` / `scope.set` /
    /// `scope.pick`; on an older Herdr they read "(unavailable)".
    pub fn needs_scope(&self) -> bool {
        matches!(self, Self::ScopePick(_) | Self::FocusToggle(_))
    }
}

fn parse_table(table: &Table, width: u16, used_default: bool) -> Result<Config> {
    let title = optional_string(table, "title")?
        .unwrap_or("commands")
        .trim()
        .to_string();
    if title.is_empty() {
        bail!("title cannot be empty");
    }
    let max_height = optional_dimension(table, "max_height", 24, 6)?;
    let values: &[Value] = match table.get("items") {
        Some(value) => value
            .as_array()
            .context("items must be an array of tables")?,
        None => &[],
    };

    let mut items = Vec::with_capacity(values.len());
    let mut warnings = Vec::new();
    // The old file's size fields would otherwise be ignored without a trace.
    for field in ["width", "height"] {
        if table.contains_key(field) {
            warnings.push(format!(
                "{field} is ignored here; set the menu size in [popup.size].menu"
            ));
        }
    }
    let mut accelerators = HashSet::new();
    for (index, value) in values.iter().enumerate() {
        match parse_item(value) {
            Ok(item) => {
                if let Some(key) = item.key() {
                    if !accelerators.insert(key) {
                        warnings.push(format!(
                            "item {}: duplicate accelerator '{}'",
                            index + 1,
                            key
                        ));
                        continue;
                    }
                }
                items.push(item);
            }
            Err(error) => warnings.push(format!("item {}: {error}", index + 1)),
        }
    }

    Ok(Config {
        title,
        width,
        max_height,
        items,
        warnings,
        used_default,
    })
}

fn parse_item(value: &Value) -> Result<MenuItem> {
    let table = value.as_table().context("entry must be a table")?;
    let item_type = optional_string(table, "type")?.unwrap_or("shell");
    match item_type {
        "separator" => {
            reject_unknown_fields(table, &["type"])?;
            Ok(MenuItem::Separator)
        }
        "shell" => {
            reject_unknown_fields(
                table,
                &["type", "key", "label", "command", "mode", "confirm", "cwd"],
            )?;
            let (key, label) = action_identity(table)?;
            let command = nonempty_string(table, "command")?;
            let mode = match optional_string(table, "mode")?.unwrap_or("foreground") {
                "foreground" => ExecutionMode::Foreground,
                "detached" => ExecutionMode::Detached,
                value => bail!("mode must be 'foreground' or 'detached', got '{value}'"),
            };
            let confirm = optional_nonempty_string(table, "confirm")?;
            let cwd = optional_nonempty_string(table, "cwd")?.map(PathBuf::from);
            if cwd
                .as_ref()
                .is_some_and(|path| path.as_os_str().to_string_lossy().contains('\0'))
            {
                bail!("cwd cannot contain a null byte");
            }
            Ok(MenuItem::Shell(ShellItem {
                key,
                label,
                command,
                mode,
                confirm,
                cwd,
            }))
        }
        "fork" => {
            reject_unknown_fields(table, &["type", "key", "label", "placement"])?;
            let (key, label) = action_identity(table)?;
            let placement = match required_string(table, "placement")? {
                "left" => ForkPlacement::Left,
                "right" => ForkPlacement::Right,
                "top" => ForkPlacement::Top,
                "bottom" => ForkPlacement::Bottom,
                value => {
                    bail!("placement must be 'left', 'right', 'top', or 'bottom', got '{value}'")
                }
            };
            Ok(MenuItem::Fork(ForkItem {
                key,
                label,
                placement,
            }))
        }
        "rotate_fork" => {
            reject_unknown_fields(table, &["type", "key", "label"])?;
            let (key, label) = action_identity(table)?;
            Ok(MenuItem::RotateFork(ActionItem { key, label }))
        }
        "redistribute" => {
            reject_unknown_fields(table, &["type", "key", "label"])?;
            let (key, label) = action_identity(table)?;
            Ok(MenuItem::Redistribute(ActionItem { key, label }))
        }
        "copy_context" => {
            reject_unknown_fields(table, &["type", "key", "label"])?;
            let (key, label) = action_identity(table)?;
            Ok(MenuItem::CopyContext(ActionItem { key, label }))
        }
        // The retired `v` row toggled the Agents plugin's excluded workspaces.
        // Herdr's native hide replaced that list, so existing menus keep the
        // row and it now opens the hide picker.
        mode @ ("focus" | "hide" | "workspace_visibility") => {
            reject_unknown_fields(table, &["type", "key", "label"])?;
            let (key, label) = action_identity(table)?;
            let mode = if mode == "focus" {
                ScopePickMode::Focus
            } else {
                ScopePickMode::Hide
            };
            Ok(MenuItem::ScopePick(ScopePickItem { key, label, mode }))
        }
        "focus_toggle" => {
            reject_unknown_fields(table, &["type", "key", "label"])?;
            let (key, label) = action_identity(table)?;
            Ok(MenuItem::FocusToggle(ActionItem { key, label }))
        }
        value => bail!("unsupported type '{value}'"),
    }
}

fn action_identity(table: &Table) -> Result<(char, String)> {
    let key = required_string(table, "key")?;
    let mut characters = key.chars();
    let key = characters.next().context("key cannot be empty")?;
    if characters.next().is_some() || !key.is_ascii_graphic() {
        bail!("key must be one printable ASCII character");
    }
    Ok((key, nonempty_string(table, "label")?))
}

fn reject_unknown_fields(table: &Table, allowed: &[&str]) -> Result<()> {
    if let Some(field) = table
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        bail!("unknown field '{field}'");
    }
    Ok(())
}

fn optional_dimension(table: &Table, field: &str, default: u16, minimum: u16) -> Result<u16> {
    Ok(optional_dimension_value(table, field, minimum)?.unwrap_or(default))
}

pub(crate) fn optional_dimension_value(
    table: &Table,
    field: &str,
    minimum: u16,
) -> Result<Option<u16>> {
    let Some(value) = table.get(field) else {
        return Ok(None);
    };
    let value = value
        .as_integer()
        .with_context(|| format!("{field} must be an integer"))?;
    let value = u16::try_from(value).with_context(|| format!("{field} is out of range"))?;
    if value < minimum {
        bail!("{field} must be at least {minimum}");
    }
    Ok(Some(value))
}

fn required_string<'a>(table: &'a Table, field: &str) -> Result<&'a str> {
    table
        .get(field)
        .with_context(|| format!("missing {field}"))?
        .as_str()
        .with_context(|| format!("{field} must be a string"))
}

fn optional_string<'a>(table: &'a Table, field: &str) -> Result<Option<&'a str>> {
    table
        .get(field)
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("{field} must be a string"))
        })
        .transpose()
}

fn nonempty_string(table: &Table, field: &str) -> Result<String> {
    let value = required_string(table, field)?.trim();
    if value.is_empty() {
        bail!("{field} cannot be empty");
    }
    Ok(value.to_string())
}

fn optional_nonempty_string(table: &Table, field: &str) -> Result<Option<String>> {
    let Some(value) = optional_string(table, field)? else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        bail!("{field} cannot be empty");
    }
    Ok(Some(value.to_string()))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn packaged_default_has_agent_fork_actions() {
        let config = Config::builtin().unwrap();

        assert_eq!(config.title, "agent forks");
        assert_eq!(config.width, 68);
        assert!(config.used_default);
        assert_eq!(config.max_height, 24);
        assert_eq!(config.items.len(), 13);
        assert!(matches!(
            config.items[0],
            MenuItem::Fork(ForkItem {
                placement: ForkPlacement::Left,
                ..
            })
        ));
        assert!(matches!(
            config.items[1],
            MenuItem::Fork(ForkItem {
                placement: ForkPlacement::Right,
                ..
            })
        ));
        assert!(matches!(
            config.items[2],
            MenuItem::Fork(ForkItem {
                placement: ForkPlacement::Top,
                ..
            })
        ));
        assert!(matches!(
            config.items[3],
            MenuItem::Fork(ForkItem {
                placement: ForkPlacement::Bottom,
                ..
            })
        ));
        assert!(matches!(config.items[4], MenuItem::Separator));
        assert!(matches!(config.items[5], MenuItem::RotateFork(_)));
        assert!(matches!(config.items[7], MenuItem::Redistribute(_)));
        assert!(matches!(config.items[8], MenuItem::CopyContext(_)));
        assert!(matches!(config.items[9], MenuItem::Separator));
        assert!(matches!(
            &config.items[10],
            MenuItem::ScopePick(ScopePickItem {
                key: 'f',
                mode: ScopePickMode::Focus,
                ..
            })
        ));
        assert!(matches!(
            &config.items[11],
            MenuItem::ScopePick(ScopePickItem {
                key: 'v',
                mode: ScopePickMode::Hide,
                ..
            })
        ));
        assert!(matches!(
            &config.items[12],
            MenuItem::FocusToggle(ActionItem { key: 'F', .. })
        ));
        assert!(config.has_scope_items());
        assert_eq!(config.shell_items().count(), 0);
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn every_shell_field_is_decoded() {
        let config = Config::parse(
            r#"
title = "tools"
max_height = 18

[[items]]
type = "shell"
key = "R"
label = "run report"
command = "report --all"
mode = "detached"
confirm = "Run it?"
cwd = "reports/daily"
"#,
        )
        .unwrap();

        assert_eq!(config.title, "tools");
        assert_eq!(config.width, DEFAULT_WIDTH);
        assert_eq!(config.max_height, 18);
        assert!(config.warnings.is_empty());
        assert_eq!(
            config.shell_items().next(),
            Some(&ShellItem {
                key: 'R',
                label: "run report".into(),
                command: "report --all".into(),
                mode: ExecutionMode::Detached,
                confirm: Some("Run it?".into()),
                cwd: Some(PathBuf::from("reports/daily")),
            })
        );
    }

    #[test]
    fn fork_and_rotate_items_are_typed_menu_actions() {
        let config = Config::parse(
            r#"
[[items]]
type = "fork"
key = "l"
label = "fork left"
placement = "left"

[[items]]
type = "rotate_fork"
key = "o"
label = "rotate fork"
"#,
        )
        .unwrap();

        assert!(matches!(
            &config.items[0],
            MenuItem::Fork(ForkItem {
                key: 'l',
                placement: ForkPlacement::Left,
                ..
            })
        ));
        assert!(matches!(
            &config.items[1],
            MenuItem::RotateFork(ActionItem { key: 'o', .. })
        ));
    }

    #[test]
    fn scope_items_parse_and_the_retired_visibility_row_is_hide() {
        let config = Config::parse(
            r#"
[[items]]
type = "focus"
key = "f"
label = "focus…"

[[items]]
type = "hide"
key = "x"
label = "hide…"

[[items]]
type = "focus_toggle"
key = "F"
label = "focus mode"

[[items]]
type = "workspace_visibility"
key = "v"
label = "excluded workspaces"

[[items]]
type = "focus"
key = "g"
label = "focus"
mode = "hide"
"#,
        )
        .unwrap();

        let pick = |key, label: &str, mode| {
            MenuItem::ScopePick(ScopePickItem {
                key,
                label: label.into(),
                mode,
            })
        };
        assert_eq!(
            config.items,
            [
                pick('f', "focus…", ScopePickMode::Focus),
                pick('x', "hide…", ScopePickMode::Hide),
                MenuItem::FocusToggle(ActionItem {
                    key: 'F',
                    label: "focus mode".into()
                }),
                pick('v', "excluded workspaces", ScopePickMode::Hide),
            ]
        );
        // The mode is the type; an extra field is a skipped item, not a mode.
        assert_eq!(config.warnings.len(), 1);
        assert!(config.warnings[0].contains("unknown field 'mode'"));
        assert!(config.has_scope_items());
        assert!(
            !Config::parse("[[items]]\nkey='a'\nlabel='a'\ncommand='a'\n")
                .unwrap()
                .has_scope_items()
        );
    }

    #[test]
    fn invalid_items_are_skipped_and_valid_order_is_preserved() {
        let config = Config::parse(
            r#"
[[items]]
key = "c"
label = "first"
command = "one"

[[items]]
key = "b"
label = ""
command = "broken"

[[items]]
type = "herdr"
key = "h"
label = "future"
action = "settings"

[[items]]
key = "c"
label = "duplicate"
command = "two"

[[items]]
type = "separator"

[[items]]
key = "R"
label = "last"
command = "three"
"#,
        )
        .unwrap();

        assert_eq!(
            config
                .shell_items()
                .map(|item| item.key)
                .collect::<Vec<_>>(),
            vec!['c', 'R']
        );
        assert!(matches!(config.items[1], MenuItem::Separator));
        assert_eq!(config.warnings.len(), 3);
        assert!(config.warnings[0].contains("item 2"));
        assert!(config.warnings[1].contains("item 3"));
        assert!(config.warnings[2].contains("item 4"));
    }

    #[test]
    fn accelerators_are_case_sensitive() {
        let config = Config::parse(
            r#"
[[items]]
key = "r"
label = "lower"
command = "lower"

[[items]]
key = "R"
label = "upper"
command = "upper"
"#,
        )
        .unwrap();

        assert_eq!(
            config
                .shell_items()
                .map(|item| item.key)
                .collect::<Vec<_>>(),
            vec!['r', 'R']
        );
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn malformed_toml_is_fatal() {
        let error = Config::parse("[[items]\n").unwrap_err().to_string();

        assert!(error.contains("failed to parse config"));
    }

    #[test]
    fn missing_file_uses_packaged_default_with_warning() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = Config::from_core(&tools_core::Config::load(&path).unwrap()).unwrap();

        assert!(config.used_default);
        assert_eq!(config.width, BUILTIN_WIDTH);
        assert_eq!(config.items.len(), 13);
        assert_eq!(config.warnings.len(), 1);
        assert!(config.warnings[0].contains("not found; using built-in defaults"));
        // Thirteen items, two warning rows and the frame.
        assert_eq!(config.popup_height(), 19);
    }

    #[test]
    fn shared_config_without_a_menu_table_uses_the_built_in_menu() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[popup.size]\nfind = { width = 100, height = 20 }\n").unwrap();

        let config = Config::from_core(&tools_core::Config::load(&path).unwrap()).unwrap();

        assert!(config.used_default);
        assert!(config.warnings[0].contains("has no [menu] table"));
    }

    #[test]
    fn shared_config_menu_table_is_the_menu_and_its_errors_name_the_table() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[menu]\ntitle = \"mine\"\n[[menu.items]]\nkey = \"a\"\nlabel = \"alpha\"\ncommand = \"alpha\"\n",
        )
        .unwrap();

        let config = Config::from_core(&tools_core::Config::load(&path).unwrap()).unwrap();
        assert_eq!(config.title, "mine");
        assert_eq!(config.width, DEFAULT_WIDTH);
        assert!(!config.used_default);
        assert_eq!(config.items.len(), 1);
        assert!(config.warnings.is_empty());

        std::fs::write(&path, "[menu]\ntitle = \"  \"\n").unwrap();
        let error = format!(
            "{:#}",
            Config::from_core(&tools_core::Config::load(&path).unwrap()).unwrap_err()
        );
        assert!(error.contains("invalid [menu]"), "{error}");
        assert!(error.contains("title cannot be empty"), "{error}");
    }

    #[test]
    fn popup_height_is_bounded_by_config() {
        let mut config = Config::parse(DEFAULT_CONFIG).unwrap();
        config.max_height = 6;

        assert_eq!(config.popup_height(), 6);
    }

    /// A fixed size is `[popup.size].menu` now (the launcher's tests cover it);
    /// the old top-level fields are named in a warning instead of silently lost.
    #[test]
    fn old_size_fields_warn_and_leave_the_auto_size() {
        let config = Config::parse(
            r#"
width = 80
height = 32

[[items]]
key = "a"
label = "alpha"
command = "alpha"
"#,
        )
        .unwrap();

        assert_eq!(config.width, DEFAULT_WIDTH);
        assert_eq!(config.warnings.len(), 2);
        assert!(config.warnings[0].contains("width"));
        assert!(config.warnings[1].contains("[popup.size].menu"));
        assert_eq!(config.popup_height(), 7);
    }
}
