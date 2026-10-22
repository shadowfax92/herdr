//! JSON-only contracts between the local connector, remote watcher, and server registry.
//! These types do not extend any frozen client endpoint codec.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SshPanesRequest {
    Attach(AttachParams),
    Report(ReportParams),
    Detach(DetachParams),
    List(ListParams),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttachParams {
    pub pane_id: String,
    pub link_id: String,
    pub host: String,
    pub connector_pid: u32,
    pub connector_pgid: u32,
    #[serde(default)]
    pub cwd_hint: Option<String>,
    pub protocol: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReportParams {
    pub link_id: String,
    pub seq: u64,
    pub shell_pid: u32,
    pub shell_name: String,
    pub foreground: RemoteForeground,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RemoteForeground {
    pub process_group_id: Option<u32>,
    pub foreground_is_pane_shell: bool,
    pub agent: Option<String>,
    pub process_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DetachParams {
    pub link_id: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListParams {
    #[serde(default)]
    pub pane_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "result", rename_all = "snake_case")]
// Short-lived JSON responses keep the contract's LinkInfo direct, like Method's wire values.
#[allow(clippy::large_enum_variant)]
pub enum SshPanesResult {
    Attached { link: LinkInfo, lease_ms: u64 },
    Reported { accepted: bool },
    Detached {},
    Links { enabled: bool, links: Vec<LinkInfo> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LinkInfo {
    pub link_id: String,
    pub host: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub state: SshLinkState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_cwd: Option<String>,
    pub agent: Option<String>,
    pub attached_ms_ago: u64,
    pub last_report_ms_ago: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneSshInfo {
    pub host: String,
    pub link_id: String,
    pub state: SshLinkState,
    // Unknown local paths are absent: plugin consumers cannot use JSON null as a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SshLinkState {
    Live,
    Stale,
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_panes_contract_requests_round_trip_without_changing_wire_shape() {
        for value in [
            serde_json::json!({"op":"attach", "pane_id":"w1:p2", "link_id":"link", "host":"studio", "connector_pid":42, "connector_pgid":42, "cwd_hint":null, "protocol":1}),
            serde_json::json!({"op":"report", "link_id":"link", "seq":17, "shell_pid":81, "shell_name":"fish", "foreground":{"process_group_id":90, "foreground_is_pane_shell":false, "agent":"claude", "process_name":"claude"}, "cwd":"/remote", "foreground_cwd":"/remote"}),
            serde_json::json!({"op":"detach", "link_id":"link", "exit_code":0}),
            serde_json::json!({"op":"list", "pane_id":null}),
        ] {
            let request: SshPanesRequest = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(request).unwrap(), value);
        }
        assert_eq!(
            serde_json::from_str::<SshLinkState>("\"future\"").unwrap(),
            SshLinkState::Unknown
        );
    }

    #[test]
    fn ssh_panes_unknown_local_cwd_is_omitted_from_pane_link_json() {
        let mut link = PaneSshInfo {
            host: "studio".into(),
            link_id: "one".into(),
            state: SshLinkState::Live,
            local_cwd: None,
        };
        let unknown = serde_json::to_value(&link).unwrap();
        assert_eq!(
            unknown,
            serde_json::json!({"host":"studio","link_id":"one","state":"live"})
        );
        assert_eq!(
            serde_json::from_value::<PaneSshInfo>(unknown).unwrap(),
            link
        );
        link.local_cwd = Some("/local".into());
        assert_eq!(serde_json::to_value(link).unwrap()["local_cwd"], "/local");
    }
}

#[cfg(test)]
mod list_tests {
    #[test]
    fn ssh_panes_list_omits_unknown_paths_and_reads_older_nulls() {
        let old = serde_json::json!({"link_id":"one", "host":"studio", "pane_id":"w1:p1", "terminal_id":"t", "state":"live", "cwd":null,"foreground_cwd":null,"local_cwd":null,"agent":null,"attached_ms_ago":0,"last_report_ms_ago":null});
        let info: super::LinkInfo = serde_json::from_value(old).unwrap();
        let value = serde_json::to_value(info).unwrap();
        for field in ["cwd", "foreground_cwd", "local_cwd"] {
            assert!(value.get(field).is_none(), "{value}");
        }
        let _: super::LinkInfo = serde_json::from_value(value).unwrap();
    }
}
