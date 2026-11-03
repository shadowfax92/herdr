//! Scratch's own Herdr calls, built on tools-core's [`Host`].
//!
//! `toggle` opens the shared `scratch` popup pane and hands it the source pane's
//! identity through popup env (Herdr strips `HERDR_PANE_ID` from popups);
//! `run-popup` then reads that pane once with `herdr pane get` so cwd and the
//! Grove token come from one consistent observation.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;
use tools_core::{Envelope, Host, PopupDims};

use crate::context::{ScratchContext, SourcePane};

/// The manifest pane every scratch popup runs in.
pub const POPUP_ENTRYPOINT: &str = "scratch";
/// The title of reap's completion notification.
pub const NOTIFICATION_TITLE: &str = "Herdr Scratch";

#[derive(Debug, PartialEq, Eq)]
pub struct PopupRequest {
    pub scratch_name: String,
    pub source_pane_id: String,
    pub source_cwd: PathBuf,
    pub tmux_prefix: String,
    /// None: the manifest pane's default size.
    pub size: Option<PopupDims>,
}

#[derive(Debug, Deserialize)]
struct PaneResult {
    pane: PaneInfo,
}

#[derive(Debug, Deserialize)]
struct PaneInfo {
    pane_id: String,
    cwd: Option<String>,
    foreground_cwd: Option<String>,
    #[serde(default)]
    tokens: HashMap<String, String>,
}

/// Opens the scratch popup as a focused native popup over the source pane.
pub fn open_popup(host: &Host, request: &PopupRequest) -> Result<()> {
    let env = popup_env(request);
    let env = env
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect::<Vec<_>>();
    host.open_popup_sized(POPUP_ENTRYPOINT, request.size, &env)
}

/// What `run-popup` needs to find its session; never a `--cwd`, so the popup
/// shell does not start somewhere the scratch did not choose.
fn popup_env(request: &PopupRequest) -> Vec<(&'static str, String)> {
    vec![
        ("HERDR_SCRATCH_NAME", request.scratch_name.clone()),
        ("HERDR_SCRATCH_SOURCE_PANE", request.source_pane_id.clone()),
        (
            "HERDR_SCRATCH_SOURCE_CWD",
            request.source_cwd.display().to_string(),
        ),
        ("HERDR_SCRATCH_PREFIX", request.tmux_prefix.clone()),
    ]
}

pub fn show_notification(host: &Host, body: &str) -> Result<()> {
    host.notify(NOTIFICATION_TITLE, body)
}

/// `herdr pane current`, for a toggle run without a plugin context.
pub fn current_pane(host: &Host) -> Result<SourcePane> {
    let pane = host
        .cli_json::<Envelope<PaneResult>, _, _>(["pane", "current"])?
        .result
        .pane;
    let cwd = pane
        .foreground_cwd
        .or(pane.cwd)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    Ok(SourcePane {
        pane_id: pane.pane_id,
        cwd,
    })
}

pub fn scratch_context(host: &Host, pane_id: &str) -> Result<ScratchContext> {
    // Resolve at run-popup, not toggle: agent metadata can change while
    // Herdr is creating the popup. One read keeps cwd and token consistent.
    let mut pane = host
        .cli_json::<Envelope<PaneResult>, _, _>(["pane", "get", pane_id])?
        .result
        .pane;
    let cwd = pane
        .foreground_cwd
        .or(pane.cwd)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .context("source pane has no cwd")?;
    Ok(ScratchContext::resolve(
        pane_id.to_owned(),
        cwd,
        pane.tokens.remove("grove_worktree"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use tools_core::testing::{fake_cli, fake_host, ok};

    /// A fake Herdr socket that records requests; the middle value owns the
    /// socket's tempdir and must outlive the host.
    fn recording_host() -> (Arc<Mutex<Vec<Value>>>, impl Sized, Host) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let (dir, host) = fake_host(move |request| {
            log.lock().unwrap().push(request.clone());
            match request["method"].as_str() {
                Some("pane.layout") => ok(
                    &request,
                    json!({"layout": {"area": {"x": 30, "y": 0, "width": 482, "height": 60}}}),
                ),
                _ => ok(&request, json!({"type": "ok"})),
            }
        });
        (seen, dir, host)
    }

    #[test]
    fn opens_a_native_popup_with_source_identity() {
        let (seen, _guard, host) = recording_host();
        open_popup(
            &host,
            &PopupRequest {
                scratch_name: "nvim".into(),
                source_pane_id: "w2:p1".into(),
                source_cwd: PathBuf::from("/tmp/project"),
                tmux_prefix: "C-a".into(),
                size: Some(PopupDims {
                    width: "70%".parse().unwrap(),
                    height: "95%".parse().unwrap(),
                }),
            },
        )
        .unwrap();

        let seen = seen.lock().unwrap();
        let params = &seen[0]["params"];
        assert_eq!(seen[0]["method"], "plugin.pane.open");
        assert_eq!(params["plugin_id"], "shadowfax.tools");
        assert_eq!(params["entrypoint"], "scratch");
        assert_eq!(params["placement"], "popup");
        assert_eq!(params["focus"], true);
        assert_eq!(params["width"], "70%");
        assert_eq!(params["height"], "95%");
        assert!(params.get("cwd").is_none());
        assert_eq!(
            params["env"],
            json!({
                "HERDR_SCRATCH_NAME": "nvim",
                "HERDR_SCRATCH_SOURCE_PANE": "w2:p1",
                "HERDR_SCRATCH_SOURCE_CWD": "/tmp/project",
                "HERDR_SCRATCH_PREFIX": "C-a",
            })
        );
    }

    #[test]
    fn unsized_popups_defer_to_the_manifest_pane() {
        let (seen, _guard, host) = recording_host();
        open_popup(
            &host,
            &PopupRequest {
                scratch_name: "shell".into(),
                source_pane_id: "w2:p1".into(),
                source_cwd: PathBuf::from("/tmp/project"),
                tmux_prefix: "C-b".into(),
                size: None,
            },
        )
        .unwrap();
        let params = &seen.lock().unwrap()[0]["params"];
        assert!(params.get("width").is_none() && params.get("height").is_none());
    }

    #[test]
    fn parses_client_width_from_layout_right_edge() {
        let (_seen, _guard, host) = recording_host();

        assert_eq!(host.client_width("w2:p1").unwrap(), 512);
    }

    #[test]
    fn scratch_context_reads_the_pane_once_and_prefers_foreground_cwd() {
        let dir = std::env::temp_dir().join(format!("scratch-herdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("calls");
        let bin = fake_cli(
            &dir,
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'\n\
                 echo '{{\"result\":{{\"pane\":{{\"pane_id\":\"'\"$3\"'\",\"cwd\":\"/tmp\",\"foreground_cwd\":\"/\",\"tokens\":{{\"grove_worktree\":\"relative\"}}}}}}}}'",
                calls.display()
            ),
        );
        let context = scratch_context(&Host::new(&bin, None), "w1:p2").unwrap();
        assert_eq!(context.source_pane, "w1:p2");
        assert_eq!(context.root, PathBuf::from("/"));
        assert_eq!(context.root_source, crate::context::RootSource::Cwd);
        assert_eq!(std::fs::read_to_string(&calls).unwrap(), "pane get w1:p2\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
