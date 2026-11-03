//! The split shortcuts' Herdr CLI calls, on tools-core's [`Host`].
//!
//! Every mutation names explicit pane IDs, so popup focus cannot retarget a
//! running operation. The grid's own calls live in `grid.rs`.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use tools_core::{Envelope, Host};

#[derive(Debug, Deserialize)]
struct PaneResult {
    pane: Pane,
}

#[derive(Debug, Deserialize)]
struct Pane {
    pane_id: String,
}

/// Splits `pane_id` to the right with the given first-child ratio, focuses the
/// new pane, and returns its ID. Herdr splits only right or down, which is why
/// a left split is this plus [`swap`].
pub fn split_right(
    host: &Host,
    pane_id: &str,
    cwd: &Path,
    first_child_ratio: f32,
) -> Result<String> {
    let response: Envelope<PaneResult> = host
        .cli_json(split_args(pane_id, cwd, first_child_ratio))
        .context("failed to parse `herdr pane split`")?;
    Ok(response.result.pane.pane_id)
}

pub fn swap(host: &Host, source_pane_id: &str, target_pane_id: &str) -> Result<()> {
    host.cli([
        "pane",
        "swap",
        "--source-pane",
        source_pane_id,
        "--target-pane",
        target_pane_id,
    ])?;
    Ok(())
}

fn split_args(pane_id: &str, cwd: &Path, first_child_ratio: f32) -> Vec<OsString> {
    vec![
        "pane".into(),
        "split".into(),
        "--pane".into(),
        pane_id.into(),
        "--direction".into(),
        "right".into(),
        "--ratio".into(),
        first_child_ratio.to_string().into(),
        "--cwd".into(),
        cwd.as_os_str().into(),
        "--focus".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_focused_right_split() {
        let args = split_args("w2:p3", Path::new("/tmp/project"), 0.7);
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            [
                "pane",
                "split",
                "--pane",
                "w2:p3",
                "--direction",
                "right",
                "--ratio",
                "0.7",
                "--cwd",
                "/tmp/project",
                "--focus"
            ]
        );
    }

    #[test]
    fn parses_the_created_pane() {
        let response: Envelope<PaneResult> =
            serde_json::from_str(r#"{"result":{"type":"pane_info","pane":{"pane_id":"w2:p4"}}}"#)
                .unwrap();

        assert_eq!(response.result.pane.pane_id, "w2:p4");
    }
}
