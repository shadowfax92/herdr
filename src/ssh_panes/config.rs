//! Live-loaded SSH transport and payload-sync settings. Defaults leave local panes untouched.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshPanesConfig {
    pub enabled: bool,
    pub inherit_new_tab: bool,
    pub auto_sync: bool,
    pub allowed_hosts: Vec<String>,
    pub remote_root: String,
    pub ssh_options: Vec<String>,
    pub sync: SyncConfig,
}

impl Default for SshPanesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            inherit_new_tab: true,
            auto_sync: true,
            allowed_hosts: Vec::new(),
            remote_root: "~/.local/share/herdr-ssh-panes".into(),
            ssh_options: Vec::new(),
            sync: SyncConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    pub nvim_appname: String,
    pub integrations: Vec<String>,
    pub paths: Vec<SyncPath>,
}

/// Destinations are remote-root-relative or home-relative, never local runtime cwd values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPath {
    pub src: String,
    pub dest: String,
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Startup normally parses Config in one pass. Quarantine only a malformed SSH
/// section before that pass, preserving serde_ignored diagnostics for valid tables
/// (including unknown keys) and the user's unrelated theme/key settings.
pub(crate) fn startup_content(content: String) -> (String, Option<String>) {
    let Ok(mut value) = content.parse::<toml::Value>() else {
        return (content, None);
    };
    let Some(section) = value.get("ssh_panes") else {
        return (content, None);
    };
    if let Err(error) = section.clone().try_into::<SshPanesConfig>() {
        let diagnostic = format!("invalid ssh panes config: {error}; disabling only ssh_panes");
        tracing::warn!(%diagnostic);
        if let Some(table) = value.as_table_mut() {
            table.remove("ssh_panes");
            return (toml::to_string(&value).unwrap_or(content), Some(diagnostic));
        }
    }
    (content, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_panes_defaults_are_opt_in_and_contract_exact() {
        let config: crate::config::Config = toml::from_str("").unwrap();
        assert!(!config.ssh_panes.enabled);
        assert_eq!(
            serde_json::to_value(config.ssh_panes).unwrap(),
            serde_json::json!({
                "enabled": false, "inherit_new_tab": true, "auto_sync": true,
                "allowed_hosts": [], "remote_root": "~/.local/share/herdr-ssh-panes",
                "ssh_options": [], "sync": {"nvim_appname": "", "integrations": [], "paths": []}
            })
        );
    }

    #[test]
    fn ssh_panes_config_accepts_paths_and_ignores_unknown_nested_keys() {
        let config: SshPanesConfig =
            toml::from_str("enabled = true\n[sync]\npaths = [{src = '~/src', dest = 'bin/src'}]")
                .unwrap();
        assert!(config.enabled);
        assert!(config.sync.paths[0].exclude.is_empty());
        for invalid in [
            "enabld = true",
            "[sync]\nnvim_appnam = 'x'",
            "[sync]\npaths = [{src = 'a', dest = 'b', excludes = []}]",
        ] {
            assert!(toml::from_str::<SshPanesConfig>(invalid).is_ok());
        }
        assert!(toml::from_str::<SshPanesConfig>("[sync]\npaths = [{src = 'a'}]").is_err());
    }
}

#[cfg(test)]
mod startup_tests {
    #[test]
    fn ssh_panes_startup_typo_and_malformed_section_preserve_theme_and_keys() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path =
            std::env::temp_dir().join(format!("herdr-ssh-config-{}.toml", std::process::id()));
        let old = std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR);
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        for section in [
            "enabeld = true",
            "enabled = 'bad'",
            "enabled = true\n[sync]\npaths = [{src = 'a'}]",
        ] {
            let content = format!(
                "[theme]\nname = 'dracula'\n[keys]\nprefix = 'ctrl+b'\n[ssh_panes]\n{}\n",
                section.replace("[sync]", "[ssh_panes.sync]")
            );
            std::fs::write(&path, content).unwrap();
            let loaded = crate::config::Config::load();
            assert_eq!(
                loaded.config.theme.name.as_deref(),
                Some("dracula"),
                "{:?}",
                loaded.diagnostics
            );
            assert_eq!(
                serde_json::to_value(&loaded.config.keys.prefix).unwrap(),
                "ctrl+b"
            );
            assert!(!loaded.config.ssh_panes.enabled);
            if !section.starts_with("enabeld") {
                assert!(
                    loaded
                        .diagnostics
                        .iter()
                        .any(|d| d.contains("invalid ssh panes config")),
                    "{:?}",
                    loaded.diagnostics
                );
            }
            if section.starts_with("enabeld") {
                assert!(
                    loaded
                        .diagnostics
                        .iter()
                        .any(|d| d.contains("ssh_panes.enabeld")),
                    "{:?}",
                    loaded.diagnostics
                );
            }
        }
        if let Some(old) = old {
            std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, old);
        } else {
            std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        }
        std::fs::remove_file(path).unwrap();
    }
}
