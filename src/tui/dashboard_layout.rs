//! Dashboard bento-grid layout.
//!
//! Target: 132 cols × 38 rows. Scales gracefully below.
//!
//! ```text
//! Rows 1-3:   Header (wordmark + stats + clock)
//! Row  4:     Horizontal rule (dim)
//! Row  5:     Tab bar
//! Row  6:     Horizontal rule (dim)
//! Rows 7-N-2: Body — 3-column bento grid
//! Row  N-1:   Bold rule (dim)
//! Row  N:     Footer keybind bar
//! ```

use ratatui::layout::Rect;

use super::widgets::{middle_stack, right_stack};

/// Blank rows between stacked panels, and between the stacks and the SSH log:
/// the same single cell as the gutter between columns.
pub const PANEL_GAP: u16 = 1;
/// Natural height of the middle and right stacks (host + agent + latency, and
/// recent + auth + ping, a gap between each). The two must agree or the SSH log
/// below them would start at a different row in each column.
pub const STACK_H: u16 =
    middle_stack::HOST_H + middle_stack::AGENT_H + middle_stack::LATENCY_H + 2 * PANEL_GAP;
const _: () = assert!(
    STACK_H == right_stack::RECENT_H + right_stack::AUTH_H + right_stack::PING_H + 2 * PANEL_GAP,
    "the middle and right stacks must be equally tall"
);
/// Smallest SSH log panel worth drawing: borders plus two rows.
const MIN_LOG_H: u16 = 4;
/// Fewest rows the last panel of a stack (Latency / Ping) needs to be drawn.
const MIN_LAST_H: u16 = 3;

/// Height of a stack's middle panel: its natural `h`, shrunk so the last panel
/// keeps [`MIN_LAST_H`] rows when the column is short. Falls back to the
/// natural height (the last panel then drops out) once shrinking would leave
/// the middle panel without a drawable box of its own.
pub fn middle_panel_h(h: u16, remaining: u16) -> u16 {
    let fit = remaining.saturating_sub(PANEL_GAP + MIN_LAST_H);
    if fit >= MIN_LAST_H {
        h.min(fit)
    } else {
        h.min(remaining)
    }
}

/// All screen regions for the dashboard view.
#[derive(Debug, Clone, Copy)]
pub struct DashboardAreas {
    /// Full terminal area.
    pub full: Rect,
    /// Header: rows 0..3 (3 lines for wordmark + stats).
    pub header: Rect,
    /// Tab bar: 1 row.
    pub tab_bar: Rect,
    /// Body: the main content area between tab bar and footer.
    pub body: Rect,
    /// Left column inside body (hosts panel).
    pub col_left: Rect,
    /// Middle column inside body.
    pub col_mid: Rect,
    /// Right column inside body.
    pub col_right: Rect,
    /// Footer: last row.
    pub footer: Rect,
}

/// Side margin of the dashboard body at terminal width `w`.
pub fn content_margin(w: u16) -> u16 {
    if w >= 132 {
        2
    } else if w >= 80 {
        1
    } else {
        0
    }
}

/// Where the SFTP host picker draws inside `body`: the hosts column's left
/// edge stretched across all three columns, one row short for its hint line.
/// Hosts -> SFTP is this rect growing out of `col_left` in place.
pub fn sftp_picker_rect(body: Rect) -> Rect {
    let margin = content_margin(body.width);
    Rect::new(
        body.x + margin,
        body.y,
        body.width.saturating_sub(margin * 2),
        body.height.saturating_sub(1),
    )
}

/// Rect `t` of the way from `a` to `b` (`t` in `[0, 1]`), edge by edge.
pub fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    let mix = |from: u16, to: u16| (from as f32 + (to as f32 - from as f32) * t).round() as u16;
    let (x0, y0) = (mix(a.x, b.x), mix(a.y, b.y));
    let (x1, y1) = (mix(a.right(), b.right()), mix(a.bottom(), b.bottom()));
    Rect::new(x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

/// Vertical split of the middle and right columns on the hosts dashboard: the
/// height the two stacks take, and the SSH log panel spanning both columns
/// below them. The log gets every remaining row, to the bottom of the body;
/// when there is no room for one, the stacks take the whole body instead.
pub fn hosts_stack_split(areas: &DashboardAreas) -> (u16, Option<Rect>) {
    let body_h = areas.col_mid.height;
    if body_h < STACK_H + PANEL_GAP + MIN_LOG_H {
        return (body_h, None);
    }
    let log = Rect::new(
        areas.col_mid.x,
        areas.col_mid.y + STACK_H + PANEL_GAP,
        areas.col_mid.width + 1 + areas.col_right.width,
        body_h - STACK_H - PANEL_GAP,
    );
    (STACK_H, Some(log))
}

/// Compute dashboard areas from terminal size (zoom level 0).
///
/// Layout adapts to terminal width:
/// - ≥132 cols: 3 columns (42+1+42+1+42 + 4 margin = 132)
/// - <132 cols: columns shrink proportionally, margins reduce
pub fn dashboard_layout(area: Rect) -> DashboardAreas {
    dashboard_layout_zoomed(area, 0)
}

/// Compute dashboard areas, giving the left (hosts) column more width as the
/// UI zoom level increases. The middle and right columns shrink to compensate
/// but keep a usable minimum width.
pub fn dashboard_layout_zoomed(area: Rect, zoom: usize) -> DashboardAreas {
    let w = area.width;
    let h = area.height;

    // Vertical bands
    // Three rows of wordmark and stats, one row of name, stats and clock when
    // the terminal is too narrow to spare two body rows for the logo.
    let header_h = if w < 120 { 1u16 } else { 3u16 };
    let rule1 = 1u16;
    let tab_h = 1u16;
    let rule2 = 1u16;
    let footer_h = 1u16;
    let rule3 = 1u16;
    let chrome = header_h + rule1 + tab_h + rule2 + rule3 + footer_h; // 8 rows, 6 when narrow
    let body_h = h.saturating_sub(chrome);

    let header = Rect::new(area.x, area.y, w, header_h.min(h));
    // Clamp to a zero-height rect when the terminal is too short for the
    // chrome — renderers skip zero-height areas instead of panicking.
    let tab_y = area.y + header_h + rule1;
    let tab_bar = if tab_y + tab_h <= area.y + h {
        Rect::new(area.x, tab_y, w, tab_h)
    } else {
        Rect::new(area.x, area.y, w, 0)
    };
    let body_y = area.y + header_h + rule1 + tab_h + rule2;
    let body = Rect::new(area.x, body_y, w, body_h);
    let footer = Rect::new(area.x, area.y + h.saturating_sub(footer_h), w, footer_h);

    // Horizontal: 3 columns with gutters
    // Target: margin(2) + col(42) + gutter(1) + col(42) + gutter(1) + col(42) + margin(2) = 132
    let margin = content_margin(w);
    let inner_w = w.saturating_sub(margin * 2);
    let gutter = 1u16;
    // Width available for the three columns after removing the two gutters.
    let usable = inner_w.saturating_sub(2 * gutter);
    let base = usable / 3;

    // Zoom shifts ~1/10 of the usable width into the left column per level,
    // while keeping the middle and right columns above a usable minimum.
    let min_side = 16u16;
    let bonus = (usable / 10).saturating_mul(zoom as u16);
    let max_left = usable.saturating_sub(2 * min_side).max(base);
    let left_w = (base + bonus).min(max_left).max(1);
    let rest = usable.saturating_sub(left_w);
    let mid_w = rest / 2;

    let col_left = Rect::new(area.x + margin, body_y, left_w, body_h);
    let col_mid = Rect::new(col_left.x + left_w + gutter, body_y, mid_w, body_h);
    let col_right = Rect::new(
        col_mid.x + mid_w + gutter,
        body_y,
        // Take remaining space (handles rounding)
        w.saturating_sub(col_mid.x + mid_w + gutter + margin - area.x),
        body_h,
    );

    DashboardAreas {
        full: area,
        header,
        tab_bar,
        body,
        col_left,
        col_mid,
        col_right,
        footer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_132x38() {
        let a = dashboard_layout(Rect::new(0, 0, 132, 38));
        assert_eq!(a.header.height, 3);
        assert_eq!(a.tab_bar.height, 1);
        assert_eq!(a.footer.height, 1);
        assert!(a.body.height >= 25);
        assert_eq!(a.col_left.width, 42);
        assert_eq!(a.col_mid.width, 42);
        // Right col takes remaining
        assert!(a.col_right.width >= 40);
    }

    #[test]
    fn hosts_columns_tile_the_body_to_the_bottom() {
        for (w, h) in [(132u16, 38u16), (100, 34), (80, 24), (200, 60), (132, 28)] {
            let a = dashboard_layout(Rect::new(0, 0, w, h));
            let (stack_h, log) = hosts_stack_split(&a);
            let bottom = a.body.bottom();
            match log {
                Some(log) => {
                    assert_eq!(stack_h, STACK_H, "{w}x{h}");
                    assert_eq!(
                        log.y,
                        a.col_mid.y + stack_h + PANEL_GAP,
                        "{w}x{h}: log one gap row under the stacks"
                    );
                    assert_eq!(log.bottom(), bottom, "{w}x{h}: log reaches the body bottom");
                    assert_eq!(log.x, a.col_mid.x);
                    assert_eq!(
                        log.right(),
                        a.col_right.right(),
                        "{w}x{h}: spans both columns"
                    );
                }
                None => assert_eq!(stack_h, a.body.height, "{w}x{h}: stacks fill the body"),
            }
            assert_eq!(
                a.col_left.bottom(),
                bottom,
                "{w}x{h}: hosts pane reaches it too"
            );
        }
    }

    #[test]
    fn sftp_picker_grows_out_of_the_hosts_column_in_place() {
        let a = dashboard_layout(Rect::new(0, 0, 132, 38));
        let picker = sftp_picker_rect(a.body);
        assert_eq!(picker.x, a.col_left.x, "same left edge, no jump");
        assert_eq!(picker.y, a.col_left.y);
        assert_eq!(
            picker.right(),
            a.col_right.right(),
            "spans the three columns"
        );
        assert_eq!(lerp_rect(a.col_left, picker, 0.0), a.col_left);
        assert_eq!(lerp_rect(a.col_left, picker, 1.0), picker);
        let mid = lerp_rect(a.col_left, picker, 0.5);
        assert_eq!(mid.x, a.col_left.x);
        assert!(mid.width > a.col_left.width && mid.width < picker.width);
    }

    #[test]
    fn narrow_80x24() {
        let a = dashboard_layout(Rect::new(0, 0, 80, 24));
        assert!(a.col_left.width > 0);
        assert!(a.col_mid.width > 0);
        assert!(a.col_right.width > 0);
        assert!(a.body.height > 0);
    }

    #[test]
    fn tiny_terminal() {
        let a = dashboard_layout(Rect::new(0, 0, 40, 10));
        assert!(a.col_left.width > 0);
        assert!(a.body.height > 0);
    }

    #[test]
    fn zoom_widens_left_column_and_shrinks_others() {
        let base = dashboard_layout_zoomed(Rect::new(0, 0, 132, 38), 0);
        let zoomed = dashboard_layout_zoomed(Rect::new(0, 0, 132, 38), 2);
        assert!(
            zoomed.col_left.width > base.col_left.width,
            "left column grows with zoom"
        );
        assert!(
            zoomed.col_mid.width < base.col_mid.width,
            "middle column shrinks with zoom"
        );
        // Columns still tile left→mid→right without overlap.
        assert!(zoomed.col_mid.x >= zoomed.col_left.x + zoomed.col_left.width);
        assert!(zoomed.col_right.x >= zoomed.col_mid.x + zoomed.col_mid.width);
    }

    #[test]
    fn zoom_keeps_side_columns_usable() {
        // Even at max zoom the middle/right columns keep a usable width.
        let z = dashboard_layout_zoomed(Rect::new(0, 0, 132, 38), 3);
        assert!(z.col_mid.width >= 14);
        assert!(z.col_right.width >= 14);
    }
}
