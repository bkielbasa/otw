//! The single-line text in the search box and its caret.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    chars: Vec<char>,
    /// Caret position, in characters.
    caret: usize,
}

/// The longest query kept.
pub const MAX_LEN: usize = 200;

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The text before the caret, for measuring where to draw it.
    pub fn before_caret(&self) -> String {
        self.chars[..self.caret].iter().collect()
    }

    /// Inserts typed or pasted text at the caret. Line breaks and tabs
    /// become spaces and other control characters are dropped. Returns
    /// whether the text changed.
    pub fn insert(&mut self, s: &str) -> bool {
        let mut changed = false;
        for c in s.chars() {
            let c = match c {
                '\r' | '\n' | '\t' => ' ',
                c if c.is_control() => continue,
                c => c,
            };
            if self.chars.len() >= MAX_LEN {
                break;
            }
            self.chars.insert(self.caret, c);
            self.caret += 1;
            changed = true;
        }
        changed
    }

    pub fn set(&mut self, s: &str) -> bool {
        let old = self.chars.clone();
        self.chars.clear();
        self.caret = 0;
        self.insert(s);
        self.chars != old
    }

    pub fn backspace(&mut self) -> bool {
        if self.caret == 0 {
            return false;
        }
        self.caret -= 1;
        self.chars.remove(self.caret);
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.caret >= self.chars.len() {
            return false;
        }
        self.chars.remove(self.caret);
        true
    }

    fn word_start_before(&self, mut i: usize) -> usize {
        while i > 0 && !is_word(self.chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(self.chars[i - 1]) {
            i -= 1;
        }
        i
    }

    fn word_end_after(&self, mut i: usize) -> usize {
        let n = self.chars.len();
        while i < n && !is_word(self.chars[i]) {
            i += 1;
        }
        while i < n && is_word(self.chars[i]) {
            i += 1;
        }
        i
    }

    /// Ctrl+Backspace: deletes the word before the caret.
    pub fn delete_word_back(&mut self) -> bool {
        let start = self.word_start_before(self.caret);
        if start == self.caret {
            return false;
        }
        self.chars.drain(start..self.caret);
        self.caret = start;
        true
    }

    /// Moves the caret; these return whether it moved.
    pub fn left(&mut self) -> bool {
        self.move_to(self.caret.saturating_sub(1))
    }

    pub fn right(&mut self) -> bool {
        self.move_to(self.caret + 1)
    }

    pub fn word_left(&mut self) -> bool {
        self.move_to(self.word_start_before(self.caret))
    }

    pub fn word_right(&mut self) -> bool {
        self.move_to(self.word_end_after(self.caret))
    }

    pub fn home(&mut self) -> bool {
        self.move_to(0)
    }

    pub fn end(&mut self) -> bool {
        self.move_to(self.chars.len())
    }

    pub fn move_to(&mut self, i: usize) -> bool {
        let i = i.min(self.chars.len());
        let moved = i != self.caret;
        self.caret = i;
        moved
    }

    /// The characters `start..end` to draw in a box `width` pixels wide so
    /// that the caret stays visible: as much as fits from `prefer_start`,
    /// scrolled right when the caret would be past the edge, or left when
    /// it is before `prefer_start`.
    pub fn visible_range(
        &self,
        prefer_start: usize,
        width: i32,
        measure: impl Fn(&str) -> i32,
    ) -> (usize, usize) {
        let s = |a: usize, b: usize| self.chars[a..b].iter().collect::<String>();
        let n = self.chars.len();
        let mut start = prefer_start.min(self.caret).min(n);
        while start < self.caret && measure(&s(start, self.caret)) > width {
            start += 1;
        }
        let mut end = n;
        while end > self.caret && measure(&s(start, end)) > width {
            end -= 1;
        }
        (start, end)
    }

    /// The characters `start..end` as text.
    pub fn slice(&self, start: usize, end: usize) -> String {
        let n = self.chars.len();
        self.chars[start.min(n)..end.min(n)].iter().collect()
    }

    /// Puts the caret at the character boundary closest to `x` pixels from
    /// the start of the text, given a way to measure text width.
    pub fn caret_from_x(&mut self, x: i32, measure: impl Fn(&str) -> i32) -> bool {
        let mut best = (0usize, i32::MAX);
        let mut prefix = String::new();
        for i in 0..=self.chars.len() {
            if i > 0 {
                prefix.push(self.chars[i - 1]);
            }
            let d = (measure(&prefix) - x).abs();
            if d < best.1 {
                best = (i, d);
            }
        }
        self.move_to(best.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(s: &str) -> TextInput {
        let mut t = TextInput::new();
        t.insert(s);
        t
    }

    #[test]
    fn typing_and_deleting() {
        let mut t = input("firefx");
        assert_eq!(t.caret(), 6);
        t.left();
        assert!(t.insert("o"));
        assert_eq!(t.text(), "firefox");
        assert_eq!(t.before_caret(), "firefo");
        assert!(t.backspace());
        assert!(t.delete());
        assert_eq!(t.text(), "firef");
        assert!(!t.delete());
        t.home();
        assert!(!t.backspace());
        assert!(t.delete());
        assert_eq!(t.text(), "iref");
    }

    #[test]
    fn control_characters_are_cleaned() {
        let mut t = input("a\tb\r\nc\u{7}d");
        assert_eq!(t.text(), "a b  cd");
        assert!(!t.insert("\u{1b}"));
        assert!(t.set("new"));
        assert!(!t.set("new"));
        assert_eq!((t.text(), t.caret()), ("new".to_string(), 3));
    }

    #[test]
    fn length_is_limited() {
        let mut t = input(&"x".repeat(MAX_LEN + 50));
        assert_eq!(t.text().len(), MAX_LEN);
        assert!(!t.insert("y"));
    }

    #[test]
    fn unicode_is_handled_per_character() {
        let mut t = input("Zażółć");
        assert!(t.backspace());
        assert_eq!(t.text(), "Zażół");
        t.left();
        assert_eq!(t.before_caret(), "Zażó");
    }

    #[test]
    fn word_movement_and_deletion() {
        let mut t = input("visual studio  code");
        assert!(t.word_left());
        assert_eq!(t.caret(), 15);
        assert!(t.word_left());
        assert_eq!(t.caret(), 7);
        assert!(t.word_right());
        assert_eq!(t.caret(), 13);
        t.end();
        assert!(t.delete_word_back());
        assert_eq!(t.text(), "visual studio  ");
        assert!(t.delete_word_back());
        assert_eq!(t.text(), "visual ");
        t.home();
        assert!(!t.delete_word_back());
        assert!(!t.word_left());
        assert!(!t.left());
        t.end();
        assert!(!t.right() && !t.word_right());
    }

    #[test]
    fn long_text_scrolls_to_keep_the_caret_visible() {
        let measure = |s: &str| s.chars().count() as i32 * 10;
        let mut t = input("abcdefghij"); // 100 px, caret at the end
        assert_eq!(t.visible_range(0, 200, measure), (0, 10));
        assert_eq!(t.visible_range(0, 50, measure), (5, 10));
        t.home();
        assert_eq!(t.visible_range(5, 50, measure), (0, 5));
        t.move_to(7);
        assert_eq!(t.visible_range(5, 50, measure), (5, 10));
        assert_eq!(t.visible_range(0, 50, measure), (2, 7));
        assert_eq!(t.slice(2, 7), "cdefg");
        assert_eq!(t.slice(8, 99), "ij");
        assert_eq!(TextInput::new().visible_range(3, 50, measure), (0, 0));
    }

    #[test]
    fn clicking_places_the_caret() {
        let mut t = input("abcd");
        let measure = |s: &str| s.chars().count() as i32 * 10;
        t.caret_from_x(14, measure);
        assert_eq!(t.caret(), 1);
        t.caret_from_x(16, measure);
        assert_eq!(t.caret(), 2);
        t.caret_from_x(-30, measure);
        assert_eq!(t.caret(), 0);
        t.caret_from_x(500, measure);
        assert_eq!(t.caret(), 4);
    }
}
