//! How much of its tab a zoomed pane covers.
//!
//! A zoomed tab shows only its focused pane. `Full` covers the whole tab area,
//! as zoom always has. `Fitted` centres the pane between empty side margins
//! once the area is wide enough, so on an ultrawide client a line of output
//! stays a readable length instead of running from the far left edge.
//!
//! `zoomed_rect` is the only place that turns a tab area into a zoomed pane's
//! rect. View computation, background PTY resizing and new-pane sizing all
//! call it, so the size a program is told, the cells drawn for it and the
//! cells a click lands on cannot drift apart.

use ratatui::layout::Rect;

/// How wide a zoomed pane is drawn.
///
/// `Tab::zoom_width` holds the width of the tab's current zoom, or of its last
/// one, and is read only while `Tab::zoomed` is true. A zoom that names no
/// width, such as a command overlay, reuses it, so an overlay opened over a
/// full-width zoom stays full width and closing it gives the tab that zoom
/// back unchanged. The saved session does not record it, so a restored tab
/// starts `Fitted`.
///
/// The order is by width: a zoom request is already met when the tab is
/// zoomed at least as wide as the request asks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ZoomWidth {
    /// Centred between `ZoomMargins` on wide areas, the whole area otherwise.
    #[default]
    Fitted,
    /// Always the whole tab area.
    Full,
}

/// The margins a `Fitted` zoom leaves, from `[ui] zoom_margin_percent` and
/// `zoom_margin_min_width`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ZoomMargins {
    /// Share of the area left empty on each side, at most `MAX_PERCENT`.
    percent: u16,
    /// Narrowest area, in columns, that gets margins.
    min_width: u16,
}

impl ZoomMargins {
    /// Two 40% margins leave a fifth of the area, which keeps every pane at
    /// least one column wide.
    pub(crate) const MAX_PERCENT: u16 = 40;
    pub(crate) const DEFAULT_PERCENT: u16 = 20;
    pub(crate) const DEFAULT_MIN_WIDTH: u16 = 400;

    /// A `percent` above `MAX_PERCENT` is clamped to it.
    pub(crate) fn new(percent: u16, min_width: u16) -> Self {
        Self {
            percent: percent.min(Self::MAX_PERCENT),
            min_width,
        }
    }
}

impl Default for ZoomMargins {
    fn default() -> Self {
        Self::new(Self::DEFAULT_PERCENT, Self::DEFAULT_MIN_WIDTH)
    }
}

/// The rect a zoomed pane fills inside its tab's `area`.
///
/// Both margins are the same whole number of columns, so the pane is exactly
/// centred and any odd column goes to the pane.
pub(crate) fn zoomed_rect(area: Rect, width: ZoomWidth, margins: ZoomMargins) -> Rect {
    if width == ZoomWidth::Full || margins.percent == 0 || area.width < margins.min_width {
        return area;
    }
    let margin = u32::from(area.width) * u32::from(margins.percent) / 100;
    let Ok(margin) = u16::try_from(margin) else {
        return area;
    };
    Rect {
        x: area.x.saturating_add(margin),
        width: area.width - 2 * margin,
        ..area
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(x: u16, width: u16) -> Rect {
        Rect::new(x, 1, width, 40)
    }

    fn right_margin(area: Rect, rect: Rect) -> u16 {
        area.right() - rect.right()
    }

    #[test]
    fn full_width_is_the_whole_area_at_any_width() {
        for width in [1, 293, 400, 483, u16::MAX] {
            let area = area(26, width.min(u16::MAX - 26));
            assert_eq!(
                zoomed_rect(area, ZoomWidth::Full, ZoomMargins::default()),
                area
            );
        }
    }

    #[test]
    fn fitted_below_the_threshold_is_the_whole_area() {
        let margins = ZoomMargins::default();
        for width in [80, 293, 340, 399] {
            let area = area(26, width);
            assert_eq!(zoomed_rect(area, ZoomWidth::Fitted, margins), area);
        }
    }

    #[test]
    fn fitted_at_and_above_the_threshold_centres_the_pane() {
        let margins = ZoomMargins::default();

        let at = area(26, 400);
        assert_eq!(
            zoomed_rect(at, ZoomWidth::Fitted, margins),
            Rect::new(26 + 80, 1, 240, 40)
        );

        // The owner's 483-column client minus the default 26-column sidebar.
        let ultrawide = area(26, 457);
        let rect = zoomed_rect(ultrawide, ZoomWidth::Fitted, margins);
        assert_eq!(rect, Rect::new(26 + 91, 1, 275, 40));
        assert_eq!(rect.x - ultrawide.x, right_margin(ultrawide, rect));
    }

    #[test]
    fn odd_widths_keep_equal_margins_and_give_the_odd_column_to_the_pane() {
        let margins = ZoomMargins::new(20, 0);
        for width in [401, 455, 483, 999] {
            let area = area(3, width);
            let rect = zoomed_rect(area, ZoomWidth::Fitted, margins);
            let left = rect.x - area.x;
            assert_eq!(left, u16::try_from(u32::from(width) * 20 / 100).unwrap());
            assert_eq!(left, right_margin(area, rect));
            assert_eq!(rect.width, width - 2 * left);
        }
    }

    #[test]
    fn zero_percent_turns_margins_off() {
        let margins = ZoomMargins::new(0, 0);
        let area = area(0, 483);
        assert_eq!(zoomed_rect(area, ZoomWidth::Fitted, margins), area);
    }

    #[test]
    fn percent_is_clamped_to_the_maximum() {
        assert_eq!(ZoomMargins::new(75, 400), ZoomMargins::new(40, 400));
        let rect = zoomed_rect(area(0, 500), ZoomWidth::Fitted, ZoomMargins::new(75, 0));
        assert_eq!((rect.x, rect.width), (200, 100));
    }

    #[test]
    fn the_widest_margins_never_leave_a_zero_width_pane() {
        let margins = ZoomMargins::new(u16::MAX, 0);
        for width in 1..=600 {
            let area = area(10, width);
            let rect = zoomed_rect(area, ZoomWidth::Fitted, margins);
            assert!(rect.width >= 1, "width {width} left {rect:?}");
            assert_eq!(rect.x - area.x, right_margin(area, rect), "width {width}");
            assert_eq!((rect.y, rect.height), (area.y, area.height));
        }
    }
}
