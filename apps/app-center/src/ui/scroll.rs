//! Vertical scrolling and the selection of the results list.

use super::geom::Rect;

/// How far one wheel notch (`WHEEL_DELTA`, 120) scrolls, in pixels.
pub const WHEEL_STEP: i32 = 72;
/// The smallest scroll bar thumb.
pub const MIN_THUMB: i32 = 28;

/// A vertically scrolled view: `content` pixels shown `viewport` at a time,
/// starting `offset` pixels down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scroll {
    pub offset: i32,
    pub content: i32,
    pub viewport: i32,
}

impl Scroll {
    pub fn max_offset(&self) -> i32 {
        (self.content - self.viewport).max(0)
    }

    fn clamp(&mut self) {
        self.offset = self.offset.clamp(0, self.max_offset());
    }

    pub fn set_content(&mut self, content: i32) {
        self.content = content.max(0);
        self.clamp();
    }

    pub fn set_viewport(&mut self, viewport: i32) {
        self.viewport = viewport.max(0);
        self.clamp();
    }

    /// Scrolls by `dy` pixels; returns whether the offset changed.
    pub fn scroll_by(&mut self, dy: i32) -> bool {
        let old = self.offset;
        self.offset = self.offset.saturating_add(dy);
        self.clamp();
        self.offset != old
    }

    pub fn scroll_to(&mut self, offset: i32) -> bool {
        let old = self.offset;
        self.offset = offset;
        self.clamp();
        self.offset != old
    }

    /// Scrolls for a `WM_MOUSEWHEEL` delta (positive = away from the user =
    /// up).
    pub fn wheel(&mut self, delta: i32) -> bool {
        self.scroll_by(-delta * WHEEL_STEP / 120)
    }

    /// Scrolls the least so that `top..bottom` (content pixels) is visible.
    pub fn ensure_visible(&mut self, top: i32, bottom: i32) -> bool {
        let old = self.offset;
        if bottom > self.offset + self.viewport {
            self.offset = bottom - self.viewport;
        }
        if top < self.offset {
            self.offset = top;
        }
        self.clamp();
        self.offset != old
    }

    pub fn scrollable(&self) -> bool {
        self.content > self.viewport
    }

    /// The scroll bar thumb inside `track`, or None when everything fits.
    pub fn thumb(&self, track: Rect) -> Option<Rect> {
        if !self.scrollable() || track.height() <= 0 {
            return None;
        }
        let th = track.height();
        let size = ((th as i64 * self.viewport as i64 / self.content as i64) as i32)
            .clamp(MIN_THUMB.min(th), th);
        let travel = th - size;
        let pos = if self.max_offset() == 0 {
            0
        } else {
            (travel as i64 * self.offset as i64 / self.max_offset() as i64) as i32
        };
        Some(Rect::new(
            track.left,
            track.top + pos,
            track.right,
            track.top + pos + size,
        ))
    }

    /// The offset that puts the top of the thumb at `thumb_top` (client y)
    /// in `track`, for dragging the thumb.
    pub fn offset_for_thumb(&self, track: Rect, thumb_top: i32) -> i32 {
        let Some(thumb) = self.thumb(track) else {
            return 0;
        };
        let travel = track.height() - thumb.height();
        if travel <= 0 {
            return 0;
        }
        let pos = (thumb_top - track.top).clamp(0, travel);
        (pos as i64 * self.max_offset() as i64 / travel as i64) as i32
    }
}

/// The results list: rows of a fixed height, one of them selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListState {
    pub count: usize,
    pub row_height: i32,
    pub selected: Option<usize>,
    pub hover: Option<usize>,
    pub scroll: Scroll,
}

impl ListState {
    pub fn new(row_height: i32) -> ListState {
        ListState {
            count: 0,
            row_height,
            selected: None,
            hover: None,
            scroll: Scroll::default(),
        }
    }

    /// New contents: nothing selected, scrolled to the top.
    pub fn reset(&mut self, count: usize) {
        self.count = count;
        self.selected = None;
        self.hover = None;
        self.scroll.offset = 0;
        self.scroll.set_content(count as i32 * self.row_height);
    }

    pub fn set_viewport(&mut self, height: i32) {
        self.scroll.set_viewport(height);
    }

    /// Rows that fit in the viewport at once (at least one).
    pub fn page(&self) -> usize {
        ((self.scroll.viewport / self.row_height.max(1)) as usize).max(1)
    }

    /// The rows at least partly visible.
    pub fn visible(&self) -> std::ops::Range<usize> {
        let h = self.row_height.max(1);
        let first = (self.scroll.offset / h) as usize;
        let last = ((self.scroll.offset + self.scroll.viewport + h - 1) / h) as usize;
        first.min(self.count)..last.min(self.count)
    }

    /// Where row `i` is, given the list's rectangle on screen.
    pub fn row_rect(&self, area: Rect, i: usize) -> Rect {
        let top = area.top + i as i32 * self.row_height - self.scroll.offset;
        Rect::new(area.left, top, area.right, top + self.row_height)
    }

    /// The row under client y `y` in the list at `area`.
    pub fn row_at(&self, area: Rect, y: i32) -> Option<usize> {
        if y < area.top || y >= area.bottom {
            return None;
        }
        let i = ((y - area.top + self.scroll.offset) / self.row_height.max(1)) as usize;
        (i < self.count).then_some(i)
    }

    /// Selects row `i` (clamped) and scrolls it into view; returns whether
    /// the selection changed.
    pub fn select(&mut self, i: usize) -> bool {
        if self.count == 0 {
            return false;
        }
        let i = i.min(self.count - 1);
        let changed = self.selected != Some(i);
        self.selected = Some(i);
        let top = i as i32 * self.row_height;
        self.scroll.ensure_visible(top, top + self.row_height);
        changed
    }

    /// Moves the selection by `delta` rows (negative = up). With nothing
    /// selected, down selects the first row and up the last.
    pub fn move_by(&mut self, delta: isize) -> bool {
        if self.count == 0 {
            return false;
        }
        let target = match self.selected {
            None if delta >= 0 => 0,
            None => self.count - 1,
            Some(s) => (s as isize + delta).clamp(0, self.count as isize - 1) as usize,
        };
        self.select(target)
    }

    pub fn page_by(&mut self, pages: isize) -> bool {
        self.move_by(pages * self.page() as isize)
    }

    pub fn first(&mut self) -> bool {
        self.select(0)
    }

    pub fn last(&mut self) -> bool {
        self.count > 0 && self.select(self.count - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scroll(content: i32, viewport: i32) -> Scroll {
        let mut s = Scroll::default();
        s.set_viewport(viewport);
        s.set_content(content);
        s
    }

    #[test]
    fn offset_is_clamped() {
        let mut s = scroll(1000, 300);
        assert_eq!(s.max_offset(), 700);
        assert!(s.scroll_by(500));
        assert!(s.scroll_by(500));
        assert_eq!(s.offset, 700);
        assert!(!s.scroll_by(10));
        assert!(s.scroll_to(-50));
        assert_eq!(s.offset, 0);
        s.offset = 700;
        s.set_content(400);
        assert_eq!(s.offset, 100);
        let mut small = scroll(100, 300);
        assert!(!small.scroll_by(50));
        assert_eq!(small.max_offset(), 0);
    }

    #[test]
    fn wheel_scrolls_up_for_positive_delta() {
        let mut s = scroll(1000, 300);
        assert!(s.wheel(-120));
        assert_eq!(s.offset, WHEEL_STEP);
        assert!(s.wheel(240));
        assert_eq!(s.offset, 0);
    }

    #[test]
    fn ensure_visible_scrolls_the_least() {
        let mut s = scroll(1000, 300);
        assert!(!s.ensure_visible(100, 200));
        assert!(s.ensure_visible(350, 400));
        assert_eq!(s.offset, 100);
        assert!(s.ensure_visible(50, 80));
        assert_eq!(s.offset, 50);
    }

    #[test]
    fn thumb_size_and_position() {
        let track = Rect::new(0, 100, 8, 400);
        assert_eq!(scroll(200, 300).thumb(track), None);
        let mut s = scroll(1200, 300);
        let t = s.thumb(track).unwrap();
        assert_eq!(t, Rect::new(0, 100, 8, 175));
        s.scroll_to(900);
        assert_eq!(s.thumb(track).unwrap().bottom, 400);
        // A huge list keeps a grabbable thumb.
        let huge = scroll(1_000_000, 300);
        assert_eq!(huge.thumb(track).unwrap().height(), MIN_THUMB);
    }

    #[test]
    fn dragging_the_thumb() {
        let track = Rect::new(0, 100, 8, 400);
        let s = scroll(1200, 300);
        // Thumb is 75 high, so it travels 225 pixels for 900 of offset.
        assert_eq!(s.offset_for_thumb(track, 100), 0);
        assert_eq!(s.offset_for_thumb(track, 100 + 225), 900);
        assert_eq!(s.offset_for_thumb(track, 100 + 75), 300);
        assert_eq!(s.offset_for_thumb(track, 9999), 900);
        assert_eq!(s.offset_for_thumb(track, -5), 0);
        assert_eq!(scroll(10, 300).offset_for_thumb(track, 200), 0);
    }

    fn list(count: usize) -> ListState {
        let mut l = ListState::new(50);
        l.set_viewport(200);
        l.reset(count);
        l
    }

    #[test]
    fn visible_rows_and_hit_testing() {
        let mut l = list(10);
        assert_eq!(l.visible(), 0..4);
        l.scroll.scroll_to(75);
        assert_eq!(l.visible(), 1..6);
        let area = Rect::new(0, 100, 300, 300);
        assert_eq!(l.row_rect(area, 1), Rect::new(0, 75, 300, 125));
        assert_eq!(l.row_at(area, 100), Some(1));
        assert_eq!(l.row_at(area, 125), Some(2));
        assert_eq!(l.row_at(area, 99), None);
        assert_eq!(l.row_at(area, 300), None);
        let short = list(2);
        assert_eq!(short.visible(), 0..2);
        assert_eq!(short.row_at(area, 250), None);
    }

    #[test]
    fn keyboard_navigation() {
        let mut l = list(10);
        assert!(l.move_by(1));
        assert_eq!(l.selected, Some(0));
        assert!(!l.move_by(-1));
        assert!(l.page_by(1));
        assert_eq!(l.selected, Some(4));
        assert_eq!(l.scroll.offset, 50);
        assert!(l.last());
        assert_eq!(l.selected, Some(9));
        assert_eq!(l.scroll.offset, 300);
        assert!(!l.move_by(5));
        assert!(l.first());
        assert_eq!(l.scroll.offset, 0);
        let mut fresh = list(10);
        assert!(fresh.move_by(-1));
        assert_eq!(fresh.selected, Some(9));
        let mut empty = list(0);
        assert!(!empty.move_by(1) && !empty.last() && !empty.select(3));
        assert_eq!(empty.selected, None);
    }

    #[test]
    fn reset_clears_selection_and_scroll() {
        let mut l = list(10);
        l.select(8);
        l.hover = Some(2);
        l.reset(3);
        assert_eq!((l.selected, l.hover, l.scroll.offset), (None, None, 0));
        assert_eq!(l.scroll.content, 150);
        assert_eq!(l.page(), 4);
    }
}
