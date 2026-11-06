//! Copy hints driven through `ClientShellState`, the way tests/copy.rs drives
//! copy mode: real snapshots and surfaces, keys through the input parser,
//! compose for painting. No PTY and no server.

use super::*;

/// prefix+f with the default prefix, ctrl+b.
const PREFIX_F: &[u8] = b"\x02f";
/// prefix+[ with the default prefix: copy mode.
const PREFIX_BRACKET: &[u8] = b"\x02[";

fn scroll(offset: u64, max: u64, rows: u64) -> Option<crate::protocol::PaneSurfaceScrollMetrics> {
    Some(crate::protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: offset,
        max_offset_from_bottom: max,
        viewport_rows: rows,
    })
}

/// A surface whose one pane shows `rows`, each padded to `width` columns,
/// following its output at the bottom.
fn hint_surface(rows: &[&str], width: u16) -> PaneSurfaceFrame {
    let height = rows.len() as u16;
    let buffer = Buffer::with_lines(rows.iter().map(|row| {
        let padding = usize::from(width).saturating_sub(row.width());
        format!("{row}{}", " ".repeat(padding))
    }));
    let mut surface = surface();
    surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    let rect = SurfaceRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    surface.panes[0].rect = rect;
    surface.panes[0].inner_rect = rect;
    surface.panes[0].scroll = scroll(0, 0, u64::from(height));
    surface
}

/// The next frame of the same pane: new cells, a newer surface and content
/// revision, the same geometry and scroll metrics.
fn redraw(surface: &mut PaneSurfaceFrame, rows: &[&str]) {
    let width = surface.panes[0].inner_rect.width;
    surface.frame = hint_surface(rows, width).frame;
    surface.surface_revision += 1;
    surface.panes[0].content_revision += 1;
}

/// A client showing `surface`, composed once so the hit map exists.
fn client(config: &Config, surface: &PaneSurfaceFrame) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(config));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface.clone());
    state.compose(106, 20).expect("composed frame");
    state
}

fn press(state: &mut ClientShellState, code: KeyCode, modifiers: KeyModifiers) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        code, modifiers,
    ))])
}

fn esc(state: &mut ClientShellState) -> ClientShellInput {
    press(state, KeyCode::Esc, KeyModifiers::empty())
}

fn clipboard(outcome: &ClientShellInput) -> Option<String> {
    outcome.actions.iter().find_map(|action| match action {
        ClientShellAction::ClipboardWrite(bytes) => String::from_utf8(bytes.clone()).ok(),
        _ => None,
    })
}

fn labels(state: &ClientShellState) -> Vec<(String, String)> {
    state.copy_hint_labels()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(label, text)| ((*label).to_owned(), (*text).to_owned()))
        .collect()
}

/// The keys `outcome` sends to pane_1, as (code, kind) pairs.
fn keys_sent_to_pane(
    outcome: &ClientShellInput,
) -> Vec<(
    crate::protocol::ClientKeyCode,
    crate::protocol::ClientKeyKind,
)> {
    outcome
        .requests
        .iter()
        .flat_map(|request| match request {
            ClientMessage::ClientShellPaneInput { pane_id, events } if pane_id == "pane_1" => {
                events
                    .iter()
                    .filter_map(|event| match event {
                        ClientPaneInputEvent::Key { code, kind, .. } => Some((code.clone(), *kind)),
                        _ => None,
                    })
                    .collect()
            }
            _ => Vec::new(),
        })
        .collect()
}

fn scrolled_to(outcome: &ClientShellInput) -> Vec<u64> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => match &request.method {
                crate::api::schema::Method::PaneScroll(params) => Some(params.offset_from_bottom),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The hinted pane's rows in a composed frame.
fn pane_rows(state: &ClientShellState, frame: &FrameData) -> Vec<String> {
    let inner = state.hits.panes[0].inner_rect;
    (inner.y..inner.bottom())
        .map(|y| {
            (inner.x..inner.right())
                .map(|x| {
                    frame.cells[usize::from(y) * usize::from(frame.width) + usize::from(x)]
                        .symbol
                        .as_str()
                })
                .collect()
        })
        .collect()
}

#[test]
fn prefix_f_labels_the_pane_and_a_label_copies_and_leaves() {
    let surface = hint_surface(&["see /tmp/a.txt", "or https://herdr.dev/x"], 30);
    let mut state = client(&Config::default(), &surface);

    let opened = state.handle_input_bytes(PREFIX_F);
    assert!(opened.requests.is_empty(), "nothing reaches the pane");
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert_eq!(
        labels(&state),
        pairs(&[("a", "https://herdr.dev/x"), ("s", "/tmp/a.txt")]),
        "the bottom-most target gets the first key"
    );

    let chosen = state.handle_input_bytes(b"s");
    assert_eq!(clipboard(&chosen).as_deref(), Some("/tmp/a.txt"));
    assert!(chosen.requests.is_empty(), "the label key stays with Herdr");
    assert_eq!(
        state
            .copy_feedback
            .as_ref()
            .map(|feedback| feedback.message.as_str()),
        Some("copied to clipboard")
    );
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert!(state.copy_mode.is_none());
}

#[test]
fn the_frozen_pane_draws_labels_and_underlines_instead_of_the_mode_bar() {
    let surface = hint_surface(&["see /tmp/a.txt", "or https://herdr.dev/x"], 30);
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_BRACKET);
    let copy_frame = state.compose(106, 20).expect("copy frame");
    assert!(
        frame_rows(&copy_frame)
            .iter()
            .any(|row| row.contains(" COPY ")),
        "copy mode shows its bar"
    );

    state.handle_input_bytes(b"f");
    let frame = state.compose(106, 20).expect("hint frame");

    assert!(
        !frame_rows(&frame).iter().any(|row| row.contains(" COPY ")),
        "no mode bar over the labels"
    );
    assert_eq!(
        pane_rows(&state, &frame)[1],
        format!("{:<30}", "or attps://herdr.dev/x"),
        "label a sits on the URL's first cell"
    );
    let inner = state.hits.panes[0].inner_rect;
    let cell = |col: u16| {
        &frame.cells
            [usize::from(inner.y + 1) * usize::from(frame.width) + usize::from(inner.x + col)]
    };
    let palette = &state.config.palette;
    assert_eq!(cell(3).bg, crate::protocol::color_to_u32(palette.yellow));
    assert_ne!(cell(3).modifier & Modifier::BOLD.bits(), 0);
    assert_ne!(
        cell(4).modifier & Modifier::UNDERLINED.bits(),
        0,
        "the URL is underlined"
    );
    assert_eq!(
        cell(2).modifier & Modifier::UNDERLINED.bits(),
        0,
        "the space is not"
    );
}

#[test]
fn labels_and_the_copied_text_hold_still_while_a_busy_pane_redraws() {
    let mut surface = hint_surface(&["⠋ build /tmp/a.txt", "waiting"], 30);
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_F);
    assert_eq!(labels(&state), pairs(&[("a", "/tmp/a.txt")]));
    let frame = state.compose(106, 20).expect("frame");
    let frozen = pane_rows(&state, &frame);

    // A spinner: a new content revision every frame, odd ones included, and
    // different text in the same cells, with the scroll metrics unchanged.
    for row in [
        "⠙ build /var/b.log",
        "⠹ build /var/c.log",
        "⠸ done  /var/d.log",
    ] {
        redraw(&mut surface, &[row, "waiting"]);
        state.set_pane_surface(surface.clone());
        assert_eq!(labels(&state), pairs(&[("a", "/tmp/a.txt")]));
        let frame = state.compose(106, 20).expect("frame");
        assert_eq!(
            pane_rows(&state, &frame),
            frozen,
            "the hinted pane holds still"
        );
    }

    let chosen = state.handle_input_bytes(b"a");
    assert_eq!(clipboard(&chosen).as_deref(), Some("/tmp/a.txt"));
}

#[test]
fn a_fullscreen_app_gets_hints_and_copying_pages_it_back_down() {
    use crate::protocol::{ClientKeyCode, ClientKeyKind};
    let stroke = |code: ClientKeyCode| {
        vec![
            (code.clone(), ClientKeyKind::Press),
            (code, ClientKeyKind::Release),
        ]
    };
    let mut surface = hint_surface(&["  ⎿ https://herdr.dev/page/2", "❯"], 40);
    surface.panes[0].alternate_screen_active = true;

    // Straight from the pane: no page was sent, so none is owed on exit.
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_F);
    assert_eq!(labels(&state), pairs(&[("a", "https://herdr.dev/page/2")]));
    let chosen = state.handle_input_bytes(b"a");
    assert_eq!(
        clipboard(&chosen).as_deref(),
        Some("https://herdr.dev/page/2")
    );
    assert!(keys_sent_to_pane(&chosen).is_empty());

    // From copy mode after paging the app up: the app redraws its older page
    // under the same scroll metrics, and copying pages it back down.
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_BRACKET);
    let page = press(&mut state, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(keys_sent_to_pane(&page), stroke(ClientKeyCode::PageUp));
    redraw(&mut surface, &["  ⎿ https://herdr.dev/page/1", "❯"]);
    state.set_pane_surface(surface.clone());

    state.handle_input_bytes(b"f");
    assert_eq!(labels(&state), pairs(&[("a", "https://herdr.dev/page/1")]));
    let chosen = state.handle_input_bytes(b"a");
    assert_eq!(
        clipboard(&chosen).as_deref(),
        Some("https://herdr.dev/page/1")
    );
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert_eq!(keys_sent_to_pane(&chosen), stroke(ClientKeyCode::PageDown));
}

#[test]
fn a_hint_copy_pages_claude_back_by_every_stroke_it_owes() {
    use crate::protocol::{ClientKeyCode, ClientKeyKind};
    let stroke = |code: ClientKeyCode| {
        vec![
            (code.clone(), ClientKeyKind::Press),
            (code, ClientKeyKind::Release),
        ]
    };
    let mut surface = hint_surface(&["  ⎿ https://herdr.dev/page/3", "❯"], 40);
    surface.panes[0].alternate_screen_active = true;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(snapshot_with(
        1,
        vec![agent_entry("pane_1", Some("claude"))],
    ));
    state.set_pane_surface(surface.clone());
    state.compose(106, 20).expect("composed frame");

    // Claude Code takes a page and a half page as one PgUp each.
    state.handle_input_bytes(PREFIX_BRACKET);
    let page = press(&mut state, KeyCode::PageUp, KeyModifiers::empty());
    assert_eq!(keys_sent_to_pane(&page), stroke(ClientKeyCode::PageUp));
    let half = press(&mut state, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(keys_sent_to_pane(&half), stroke(ClientKeyCode::PageUp));
    redraw(&mut surface, &["  ⎿ https://herdr.dev/page/1", "❯"]);
    state.set_pane_surface(surface.clone());

    state.handle_input_bytes(b"f");
    assert_eq!(labels(&state), pairs(&[("a", "https://herdr.dev/page/1")]));
    let chosen = state.handle_input_bytes(b"a");
    assert_eq!(
        clipboard(&chosen).as_deref(),
        Some("https://herdr.dev/page/1")
    );
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert_eq!(
        keys_sent_to_pane(&chosen),
        [
            stroke(ClientKeyCode::PageDown),
            stroke(ClientKeyCode::PageDown)
        ]
        .concat()
    );
}

#[test]
fn a_hint_copy_returns_codex_to_its_latest_output() {
    use crate::protocol::{ClientKeyCode, ClientKeyKind, ClientMouseKind};
    let mut surface = hint_surface(&["  ⎿ https://herdr.dev/page/3", "❯"], 40);
    surface.panes[0].alternate_screen_active = true;
    surface.panes[0].mouse_reporting = true;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(snapshot_with(1, vec![agent_entry("pane_1", Some("codex"))]));
    state.set_pane_surface(surface.clone());
    state.compose(106, 20).expect("composed frame");

    // A page up by key, then a half page up by wheel. How many notches a
    // half page takes is tests/copy.rs's concern.
    state.handle_input_bytes(PREFIX_BRACKET);
    let page = press(&mut state, KeyCode::PageUp, KeyModifiers::empty());
    assert_eq!(
        keys_sent_to_pane(&page),
        [
            (ClientKeyCode::PageUp, ClientKeyKind::Press),
            (ClientKeyCode::PageUp, ClientKeyKind::Release)
        ]
    );
    let half = press(&mut state, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert!(keys_sent_to_pane(&half).is_empty());
    let wheel = mouse_events_sent_to_pane(&half);
    assert!(!wheel.is_empty());
    assert!(wheel.iter().all(|event| matches!(
        event,
        ClientPaneInputEvent::Mouse {
            kind: ClientMouseKind::ScrollUp,
            ..
        }
    )));
    redraw(&mut surface, &["  ⎿ https://herdr.dev/page/1", "❯"]);
    state.set_pane_surface(surface.clone());

    state.handle_input_bytes(b"f");
    assert_eq!(labels(&state), pairs(&[("a", "https://herdr.dev/page/1")]));
    let chosen = state.handle_input_bytes(b"a");
    assert_eq!(
        clipboard(&chosen).as_deref(),
        Some("https://herdr.dev/page/1")
    );
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert_eq!(keys_to(&chosen, "pane_1"), ctrl_end_stroke());
    assert!(mouse_events_sent_to_pane(&chosen).is_empty());
}

#[test]
fn f_in_copy_mode_waits_for_the_page_it_asked_for_and_esc_returns_there() {
    let mut surface = hint_surface(&["/new/one", "/new/two"], 20);
    surface.panes[0].scroll = scroll(0, 20, 2);
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_BRACKET);
    let page = press(&mut state, KeyCode::PageUp, KeyModifiers::empty());
    assert_eq!(
        scrolled_to(&page),
        [1],
        "copy mode asked for the older page"
    );

    // `f` right away: the surface still shows the newest page, which must
    // not be frozen.
    state.handle_input_bytes(b"f");
    assert!(state.copy_hints_open());
    assert!(labels(&state).is_empty(), "waiting for the page");

    surface.surface_revision += 1;
    surface.panes[0].scroll = scroll(1, 20, 2);
    surface.frame = hint_surface(&["/old/zero", "/new/one"], 20).frame;
    state.set_pane_surface(surface.clone());
    assert_eq!(
        labels(&state),
        pairs(&[("a", "/new/one"), ("s", "/old/zero")])
    );
    let paged = state.copy_mode.clone().expect("copy mode");
    let [ClientShellAction::Endpoint { request, .. }] = &page.actions[..] else {
        panic!("one pane.scroll request");
    };
    state.handle_endpoint_result("boot-1", &request.id, Ok(pane_scroll_result(1, 20, 2)));

    esc(&mut state);
    assert_eq!(state.mode, ClientShellMode::Copy, "back in copy mode");
    assert!(!state.copy_hints_open());
    let copy_mode = state.copy_mode.as_ref().expect("copy mode");
    assert_eq!(copy_mode.offset_from_bottom, 1, "on the same page");
    assert_eq!(copy_mode.cursor, paged.cursor);

    state.handle_input_bytes(b"f");
    let chosen = state.handle_input_bytes(b"s");
    assert_eq!(clipboard(&chosen).as_deref(), Some("/old/zero"));
    assert_eq!(scrolled_to(&chosen), [0], "a copy lands on the live bottom");
    assert_eq!(state.mode, ClientShellMode::Terminal);
}

#[test]
fn a_copy_from_a_scrolled_pane_lands_on_the_live_bottom_and_esc_stays() {
    let mut surface = hint_surface(&["/old/zero", "/old/one"], 20);
    surface.panes[0].scroll = scroll(5, 20, 2);

    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_F);
    let chosen = state.handle_input_bytes(b"a");
    assert_eq!(clipboard(&chosen).as_deref(), Some("/old/one"));
    assert_eq!(scrolled_to(&chosen), [0], "the copy returns to the bottom");
    assert_eq!(state.mode, ClientShellMode::Terminal);

    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_F);
    let cancelled = esc(&mut state);
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert!(
        scrolled_to(&cancelled).iter().all(|offset| *offset == 5),
        "Esc leaves the pane where it was scrolled"
    );
}

fn ab_targets() -> (ClientShellState, PaneSurfaceFrame) {
    let config: Config = toml::from_str("[copy_hints]\nalphabet = \"ab\"\n").expect("config");
    let surface = hint_surface(&["/x/1", "/x/2", "/x/3"], 10);
    let mut state = client(&config, &surface);
    state.handle_input_bytes(PREFIX_F);
    assert_eq!(
        labels(&state),
        pairs(&[("a", "/x/3"), ("ba", "/x/2"), ("bb", "/x/1")])
    );
    (state, surface)
}

#[test]
fn a_typed_prefix_narrows_and_esc_clears_it_before_leaving() {
    let (mut state, _) = ab_targets();

    state.handle_input_bytes(b"b");
    assert_eq!(labels(&state), pairs(&[("ba", "/x/2"), ("bb", "/x/1")]));
    esc(&mut state);
    assert_eq!(labels(&state).len(), 3, "Esc clears the partial label");
    assert_eq!(state.mode, ClientShellMode::Copy);

    let left = esc(&mut state);
    assert!(clipboard(&left).is_none());
    assert_eq!(state.mode, ClientShellMode::Terminal, "Esc again leaves");
    assert!(state.copy_mode.is_none());
}

#[test]
fn backspace_takes_back_a_typed_key() {
    let (mut state, _) = ab_targets();

    state.handle_input_bytes(b"b");
    press(&mut state, KeyCode::Backspace, KeyModifiers::empty());
    assert_eq!(labels(&state).len(), 3);
    let chosen = state.handle_input_bytes(b"a");
    assert_eq!(clipboard(&chosen).as_deref(), Some("/x/3"));
}

#[test]
fn an_unknown_prefix_resets_without_leaving() {
    let (mut state, _) = ab_targets();

    state.handle_input_bytes(b"bq");
    assert_eq!(labels(&state).len(), 3, "bq is no label");
    assert!(state.copy_hints_open());
    let chosen = state.handle_input_bytes(b"bb");
    assert_eq!(clipboard(&chosen).as_deref(), Some("/x/1"));
}

#[test]
fn a_pane_with_no_targets_closes_hints_at_once_with_a_notice() {
    let surface = hint_surface(&["nothing to copy here", ""], 30);

    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_F);
    assert_eq!(state.mode, ClientShellMode::Terminal, "back in the pane");
    assert!(state.copy_mode.is_none());
    assert_eq!(
        state
            .copy_feedback
            .as_ref()
            .map(|feedback| feedback.message.as_str()),
        Some("no copy targets")
    );

    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_BRACKET);
    state.handle_input_bytes(b"f");
    assert_eq!(state.mode, ClientShellMode::Copy, "back in copy mode");
    assert!(state.copy_mode.is_some());
    assert!(!state.copy_hints_open());
}

fn focused_elsewhere() -> ClientShellSnapshot {
    let mut unfocused = snapshot();
    unfocused.focused_pane_id = Some("pane_2".into());
    unfocused.panes[0].focused = false;
    unfocused.panes.push(ClientShellPane {
        pane_id: "pane_2".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        label: None,
        cwd: Some("/repo".into()),
        foreground_cwd: Some("/repo".into()),
        focused: true,
        right_click_passthrough: false,
    });
    unfocused
}

#[test]
fn resizing_or_losing_focus_ends_a_session_opened_from_the_pane() {
    let mut surface = hint_surface(&["/tmp/a", ""], 20);
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_F);
    assert!(state.copy_hints_open());
    surface.surface_revision += 1;
    surface.panes[0].rect.width = 18;
    surface.panes[0].inner_rect.width = 18;
    state.set_pane_surface(surface);
    assert!(state.copy_mode.is_none(), "the capture no longer fits");
    assert_eq!(state.mode, ClientShellMode::Terminal);

    let surface = hint_surface(&["/tmp/a", ""], 20);
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_F);
    state.set_snapshot(Box::new(focused_elsewhere()));
    assert!(state.copy_mode.is_none());
    state.set_snapshot(Box::new(snapshot()));
    assert_eq!(
        state.mode,
        ClientShellMode::Terminal,
        "the pane does not come back in copy mode"
    );
}

#[test]
fn focus_loss_and_popups_close_hints_opened_in_copy_mode_and_copy_mode_resumes() {
    let surface = hint_surface(&["/tmp/a", ""], 20);
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_BRACKET);
    state.handle_input_bytes(b"f");
    state.set_snapshot(Box::new(focused_elsewhere()));
    assert!(!state.copy_hints_open());
    state.set_snapshot(Box::new(snapshot()));
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert!(!state.copy_hints_open());

    let mut surface = surface;
    state.handle_input_bytes(b"f");
    assert!(state.copy_hints_open());
    let mut with_popup = surface.clone();
    with_popup.popup = surface_with_popup().popup;
    with_popup.surface_revision += 1;
    state.set_pane_surface(with_popup);
    assert!(!state.copy_hints_open(), "a popup owns input");
    surface.surface_revision += 2;
    state.set_pane_surface(surface);
    assert_eq!(state.mode, ClientShellMode::Copy, "copy mode resumes");
    assert!(!state.copy_hints_open());
}

#[test]
fn hints_shadow_copy_mode_commands() {
    let config: Config = toml::from_str(
        r#"
[copy_hints]
alphabet = "cab"

[[keys.command]]
key = "c"
mode = "copy"
type = "plugin_action"
command = "shadowfax.comments.capture"
"#,
    )
    .expect("config");
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let mut projection = snapshot();
    projection
        .commands
        .push(crate::protocol::ClientShellCommand {
            command_id: "comments-capture".into(),
            binding_label: "c".into(),
            binding_labels: vec!["c".into()],
            action: crate::protocol::ClientShellCommandAction::CopyPluginAction,
            description: None,
        });
    state.set_snapshot(Box::new(projection));
    state.set_pane_surface(hint_surface(&["/tmp/a", ""], 20));
    state.compose(106, 20).expect("composed frame");
    let invokes = |outcome: &ClientShellInput| {
        outcome.actions.iter().any(|action| {
            matches!(
                action,
                ClientShellAction::Endpoint { request, .. }
                    if matches!(request.method, crate::api::schema::Method::CommandInvoke(_))
            )
        })
    };

    state.handle_input_bytes(PREFIX_BRACKET);
    state.handle_input_bytes(b"f");
    let typed = state.handle_input_bytes(b"c");
    assert!(!invokes(&typed), "c is a label while hints are open");
    assert_eq!(clipboard(&typed).as_deref(), Some("/tmp/a"));

    state.handle_input_bytes(PREFIX_BRACKET);
    let command = state.handle_input_bytes(b"c");
    assert!(invokes(&command), "the binding still works in copy mode");
}

#[test]
fn the_prefix_key_does_not_escape_hints() {
    let surface = hint_surface(&["/tmp/a", "/tmp/b"], 20);
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_F);

    let prefix = state.handle_input_bytes(b"\x02");
    assert!(prefix.requests.is_empty());
    assert_eq!(state.mode, ClientShellMode::Copy, "no prefix mode");
    assert_eq!(labels(&state).len(), 2);
    let chosen = state.handle_input_bytes(b"s");
    assert_eq!(clipboard(&chosen).as_deref(), Some("/tmp/a"));
}

#[test]
fn a_held_key_never_types_a_label() {
    let surface = hint_surface(&["/p/1", "/p/2", "/p/3", "/p/4"], 10);
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_BRACKET);
    press(&mut state, KeyCode::Char('f'), KeyModifiers::empty());
    assert_eq!(labels(&state).len(), 4, "f labels the pane; /p/1 gets f");

    let repeat = state.handle_raw_events(vec![RawInputEvent::Key(
        crate::input::TerminalKey::new(KeyCode::Char('f'), KeyModifiers::empty())
            .with_kind(crossterm::event::KeyEventKind::Repeat),
    )]);
    assert!(clipboard(&repeat).is_none(), "the repeat of the opening f");
    assert!(state.copy_hints_open());
}

#[test]
fn pastes_and_committed_text_reach_neither_hints_nor_the_pane() {
    let surface = hint_surface(&["/tmp/a", ""], 20);
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_F);

    let outcome = state.handle_raw_events(vec![
        RawInputEvent::Paste("a".into()),
        RawInputEvent::Text(crate::input::TextCommit::new("a")),
    ]);
    assert!(outcome.requests.is_empty());
    assert!(clipboard(&outcome).is_none());
    assert_eq!(labels(&state), pairs(&[("a", "/tmp/a")]));
    assert!(!state.modal_paste_target_active());
    assert!(state.clipboard_image_target().is_none());
}

#[test]
fn a_mouse_press_leaves_hints_before_it_acts_and_motion_does_not() {
    let surface = hint_surface(&["/tmp/a", ""], 20);
    let mut state = client(&Config::default(), &surface);
    state.handle_input_bytes(PREFIX_F);
    let inner = state.hits.panes[0].inner_rect;
    let mouse = |kind| {
        RawInputEvent::Mouse(MouseEvent {
            kind,
            column: inner.x,
            row: inner.y,
            modifiers: KeyModifiers::empty(),
        })
    };

    state.handle_raw_events(vec![mouse(MouseEventKind::Moved)]);
    assert!(state.copy_hints_open());
    state.handle_raw_events(vec![mouse(MouseEventKind::Down(MouseButton::Left))]);
    assert!(state.copy_mode.is_none());
    assert_eq!(state.mode, ClientShellMode::Terminal);
}

#[test]
fn live_reload_swaps_the_rules_and_an_invalid_table_keeps_them() {
    let surface = hint_surface(&["ticket TKT-42 in /tmp/a", ""], 30);
    let mut state = client(&Config::default(), &surface);
    let tickets: Config = toml::from_str(
        "[copy_hints]\nenabled_builtin_patterns = []\n\n[[copy_hints.patterns]]\nregex = 'TKT-[0-9]+'\n",
    )
    .expect("config");

    state.config.apply_live_config(&tickets, &[], &[]);
    state.handle_input_bytes(PREFIX_F);
    assert_eq!(labels(&state), pairs(&[("a", "TKT-42")]));
    esc(&mut state);

    state
        .config
        .apply_live_config(&Config::default(), &[], &["copy_hints".to_owned()]);
    state.handle_input_bytes(PREFIX_F);
    assert_eq!(labels(&state), pairs(&[("a", "TKT-42")]), "kept the rules");
}
