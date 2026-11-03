//! Equalize's socket API calls, on [`Host::request`]. The CLI has no command
//! for exporting a tab's split tree or setting one split's ratio, so these two
//! go through the socket that `HERDR_SOCKET_PATH` names.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use tools_core::Host;

use crate::layout::LayoutDescription;

#[derive(Debug, Deserialize)]
struct LayoutExportResult {
    layout: LayoutDescription,
}

/// `layout.export`: the split tree of the tab holding `pane_id`.
pub fn export_layout(host: &Host, pane_id: &str) -> Result<LayoutDescription> {
    let result = host.request("layout.export", json!({"pane_id": pane_id}))?;
    let export: LayoutExportResult =
        serde_json::from_value(result).context("invalid layout.export response")?;
    Ok(export.layout)
}

/// `layout.set_split_ratio`: sets the split at `path` (false = first child,
/// true = second, from the root) in `tab_id`.
pub fn set_split_ratio(host: &Host, tab_id: &str, path: &[bool], ratio: f32) -> Result<()> {
    host.request(
        "layout.set_split_ratio",
        json!({
            "tab_id": tab_id,
            "path": path,
            "ratio": ratio,
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutNode;
    use tools_core::testing::{fake_host, ok};
    use tools_core::HostError;

    #[test]
    fn extracts_a_success_result() {
        let (_dir, host) = fake_host(|request| {
            assert_eq!(request["method"], "layout.export");
            assert_eq!(request["params"], json!({"pane_id": "w1:p1"}));
            ok(
                &request,
                json!({"type": "layout", "layout": {"tab_id": "w1:t1",
                    "root": {"type": "pane", "pane_id": "w1:p1"}}}),
            )
        });

        let layout = export_layout(&host, "w1:p1").unwrap();

        assert_eq!(layout.tab_id, "w1:t1");
        assert!(matches!(layout.root, LayoutNode::Pane { pane_id: Some(ref id) } if id == "w1:p1"));
    }

    #[test]
    fn reports_an_api_error() {
        let (_dir, host) = fake_host(
            |request| json!({"id": request["id"], "error": {"code": "split_not_found", "message": "missing"}}),
        );

        let error = set_split_ratio(&host, "w1:t1", &[true], 0.5).unwrap_err();

        let refusal = error.downcast_ref::<HostError>().unwrap();
        assert_eq!(refusal.code, "split_not_found");
        assert_eq!(
            error.to_string(),
            "layout.set_split_ratio (split_not_found): missing"
        );
    }
}
