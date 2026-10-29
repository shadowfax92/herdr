//! Landing glow: a short color pulse on the pane a jump lands on.
//!
//! A jump whose destination the user did not point at (next/previous/focus
//! agent, `next_attention_agent`, a sidebar or mobile agent pick, a navigator
//! pane pick, a notification) moves focus into a tab that often holds several
//! panes. The landed pane's border then blooms into a one-cell ring and
//! settles back into the normal focus border, the cmux attention pulse
//! translated into cells.
//!
//! The glow is presentation state, so it lives entirely in the client. The
//! server keeps rendering plain borders; this module recolors the composed
//! frame's cells for the one landed pane, after the server surface is blitted
//! and before the mode bar, selection, popups and toasts draw over it.
//!
//! ```text
//!   jump ──arm()──► pending { machine, pane }
//!                      │  first composed frame whose surface shows that
//!                      │  pane focused on that machine
//!                      ▼
//!                   active { variant, started_at } ──► recolor every frame
//!                      │  level changes at fixed step times; the client
//!                      │  timer wakes only at the next step
//!                      ▼
//!                   cleared: last step, focus moved away, or a new jump
//! ```
//!
//! The clock starts on the frame that first shows the pane focused, not at
//! the key press, so a remote machine's round trip cannot eat the peak.

use std::time::{Duration, Instant};

use super::*;
use crate::config::LandingGlowConfig;
use crate::protocol::CellData;

type Rgb = (u8, u8, u8);

/// A landing this soon after the previous one plays Quiet instead of Ring, so
/// holding a jump key gives a faint tick per pane instead of a strobe.
const BURST_WINDOW: Duration = Duration::from_millis(400);

/// An armed jump whose focus never shows up (a rejected request, a failed
/// machine handoff) is dropped after this, so a much later, unrelated focus
/// change to the same pane does not glow.
const PENDING_TIMEOUT: Duration = Duration::from_secs(3);

/// Box-drawing glyphs the server uses for pane borders and shared dividers.
const LINE_GLYPHS: [&str; 11] = ["─", "│", "┌", "┐", "└", "┘", "├", "┤", "┬", "┴", "┼"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    Ring,
    Quiet,
    Static,
}

impl Variant {
    /// `(ms after the landing frame, level in quarters of the peak)`.
    ///
    /// Ring's times are where cmux's 600 ms pulse (peak at 30%, quadratic
    /// ease-out up, quadratic ease-in down) crosses each quarter level, so each
    /// step costs exactly one repaint. Every table ends at level 0: the glow is
    /// over and no timer remains.
    fn steps(self) -> &'static [(u64, u8)] {
        match self {
            Self::Ring => &[
                (0, 2),
                (70, 3),
                (120, 4),
                (330, 3),
                (440, 2),
                (515, 1),
                (575, 0),
            ],
            Self::Quiet => &[(0, 4), (180, 2), (310, 0)],
            Self::Static => &[(0, 4), (500, 0)],
        }
    }

    /// Share of the accent blended into border-cell backgrounds at full level.
    /// Ring's 0.20 is exactly gruvbox's own `selection_bg`.
    fn border_tint(self) -> f32 {
        match self {
            Self::Ring => 0.20,
            Self::Quiet => 0.08,
            Self::Static => 0.10,
        }
    }

    /// Share for the ring of content cells just inside the border. It stands in
    /// for the falloff of cmux's soft shadow; only Ring has one.
    fn halo_tint(self) -> f32 {
        match self {
            Self::Ring => 0.08,
            Self::Quiet | Self::Static => 0.0,
        }
    }

    /// Index of the step showing `elapsed` after the landing frame, or None
    /// once the glow has reached its final level 0.
    fn step_at(self, elapsed: Duration) -> Option<usize> {
        let elapsed_ms = elapsed.as_millis();
        let steps = self.steps();
        let index = steps
            .iter()
            .rposition(|(at, _)| u128::from(*at) <= elapsed_ms)?;
        (steps[index].1 > 0).then_some(index)
    }
}

/// A jump waiting for the frame that shows it landed.
struct Pending {
    endpoint_id: ClientEndpointId,
    pane_id: String,
    armed_at: Instant,
}

struct Active {
    endpoint_id: ClientEndpointId,
    pane_id: String,
    variant: Variant,
    started_at: Instant,
    /// The step drawn last; the next repaint deadline is the step after it.
    step: usize,
}

impl Active {
    fn is(&self, endpoint_id: &ClientEndpointId, pane_id: &str) -> bool {
        &self.endpoint_id == endpoint_id && self.pane_id == pane_id
    }
}

/// One glow slot per client: a new jump replaces whatever was pending or
/// glowing.
#[derive(Default)]
pub(super) struct LandingGlow {
    pending: Option<Pending>,
    active: Option<Active>,
    /// When the previous glow started, for the burst rule.
    last_landing_at: Option<Instant>,
    /// Tests pin the clock here instead of sleeping.
    #[cfg(test)]
    pub(super) clock: Option<Instant>,
}

impl LandingGlow {
    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = self.clock {
            return now;
        }
        Instant::now()
    }

    /// Records a jump. Any glow on another pane ends at once; the new one waits
    /// for `compose` to see its pane focused.
    ///
    /// A jump to the pane that already has focus moves nothing, so there is
    /// nothing to find: it only cancels an older pending jump. That also means
    /// landing again on the glowing pane leaves its glow running instead of
    /// restarting it.
    fn arm(&mut self, endpoint_id: &ClientEndpointId, pane_id: &str, already_focused: bool) {
        if already_focused {
            self.pending = None;
            return;
        }
        self.active = None;
        self.pending = Some(Pending {
            endpoint_id: endpoint_id.clone(),
            pane_id: pane_id.to_owned(),
            armed_at: self.now(),
        });
    }

    /// When the client must recompose for the next level, if a glow is running.
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        let active = self.active.as_ref()?;
        let (at, _) = active.variant.steps().get(active.step + 1)?;
        Some(active.started_at + Duration::from_millis(*at))
    }

    /// Reduced motion clears on the first key or text sent to the glowing pane
    /// instead of waiting out its 500 ms. Returns whether a repaint is needed.
    pub(super) fn clear_static_on_input(&mut self, requests: &[ClientMessage]) -> bool {
        let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.variant == Variant::Static)
        else {
            return false;
        };
        let typed = requests.iter().any(|request| {
            matches!(
                request,
                ClientMessage::ClientShellPaneInput { pane_id, events }
                    if *pane_id == active.pane_id
                        && events.iter().any(|event| !matches!(event, ClientPaneInputEvent::Mouse { .. }))
            )
        });
        if typed {
            self.active = None;
        }
        typed
    }

    /// Starts a pending glow once `surface` shows its pane focused, then
    /// recolors the active glow's cells in `frame`. `origin` is where the
    /// surface sits in the frame.
    ///
    /// Runs on every composed frame and returns at once when no glow is pending
    /// or running, so a client without a recent jump pays one branch. While a
    /// glow runs it touches only the landed pane's border and inner ring.
    pub(super) fn compose(
        &mut self,
        frame: &mut FrameData,
        surface: &PaneSurfaceFrame,
        origin: Rect,
        endpoint_id: &ClientEndpointId,
        config: &ClientShellConfig,
        host_background: Option<crate::terminal_theme::RgbColor>,
    ) {
        if self.pending.is_none() && self.active.is_none() {
            return;
        }
        if config.landing_glow == LandingGlowConfig::Off {
            self.pending = None;
            self.active = None;
            return;
        }
        let now = self.now();
        let focused = surface.panes.iter().find(|pane| pane.focused);
        if let Some(pending) = self.pending.take() {
            let landed = &pending.endpoint_id == endpoint_id
                && focused.is_some_and(|pane| pane.pane_id == pending.pane_id);
            if landed {
                let burst = self
                    .last_landing_at
                    .is_some_and(|at| now.duration_since(at) < BURST_WINDOW);
                // A zoomed tab surfaces only its zoomed pane, so this also
                // covers zoom: with one pane visible the glow only confirms.
                let lone_pane = surface.panes.len() == 1;
                let variant = match config.landing_glow {
                    LandingGlowConfig::Ring if burst || lone_pane => Variant::Quiet,
                    LandingGlowConfig::Ring => Variant::Ring,
                    LandingGlowConfig::Quiet => Variant::Quiet,
                    LandingGlowConfig::Static | LandingGlowConfig::Off => Variant::Static,
                };
                self.active = Some(Active {
                    endpoint_id: pending.endpoint_id,
                    pane_id: pending.pane_id,
                    variant,
                    started_at: now,
                    step: 0,
                });
                self.last_landing_at = Some(now);
            } else if now.duration_since(pending.armed_at) <= PENDING_TIMEOUT {
                self.pending = Some(pending);
            }
        }
        let Some(active) = self.active.as_mut() else {
            return;
        };
        // The glow belongs to the focused pane. Focus moving anywhere else,
        // including another machine, ends it.
        let Some(pane) = focused.filter(|pane| active.is(endpoint_id, &pane.pane_id)) else {
            self.active = None;
            return;
        };
        let Some(step) = active
            .variant
            .step_at(now.duration_since(active.started_at))
        else {
            self.active = None;
            return;
        };
        active.step = step;
        let level = f32::from(active.variant.steps()[step].1) / 4.0;
        let offset = |rect: crate::protocol::SurfaceRect| {
            Rect::new(
                origin.x.saturating_add(rect.x),
                origin.y.saturating_add(rect.y),
                rect.width,
                rect.height,
            )
        };
        paint(
            frame,
            PaneCells {
                rect: offset(pane.rect),
                inner: offset(pane.inner_rect),
                surface: origin,
            },
            active.variant,
            level,
            Colors::resolve(&config.palette, host_background),
        );
    }
}

impl ClientShellState {
    /// Arms the landing glow for a jump to `pane_id` on `endpoint_id`.
    ///
    /// Arming never asks for a repaint: the server's focus change brings the
    /// frame the glow starts on, and a jump that moves nothing does not glow.
    pub(super) fn arm_landing_glow(&mut self, endpoint_id: &ClientEndpointId, pane_id: &str) {
        if self.config.landing_glow == LandingGlowConfig::Off {
            return;
        }
        let already_focused = endpoint_id == &self.active_endpoint_id
            && self.focused_pane_id().as_deref() == Some(pane_id);
        self.landing_glow.arm(endpoint_id, pane_id, already_focused);
    }

    /// `arm_landing_glow` for a pane on the active machine.
    pub(super) fn arm_landing_glow_here(&mut self, pane_id: &str) {
        let endpoint_id = self.active_endpoint_id.clone();
        self.arm_landing_glow(&endpoint_id, pane_id);
    }

    /// Arms for the pane a focus target lands on. Workspace and tab switches
    /// do not glow.
    pub(super) fn arm_landing_glow_for_target(
        &mut self,
        endpoint_id: &ClientEndpointId,
        target: &ClientEndpointFocusTarget,
    ) {
        let pane_id = match target {
            ClientEndpointFocusTarget::Pane(pane_id) => pane_id,
            #[cfg(windows)]
            ClientEndpointFocusTarget::Notification { pane_id, .. } => pane_id,
            ClientEndpointFocusTarget::Workspace(_) | ClientEndpointFocusTarget::Tab(_) => return,
        };
        self.arm_landing_glow(endpoint_id, pane_id);
    }

    /// Single-machine agent keys resolve to a `PaneFocus` request before it is
    /// sent; arm for its pane. Every other action is left alone.
    pub(super) fn arm_landing_glow_for_action(
        &mut self,
        action: crate::input::KeybindAction,
        method: &crate::api::schema::Method,
    ) {
        use crate::input::KeybindAction;
        if !matches!(
            action,
            KeybindAction::FocusAgent(_) | KeybindAction::PreviousAgent | KeybindAction::NextAgent
        ) {
            return;
        }
        if let crate::api::schema::Method::PaneFocus(target) = method {
            self.arm_landing_glow_here(&target.pane_id);
        }
    }

    /// Timer hook: whether the running glow has reached its next level.
    pub(crate) fn tick_landing_glow(&mut self, now: Instant) -> bool {
        self.landing_glow
            .next_deadline()
            .is_some_and(|deadline| now >= deadline)
    }
}

/// The landed pane's geometry in frame coordinates.
struct PaneCells {
    rect: Rect,
    inner: Rect,
    /// The pane surface's area. Shared dividers outside the pane are recolored
    /// only inside it, never in the sidebar or tab bar.
    surface: Rect,
}

/// The palette colors the glow is derived from, all as RGB so they can be
/// blended per cell.
struct Colors {
    accent: Rgb,
    /// The border glyph color at full level.
    peak: Rgb,
    /// What a `Reset` background really looks like, when known.
    base: Option<Rgb>,
}

impl Colors {
    /// None when the theme's accent is not an RGB color (the 16-color
    /// terminal theme); the glow then only bolds the border.
    fn resolve(
        palette: &Palette,
        host_background: Option<crate::terminal_theme::RgbColor>,
    ) -> Option<Self> {
        let accent = rgb(palette.accent)?;
        let base = host_background
            .map(|color| (color.r, color.g, color.b))
            .or_else(|| rgb(palette.panel_bg))
            .or_else(|| rgb(palette.surface_dim));
        let dark = base.is_none_or(|base| luminance(base) < 0.5);
        // The spec's peak is the palette's yellow, which for gruvbox is the
        // bright twin of its accent. For themes whose yellow is another hue,
        // mixing accent toward it would pass through a third color, so the
        // accent is pushed toward white on dark grounds (black on light).
        let peak = rgb(palette.yellow)
            .filter(|yellow| {
                same_hue_family(accent, *yellow) && (luminance(*yellow) > luminance(accent)) == dark
            })
            .unwrap_or_else(|| {
                let target = if dark { (255, 255, 255) } else { (0, 0, 0) };
                mix(accent, target, 0.4)
            });
        Some(Self { accent, peak, base })
    }
}

/// Recolors one frame of the glow at `level` (0 to 1 of the peak).
///
/// Border cells (glyphs and the title on the border row) take the glyph color
/// and a background tint; for Ring the ring of cells just inside takes a
/// fainter background tint. Glyphs and content colors are never changed. Each
/// cell is visited once, since blending twice would double the tint.
fn paint(
    frame: &mut FrameData,
    pane: PaneCells,
    variant: Variant,
    level: f32,
    colors: Option<Colors>,
) {
    let PaneCells {
        rect,
        inner,
        surface,
    } = pane;
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let border_tint = variant.border_tint() * level;
    let halo_tint = variant.halo_tint() * level;
    let glyph = colors
        .as_ref()
        .map(|colors| mix(colors.accent, colors.peak, level));
    let border_cell = |cell: &mut CellData| match (&colors, glyph) {
        (Some(colors), Some(glyph)) => {
            cell.fg = pack(glyph);
            blend_bg(cell, colors, border_tint);
        }
        _ if level >= 0.5 => cell.modifier |= Modifier::BOLD.bits(),
        _ => {}
    };
    let tint_cell = |cell: &mut CellData, amount: f32| {
        if let Some(colors) = &colors {
            blend_bg(cell, colors, amount);
        }
    };

    let top = inner.y > rect.y;
    let left = inner.x > rect.x;
    let bottom = inner.bottom() < rect.bottom();
    // A one-column gap on the right is either a border or a reserved scrollbar
    // column; only the border holds a line glyph.
    let right = match rect.right().saturating_sub(inner.right()) {
        0 => false,
        1 => is_line_glyph(frame, rect.right() - 1, inner.y),
        _ => true,
    };

    if !(top || bottom || left || right) {
        // Borderless pane (a lone pane, or `pane_borders = "off"`): the
        // outermost cells carry the border tint and keep their glyph colors.
        for_each_perimeter_cell(frame, rect, |cell| tint_cell(cell, border_tint));
        let halo = Rect::new(
            rect.x + 1,
            rect.y + 1,
            rect.width.saturating_sub(2),
            rect.height.saturating_sub(2),
        );
        for_each_perimeter_cell(frame, halo, |cell| tint_cell(cell, halo_tint));
        return;
    }

    let last_row = rect.bottom() - 1;
    let last_col = rect.right() - 1;
    for y in rect.y..rect.bottom() {
        if (top && y == rect.y) || (bottom && y == last_row) {
            for x in rect.x..rect.right() {
                with_cell(frame, x, y, border_cell);
            }
            continue;
        }
        if left {
            with_cell(frame, rect.x, y, border_cell);
        }
        if right {
            with_cell(frame, last_col, y, border_cell);
        }
    }

    // Without pane gaps neighbors share one divider, drawn as the right or
    // bottom neighbor's border. The server already colors the landed pane's
    // share of it as focused, so it glows too.
    let mut shared_divider = |x: u16, y: u16| {
        if super::contains(surface, (x, y)) && is_line_glyph(frame, x, y) {
            with_cell(frame, x, y, border_cell);
        }
    };
    if !right {
        for y in rect.y..rect.bottom() {
            shared_divider(rect.right(), y);
        }
    }
    if !bottom {
        for x in rect.x..rect.right() {
            shared_divider(x, rect.bottom());
        }
    }
    if !right && !bottom {
        shared_divider(rect.right(), rect.bottom());
    }

    if halo_tint > 0.0 {
        let halo = Rect::new(
            rect.x + u16::from(left),
            rect.y + u16::from(top),
            rect.width - u16::from(left) - u16::from(right),
            rect.height - u16::from(top) - u16::from(bottom),
        );
        for_each_perimeter_cell(frame, halo, |cell| tint_cell(cell, halo_tint));
    }
}

fn with_cell(frame: &mut FrameData, x: u16, y: u16, edit: impl Fn(&mut CellData)) {
    if x >= frame.width || y >= frame.height {
        return;
    }
    let index = usize::from(y) * usize::from(frame.width) + usize::from(x);
    if let Some(cell) = frame.cells.get_mut(index) {
        edit(cell);
    }
}

fn for_each_perimeter_cell(frame: &mut FrameData, area: Rect, edit: impl Fn(&mut CellData)) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let last_row = area.bottom() - 1;
    for y in area.y..area.bottom() {
        if y == area.y || y == last_row {
            for x in area.x..area.right() {
                with_cell(frame, x, y, &edit);
            }
        } else {
            with_cell(frame, area.x, y, &edit);
            if area.width > 1 {
                with_cell(frame, area.right() - 1, y, &edit);
            }
        }
    }
}

fn is_line_glyph(frame: &FrameData, x: u16, y: u16) -> bool {
    if x >= frame.width || y >= frame.height {
        return false;
    }
    frame
        .cells
        .get(usize::from(y) * usize::from(frame.width) + usize::from(x))
        .is_some_and(|cell| LINE_GLYPHS.contains(&cell.symbol.as_str()))
}

/// Blends the accent into a cell's background. A `Reset` background blends
/// from the real terminal background when it is known; named and indexed
/// backgrounds have no known RGB and are left alone.
fn blend_bg(cell: &mut CellData, colors: &Colors, amount: f32) {
    if amount <= 0.0 {
        return;
    }
    let from = match cell.bg {
        RESET => colors.base,
        packed => unpack_rgb(packed),
    };
    if let Some(from) = from {
        cell.bg = pack(mix(from, colors.accent, amount));
    }
}

/// `CellData` colors use `color_to_u32`'s packing: `Reset` is 0 and RGB is
/// tagged `0x02` in the top byte.
const RESET: u32 = 0;

fn unpack_rgb(packed: u32) -> Option<Rgb> {
    (packed >> 24 == 0x02).then_some(((packed >> 16) as u8, (packed >> 8) as u8, packed as u8))
}

fn pack((r, g, b): Rgb) -> u32 {
    crate::protocol::color_to_u32(ratatui::style::Color::Rgb(r, g, b))
}

fn rgb(color: ratatui::style::Color) -> Option<Rgb> {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// Per-channel linear mix with halves rounded away from zero, the same rule as
/// the server's `mix_rgb` for selection colors.
fn mix(base: Rgb, target: Rgb, amount: f32) -> Rgb {
    let channel = |base: u8, target: u8| {
        (f32::from(base) + (f32::from(target) - f32::from(base)) * amount).round() as u8
    };
    (
        channel(base.0, target.0),
        channel(base.1, target.1),
        channel(base.2, target.2),
    )
}

fn luminance((r, g, b): Rgb) -> f32 {
    (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0
}

/// Whether two saturated colors sit within 30° of hue of each other.
fn same_hue_family(a: Rgb, b: Rgb) -> bool {
    let (Some(hue_a), Some(hue_b)) = (hue(a), hue(b)) else {
        return false;
    };
    let gap = (hue_a - hue_b).abs();
    gap.min(360.0 - gap) <= 30.0
}

/// HSV hue in degrees, or None for grays and near-grays.
fn hue((r, g, b): Rgb) -> Option<f32> {
    let (r, g, b) = (
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
    );
    let max = r.max(g).max(b);
    let delta = max - r.min(g).min(b);
    if max == 0.0 || delta / max < 0.25 {
        return None;
    }
    let sector = if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    Some(sector * 60.0)
}
