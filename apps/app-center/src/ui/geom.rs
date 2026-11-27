//! Rectangles in client pixels, like a Win32 `RECT`.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// A rectangle from its top-left corner and size.
    pub const fn xywh(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, x + w, y + h)
    }

    pub fn width(&self) -> i32 {
        (self.right - self.left).max(0)
    }

    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(0)
    }

    pub fn is_empty(&self) -> bool {
        self.width() == 0 || self.height() == 0
    }

    /// Whether the point is inside; the right and bottom edges are outside.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// Shrinks by `dx` on the left and right and `dy` on the top and bottom.
    pub fn inset(&self, dx: i32, dy: i32) -> Rect {
        let r = Rect::new(
            self.left + dx,
            self.top + dy,
            self.right - dx,
            self.bottom - dy,
        );
        Rect::new(r.left, r.top, r.right.max(r.left), r.bottom.max(r.top))
    }

    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(
            self.left + dx,
            self.top + dy,
            self.right + dx,
            self.bottom + dy,
        )
    }

    pub fn intersect(&self, other: &Rect) -> Rect {
        let r = Rect::new(
            self.left.max(other.left),
            self.top.max(other.top),
            self.right.min(other.right),
            self.bottom.min(other.bottom),
        );
        if r.right <= r.left || r.bottom <= r.top {
            Rect::default()
        } else {
            r
        }
    }

    /// The point where something of size `w`×`h` is centred in this rect.
    pub fn centered(&self, w: i32, h: i32) -> (i32, i32) {
        (
            self.left + (self.width() - w) / 2,
            self.top + (self.height() - h) / 2,
        )
    }
}

/// Things that can be clicked, with where they were last painted.
#[derive(Debug, Clone, Default)]
pub struct HitMap<T> {
    targets: Vec<(Rect, T)>,
}

impl<T: Clone> HitMap<T> {
    pub fn new() -> Self {
        HitMap {
            targets: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.targets.clear();
    }

    pub fn add(&mut self, rect: Rect, target: T) {
        if !rect.is_empty() {
            self.targets.push((rect, target));
        }
    }

    /// The target at a point; the last one added wins, as it was painted
    /// on top.
    pub fn hit(&self, x: i32, y: i32) -> Option<T> {
        self.targets
            .iter()
            .rev()
            .find(|(r, _)| r.contains(x, y))
            .map(|(_, t)| t.clone())
    }

    pub fn rect_of(&self, pred: impl Fn(&T) -> bool) -> Option<Rect> {
        self.targets.iter().find(|(_, t)| pred(t)).map(|(r, _)| *r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_containment() {
        let r = Rect::xywh(10, 20, 30, 40);
        assert_eq!(r, Rect::new(10, 20, 40, 60));
        assert_eq!((r.width(), r.height()), (30, 40));
        assert!(r.contains(10, 20));
        assert!(!r.contains(40, 20));
        assert!(!r.contains(10, 60));
        assert!(Rect::new(5, 5, 3, 9).is_empty());
        assert_eq!(Rect::new(5, 5, 3, 9).width(), 0);
    }

    #[test]
    fn inset_offset_intersect_center() {
        let r = Rect::new(0, 0, 100, 50);
        assert_eq!(r.inset(10, 5), Rect::new(10, 5, 90, 45));
        assert!(r.inset(60, 0).is_empty());
        assert_eq!(r.offset(3, -2), Rect::new(3, -2, 103, 48));
        assert_eq!(
            r.intersect(&Rect::new(50, 25, 200, 200)),
            Rect::new(50, 25, 100, 50)
        );
        assert!(r.intersect(&Rect::new(100, 0, 200, 50)).is_empty());
        assert_eq!(r.centered(20, 10), (40, 20));
    }

    #[test]
    fn hit_map_prefers_the_topmost() {
        let mut m = HitMap::new();
        m.add(Rect::new(0, 0, 100, 100), "panel");
        m.add(Rect::new(10, 10, 20, 20), "button");
        m.add(Rect::new(0, 0, 0, 0), "empty");
        assert_eq!(m.hit(15, 15), Some("button"));
        assert_eq!(m.hit(50, 50), Some("panel"));
        assert_eq!(m.hit(150, 50), None);
        assert_eq!(
            m.rect_of(|t| *t == "button"),
            Some(Rect::new(10, 10, 20, 20))
        );
        m.clear();
        assert_eq!(m.hit(15, 15), None);
    }
}
