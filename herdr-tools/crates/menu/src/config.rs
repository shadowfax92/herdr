use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use toml::value::Table;
use toml::Value;

pub const DEFAULT_CONFIG: &str = include_str!("../config.default.toml");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub title: String,
    pub width: u16,
    pub height: Option<u16>,
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
    WorkspaceVisibility(ActionItem),
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
    pub fn parse(source: &str) -> Result<Self> {
        parse_config(source, false)
    }

    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(source) => parse_config(&source, false)
                .with_context(|| format!("failed to load {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut config = parse_config(DEFAULT_CONFIG, true)?;
                config.warnings.insert(
                    0,
                    format!("{} not found; using built-in defaults", path.display()),
                );
                Ok(config)
            }
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub fn popup_height(&self) -> u16 {
        if let Some(height) = self.height {
            return height;
        }
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
            | MenuItem::WorkspaceVisibility(_)
            | MenuItem::Separator => None,
        })
    }
}

impl MenuItem {
    pub fn key(&self) -> Option<char> {
        match self {
            Self::Shell(item) => Some(item.key),
            Self::Fork(item) => Some(item.key),
            Self::RotateFork(item)
            | Self::Redistribute(item)
            | Self::CopyContext(item)
            | Self::WorkspaceVisibility(item) => Some(item.key),
            Self::Separator => None,
        }
    }

    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Shell(item) => Some(&item.label),
            Self::Fork(item) => Some(&item.label),
            Self::RotateFork(item)
            | Self::Redistribute(item)
            | Self::CopyContext(item)
            | Self::WorkspaceVisibility(item) => Some(&item.label),
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
            | Self::WorkspaceVisibility(_)
            | Self::Separator => None,
        }
    }

    pub fn is_selectable(&self) -> bool {
        !matches!(self, Self::Separator)
    }
}

fn parse_config(source: &str, used_default: bool) -> Result<Config> {
    let document: Value = toml::from_str(source).context("failed to parse config TOML")?;
    let table = document
        .as_table()
        .context("config root must be a TOML table")?;
    let title = optional_string(table, "title")?
        .unwrap_or("commands")
        .trim()
        .to_string();
    if title.is_empty() {
        bail!("title cannot be empty");
    }
    let width = optional_dimension(table, "width", 64, 20)?;
    let height = optional_dimension_value(table, "height", 6)?;
    let max_height = optional_dimension(table, "max_height", 24, 6)?;
    let values: &[Value] = match table.get("items") {
        Some(value) => value
            .as_array()
            .context("items must be an array of tables")?,
        None => &[],
    };

    let mut items = Vec::with_capacity(values.len());
    let mut warnings = Vec::new();
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
        height,
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
        "workspace_visibility" => {
            reject_unknown_fields(table, &["type", "key", "label"])?;
            let (key, label) = action_identity(table)?;
            Ok(MenuItem::WorkspaceVisibility(ActionItem { key, label }))
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

fn optional_dimension_value(table: &Table, field: &str, minimum: u16) -> Result<Option<u16>> {
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
        let config = Config::parse(DEFAULT_CONFIG).unwrap();

        assert_eq!(config.title, "agent forks");
        assert_eq!(config.width, 68);
        assert_eq!(config.height, None);
        assert_eq!(config.max_height, 24);
        assert_eq!(config.items.len(), 10);
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
        assert!(matches!(config.items[9], MenuItem::WorkspaceVisibility(_)));
        assert_eq!(config.shell_items().count(), 0);
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn every_shell_field_is_decoded() {
        let config = Config::parse(
            r#"
title = "tools"
width = 72
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
        assert_eq!(config.width, 72);
        assert_eq!(config.max_height, 18);
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

        let config = Config::load(&path).unwrap();

        assert!(config.used_default);
        assert_eq!(config.items.len(), 10);
        assert_eq!(config.warnings.len(), 1);
        assert!(config.warnings[0].contains("using built-in defaults"));
    }

    #[test]
    fn popup_height_is_bounded_by_config() {
        let mut config = Config::parse(DEFAULT_CONFIG).unwrap();
        config.max_height = 6;

        assert_eq!(config.popup_height(), 6);
    }

    #[test]
    fn explicit_popup_height_overrides_auto_size() {
        let config = Config::parse(
            r#"
height = 32

[[items]]
key = "a"
label = "alpha"
command = "alpha"
"#,
        )
        .unwrap();

        assert_eq!(config.height, Some(32));
        assert_eq!(config.popup_height(), 32);
    }
}
