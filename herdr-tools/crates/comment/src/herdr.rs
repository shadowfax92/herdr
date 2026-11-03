//! The comment mode's Herdr calls, built on tools-core's [`Host`].
//!
//! [`HerdrClient`] is the seam the annotation and review services use, so their
//! tests run against recording doubles. [`HostHerdr`] implements it over one
//! [`Host`]:
//!
//! ```text
//!  capture_snapshot ── CLI: pane get (scroll) + pane read ×4 (recent/visible × text/ansi)
//!  client_width     ── socket: pane.layout              (popup profile selection)
//!  open_* popups    ── socket: plugin.pane.open         (Host::open_popup_sized)
//!  send_input       ── socket: pane.send_input, keys [] (bracketed paste, no Enter)
//!  notify           ── CLI: notification show … --sound none
//! ```
//!
//! Every refusal is reported in the old plugin's words: Herdr's error code (or
//! the CLI's output) is classified as [`FailureKind`] and becomes "could not
//! <operation>: …", which is what the action's notification shows.

use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tools_core::{Host, HostError, Paths, PopupDims};

use crate::config::{CAPTURE_POPUP, REVIEW_POPUP};
use crate::model::PaneSnapshot;
use crate::snapshot::{assemble, rows, ScrollState, CAPTURE_ROWS};

const MAX_API_REQUEST_BYTES: usize = 1024 * 1024;
/// `herdr-tools-<pid>-<n>` at its widest (u32 pid, u64 counter).
const LONGEST_REQUEST_ID: &str = "herdr-tools-4294967295-18446744073709551615";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    UiBusy,
    PaneMissing,
    Command,
}

/// The Herdr operations the comment services need. `popup` is the configured
/// size, or None for the manifest pane's default.
pub trait HerdrClient {
    fn capture_snapshot(&self, pane_id: &str) -> Result<PaneSnapshot>;
    fn open_annotation(&self, run_id: &str, popup: Option<PopupDims>) -> Result<()>;
    fn open_inline_annotation(&self, run_id: &str, popup: Option<PopupDims>) -> Result<()>;
    fn open_review(&self, review_id: &str, popup: Option<PopupDims>) -> Result<()>;
    fn send_input(&self, pane_id: &str, text: &str) -> Result<()>;
    fn notify(&self, title: &str, body: &str) -> Result<()>;
}

/// A popup this mode opens: the manifest pane entrypoint and the env it gets.
///
/// Popups carry only opaque state ids. The snapshot, comments and review text
/// stay in the private state dir and never travel through Herdr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupRequest {
    pub entrypoint: &'static str,
    pub env: Vec<(&'static str, String)>,
}

/// [`HerdrClient`] over a [`Host`] for one Herdr session.
#[derive(Debug, Clone)]
pub struct HostHerdr {
    host: Host,
}

impl HostHerdr {
    /// A client for `bin` (or its installed sibling, see [`resolve_herdr_bin`])
    /// and the session's API socket.
    pub fn new(bin: impl Into<PathBuf>, socket_path: impl Into<PathBuf>) -> Self {
        Self {
            host: Host::new(resolve_herdr_bin(bin.into()), Some(socket_path.into())),
        }
    }

    /// A client for the session that started this process, from
    /// `HERDR_BIN_PATH` and `HERDR_SOCKET_PATH`, when Herdr set both.
    pub fn from_env() -> Option<Self> {
        let bin = std::env::var_os("HERDR_BIN_PATH")?;
        let socket_path = std::env::var_os("HERDR_SOCKET_PATH")?;
        Some(Self::new(bin, socket_path))
    }

    /// The underlying core client.
    pub fn host(&self) -> &Host {
        &self.host
    }

    fn run(&self, args: &[String], operation: &str) -> Result<Output> {
        let output = self
            .host
            .cli_output(args)
            .with_context(|| format!("failed to start Herdr while trying to {operation}"))?;
        if output.status.success() {
            return Ok(output);
        }
        fail(operation, &output)
    }

    fn read_pane(
        &self,
        pane_id: &str,
        source: &str,
        format: &str,
        lines: Option<u32>,
    ) -> Result<String> {
        let output = self.run(
            &pane_read_args(pane_id, source, format, lines),
            "capture pane text",
        )?;
        String::from_utf8(output.stdout).context("Herdr returned non-UTF-8 pane text")
    }

    fn scroll_state(&self, pane_id: &str) -> Result<Option<ScrollState>> {
        let output = self.run(&pane_get_args(pane_id), "inspect pane scroll state")?;
        let response: PaneEnvelope =
            serde_json::from_slice(&output.stdout).context("invalid Herdr pane response")?;
        response
            .result
            .pane
            .scroll
            .map(RawScroll::try_into)
            .transpose()
    }

    /// The client's width in cells, which popup profiles match on.
    pub fn client_width(&self, pane_id: &str) -> Result<u16> {
        self.host.client_width(pane_id)
    }

    fn open(&self, request: PopupRequest, popup: Option<PopupDims>, operation: &str) -> Result<()> {
        let env = request
            .env
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect::<Vec<_>>();
        self.host
            .open_popup_sized(request.entrypoint, popup, &env)
            .or_else(|error| fail_host(operation, &error))
    }
}

/// Herdr's handoff can leave `HERDR_BIN_PATH` naming a staged binary that has
/// since been renamed into place (`herdr.fast-scroll-staging` → `herdr`). Use
/// the installed sibling then, rather than failing or picking whatever `herdr`
/// is first on `PATH`.
pub fn resolve_herdr_bin(bin: PathBuf) -> PathBuf {
    if bin.is_file() {
        return bin;
    }
    bin.parent()
        .map(|parent| parent.join("herdr"))
        .filter(|candidate| candidate.is_file())
        .unwrap_or(bin)
}

impl HerdrClient for HostHerdr {
    fn capture_snapshot(&self, pane_id: &str) -> Result<PaneSnapshot> {
        let scroll = self.scroll_state(pane_id)?;
        let recent_text = self.read_pane(pane_id, "recent", "text", Some(CAPTURE_ROWS))?;
        let recent_ansi = self.read_pane(pane_id, "recent", "ansi", Some(CAPTURE_ROWS))?;
        let visible_text = self.read_pane(pane_id, "visible", "text", None)?;
        let visible_ansi = self.read_pane(pane_id, "visible", "ansi", None)?;
        let scroll = scroll.unwrap_or_else(|| ScrollState {
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            viewport_rows: rows(&visible_text).len(),
        });
        assemble(
            &recent_text,
            &recent_ansi,
            &visible_text,
            &visible_ansi,
            scroll,
        )
    }

    fn open_annotation(&self, run_id: &str, popup: Option<PopupDims>) -> Result<()> {
        self.open(annotation_popup(run_id), popup, "open the annotation popup")
    }

    fn open_inline_annotation(&self, run_id: &str, popup: Option<PopupDims>) -> Result<()> {
        self.open(
            inline_annotation_popup(run_id),
            popup,
            "open the inline comment popup",
        )
    }

    fn open_review(&self, review_id: &str, popup: Option<PopupDims>) -> Result<()> {
        self.open(review_popup(review_id), popup, "open the review popup")
    }

    fn send_input(&self, pane_id: &str, text: &str) -> Result<()> {
        validate_pane_id(pane_id)?;
        // Herdr caps one socket request at 1 MiB; refuse before connecting. The
        // id is the longest one Host can send, so the frame is never undersized.
        pane_send_input_request(LONGEST_REQUEST_ID, pane_id, text)?;
        let result = self
            .host
            .request("pane.send_input", pane_send_input_params(pane_id, text))
            .or_else(|error| match error.downcast_ref::<HostError>() {
                Some(_) => fail_host("insert comments", &error),
                None => bail!("could not insert comments: {error}"),
            })?;
        if result["type"] == "ok" {
            return Ok(());
        }
        bail!("could not insert comments: Herdr returned an unexpected response")
    }

    fn notify(&self, title: &str, body: &str) -> Result<()> {
        self.run(&notification_args(title, body), "show a notification")?;
        Ok(())
    }
}

pub fn annotation_popup(run_id: &str) -> PopupRequest {
    annotation_popup_with_mode(run_id, false)
}

pub fn inline_annotation_popup(run_id: &str) -> PopupRequest {
    annotation_popup_with_mode(run_id, true)
}

/// Inline comments reuse the capture entrypoint; `HERDR_COMMENTS_INLINE=1`
/// tells the popup to open straight into the comment editor.
fn annotation_popup_with_mode(run_id: &str, inline: bool) -> PopupRequest {
    let mut env = vec![("HERDR_COMMENTS_RUN_ID", run_id.to_owned())];
    if inline {
        env.push(("HERDR_COMMENTS_INLINE", "1".to_owned()));
    }
    PopupRequest {
        entrypoint: CAPTURE_POPUP,
        env,
    }
}

pub fn review_popup(review_id: &str) -> PopupRequest {
    PopupRequest {
        entrypoint: REVIEW_POPUP,
        env: vec![("HERDR_COMMENTS_REVIEW_ID", review_id.to_owned())],
    }
}

pub fn pane_read_args(
    pane_id: &str,
    source: &str,
    format: &str,
    lines: Option<u32>,
) -> Vec<String> {
    let mut args = vec![
        "pane".into(),
        "read".into(),
        pane_id.into(),
        "--source".into(),
        source.into(),
        "--format".into(),
        format.into(),
    ];
    if let Some(lines) = lines {
        args.push("--lines".into());
        args.push(lines.to_string());
    }
    args
}

pub fn pane_get_args(pane_id: &str) -> Vec<String> {
    vec!["pane".into(), "get".into(), pane_id.into()]
}

/// The `pane.send_input` params: the text as one bracketed paste, and an empty
/// key list so nothing is submitted.
pub fn pane_send_input_params(pane_id: &str, text: &str) -> Value {
    json!({"pane_id": pane_id, "text": text, "keys": []})
}

/// The whole framed `pane.send_input` request, refused when it exceeds Herdr's
/// socket request limit.
pub fn pane_send_input_request(request_id: &str, pane_id: &str, text: &str) -> Result<String> {
    let request = serde_json::to_string(&PaneSendInputRequest {
        id: request_id,
        method: "pane.send_input",
        params: PaneSendInputParams {
            pane_id,
            text,
            keys: Vec::new(),
        },
    })?;
    if request.len().saturating_add(1) > MAX_API_REQUEST_BYTES {
        bail!("could not insert comments: the review exceeds Herdr's socket request limit");
    }
    Ok(request)
}

fn notification_args(title: &str, body: &str) -> Vec<String> {
    vec![
        "notification".into(),
        "show".into(),
        title.into(),
        "--body".into(),
        body.into(),
        "--sound".into(),
        "none".into(),
    ]
}

pub fn classify_failure(output: &str) -> FailureKind {
    let output = output.to_ascii_lowercase();
    if output.contains("ui_busy") {
        FailureKind::UiBusy
    } else if output.contains("pane_not_found")
        || output.contains("pane not found")
        || output.contains("no such pane")
    {
        FailureKind::PaneMissing
    } else {
        FailureKind::Command
    }
}

fn fail<T>(operation: &str, output: &Output) -> Result<T> {
    fail_with_kind(operation, classify_failure(&diagnostic(output)))
}

/// A failed [`Host`] call: Herdr's own refusal is classified on its code and
/// message; anything else (no server, a timeout) is a rejected request, as the
/// old CLI call reported it.
fn fail_host<T>(operation: &str, error: &anyhow::Error) -> Result<T> {
    let kind = match error.downcast_ref::<HostError>() {
        Some(refusal) => classify_failure(&format!("{}\n{}", refusal.code, refusal.message)),
        None => FailureKind::Command,
    };
    fail_with_kind(operation, kind)
}

fn fail_with_kind<T>(operation: &str, kind: FailureKind) -> Result<T> {
    match kind {
        FailureKind::UiBusy => {
            bail!("could not {operation}: close the active Herdr popup or modal")
        }
        FailureKind::PaneMissing => {
            bail!("could not {operation}: the originating pane no longer exists")
        }
        FailureKind::Command => bail!("could not {operation}: Herdr rejected the request"),
    }
}

fn diagnostic(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn validate_pane_id(pane_id: &str) -> Result<()> {
    if pane_id.trim().is_empty() || pane_id.chars().any(char::is_control) {
        bail!("Herdr returned an invalid pane identifier");
    }
    Ok(())
}

/// The plugin install dir, which holds `nvim/comment/`. A popup's cwd is the
/// source pane's, so this is core's root (`HERDR_PLUGIN_ROOT`, else the dir
/// above `bin/herdr-tools`), never the current directory.
pub fn plugin_root() -> Result<PathBuf> {
    Paths::from_env()
        .root()
        .map(Path::to_path_buf)
        .ok()
        .filter(|path| path.is_dir())
        .context("Herdr plugin root is unavailable")
}

pub fn review_script(root: &Path) -> PathBuf {
    root.join("nvim/comment/review.lua")
}

#[derive(Deserialize)]
struct PaneEnvelope {
    result: PaneResult,
}

#[derive(Deserialize)]
struct PaneResult {
    pane: RawPane,
}

#[derive(Deserialize)]
struct RawPane {
    scroll: Option<RawScroll>,
}

#[derive(Deserialize)]
struct RawScroll {
    offset_from_bottom: u64,
    max_offset_from_bottom: u64,
    viewport_rows: u64,
}

impl TryFrom<RawScroll> for ScrollState {
    type Error = anyhow::Error;

    fn try_from(value: RawScroll) -> Result<Self> {
        Ok(Self {
            offset_from_bottom: value.offset_from_bottom.try_into()?,
            max_offset_from_bottom: value.max_offset_from_bottom.try_into()?,
            viewport_rows: value.viewport_rows.try_into()?,
        })
    }
}

#[derive(Serialize)]
struct PaneSendInputRequest<'a> {
    id: &'a str,
    method: &'static str,
    params: PaneSendInputParams<'a>,
}

#[derive(Serialize)]
struct PaneSendInputParams<'a> {
    pane_id: &'a str,
    text: &'a str,
    keys: Vec<&'a str>,
}
