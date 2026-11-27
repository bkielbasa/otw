//! Where everything goes in the window.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │ ▪▪ App Center    [ 🔍 Search apps…                ] [Refresh] │ header
//! ├────────────────────┬─────────────────────────────────────────┤
//! │ Popular apps       │  Firefox                                │
//! │ ▌Firefox     139.0 │  Mozilla · 139.0                        │
//! │  Mozilla.Firefox   │  [ Download ]                           │
//! │  7-Zip       26.06 │  ━━━━━━━━━━━━━━━━━━━━                   │
//! │  …                ▐│  About …                                │
//! ├────────────────────┴─────────────────────────────────────────┤
//! │ 10 422 packages · index from cache                            │ status
//! └──────────────────────────────────────────────────────────────┘
//! ```

use super::geom::Rect;

pub const HEADER_HEIGHT: i32 = 76;
pub const STATUS_HEIGHT: i32 = 32;
pub const PADDING: i32 = 24;
pub const SEARCH_HEIGHT: i32 = 40;
pub const SEARCH_MAX_WIDTH: i32 = 680;
pub const BRAND_WIDTH: i32 = 210;
pub const REFRESH_WIDTH: i32 = 104;
pub const BUTTON_HEIGHT: i32 = 40;
pub const BUTTON_PADDING: i32 = 22;
pub const BUTTON_GAP: i32 = 12;
pub const ROW_HEIGHT: i32 = 62;
pub const LIST_CAPTION_HEIGHT: i32 = 44;
pub const SCROLLBAR_WIDTH: i32 = 6;
pub const DETAILS_PADDING: i32 = 36;
/// The smallest window client area that still lays out sensibly.
pub const MIN_WIDTH: i32 = 760;
pub const MIN_HEIGHT: i32 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Layout {
    pub client: Rect,
    pub header: Rect,
    /// The logo and "App Center".
    pub brand: Rect,
    pub search: Rect,
    pub refresh: Rect,
    /// The whole results column, caption included.
    pub list_panel: Rect,
    /// "Popular apps" / "12 results".
    pub list_caption: Rect,
    /// Where rows are drawn.
    pub list: Rect,
    pub list_scrollbar: Rect,
    pub details_panel: Rect,
    /// The scrolled content area of the details panel.
    pub details: Rect,
    pub details_scrollbar: Rect,
    pub status: Rect,
}

/// The width of the results column for a window `width` wide.
pub fn list_width(width: i32) -> i32 {
    (width * 2 / 5).clamp(300, 440)
}

pub fn compute(width: i32, height: i32) -> Layout {
    let width = width.max(0);
    let height = height.max(0);
    let client = Rect::new(0, 0, width, height);
    let header = Rect::new(0, 0, width, HEADER_HEIGHT.min(height));
    let mid = (HEADER_HEIGHT - SEARCH_HEIGHT) / 2;
    let brand = Rect::new(PADDING, 0, PADDING + BRAND_WIDTH, header.bottom);
    let refresh = Rect::new(
        width - PADDING - REFRESH_WIDTH,
        mid,
        width - PADDING,
        mid + SEARCH_HEIGHT,
    );
    let search_left = brand.right;
    let search_right = (refresh.left - BUTTON_GAP).min(search_left + SEARCH_MAX_WIDTH);
    let search = Rect::new(
        search_left,
        mid,
        search_right.max(search_left),
        mid + SEARCH_HEIGHT,
    );
    let status_top = (height - STATUS_HEIGHT).max(header.bottom);
    let status = Rect::new(0, status_top, width, height);
    let lw = list_width(width).min(width);
    let list_panel = Rect::new(0, header.bottom, lw, status_top);
    let list_caption = Rect::new(
        0,
        header.bottom,
        lw,
        (header.bottom + LIST_CAPTION_HEIGHT).min(status_top),
    );
    let list = Rect::new(0, list_caption.bottom, lw, status_top);
    let list_scrollbar = Rect::new(
        lw - SCROLLBAR_WIDTH - 4,
        list.top + 4,
        lw - 4,
        (list.bottom - 4).max(list.top + 4),
    );
    // One pixel for the divider.
    let details_panel = Rect::new((lw + 1).min(width), header.bottom, width, status_top);
    let details = Rect::new(
        details_panel.left + DETAILS_PADDING,
        details_panel.top,
        (details_panel.right - DETAILS_PADDING).max(details_panel.left + DETAILS_PADDING),
        details_panel.bottom,
    );
    let details_scrollbar = Rect::new(
        width - SCROLLBAR_WIDTH - 4,
        details_panel.top + 4,
        width - 4,
        (details_panel.bottom - 4).max(details_panel.top + 4),
    );
    Layout {
        client,
        header,
        brand,
        search,
        refresh,
        list_panel,
        list_caption,
        list,
        list_scrollbar,
        details_panel,
        details,
        details_scrollbar,
        status,
    }
}

/// Lays out a row of buttons starting at (`x`, `y`) for labels that are
/// `label_widths` wide; buttons that would pass `max_right` wrap to a new
/// row.
pub fn button_row(x: i32, y: i32, max_right: i32, label_widths: &[i32]) -> Vec<Rect> {
    let mut out = Vec::with_capacity(label_widths.len());
    let (mut cx, mut cy) = (x, y);
    for &lw in label_widths {
        let w = (lw + 2 * BUTTON_PADDING).max(110);
        if cx > x && cx + w > max_right {
            cx = x;
            cy += BUTTON_HEIGHT + BUTTON_GAP;
        }
        out.push(Rect::xywh(cx, cy, w, BUTTON_HEIGHT));
        cx += w + BUTTON_GAP;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_tile_the_window() {
        let l = compute(1100, 720);
        assert_eq!(l.header, Rect::new(0, 0, 1100, HEADER_HEIGHT));
        assert_eq!(l.status, Rect::new(0, 720 - STATUS_HEIGHT, 1100, 720));
        assert_eq!(l.list_panel.right, 440);
        assert_eq!(l.details_panel.left, 441);
        assert_eq!(l.list_panel.top, l.header.bottom);
        assert_eq!(l.list_panel.bottom, l.status.top);
        assert_eq!(l.list.top, l.list_caption.bottom);
        assert_eq!(l.details.left, 441 + DETAILS_PADDING);
        assert_eq!(l.details.right, 1100 - DETAILS_PADDING);
    }

    #[test]
    fn header_controls_do_not_overlap() {
        for w in [MIN_WIDTH, 900, 1280, 2560] {
            let l = compute(w, 700);
            assert!(l.brand.right <= l.search.left, "{w}");
            assert!(l.search.right + BUTTON_GAP <= l.refresh.left, "{w}");
            assert!(l.search.width() <= SEARCH_MAX_WIDTH);
            assert!(l.search.width() > 200, "{w}: {:?}", l.search);
            assert_eq!(l.refresh.right, w - PADDING);
            assert_eq!(l.search.height(), SEARCH_HEIGHT);
        }
    }

    #[test]
    fn list_width_is_bounded() {
        assert_eq!(list_width(500), 300);
        assert_eq!(list_width(900), 360);
        assert_eq!(list_width(3000), 440);
    }

    #[test]
    fn scrollbars_are_inside_their_panels() {
        let l = compute(1000, 600);
        assert!(l.list_scrollbar.right <= l.list_panel.right);
        assert!(l.list_scrollbar.left > l.list_panel.left);
        assert_eq!(l.list_scrollbar.width(), SCROLLBAR_WIDTH);
        assert!(l.details_scrollbar.right <= l.details_panel.right);
        assert!(l.details_scrollbar.left >= l.details.right);
    }

    #[test]
    fn tiny_windows_do_not_produce_negative_rects() {
        for (w, h) in [(0, 0), (50, 30), (200, 100)] {
            let l = compute(w, h);
            for r in [
                l.header,
                l.search,
                l.list,
                l.details,
                l.status,
                l.list_scrollbar,
            ] {
                assert!(r.right >= r.left && r.bottom >= r.top, "{w}x{h}: {r:?}");
            }
        }
    }

    #[test]
    fn buttons_flow_and_wrap() {
        let r = button_row(100, 50, 1000, &[60, 200]);
        assert_eq!(r[0], Rect::xywh(100, 50, 110, BUTTON_HEIGHT));
        assert_eq!(r[1], Rect::xywh(222, 50, 244, BUTTON_HEIGHT));
        let wrapped = button_row(100, 50, 400, &[200, 200]);
        assert_eq!(wrapped[1].left, 100);
        assert_eq!(wrapped[1].top, 50 + BUTTON_HEIGHT + BUTTON_GAP);
        assert!(button_row(0, 0, 10, &[]).is_empty());
    }
}
