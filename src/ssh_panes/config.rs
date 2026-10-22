//! Live-loaded SSH transport and payload-sync settings. Defaults leave local panes untouched.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    pub nvim_appname: String,
    pub integrations: Vec<String>,
    pub paths: Vec<SyncPath>,
}

/// Destinations are remote-root-relative or home-relative, never local runtime cwd values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncPath {
    pub src: String,
    pub dest: String,
    #[serde(default)]
    pub exclude: Vec<String>,
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
    fn ssh_panes_config_accepts_paths_and_rejects_unknown_nested_keys() {
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
            assert!(toml::from_str::<SshPanesConfig>(invalid)
                .unwrap_err()
                .to_string()
                .contains("unknown field"));
        }
        assert!(toml::from_str::<SshPanesConfig>("[sync]\npaths = [{src = 'a'}]").is_err());
    }
}
