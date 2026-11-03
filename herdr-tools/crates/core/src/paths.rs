//! Where the plugin's files live. Herdr hands every action, popup, startup hook
//! and event the authoritative directories; the fallbacks only serve commands a
//! person runs from a shell (`herdr-tools migrate-config`, a mode's diagnostics)
//! and mirror Herdr's own `XDG_*` rules so both agree on the same paths.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::PLUGIN_ID;

/// The plugin's config dir, state dir and install root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    config_dir: PathBuf,
    state_dir: PathBuf,
    root: Option<PathBuf>,
}

impl Paths {
    /// Reads `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR` and `HERDR_PLUGIN_ROOT`.
    pub fn from_env() -> Self {
        let current_exe = std::env::current_exe().ok();
        Self::from_lookup(|key| std::env::var_os(key), current_exe.as_deref())
    }

    /// [`Paths::from_env`] over an injected environment, for tests.
    ///
    /// Without Herdr's variables: config is `$XDG_CONFIG_HOME/herdr/plugins/config/shadowfax.tools`
    /// (else `~/.config/…`), state is `$XDG_STATE_HOME/herdr/plugins/shadowfax.tools`
    /// (else `~/.local/state/…`), and the root is the directory above `bin/herdr-tools`.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<OsString>,
        current_exe: Option<&Path>,
    ) -> Self {
        let present = |key: &str| {
            lookup(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let home = || present("HOME").unwrap_or_else(std::env::temp_dir);
        let config_dir = present("HERDR_PLUGIN_CONFIG_DIR").unwrap_or_else(|| {
            present("XDG_CONFIG_HOME")
                .unwrap_or_else(|| home().join(".config"))
                .join("herdr/plugins/config")
                .join(PLUGIN_ID)
        });
        let state_dir = present("HERDR_PLUGIN_STATE_DIR").unwrap_or_else(|| {
            present("XDG_STATE_HOME")
                .unwrap_or_else(|| home().join(".local/state"))
                .join("herdr/plugins")
                .join(PLUGIN_ID)
        });
        // A popup's cwd is the source pane's, not the plugin root, so the old
        // `current_dir()` fallback was wrong there. The laid-out plugin dir
        // always holds the binary at `<root>/bin/herdr-tools`.
        let root = present("HERDR_PLUGIN_ROOT").or_else(|| {
            let bin = current_exe?.parent()?;
            (bin.file_name()? == "bin").then(|| bin.parent().map(Path::to_path_buf))?
        });
        Self {
            config_dir,
            state_dir,
            root,
        }
    }

    /// `~/.config/herdr/plugins/config/shadowfax.tools`, owned by the user.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// `<config_dir>/config.toml`, the one config file every mode reads.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// `~/.local/state/herdr/plugins/shadowfax.tools`, private runtime state.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The plugin's install directory (manifest, `bin/`, `nvim/<mode>/`).
    pub fn root(&self) -> Result<&Path> {
        self.root
            .as_deref()
            .context("the herdr-tools plugin root is unknown (HERDR_PLUGIN_ROOT is not set)")
    }

    /// `~/.config/herdr/plugins/config`: the parent holding each old plugin's
    /// `shadowfax.<old>/` config dir, read by `migrate-config`.
    pub fn legacy_config_root(&self) -> PathBuf {
        self.config_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.config_dir.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn paths(vars: &[(&str, &str)], exe: Option<&str>) -> Paths {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(value)))
            .collect();
        Paths::from_lookup(|key| vars.get(key).cloned(), exe.map(Path::new))
    }

    #[test]
    fn herdr_variables_win() {
        let paths = paths(
            &[
                ("HERDR_PLUGIN_CONFIG_DIR", "/c/shadowfax.tools"),
                ("HERDR_PLUGIN_STATE_DIR", "/s/shadowfax.tools"),
                ("HERDR_PLUGIN_ROOT", "/r"),
                ("XDG_CONFIG_HOME", "/ignored"),
            ],
            Some("/elsewhere/bin/herdr-tools"),
        );
        assert_eq!(
            paths.config_file(),
            Path::new("/c/shadowfax.tools/config.toml")
        );
        assert_eq!(paths.state_dir(), Path::new("/s/shadowfax.tools"));
        assert_eq!(paths.root().unwrap(), Path::new("/r"));
        assert_eq!(paths.legacy_config_root(), Path::new("/c"));
    }

    #[test]
    fn shell_fallbacks_follow_xdg_then_home() {
        let xdg = paths(
            &[
                ("XDG_CONFIG_HOME", "/x/config"),
                ("XDG_STATE_HOME", "/x/state"),
                ("HOME", "/h"),
            ],
            None,
        );
        assert_eq!(
            xdg.config_dir(),
            Path::new("/x/config/herdr/plugins/config/shadowfax.tools")
        );
        assert_eq!(
            xdg.state_dir(),
            Path::new("/x/state/herdr/plugins/shadowfax.tools")
        );
        assert_eq!(
            xdg.legacy_config_root(),
            Path::new("/x/config/herdr/plugins/config")
        );

        let home = paths(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "")], None);
        assert_eq!(
            home.config_dir(),
            Path::new("/h/.config/herdr/plugins/config/shadowfax.tools")
        );
        assert_eq!(
            home.state_dir(),
            Path::new("/h/.local/state/herdr/plugins/shadowfax.tools")
        );
    }

    #[test]
    fn root_falls_back_to_the_laid_out_binary_only() {
        let laid_out = paths(&[], Some("/p/shadowfax.tools/bin/herdr-tools"));
        assert_eq!(laid_out.root().unwrap(), Path::new("/p/shadowfax.tools"));
        let cargo = paths(&[], Some("/repo/target/debug/herdr-tools"));
        assert!(cargo.root().is_err());
    }
}
