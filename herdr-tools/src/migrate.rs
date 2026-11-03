//! `herdr-tools migrate-config`: builds the one `config.toml` from the seven old
//! plugins' configs.
//!
//! ```text
//!  ~/.config/herdr/plugins/config/            (legacy root)
//!    shadowfax.talon/config.toml   ─ tools_history::migrate_legacy ─┐
//!    shadowfax.comments/config.yaml ─ tools_comment::migrate_legacy ─┼─ merge ─ rewrite old
//!    …one fragment per mode…                                         │         action ids
//!                                                                    ┘         in [menu]
//!    shadowfax.tools/config.toml  ◀── validate as Config ◀────────────────────┘
//! ```
//!
//! Each mode knows only its own old format and returns a fragment of the new
//! file. Fragments merge table by table; the only shared table is `[popup]`, where
//! `[[popup.profile]]` entries with the same name and width range combine their
//! sizes (comments and scratch both had `laptop`/`2-3-ultrawide`/`full-ultrawide`).
//! Any other key two modes set differently is an error rather than a silent pick.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use toml::{Table, Value};
use tools_core::{legacy, Config, Paths};

type Migrator = fn(&Path) -> Result<Table>;

/// Every mode's migrator, in config-file order.
const MIGRATORS: &[(&str, Migrator)] = &[
    (tools_find::MODE, tools_find::migrate_legacy),
    (tools_move::MODE, tools_move::migrate_legacy),
    (tools_history::MODE, tools_history::migrate_legacy),
    (tools_menu::MODE, tools_menu::migrate_legacy),
    (tools_comment::MODE, tools_comment::migrate_legacy),
    (tools_layout::MODE, tools_layout::migrate_legacy),
    (tools_scratch::MODE, tools_scratch::migrate_legacy),
];

pub fn run(legacy_root: Option<PathBuf>, force: bool, stdout: bool) -> Result<()> {
    let paths = Paths::from_env();
    let legacy_root = legacy_root.unwrap_or_else(|| paths.legacy_config_root());
    let mut fragments = Vec::new();
    for (mode, migrate) in MIGRATORS {
        let fragment = migrate(&legacy_root)
            .with_context(|| format!("{mode}: could not migrate its old config"))?;
        fragments.push((*mode, fragment));
    }
    let text = render(&legacy_root, &merge(fragments)?)?;
    // Validate exactly as every mode will read it, before anything is written.
    Config::parse(&text).context("the merged config does not validate")?;
    if stdout {
        print!("{text}");
        return Ok(());
    }
    let target = paths.config_file();
    if let Some(backup) = write(&target, &text, force)? {
        println!("Backed up the previous config to {}", backup.display());
    }
    println!("Wrote {}", target.display());
    Ok(())
}

/// Merges mode fragments in order; see the module docs for the rules.
pub(crate) fn merge(fragments: Vec<(&str, Table)>) -> Result<Table> {
    let mut merged = Table::new();
    for (mode, fragment) in fragments {
        merge_table(&mut merged, fragment, "", mode)?;
    }
    if let Some(Value::Table(menu)) = merged.get_mut("menu") {
        rewrite_strings(menu);
    }
    Ok(merged)
}

fn merge_table(target: &mut Table, source: Table, path: &str, mode: &str) -> Result<()> {
    for (key, value) in source {
        let key_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        match (target.get_mut(&key), value) {
            (None, value) => {
                target.insert(key, value);
            }
            (Some(Value::Table(existing)), Value::Table(incoming)) => {
                merge_table(existing, incoming, &key_path, mode)?;
            }
            (Some(Value::Array(existing)), Value::Array(incoming))
                if key_path == "popup.profile" =>
            {
                merge_profiles(existing, incoming, mode)?;
            }
            (Some(existing), value) if *existing == value => {}
            (Some(_), _) => bail!("{mode} sets `{key_path}` differently from an earlier mode"),
        }
    }
    Ok(())
}

/// Profiles with the same name and width range are one profile.
fn merge_profiles(existing: &mut Vec<Value>, incoming: Vec<Value>, mode: &str) -> Result<()> {
    let identity = |profile: &Value| {
        ["name", "min_client_width", "max_client_width"].map(|key| profile.get(key).cloned())
    };
    for profile in incoming {
        let same = existing
            .iter_mut()
            .find(|candidate| identity(candidate) == identity(&profile));
        match (same, profile) {
            (Some(Value::Table(same)), Value::Table(profile)) => {
                let name = same.get("name").and_then(Value::as_str).unwrap_or("?");
                let path = format!("popup.profile.{name}");
                merge_table(same, profile, &path, mode)?;
            }
            (_, profile) => existing.push(profile),
        }
    }
    Ok(())
}

/// Old action ids inside menu items (e.g. `plugin action invoke shadowfax.scratch.reap`).
fn rewrite_strings(table: &mut Table) {
    fn rewrite(value: &mut Value) {
        match value {
            Value::String(text) => {
                if let std::borrow::Cow::Owned(new) = legacy::rewrite_action_ids(text) {
                    *text = new;
                }
            }
            Value::Array(values) => values.iter_mut().for_each(rewrite),
            Value::Table(table) => table.iter_mut().for_each(|(_, value)| rewrite(value)),
            _ => {}
        }
    }
    table.iter_mut().for_each(|(_, value)| rewrite(value));
}

fn render(legacy_root: &Path, merged: &Table) -> Result<String> {
    let body = toml::to_string_pretty(merged).context("could not render the merged config")?;
    Ok(format!(
        "# shadowfax.tools config, generated by `herdr-tools migrate-config`\n\
         # from the old plugin configs in {}.\n\
         # One table per mode plus [popup]; see the herdr-tools README.\n\n{body}",
        legacy_root.display()
    ))
}

/// Writes `text` to `target` by same-directory rename. An existing file is
/// refused, or with `force` kept as a timestamped backup (returned).
fn write(target: &Path, text: &str, force: bool) -> Result<Option<PathBuf>> {
    let directory = target
        .parent()
        .context("the config path has no directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let exists = target.try_exists()?;
    if exists && !force {
        bail!(
            "{} already exists; pass --force to replace it (the old file is kept as a backup)",
            target.display()
        );
    }
    let temporary = target.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&temporary, text)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    let backup = if exists {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let backup = target.with_extension(format!("toml.before-migrate-{stamp}"));
        std::fs::copy(target, &backup)
            .with_context(|| format!("failed to back up {}", target.display()))?;
        Some(backup)
    } else {
        None
    };
    std::fs::rename(&temporary, target)
        .with_context(|| format!("failed to replace {}", target.display()))?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(source: &str) -> Table {
        source.parse().unwrap()
    }

    #[test]
    fn fragments_merge_and_shared_profiles_combine() {
        let comment = table(
            r#"
[comment]
inline_comments = true
[popup.size]
comment-capture = { width = "90%", height = "90%" }
[[popup.profile]]
name = "laptop"
max_client_width = 310
size.comment-capture = { width = "95%", height = "90%" }
"#,
        );
        let scratch = table(
            r#"
[scratch.cleanup]
ttl_hours = 24
[popup.size]
scratch-nvim = { width = "90%", height = "95%" }
[[popup.profile]]
name = "laptop"
max_client_width = 310
size.scratch-nvim = { width = "95%", height = "95%" }
[[popup.profile]]
name = "full-ultrawide"
min_client_width = 400
size.scratch-nvim = { width = "70%", height = "95%" }
"#,
        );
        let merged = merge(vec![("comment", comment), ("scratch", scratch)]).unwrap();
        let text = toml::to_string_pretty(&merged).unwrap();
        let config = Config::parse(&text).unwrap();

        let profiles = &config.popup_config().profile;
        assert_eq!(profiles.len(), 2, "{text}");
        assert_eq!(profiles[0].size.len(), 2, "{text}");
        assert_eq!(
            config
                .popup("scratch-nvim", Some(300))
                .unwrap()
                .profile
                .as_deref(),
            Some("laptop")
        );
        assert!(config
            .popup("comment-capture", Some(512))
            .unwrap()
            .profile
            .is_none());
        assert!(config.section_table("comment").unwrap().is_some());
        assert!(config.section_table("scratch").unwrap().is_some());
    }

    #[test]
    fn conflicting_values_are_refused() {
        let first = table("[popup.size]\nmenu = { width = 64, height = 16 }\n");
        let second = table("[popup.size]\nmenu = { width = 80, height = 16 }\n");
        let error = merge(vec![("menu", first.clone()), ("layout", second)]).unwrap_err();
        assert!(
            error.to_string().contains("popup.size.menu.width"),
            "{error}"
        );
        // Agreeing values are fine.
        assert!(merge(vec![("menu", first.clone()), ("layout", first)]).is_ok());
    }

    #[test]
    fn old_action_ids_in_menu_items_are_rewritten() {
        let menu = table(
            r#"
[[menu.items]]
key = "x"
command = '"$HERDR_BIN_PATH" plugin action invoke shadowfax.scratch.reap'
"#,
        );
        let merged = merge(vec![("menu", menu)]).unwrap();
        assert_eq!(
            merged["menu"]["items"][0]["command"].as_str(),
            Some(r#""$HERDR_BIN_PATH" plugin action invoke shadowfax.tools.scratch-reap"#)
        );
    }

    #[test]
    fn existing_configs_are_refused_without_force_and_backed_up_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("shadowfax.tools/config.toml");
        assert_eq!(write(&target, "first = 1\n", false).unwrap(), None);
        let error = write(&target, "second = 2\n", false).unwrap_err();
        assert!(error.to_string().contains("--force"), "{error}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first = 1\n");

        let backup = write(&target, "second = 2\n", true).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second = 2\n");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "first = 1\n");
        let leftovers = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn stub_fragments_render_a_valid_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut fragments = Vec::new();
        for (mode, migrate) in MIGRATORS {
            fragments.push((*mode, migrate(dir.path()).unwrap()));
        }
        let text = render(dir.path(), &merge(fragments).unwrap()).unwrap();
        Config::parse(&text).unwrap();
        assert!(text.starts_with("# shadowfax.tools config"));
    }
}
