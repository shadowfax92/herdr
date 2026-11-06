//! How copy mode pages a fullscreen app, which keeps its own history.
//!
//! Herdr keeps no scrollback for the alternate screen, so a copy-mode page
//! key reaches the app as input it scrolls on. PgUp and PgDn are the one key
//! pair every fullscreen app treats as a harmless scroll, so by default each
//! page key sends one and the app decides how far it goes. Claude Code
//! scrolls half its view per PgUp, so there ctrl+u is already half a page.
//! The keys that move half a page in less or vim edit or quit elsewhere.
//! ctrl+u kills Claude Code's prompt and edits Codex's composer, ctrl+d quits
//! codex, pi and opencode at an empty prompt, and k9s deletes a resource on
//! ctrl+d.
//!
//! Codex scrolls a whole view per PgUp and binds no half-page key in its main
//! view, but its wheel scrolls one row per notch. So while Codex reports the
//! mouse, ctrl+u and ctrl+d move it half its view in wheel notches. The exit
//! does not replay those notches. A replay would need either a cap that stops
//! paging partway up a long session or a page-back past the server's
//! per-message event limit. It would also need mouse reporting at exit, and
//! it can stop short of the live output once new output has landed. The exit
//! sends Codex its own jump to its latest output instead, once, whatever copy
//! mode sent.
//!
//! ```text
//! page key -> move_copy_page(direction, unit)                         copy_mode.rs
//!     normal screen -> herdr's own offset, copy_mode_page_lines
//!     alternate     -> FullscreenPaging::page(direction, unit, PagedPane)
//!                        AppMove::Keys  -> push_key_stroke                shell.rs
//!                        AppMove::Wheel -> push_wheel_notches             shell.rs
//! q, Esc, y, Enter, a hint copy, copy mode on another pane -> exit_copy_mode
//!     still alternate -> FullscreenPaging::page_back -> push_key_stroke
//!                        n PgDn for Owed::Strokes(n), 1 jump for Owed::Latest
//! ```
//!
//! Detection is read on every key rather than pinned for the session, so a
//! label that lands or ends while copy mode is open changes how the next move
//! goes out. The debt changes only when a move goes out, and a jump once owed
//! stays owed, so a Codex label lost mid-session still gets its jump on exit.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::config::KeyCombo;
use crate::copy_mode::{copy_mode_page_lines, PageDirection, PageUnit};
use crate::detect::Agent;

const PAGE_UP: KeyCombo = (KeyCode::PageUp, KeyModifiers::NONE);
const PAGE_DOWN: KeyCombo = (KeyCode::PageDown, KeyModifiers::NONE);

/// The most PgUp strokes copy mode will owe an app.
///
/// The page-back goes out as one input message at two events per stroke, a
/// press and its synthesized release, and the server disconnects a client
/// whose message carries more than `MAX_INPUT_EVENT_BATCH` events. A quarter
/// of that in strokes keeps the page-back to half a message, which leaves
/// room for other keys merged into the same message. That is 1024 strokes,
/// which in a 49-row pane is about 21k rows of Claude Code's transcript or
/// 49k rows of less. Derived from the server's constant so the two cannot
/// drift apart.
const MAX_STROKES_UP: u32 = (crate::server::client_transport::MAX_INPUT_EVENT_BATCH / 4) as u32;

/// The most wheel notches one half page sends, half of a 512-row view, which
/// no terminal reaches. The bound keeps one half page far below the server's
/// per-message event limit and below the pane's PTY write queue, 1024 writes
/// on Unix, where the server drops the rest of a message once the queue is
/// full.
const MAX_NOTCHES: u16 = 256;

/// What the client knows about the paged pane when a page key arrives.
/// `page_fullscreen_app` reads it on every key from the snapshot, the
/// retained surface and the copy session, so the ledger reads no shell state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PagedPane {
    /// The pane's detected agent, `None` before detection names it and after
    /// the agent exits.
    pub(super) agent: Option<Agent>,
    /// Whether the app reports the mouse. The server turns a wheel notch for
    /// a pane that does not into an arrow key or into herdr's own scroll, and
    /// arrows walk Codex's prompt history, so no notch goes out without it.
    pub(super) reports_mouse: bool,
    /// The pane's inner width, which places the cell a notch points at.
    pub(super) columns: u16,
    /// The pane's inner height, which sizes a half page of notches.
    pub(super) rows: u16,
}

/// What herdr has measured about scrolling one app's fullscreen view.
///
/// Each variant pairs a way to move the app with a way back. A wheel move
/// cannot be paid back by count, so the only variant that sends wheel notches
/// also names the app's jump to its latest output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppProfile {
    /// One PgUp or PgDn per page key, whatever the unit asks, and one PgDn
    /// per PgUp owed on exit. Claude Code gets this, as does every app herdr
    /// has not measured.
    PageKeys,
    /// A half page is wheel notches, one row each, while the pane reports
    /// the mouse, and a full page is one PgUp or PgDn. Once anything has gone
    /// up, the exit sends `latest` once.
    WheelRows {
        /// Pane rows outside the app's scrolling view, such as a header, a
        /// composer and a footer. A half page is half of the rows left.
        chrome_rows: u16,
        /// The app's jump to its latest output from anywhere in its history.
        latest: KeyCombo,
    },
}

/// The profile for `agent`. Add a row only from a measurement. A guessed
/// `WheelRows` for an app whose wheel accelerates would make ctrl+u move an
/// unpredictable distance, and a guessed `latest` that the app binds to
/// something else would fire on every exit.
fn app_profile(agent: Option<Agent>) -> AppProfile {
    match agent {
        // codex-cli 0.166.0 in a 49-row pane (measured). One PgUp moved 41
        // rows, which its source computes as the transcript view less one, so
        // the view is 42 rows and the header, composer and footer take 7. A
        // wheel notch moved one row and ten moved ten, either way, but only
        // over the transcript, not over the sticky header row above it or the
        // composer and footer below it. ctrl+End returned Codex to its latest
        // output from a page up and left a draft typed in the composer.
        //
        // From its source at rust-v0.166.0. The main view scrolls only on
        // PageUp and PageDown. ctrl+u edits the composer and ctrl+d quits on
        // an empty one, so herdr sends neither. A notch scrolls
        // tui.mouse_scroll_speed rows, 1.0 by default. A faster setting moves
        // ctrl+u farther but cannot strand the exit, which counts nothing.
        // ctrl+End is JumpTarget::Latest. Esc also returns to the latest, but
        // only while scrolled, and otherwise it can interrupt a running turn.
        Some(Agent::Codex) => AppProfile::WheelRows {
            chrome_rows: 7,
            latest: (KeyCode::End, KeyModifiers::CONTROL),
        },
        // Claude Code's PgUp scrolls half its view, 21 of 44 rows measured in
        // 2.1.319, so one PgUp is already its half page.
        _ => AppProfile::PageKeys,
    }
}

/// What one page key sends the app, already counted by the ledger.
/// `push_key_stroke` and `push_wheel_notches` turn it into wire events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AppMove {
    /// `strokes` presses of `key`, each with its synthesized release. One,
    /// or zero at `MAX_STROKES_UP`, where the app must not move.
    Keys { key: KeyCombo, strokes: u32 },
    /// `notches` wheel notches toward `direction`, at least one and at most
    /// `MAX_NOTCHES`, pointed at the pane-relative `cell` as (column, row).
    Wheel {
        direction: PageDirection,
        notches: u16,
        cell: (u16, u16),
    },
}

/// What leaving copy mode owes the app, a two-state machine over one session.
///
/// `Strokes` counts PgUps sent and not yet paged back, from zero to
/// `MAX_STROKES_UP`. `Latest` is absorbing. Once an app with a jump has gone
/// up, one stroke of the jump returns it to its latest output whatever was
/// sent before or after, so nothing more is counted or capped. No state owes
/// wheel notches, so no exit replays them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owed {
    Strokes(u32),
    Latest(KeyCombo),
}

impl Default for Owed {
    fn default() -> Self {
        Self::Strokes(0)
    }
}

/// What copy mode owes a fullscreen app it paged. One per copy session, in
/// `ClientCopyModeState`.
///
/// The field is private, so only `page` changes it and only `page_back`
/// reads it. `page_back` takes the ledger by value, and the type is not
/// `Copy`, so a session pays back at most once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct FullscreenPaging {
    owed: Owed,
}

impl FullscreenPaging {
    /// What to send the app for one page key, given what the client knows
    /// about its pane. The ledger has already counted the move, so the
    /// caller must send it.
    ///
    /// A half page to a `WheelRows` app whose pane reports the mouse goes out
    /// as wheel notches. Every other move is one PgUp or PgDn. Any up move to
    /// a `WheelRows` app leaves the exit owing that app's jump, and once the
    /// jump is owed nothing is counted or capped. Until then a PgUp adds a
    /// stroke, or sends nothing once `MAX_STROKES_UP` are owed, and a PgDn
    /// pays one back. A PgDn always goes out, because the app may have been
    /// scrolled up before copy mode began. A wheel down pays nothing back,
    /// which can only overpay, and an overpaid PgDn stops at the app's live
    /// bottom.
    #[must_use = "the ledger already counts this move, so send it or the page-back drifts"]
    pub(super) fn page(
        &mut self,
        direction: PageDirection,
        unit: PageUnit,
        pane: PagedPane,
    ) -> AppMove {
        let profile = app_profile(pane.agent);
        if let (AppProfile::WheelRows { latest, .. }, PageDirection::Up) = (profile, direction) {
            self.owed = Owed::Latest(latest);
        }
        match profile {
            AppProfile::WheelRows { chrome_rows, .. }
                if unit == PageUnit::Half && pane.reports_mouse =>
            {
                // Half the app's view rather than half the pane, so a half
                // page stays half in a split, where the chrome takes more of
                // the pane. The notches aim at the view's middle, because the
                // wheel scrolls only while the pointer is over the transcript.
                let view = pane.rows.saturating_sub(chrome_rows);
                let notches =
                    copy_mode_page_lines(view, PageUnit::Half).min(usize::from(MAX_NOTCHES));
                AppMove::Wheel {
                    direction,
                    notches: notches as u16,
                    cell: (pane.columns / 2, view / 2),
                }
            }
            _ => self.page_key(direction),
        }
    }

    /// One PgUp or PgDn, counted while the exit owes strokes and not at all
    /// once it owes a jump.
    fn page_key(&mut self, direction: PageDirection) -> AppMove {
        let key = match direction {
            PageDirection::Up => PAGE_UP,
            PageDirection::Down => PAGE_DOWN,
        };
        let strokes = match (&mut self.owed, direction) {
            (Owed::Strokes(owed), PageDirection::Up) if *owed < MAX_STROKES_UP => {
                *owed += 1;
                1
            }
            (Owed::Strokes(_), PageDirection::Up) => 0,
            (Owed::Strokes(owed), PageDirection::Down) => {
                *owed = owed.saturating_sub(1);
                1
            }
            (Owed::Latest(_), _) => 1,
        };
        AppMove::Keys { key, strokes }
    }

    /// What returns the app to where it follows its output, as strokes of
    /// one key. That is one PgDn per PgUp owed, none when nothing is owed, or
    /// one stroke of the app's jump to its latest output. The caller sends it
    /// only while the pane still shows the alternate screen.
    #[must_use = "dropping the page-back leaves the app scrolled up"]
    pub(super) fn page_back(self) -> (KeyCombo, u32) {
        match self.owed {
            Owed::Strokes(strokes) => (PAGE_DOWN, strokes),
            Owed::Latest(latest) => (latest, 1),
        }
    }
}
