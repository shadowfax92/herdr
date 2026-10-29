//! Landing glow: a jump the user did not point at makes the landed pane's
//! border bloom and settle. These tests drive the shell's real entry points
//! (key bindings, mouse clicks, notifications, endpoint snapshots and
//! surfaces), compose frames at pinned clock times, and read cell colors on
//! the landed pane's border, its inner ring, and its neighbors.
use super::*;
use crate::config::LandingGlowConfig;
use crate::input::{KeybindAction, KeybindMatch};
use crate::protocol::CellData;
use ratatui::style::Color;
use ratatui::widgets::{Block, Widget};
use std::time::{Duration, Instant};

type Rgb = (u8, u8, u8);

const PANES: [&str; 3] = ["pane_1", "pane_2", "pane_3"];

// gruvbox, the theme the glow was designed in.
const ACCENT: Rgb = (215, 153, 33);
const OVERLAY0: Rgb = (146, 131, 116);
const PEAK_GLYPH: Rgb = (250, 189, 47);
const PEAK_BORDER_BG: Rgb = (75, 63, 39);
const PEAK_HALO_BG: Rgb = (54, 49, 39);
const HALF_GLYPH: Rgb = (233, 171, 40);
const HALF_BORDER_BG: Rgb = (58, 51, 39);
const HALF_HALO_BG: Rgb = (47, 45, 40);
const QUIET_PEAK_BORDER_BG: Rgb = (54, 49, 39);
const STATIC_BORDER_BG: Rgb = (58, 51, 39);

fn packed((r, g, b): Rgb) -> u32 {
    crate::protocol::color_to_u32(Color::Rgb(r, g, b))
}

const RESET: u32 = 0;

fn agent(pane_id: &str, status: AgentStatus) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some(pane_id.into()),
        display_agent: None,
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }
}

fn glow_state(style: LandingGlowConfig, start: Instant) -> ClientShellState {
    let mut config = Config::default();
    config.theme.name = Some("gruvbox".into());
    config.ui.landing_glow = style;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    assert_eq!(state.config.palette.accent, Color::Rgb(215, 153, 33));
    state.landing_glow.clock = Some(start);
    state
}

/// One tab with three agent panes side by side, `focused` holding focus.
fn tab_snapshot(revision: u64, focused: &str) -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.revision = revision;
    snapshot.focused_pane_id = Some(focused.into());
    snapshot.panes = PANES
        .iter()
        .map(|pane_id| ClientShellPane {
            pane_id: (*pane_id).into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            label: None,
            cwd: Some("/repo".into()),
            foreground_cwd: Some("/repo".into()),
            focused: *pane_id == focused,
            right_click_passthrough: false,
        })
        .collect();
    snapshot.agents = vec![
        agent("pane_1", AgentStatus::Idle),
        agent("pane_2", AgentStatus::Idle),
        agent("pane_3", AgentStatus::Blocked),
    ];
    snapshot
}

/// The server-rendered surface for `pane_ids`, each its own bordered box as
/// with `pane_gaps = true`: accent border on the focused pane, overlay0 on
/// the rest, and no background colors.
fn tab_surface(boot_id: &str, revision: u64, pane_ids: &[&str], focused: &str) -> PaneSurfaceFrame {
    const WIDTH: u16 = 12;
    const HEIGHT: u16 = 6;
    let palette = crate::app::state::Palette::gruvbox();
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH * pane_ids.len() as u16, HEIGHT));
    let panes = pane_ids
        .iter()
        .enumerate()
        .map(|(index, pane_id)| {
            let rect = Rect::new(index as u16 * WIDTH, 0, WIDTH, HEIGHT);
            let inner = Rect::new(rect.x + 1, rect.y + 1, WIDTH - 2, HEIGHT - 2);
            let is_focused = *pane_id == focused;
            let color = if is_focused {
                palette.accent
            } else {
                palette.overlay0
            };
            Block::bordered()
                .border_style(Style::default().fg(color))
                .title(" claude ")
                .render(rect, &mut buffer);
            buffer.set_string(inner.x, inner.y, "ok", Style::default());
            PaneSurfacePane {
                pane_id: (*pane_id).into(),
                content_revision: 0,
                rect: rect.into(),
                inner_rect: inner.into(),
                scrollbar_rect: None,
                scroll: None,
                focused: is_focused,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width: 0,
                pixel_height: 0,
            }
        })
        .collect();
    PaneSurfaceFrame {
        boot_id: boot_id.into(),
        projection_revision: revision,
        surface_revision: revision,
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]),
        panes,
        splits: Vec::new(),
        popup: None,
        graphics: crate::protocol::SurfaceGraphicsScene::default(),
    }
}

/// What the server sends once focus has moved to `focused`.
fn server_focuses(state: &mut ClientShellState, revision: u64, focused: &str) {
    state.set_snapshot(Box::new(tab_snapshot(revision, focused)));
    state.set_pane_surface(tab_surface("boot-1", revision, &PANES, focused));
}

fn start_on_pane_1(style: LandingGlowConfig, start: Instant) -> ClientShellState {
    let mut state = glow_state(style, start);
    server_focuses(&mut state, 1, "pane_1");
    state
}

fn press(state: &mut ClientShellState, action: KeybindAction) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.record_binding(KeybindMatch::Action(action), &mut outcome);
    outcome
}

fn focus_request(outcome: &ClientShellInput) -> Option<String> {
    outcome.actions.iter().find_map(|action| match action {
        ClientShellAction::Endpoint { request, .. } => match &request.method {
            crate::api::schema::Method::PaneFocus(target) => Some(target.pane_id.clone()),
            _ => None,
        },
        ClientShellAction::ActivateEndpoint {
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
            ..
        } => Some(pane_id.clone()),
        _ => None,
    })
}

fn compose_at(state: &mut ClientShellState, at: Instant) -> FrameData {
    state.landing_glow.clock = Some(at);
    state.compose(100, 20).expect("composed frame").frame
}

/// Colors at three points of one pane: a left border cell, a top border cell
/// inside the title, and the content cell just inside the left border (the
/// inner ring), all on rows away from the corners.
#[derive(Debug, PartialEq, Eq)]
struct PaneColors {
    border_fg: u32,
    border_bg: u32,
    title_fg: u32,
    title_bg: u32,
    halo_bg: u32,
}

fn cell(frame: &FrameData, x: u16, y: u16) -> &CellData {
    &frame.cells[usize::from(y) * usize::from(frame.width) + usize::from(x)]
}

fn pane_colors(state: &ClientShellState, frame: &FrameData, pane_id: &str) -> PaneColors {
    let hit = state
        .hits
        .panes
        .iter()
        .find(|hit| hit.pane_id == pane_id)
        .unwrap_or_else(|| panic!("{pane_id} is not in the surface"));
    let border = cell(frame, hit.rect.x, hit.rect.y + 2);
    assert_eq!(border.symbol, "│", "left border of {pane_id}");
    let title = cell(frame, hit.rect.x + 2, hit.rect.y);
    assert_eq!(title.symbol, "c", "title of {pane_id}");
    let halo = cell(frame, hit.inner_rect.x, hit.inner_rect.y + 2);
    PaneColors {
        border_fg: border.fg,
        border_bg: border.bg,
        title_fg: title.fg,
        title_bg: title.bg,
        halo_bg: halo.bg,
    }
}

fn glowing(glyph: Rgb, border_bg: Rgb, halo_bg: Option<Rgb>) -> PaneColors {
    PaneColors {
        border_fg: packed(glyph),
        border_bg: packed(border_bg),
        title_fg: packed(glyph),
        title_bg: packed(border_bg),
        halo_bg: halo_bg.map_or(RESET, packed),
    }
}

fn plain(border: Rgb) -> PaneColors {
    PaneColors {
        border_fg: packed(border),
        border_bg: RESET,
        title_fg: packed(border),
        title_bg: RESET,
        halo_bg: RESET,
    }
}

/// Lands on pane_2 with next_agent and composes the landing frame at
/// `landed_at`. Returns that frame.
fn land_on_pane_2(state: &mut ClientShellState, landed_at: Instant) -> FrameData {
    let outcome = press(state, KeybindAction::NextAgent);
    assert_eq!(focus_request(&outcome).as_deref(), Some("pane_2"));
    server_focuses(state, 2, "pane_2");
    compose_at(state, landed_at)
}

#[test]
fn ring_blooms_peaks_and_settles_on_the_landed_pane_only() {
    let t0 = Instant::now();
    let mut state = start_on_pane_1(LandingGlowConfig::Ring, t0);
    let before = compose_at(&mut state, t0);
    assert_eq!(pane_colors(&state, &before, "pane_2"), plain(OVERLAY0));

    // The key press goes out at t0; the server's answer lands 40 ms later. The
    // clock starts there, not at the press.
    let landed = t0 + Duration::from_millis(40);
    let frame = land_on_pane_2(&mut state, landed);
    assert_eq!(
        pane_colors(&state, &frame, "pane_2"),
        glowing(HALF_GLYPH, HALF_BORDER_BG, Some(HALF_HALO_BG)),
        "landing frame is level 0.5"
    );
    assert_eq!(pane_colors(&state, &frame, "pane_1"), plain(OVERLAY0));
    assert_eq!(pane_colors(&state, &frame, "pane_3"), plain(OVERLAY0));

    // Between steps the timer has nothing to do.
    assert!(!state.tick_landing_glow(landed + Duration::from_millis(60)));
    assert!(state.tick_landing_glow(landed + Duration::from_millis(70)));

    let frame = compose_at(&mut state, landed + Duration::from_millis(120));
    assert_eq!(
        pane_colors(&state, &frame, "pane_2"),
        glowing(PEAK_GLYPH, PEAK_BORDER_BG, Some(PEAK_HALO_BG)),
        "peak at 120 ms"
    );
    assert!(!state.tick_landing_glow(landed + Duration::from_millis(329)));

    let frame = compose_at(&mut state, landed + Duration::from_millis(330));
    let decaying = pane_colors(&state, &frame, "pane_2");
    assert_eq!(decaying.border_fg, packed((241, 180, 44)));
    assert_eq!(decaying.border_bg, packed((66, 57, 39)));
    assert!(
        decaying.halo_bg != packed(PEAK_HALO_BG) && decaying.halo_bg != packed(HALF_HALO_BG),
        "level 0.75 halo sits between half and peak: {:#x}",
        decaying.halo_bg
    );

    let frame = compose_at(&mut state, landed + Duration::from_millis(515));
    assert_eq!(
        pane_colors(&state, &frame, "pane_2").border_bg,
        packed((49, 46, 40)),
        "level 0.25"
    );

    // Settled: the server's own focus border, and no timer left.
    let frame = compose_at(&mut state, landed + Duration::from_millis(575));
    assert_eq!(pane_colors(&state, &frame, "pane_2"), plain(ACCENT));
    assert_eq!(state.landing_glow.next_deadline(), None);
    assert!(!state.tick_landing_glow(landed + Duration::from_secs(5)));
}

#[test]
fn a_burst_of_jumps_and_a_lone_pane_play_quiet() {
    let t0 = Instant::now();
    let mut state = start_on_pane_1(LandingGlowConfig::Ring, t0);
    compose_at(&mut state, t0);
    let frame = land_on_pane_2(&mut state, t0);
    assert_eq!(
        pane_colors(&state, &frame, "pane_2").border_bg,
        packed(HALF_BORDER_BG)
    );

    // Next press 200 ms later: inside the burst window.
    let outcome = press(&mut state, KeybindAction::NextAgent);
    assert_eq!(focus_request(&outcome).as_deref(), Some("pane_3"));
    server_focuses(&mut state, 3, "pane_3");
    let frame = compose_at(&mut state, t0 + Duration::from_millis(200));
    assert_eq!(
        pane_colors(&state, &frame, "pane_3"),
        glowing(PEAK_GLYPH, QUIET_PEAK_BORDER_BG, None),
        "quiet: border only, 8% tint"
    );
    assert_eq!(pane_colors(&state, &frame, "pane_2"), plain(OVERLAY0));
    let frame = compose_at(&mut state, t0 + Duration::from_millis(200 + 180));
    // Quiet's half level tints 4% of the accent, the same mix as Ring's half halo.
    assert_eq!(
        pane_colors(&state, &frame, "pane_3"),
        glowing(HALF_GLYPH, HALF_HALO_BG, None),
        "quiet half level at 180 ms"
    );
    let frame = compose_at(&mut state, t0 + Duration::from_millis(200 + 310));
    assert_eq!(pane_colors(&state, &frame, "pane_3"), plain(ACCENT));

    // A zoomed tab surfaces only the zoomed pane: Quiet even outside a burst.
    let later = t0 + Duration::from_secs(5);
    state.landing_glow.clock = Some(later);
    let outcome = press(&mut state, KeybindAction::NextAgent);
    assert_eq!(focus_request(&outcome).as_deref(), Some("pane_1"));
    state.set_snapshot(Box::new(tab_snapshot(4, "pane_1")));
    state.set_pane_surface(tab_surface("boot-1", 4, &["pane_1"], "pane_1"));
    let frame = compose_at(&mut state, later);
    assert_eq!(
        pane_colors(&state, &frame, "pane_1"),
        glowing(PEAK_GLYPH, QUIET_PEAK_BORDER_BG, None)
    );
}

#[test]
fn static_holds_until_a_key_or_500_ms_and_off_draws_nothing() {
    let t0 = Instant::now();
    let mut state = start_on_pane_1(LandingGlowConfig::Static, t0);
    compose_at(&mut state, t0);
    let frame = land_on_pane_2(&mut state, t0);
    let lit = glowing(PEAK_GLYPH, STATIC_BORDER_BG, None);
    assert_eq!(pane_colors(&state, &frame, "pane_2"), lit);
    let frame = compose_at(&mut state, t0 + Duration::from_millis(499));
    assert_eq!(
        pane_colors(&state, &frame, "pane_2"),
        lit,
        "no intermediate step"
    );

    // The first key sent to the pane clears it in one step.
    let typed = state.handle_raw_events(vec![RawInputEvent::Text(crate::input::TextCommit::new(
        "y",
    ))]);
    assert!(typed.repaint, "clearing repaints");
    let frame = compose_at(&mut state, t0 + Duration::from_millis(499));
    assert_eq!(pane_colors(&state, &frame, "pane_2"), plain(ACCENT));

    // Untouched, it clears on its own at 500 ms.
    state.landing_glow.clock = Some(t0 + Duration::from_secs(5));
    press(&mut state, KeybindAction::NextAgent);
    server_focuses(&mut state, 3, "pane_3");
    let landed = t0 + Duration::from_secs(5);
    let frame = compose_at(&mut state, landed);
    assert_eq!(pane_colors(&state, &frame, "pane_3"), lit);
    let frame = compose_at(&mut state, landed + Duration::from_millis(500));
    assert_eq!(pane_colors(&state, &frame, "pane_3"), plain(ACCENT));

    // Switched off by a config reload, the same jump draws nothing and arms
    // no timer.
    state.config.landing_glow = LandingGlowConfig::Off;
    let later = t0 + Duration::from_secs(10);
    state.landing_glow.clock = Some(later);
    let outcome = press(&mut state, KeybindAction::NextAgent);
    assert_eq!(focus_request(&outcome).as_deref(), Some("pane_1"));
    server_focuses(&mut state, 4, "pane_1");
    let frame = compose_at(&mut state, later);
    assert_eq!(pane_colors(&state, &frame, "pane_1"), plain(ACCENT));
    assert_eq!(state.landing_glow.next_deadline(), None);
}

#[test]
fn directional_focus_and_clicks_inside_a_pane_do_not_glow() {
    let t0 = Instant::now();
    let mut state = start_on_pane_1(LandingGlowConfig::Ring, t0);
    compose_at(&mut state, t0);
    // Control: an agent jump glows.
    let frame = land_on_pane_2(&mut state, t0);
    assert_eq!(
        pane_colors(&state, &frame, "pane_2").border_bg,
        packed(HALF_BORDER_BG)
    );

    // A direction key mid-glow: focus leaving pane_2 ends its glow, and the
    // pane it lands on gets none.
    let later = t0 + Duration::from_millis(100);
    state.landing_glow.clock = Some(later);
    let outcome = press(&mut state, KeybindAction::FocusPaneRight);
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(request.method, crate::api::schema::Method::PaneFocusDirection(_))
    )));
    server_focuses(&mut state, 3, "pane_3");
    let frame = compose_at(&mut state, later);
    assert_eq!(pane_colors(&state, &frame, "pane_3"), plain(ACCENT));
    assert_eq!(pane_colors(&state, &frame, "pane_2"), plain(OVERLAY0));

    // So does clicking into a pane.
    let pane_1 = state
        .hits
        .panes
        .iter()
        .find(|hit| hit.pane_id == "pane_1")
        .expect("pane_1 hit")
        .inner_rect;
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane_1.x + 3,
        row: pane_1.y + 2,
        modifiers: KeyModifiers::empty(),
    })]);
    assert_eq!(focus_request(&click).as_deref(), Some("pane_1"));
    server_focuses(&mut state, 4, "pane_1");
    let frame = compose_at(&mut state, later + Duration::from_millis(10));
    assert_eq!(pane_colors(&state, &frame, "pane_1"), plain(ACCENT));
    assert_eq!(state.landing_glow.next_deadline(), None);
}

#[test]
fn sidebar_agent_clicks_and_notifications_glow() {
    let t0 = Instant::now();
    let mut config = Config::default();
    config.theme.name = Some("gruvbox".into());
    let mut shell_config = ClientShellConfig::from_config(&config);
    shell_config.toast_delivery = crate::config::ToastDelivery::Herdr;
    shell_config.toast_delay_seconds = 0;
    let mut state = ClientShellState::new(shell_config);
    state.landing_glow.clock = Some(t0);
    server_focuses(&mut state, 1, "pane_1");
    compose_at(&mut state, t0);

    let row = state
        .hits
        .agents
        .iter()
        .find(|(_, pane_id)| pane_id == "pane_2")
        .map(|(rect, _)| *rect)
        .expect("pane_2 agent row");
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: row.x + 1,
        row: row.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert_eq!(focus_request(&click).as_deref(), Some("pane_2"));
    server_focuses(&mut state, 2, "pane_2");
    let frame = compose_at(&mut state, t0);
    assert_eq!(
        pane_colors(&state, &frame, "pane_2"),
        glowing(HALF_GLYPH, HALF_BORDER_BG, Some(HALF_HALO_BG))
    );

    let later = t0 + Duration::from_secs(2);
    state.landing_glow.clock = Some(later);
    state.receive_notification(
        &ClientEndpointId::Local,
        SemanticNotification {
            kind: SemanticNotificationKind::NeedsAttention,
            title: "claude needs attention".into(),
            body: None,
            sound: None,
            agent: Some("claude".into()),
            workspace_id: None,
            tab_id: None,
            pane_id: Some("pane_3".into()),
            position: None,
        },
        later,
    );
    let open = press(&mut state, KeybindAction::OpenNotificationTarget);
    assert_eq!(focus_request(&open).as_deref(), Some("pane_3"));
    server_focuses(&mut state, 3, "pane_3");
    let frame = compose_at(&mut state, later);
    assert_eq!(
        pane_colors(&state, &frame, "pane_3"),
        glowing(HALF_GLYPH, HALF_BORDER_BG, Some(HALF_HALO_BG))
    );
}

#[test]
fn a_jump_to_another_machine_glows_when_its_surface_arrives() {
    let t0 = Instant::now();
    let mut state = glow_state(LandingGlowConfig::Ring, t0);
    let profile = SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
            .expect("profile id"),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
    };
    let remote_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote_id, ClientEndpointStatus::Online);
    let mut local = tab_snapshot(1, "pane_1");
    local.agents = vec![agent("pane_1", AgentStatus::Idle)];
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(tab_surface("boot-1", 1, &PANES, "pane_1"));
    let mut remote = tab_snapshot(1, "pane_2");
    remote.boot_id = "remote-boot".into();
    remote.agents = vec![agent("pane_2", AgentStatus::Blocked)];
    state.set_endpoint_snapshot(&remote_id, Box::new(remote));
    compose_at(&mut state, t0);

    // The multi-machine agent walk hands the jump to the runtime.
    let outcome = press(&mut state, KeybindAction::NextAgent);
    assert!(
        matches!(
            outcome.actions.as_slice(),
            [ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
            }] if endpoint_id == &remote_id && pane_id == "pane_2"
        ),
        "{:?}",
        outcome.actions
    );
    // Frames from the old machine keep coming while the handoff runs; the
    // local pane_2 is a different pane and must not glow.
    let frame = compose_at(&mut state, t0 + Duration::from_millis(300));
    assert_eq!(pane_colors(&state, &frame, "pane_2"), plain(OVERLAY0));

    // The handoff completes 1.5 s later. The glow starts on that first frame,
    // so the round trip does not eat the peak.
    let landed = t0 + Duration::from_millis(1500);
    assert!(state.activate_endpoint_projection(&remote_id));
    state.set_pane_surface(tab_surface("remote-boot", 1, &PANES, "pane_2"));
    let frame = compose_at(&mut state, landed);
    assert_eq!(
        pane_colors(&state, &frame, "pane_2"),
        glowing(HALF_GLYPH, HALF_BORDER_BG, Some(HALF_HALO_BG))
    );
    assert_eq!(pane_colors(&state, &frame, "pane_1"), plain(OVERLAY0));
    let frame = compose_at(&mut state, landed + Duration::from_millis(120));
    assert_eq!(
        pane_colors(&state, &frame, "pane_2"),
        glowing(PEAK_GLYPH, PEAK_BORDER_BG, Some(PEAK_HALO_BG))
    );
}

#[test]
fn a_new_jump_clears_the_old_glow_and_relanding_does_not_restart_it() {
    let t0 = Instant::now();
    let mut state = start_on_pane_1(LandingGlowConfig::Ring, t0);
    compose_at(&mut state, t0);
    land_on_pane_2(&mut state, t0);

    // focus_agent onto the pane that is already glowing: no restart, so at
    // 330 ms it is still decaying from the first landing.
    state.landing_glow.clock = Some(t0 + Duration::from_millis(300));
    let again = press(&mut state, KeybindAction::FocusAgent(1));
    assert_eq!(focus_request(&again).as_deref(), Some("pane_2"));
    let frame = compose_at(&mut state, t0 + Duration::from_millis(330));
    assert_eq!(
        pane_colors(&state, &frame, "pane_2").border_fg,
        packed((241, 180, 44))
    );

    // A jump elsewhere clears pane_2 in the very next frame, before the
    // server has even moved focus.
    let jump_at = t0 + Duration::from_millis(340);
    state.landing_glow.clock = Some(jump_at);
    let outcome = press(&mut state, KeybindAction::NextAgent);
    assert_eq!(focus_request(&outcome).as_deref(), Some("pane_3"));
    let frame = compose_at(&mut state, jump_at);
    assert_eq!(pane_colors(&state, &frame, "pane_2"), plain(ACCENT));

    // The landing comes after the burst window, so it is a full Ring.
    server_focuses(&mut state, 3, "pane_3");
    let frame = compose_at(&mut state, t0 + Duration::from_millis(500));
    assert_eq!(
        pane_colors(&state, &frame, "pane_3"),
        glowing(HALF_GLYPH, HALF_BORDER_BG, Some(HALF_HALO_BG))
    );
    assert_eq!(pane_colors(&state, &frame, "pane_2"), plain(OVERLAY0));
}

#[test]
fn landing_glow_parses_and_defaults_to_ring() {
    assert_eq!(Config::default().ui.landing_glow, LandingGlowConfig::Ring);
    for (value, expected) in [
        ("ring", LandingGlowConfig::Ring),
        ("quiet", LandingGlowConfig::Quiet),
        ("static", LandingGlowConfig::Static),
        ("off", LandingGlowConfig::Off),
    ] {
        let config: Config =
            toml::from_str(&format!("[ui]\nlanding_glow = \"{value}\"\n")).expect("config");
        assert_eq!(config.ui.landing_glow, expected);
    }
    assert!(toml::from_str::<Config>("[ui]\nlanding_glow = \"pulse\"\n").is_err());
}
