//! The `comment` mode of herdr-tools: `herdr-tools comment <command>`.
//!
//! Select and comment on frozen pane history, review the comments in Neovim, and
//! paste the reviewed draft into the pane. Runtime Neovim assets live in the
//! plugin dir at `nvim/comment/` (`Paths::root()`).
//!
//! Ported from `/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-comments/`
//! (plugin `shadowfax.comments`, HEAD `bb76c01`); that tree is read-only.
//!
//! ```text
//!  action capture ── snapshot or selection ─▶ store runs/ ─▶ popup comment-capture
//!                                                              (capture-popup: tui)
//!                                                              └─▶ store collections/
//!  action review ─── collection ─▶ store reviews/ ─▶ popup comment-review
//!                                                     (review-popup: nvim ─▶ confirm-review)
//!                                                     └─▶ store ready/
//!  action paste ──── ready draft or collection ─▶ pane.send_input ─▶ delete what was pasted
//! ```
//!
//! Every action and popup is a separate process; they share state only through
//! the private store under the plugin state dir, addressed by opaque ids passed
//! in popup env (`HERDR_COMMENTS_RUN_ID`, `HERDR_COMMENTS_REVIEW_ID`).

pub mod annotation;
pub mod config;
pub mod context;
pub mod format;
pub mod herdr;
pub mod model;
pub mod review;
pub mod snapshot;
pub mod store;
pub mod tui;

use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use tools_core::Paths;

use annotation::{review_target, AnnotationService};
use config::{LoadedConfig, CAPTURE_POPUP, REVIEW_POPUP};
use context::ActionContext;
use herdr::{HerdrClient, HostHerdr};
use review::{NvimEditor, ReviewService};
use store::Store;

/// Comment state lives under `<plugin state dir>/comment/`. The state dir is
/// shared by every mode, so each mode keeps its own subdirectory and no mode's
/// cleanup can touch another's files.
pub fn state_root() -> std::path::PathBuf {
    Paths::from_env().state_dir().join("comment")
}

/// This mode's name: the first `herdr-tools` argument and its config table.
pub const MODE: &str = "comment";

/// The title of every notification this mode shows (formerly "Herdr Comments").
pub const NOTIFICATION_TITLE: &str = "Comments";

/// `herdr-tools comment …`. Manifest actions, panes and hooks call these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Snapshot the invoking pane and open the annotation popup.
    Capture,
    /// Run the annotation popup (normally launched by Herdr).
    CapturePopup,
    /// Open the review popup for this pane's collected comments.
    Review,
    /// Run the review popup (normally launched by Herdr).
    ReviewPopup,
    /// Paste this pane's reviewed draft or collected comments without submitting it.
    Paste,
    /// Mark a review ready; called back by the Neovim review script.
    ConfirmReview {
        #[arg(long)]
        id: String,
    },
}

/// Runs one `comment` command. A failure is also shown as a Herdr notification
/// when Herdr started the process; `main` then prints it to stderr, which lands
/// in `herdr plugin log`.
pub fn run(command: Command) -> Result<()> {
    let result = match command {
        Command::Capture => capture_action(),
        Command::CapturePopup => capture_popup(),
        Command::Review => review_action(),
        Command::ReviewPopup => review_popup(),
        Command::Paste => paste_action(),
        Command::ConfirmReview { id } => confirm_review(&id),
    };
    if let Err(error) = &result {
        notify_error(error);
    }
    result
}

fn notify_error(error: &anyhow::Error) {
    if let Some(herdr) = HostHerdr::from_env() {
        let _ = herdr.notify(NOTIFICATION_TITLE, &error.to_string());
    }
}

/// This mode's fragment of the new `config.toml`, built from the old plugin's
/// config under `legacy_config_root` (`~/.config/herdr/plugins/config`). Empty
/// when the old plugin has none.
///
/// Old config: `shadowfax.comments/config.yaml` (inline_comments, popups, profiles); popups
/// `capture`/`review` map onto popup keys `comment-capture`/`comment-review`. The
/// fragment is `[comment] inline_comments` (always explicit), the base sizes in
/// `[popup.size]`, and one `[[popup.profile]]` per old profile, in order, each
/// sizing both keys so every client width resolves as it did before.
///
/// **State: the cutover should copy it.** Collected comments and ready drafts
/// are durable user data, and the old plugin kept them in its own state dir,
/// `~/.local/state/herdr/plugins/shadowfax.comments/`. The new mode reads the
/// `shadowfax.tools` state dir instead, with the same layout and the same scope
/// hashing (session socket path + pane id), so copying
/// `shadowfax.comments/{collections,ready}/` into
/// `~/.local/state/herdr/plugins/shadowfax.tools/` (keeping the 0700 dirs and
/// 0600 files, and no symlinks) carries pending comments over to the same
/// panes. Skip `runs/` and `reviews/` (interrupted sessions that expire after
/// 24 hours) and `active/` / `drafts/` (left by older versions, unread now).
pub fn migrate_legacy(legacy_config_root: &Path) -> Result<toml::Table> {
    config::legacy::fragment(legacy_config_root)
}

pub fn capture_action() -> Result<()> {
    let context = ActionContext::from_env()?;
    let store = Store::open(&context.state_dir)?;
    let herdr = HostHerdr::new(&context.herdr_bin, &context.session_identity);
    let scope = store::scope_id(&context.session_identity, &context.pane_id);
    let config = LoadedConfig::load()?;
    let popup = config.popup(CAPTURE_POPUP, herdr.client_width(&context.pane_id).ok());
    let service = AnnotationService::new(&store, &herdr);
    if config.inline_comments() {
        let selected_text = context
            .selected_text
            .as_deref()
            .context("inline comments require a selection in Herdr Copy mode")?;
        service
            .start_inline(&context.pane_id, &scope, selected_text, popup)
            .map(|_| ())
    } else {
        service.start(&context.pane_id, &scope, popup).map(|_| ())
    }
}

pub fn capture_popup() -> Result<()> {
    let run_id =
        std::env::var("HERDR_COMMENTS_RUN_ID").context("HERDR_COMMENTS_RUN_ID is missing")?;
    let store = Store::open(state_root())?;
    if std::env::var("HERDR_COMMENTS_INLINE").is_ok_and(|value| value == "1") {
        tui::run_inline_annotation(&store, &run_id)
    } else {
        tui::run_annotation(&store, &run_id)
    }
}

pub fn review_action() -> Result<()> {
    let context = ActionContext::from_env()?;
    let store = Store::open(&context.state_dir)?;
    let herdr = HostHerdr::new(&context.herdr_bin, &context.session_identity);
    let target = review_target(&context);
    let popup =
        LoadedConfig::load()?.popup(REVIEW_POPUP, herdr.client_width(&context.pane_id).ok());
    ReviewService::new(&store, &herdr)
        .start(&target, popup)
        .map(|_| ())
}

pub fn paste_action() -> Result<()> {
    let context = ActionContext::from_env()?;
    let store = Store::open(&context.state_dir)?;
    let herdr = HostHerdr::new(&context.herdr_bin, &context.session_identity);
    let target = review_target(&context);
    ReviewService::new(&store, &herdr)
        .paste_ready(&target)
        .map(|_| ())
}

pub fn review_popup() -> Result<()> {
    let review_id =
        std::env::var("HERDR_COMMENTS_REVIEW_ID").context("HERDR_COMMENTS_REVIEW_ID is missing")?;
    let herdr_bin = std::env::var("HERDR_BIN_PATH").context("HERDR_BIN_PATH is missing")?;
    let socket_path = std::env::var("HERDR_SOCKET_PATH").context("HERDR_SOCKET_PATH is missing")?;
    let store = Store::open(state_root())?;
    let herdr = HostHerdr::new(herdr_bin, socket_path);
    let root = herdr::plugin_root()?;
    let editor = NvimEditor::new(
        std::env::var_os("HERDR_COMMENTS_NVIM").unwrap_or_else(|| "nvim".into()),
        std::env::current_exe().context("herdr-tools executable path is unavailable")?,
        herdr::review_script(&root),
    );
    editor.run(&review_id, &store.review_markdown_path(&review_id)?)?;
    ReviewService::new(&store, &herdr)
        .finish(&review_id)
        .map(|_| ())
}

pub fn confirm_review(review_id: &str) -> Result<()> {
    Store::open(state_root())?.confirm_review(review_id)
}
