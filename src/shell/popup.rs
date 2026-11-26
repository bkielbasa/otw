//! Cascading popup menus in the Win95 style: the Start menu, menu-bar
//! dropdowns and context menus. Pure layout and interaction state; the
//! shell paints them with [`paint`].

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::{
    win32::gdi::sys_color,
    win32::gfx::{self, Rect},
    win32::user::{text_width, Painter},
};

use super::{
    icons::{self, Bitmap},
    volume::VolPath,
};

/// What choosing a menu item does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Minimize,
    Maximize,
    Restore,
    Open(VolPath),
    Explore(VolPath),
    OpenSelection,
    NewFolder,
    NewTextDocument,
    Delete,
    Rename,
    Cut,
    Copy,
    Paste,
    Properties,
    Refresh,
    Up,
    Close,
    Run,
    ShutDown,
    About,
}

#[derive(Clone, Debug)]
pub enum Kind {
    Command(Command),
    Submenu(Vec<Item>),
    Separator,
}

#[derive(Clone, Debug)]
pub struct Item {
    /// Label with an `&` before the mnemonic character.
    pub label: String,
    /// Shortcut-key text shown right-aligned, e.g. `Del`.
    pub accel: String,
    pub kind: Kind,
    pub enabled: bool,
    pub default: bool,
    pub icon: Option<Bitmap>,
    pub overlay: bool,
}

impl Item {
    pub fn command(label: &str, command: Command) -> Self {
        Self {
            label: label.into(),
            accel: String::new(),
            kind: Kind::Command(command),
            enabled: true,
            default: false,
            icon: None,
            overlay: false,
        }
    }

    pub fn submenu(label: &str, items: Vec<Item>) -> Self {
        Self {
            kind: Kind::Submenu(items),
            ..Self::command(label, Command::Refresh)
        }
    }

    pub fn separator() -> Self {
        Self {
            kind: Kind::Separator,
            ..Self::command("", Command::Refresh)
        }
    }

    /// The placeholder Windows shows in an empty submenu.
    pub fn empty() -> Self {
        Self {
            enabled: false,
            ..Self::command("(Empty)", Command::Refresh)
        }
    }

    pub fn accel(mut self, accel: &str) -> Self {
        self.accel = accel.into();
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn default_item(mut self) -> Self {
        self.default = true;
        self
    }

    pub fn icon(mut self, icon: Bitmap, overlay: bool) -> Self {
        self.icon = Some(icon);
        self.overlay = overlay;
        self
    }

    fn selectable(&self) -> bool {
        !matches!(self.kind, Kind::Separator)
    }

    fn mnemonic(&self) -> Option<char> {
        let bytes = self.label.as_bytes();
        bytes
            .windows(2)
            .find(|pair| pair[0] == b'&' && pair[1] != b'&')
            .map(|pair| (pair[1] as char).to_ascii_lowercase())
    }

    fn first_letter(&self) -> Option<char> {
        self.label
            .chars()
            .find(|c| *c != '&')
            .map(|c| c.to_ascii_lowercase())
    }
}

/// How a menu level is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// Rows with 16x16 icons.
    Icons,
    /// The Start menu's top level: tall rows, large icons, side banner.
    Start,
    /// Plain text rows, like an application's menus.
    Plain,
}

const SEPARATOR_HEIGHT: i32 = 8;
const BANNER_WIDTH: i32 = 21;

struct Level {
    items: Vec<Item>,
    style: Style,
    rect: Rect,
    rows: Vec<Rect>,
    accel_x: i32,
    hot: Option<usize>,
}

fn row_height(style: Style) -> i32 {
    match style {
        Style::Start => 32,
        Style::Icons => 20,
        Style::Plain => 18,
    }
}

fn label_x(style: Style) -> i32 {
    match style {
        Style::Start => BANNER_WIDTH + 42,
        Style::Icons => 26,
        Style::Plain => 20,
    }
}

impl Level {
    fn new(items: Vec<Item>, style: Style) -> Self {
        let label_width = items
            .iter()
            .map(|item| text_width(item.label.as_bytes()))
            .max()
            .unwrap_or(0);
        let accel_width = items
            .iter()
            .map(|item| text_width(item.accel.as_bytes()))
            .max()
            .unwrap_or(0);
        let gap = if accel_width > 0 { 24 } else { 0 };
        let mut width = 3 + label_x(style) + label_width + gap + accel_width + 22 + 3;
        if style == Style::Start {
            width = width.max(150);
        }
        let mut y = 3;
        let mut rows = Vec::with_capacity(items.len());
        let left = if style == Style::Start {
            3 + BANNER_WIDTH
        } else {
            3
        };
        for item in &items {
            let height = if matches!(item.kind, Kind::Separator) {
                SEPARATOR_HEIGHT
            } else {
                row_height(style)
            };
            rows.push(Rect::new(left, y, width - 3, y + height));
            y += height;
        }
        Self {
            accel_x: 3 + label_x(style) + label_width + gap,
            items,
            style,
            rect: Rect::sized(0, 0, width.max(80), y + 3),
            rows,
            hot: None,
        }
    }

    fn place(&mut self, x: i32, y: i32) {
        self.rect = Rect::sized(x, y, self.rect.width(), self.rect.height());
    }

    fn row_at(&self, x: i32, y: i32) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.offset(self.rect.left, self.rect.top).contains(x, y))
    }

    fn screen_row(&self, index: usize) -> Rect {
        self.rows[index].offset(self.rect.left, self.rect.top)
    }
}

/// Where a menu opens.
#[derive(Clone, Copy, Debug)]
pub enum Anchor {
    /// Top-left at a point (context menus, flipped to stay on screen).
    Point(i32, i32),
    /// Below a menu-bar item.
    Below(Rect),
    /// Bottom-left at a point (the Start menu, above the taskbar).
    Above(i32, i32),
}

/// Result of feeding input to a menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing visible changed.
    None,
    /// The highlight or the open submenus changed.
    Changed,
    Chosen(Command),
    /// The menu should close (Escape at the top, or a click outside).
    Dismiss,
    /// Left/Right past the top level: switch to the neighbouring menu-bar menu.
    Previous,
    Next,
}

pub struct Menu {
    levels: Vec<Level>,
    screen: (i32, i32),
}

impl Menu {
    pub fn open(items: Vec<Item>, style: Style, anchor: Anchor, screen: (i32, i32)) -> Self {
        let mut level = Level::new(items, style);
        let (w, h) = (level.rect.width(), level.rect.height());
        let (x, y) = match anchor {
            Anchor::Point(x, y) => (
                if x + w > screen.0 { x - w } else { x },
                if y + h > screen.1 { y - h } else { y },
            ),
            Anchor::Below(rect) => (rect.left, rect.bottom),
            Anchor::Above(x, bottom) => (x, bottom - h),
        };
        level.place(
            x.clamp(0, (screen.0 - w).max(0)),
            y.clamp(0, (screen.1 - h).max(0)),
        );
        Self {
            levels: vec![level],
            screen,
        }
    }

    /// Whether a point is over any open level.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.levels.iter().any(|level| level.rect.contains(x, y))
    }

    pub fn depth(&self) -> usize {
        self.levels.len()
    }

    /// The highlighted item of the innermost level, for tests.
    pub fn hot_label(&self) -> Option<&str> {
        let level = self.levels.last()?;
        level.hot.map(|index| level.items[index].label.as_str())
    }

    /// Screen rectangle of the item labelled `label` in any open level.
    pub fn item_rect(&self, label: &str) -> Option<Rect> {
        self.levels.iter().find_map(|level| {
            level
                .items
                .iter()
                .position(|item| item.label.replace('&', "") == label)
                .map(|index| level.screen_row(index))
        })
    }

    fn open_child(&mut self, level: usize, index: usize, highlight_first: bool) {
        self.levels.truncate(level + 1);
        let parent = &self.levels[level];
        let Kind::Submenu(items) = &parent.items[index].kind else {
            return;
        };
        let items = if items.is_empty() {
            vec![Item::empty()]
        } else {
            items.clone()
        };
        let row = parent.screen_row(index);
        let parent_rect = parent.rect;
        let mut child = Level::new(items, Style::Icons);
        let (w, h) = (child.rect.width(), child.rect.height());
        let x = if parent_rect.right - 3 + w <= self.screen.0 {
            parent_rect.right - 3
        } else {
            (parent_rect.left - w + 3).max(0)
        };
        let y = (row.top - 3).min(self.screen.1 - h).max(0);
        child.place(x, y);
        if highlight_first {
            child.hot = child.items.iter().position(Item::selectable);
        }
        self.levels.push(child);
    }

    fn set_hot(&mut self, level: usize, index: Option<usize>) -> bool {
        if self.levels[level].hot == index
            && self.levels.len() == level + 1 + usize::from(self.opens_child(level, index))
        {
            return false;
        }
        self.levels[level].hot = index;
        self.levels.truncate(level + 1);
        if let Some(index) = index {
            if self.opens_child(level, Some(index)) {
                self.open_child(level, index, false);
            }
        }
        true
    }

    fn opens_child(&self, level: usize, index: Option<usize>) -> bool {
        index.is_some_and(|index| {
            let item = &self.levels[level].items[index];
            item.enabled && matches!(item.kind, Kind::Submenu(_))
        })
    }

    /// Pointer motion: highlight the row under the pointer and open its
    /// submenu, like Windows does after hovering.
    pub fn hover(&mut self, x: i32, y: i32) -> Outcome {
        for level in (0..self.levels.len()).rev() {
            if !self.levels[level].rect.contains(x, y) {
                continue;
            }
            let row = self.levels[level]
                .row_at(x, y)
                .filter(|&index| self.levels[level].items[index].selectable());
            // Keep an open submenu while crossing its parent's separators.
            if row.is_none() {
                return Outcome::None;
            }
            return if self.set_hot(level, row) {
                Outcome::Changed
            } else {
                Outcome::None
            };
        }
        Outcome::None
    }

    pub fn click(&mut self, x: i32, y: i32) -> Outcome {
        for level in (0..self.levels.len()).rev() {
            if !self.levels[level].rect.contains(x, y) {
                continue;
            }
            let Some(index) = self.levels[level].row_at(x, y) else {
                return Outcome::None;
            };
            return self.activate(level, index);
        }
        Outcome::Dismiss
    }

    fn activate(&mut self, level: usize, index: usize) -> Outcome {
        let item = self.levels[level].items[index].clone();
        if !item.enabled {
            return Outcome::None;
        }
        match item.kind {
            Kind::Command(command) => Outcome::Chosen(command),
            Kind::Submenu(_) => {
                self.levels[level].hot = Some(index);
                self.open_child(level, index, true);
                Outcome::Changed
            }
            Kind::Separator => Outcome::None,
        }
    }

    fn step(&mut self, delta: isize) -> Outcome {
        let level = self.levels.len() - 1;
        let items = &self.levels[level].items;
        let count = items.len() as isize;
        if count == 0 {
            return Outcome::None;
        }
        let mut index = self.levels[level]
            .hot
            .map(|i| i as isize)
            .unwrap_or(if delta > 0 { -1 } else { count });
        for _ in 0..count {
            index = (index + delta).rem_euclid(count);
            if items[index as usize].selectable() {
                self.levels[level].hot = Some(index as usize);
                return Outcome::Changed;
            }
        }
        Outcome::None
    }

    pub fn key(&mut self, key: super::Key) -> Outcome {
        use super::Key;
        let level = self.levels.len() - 1;
        match key {
            Key::Down => self.step(1),
            Key::Up => self.step(-1),
            Key::Right => match self.levels[level].hot {
                Some(index) if self.opens_child(level, Some(index)) => {
                    self.open_child(level, index, true);
                    Outcome::Changed
                }
                _ => Outcome::Next,
            },
            Key::Left => {
                if level > 0 {
                    self.levels.pop();
                    Outcome::Changed
                } else {
                    Outcome::Previous
                }
            }
            Key::Escape => {
                if level > 0 {
                    self.levels.pop();
                    Outcome::Changed
                } else {
                    Outcome::Dismiss
                }
            }
            Key::Enter => match self.levels[level].hot {
                Some(index) => self.activate(level, index),
                None => Outcome::None,
            },
            Key::Char(ch) => self.mnemonic(ch.to_ascii_lowercase()),
            _ => Outcome::None,
        }
    }

    /// A typed letter chooses the item with that mnemonic; without one, it
    /// cycles through items starting with that letter.
    fn mnemonic(&mut self, ch: char) -> Outcome {
        let level = self.levels.len() - 1;
        let items = &self.levels[level].items;
        let explicit: Vec<usize> = (0..items.len())
            .filter(|&i| items[i].enabled && items[i].mnemonic() == Some(ch))
            .collect();
        if explicit.len() == 1 {
            return self.activate(level, explicit[0]);
        }
        let candidates: Vec<usize> = if explicit.is_empty() {
            (0..items.len())
                .filter(|&i| items[i].selectable() && items[i].first_letter() == Some(ch))
                .collect()
        } else {
            explicit
        };
        match candidates.len() {
            0 => Outcome::None,
            1 => self.activate(level, candidates[0]),
            _ => {
                let current = self.levels[level].hot;
                let next = candidates
                    .iter()
                    .copied()
                    .find(|&i| current.map_or(true, |c| i > c))
                    .unwrap_or(candidates[0]);
                self.levels[level].hot = Some(next);
                self.levels.truncate(level + 1);
                Outcome::Changed
            }
        }
    }
}

/// Draw every open level of a menu.
pub fn paint(p: &mut Painter<'_>, menu: &Menu) {
    for level in &menu.levels {
        paint_level(p, level);
    }
}

fn paint_level(p: &mut Painter<'_>, level: &Level) {
    let full = level.rect;
    p.fill(full, sys_color(4));
    p.raised(full);
    if level.style == Style::Start {
        let banner = Rect::new(
            full.left + 3,
            full.top + 3,
            full.left + 3 + BANNER_WIDTH,
            full.bottom - 3,
        );
        p.fill(banner, 0x808080);
        vertical_text(
            p,
            banner.left + 3,
            banner.bottom - 6,
            b"OTW Desktop",
            0xC0C0C0,
        );
    }
    for (index, item) in level.items.iter().enumerate() {
        let row = level.screen_row(index);
        if matches!(item.kind, Kind::Separator) {
            let y = row.top + row.height() / 2 - 1;
            p.hline(row.left + 1, row.right - 1, y, sys_color(16));
            p.hline(row.left + 1, row.right - 1, y + 1, sys_color(20));
            continue;
        }
        let hot = level.hot == Some(index);
        if hot {
            p.fill(row, sys_color(13));
        }
        let color = if hot { sys_color(14) } else { sys_color(7) };
        let (icon_size, icon_x) = match level.style {
            Style::Start => (32, row.left + 4),
            _ => (16, row.left + 3),
        };
        if let Some(icon) = &item.icon {
            let icon_y = row.top + (row.height() - icon_size) / 2;
            icons::draw(p, icon, icon_x, icon_y, icon_size, false);
            if item.overlay {
                icons::draw_shortcut_overlay(p, icon_x, icon_y, icon_size);
            }
        }
        let text_x = full.left + label_x(level.style);
        let text_y = row.top + (row.height() - 13) / 2;
        let label = item.label.as_bytes();
        if !item.enabled {
            if hot {
                p.mnemonic_text(text_x, text_y, label, sys_color(16));
            } else {
                p.disabled_text(text_x, text_y, label);
            }
        } else if item.default {
            // Default items (the double-click action) are bold.
            let plain: Vec<u8> = label.iter().copied().filter(|b| *b != b'&').collect();
            p.text(text_x, text_y, &plain, color, true);
        } else {
            p.mnemonic_text(text_x, text_y, label, color);
        }
        if !item.accel.is_empty() {
            p.text(
                full.left + level.accel_x,
                text_y,
                item.accel.as_bytes(),
                if item.enabled { color } else { sys_color(16) },
                false,
            );
        }
        if matches!(item.kind, Kind::Submenu(_)) {
            let ax = row.right - 10;
            let ay = row.top + row.height() / 2;
            for i in 0..4 {
                p.vline(ax + i, ay - 3 + i, ay + 4 - i, color);
            }
        }
    }
}

/// Text rotated 90° counter-clockwise, reading bottom to top.
fn vertical_text(p: &mut Painter<'_>, x: i32, bottom: i32, text: &[u8], color: u32) {
    let face = gfx::pick_face(13, true);
    let mut pen = bottom;
    for &byte in text {
        let advance = face.advance(byte);
        for gy in 0..face.height {
            for gx in 0..advance {
                if face.pixel(byte, gx, gy) {
                    p.pixel(x + gy, pen - gx, color);
                }
            }
        }
        pen -= advance;
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;
    use crate::shell::Key;

    fn sample() -> Menu {
        let items = vec![
            Item::submenu(
                "&Programs",
                vec![
                    Item::submenu("Accessories", vec![Item::command("Notepad", Command::Run)]),
                    Item::command("Explorer", Command::Up),
                ],
            ),
            Item::submenu("&Documents", vec![]),
            Item::separator(),
            Item::command("&Run...", Command::Run),
            Item::command("Sh&ut Down...", Command::ShutDown),
        ];
        Menu::open(items, Style::Start, Anchor::Above(2, 672), (1024, 700))
    }

    #[test]
    fn start_menu_sits_above_the_taskbar() {
        let menu = sample();
        assert_eq!(menu.levels[0].rect.bottom, 672);
        assert_eq!(menu.levels[0].rect.left, 2);
    }

    #[test]
    fn hovering_cascades_submenus_and_clicking_chooses() {
        let mut menu = sample();
        let programs = menu.item_rect("Programs").unwrap();
        assert_eq!(
            menu.hover(programs.left + 5, programs.top + 5),
            Outcome::Changed
        );
        assert_eq!(menu.depth(), 2);
        let accessories = menu.item_rect("Accessories").unwrap();
        assert!(accessories.left >= programs.right - 3);
        menu.hover(accessories.left + 5, accessories.top + 5);
        assert_eq!(menu.depth(), 3);
        let notepad = menu.item_rect("Notepad").unwrap();
        assert_eq!(
            menu.click(notepad.left + 5, notepad.top + 5),
            Outcome::Chosen(Command::Run)
        );
        // An empty submenu shows a disabled placeholder.
        let documents = menu.item_rect("Documents").unwrap();
        menu.hover(documents.left + 5, documents.top + 5);
        assert!(menu.item_rect("(Empty)").is_some());
        assert_eq!(menu.click(5, 5), Outcome::Dismiss);
    }

    #[test]
    fn keyboard_navigation_and_mnemonics() {
        let mut menu = sample();
        assert_eq!(menu.key(Key::Down), Outcome::Changed);
        assert_eq!(menu.hot_label(), Some("&Programs"));
        assert_eq!(menu.key(Key::Up), Outcome::Changed);
        assert_eq!(menu.hot_label(), Some("Sh&ut Down..."));
        assert_eq!(menu.key(Key::Char('p')), Outcome::Changed);
        assert_eq!(menu.depth(), 2);
        assert_eq!(menu.hot_label(), Some("Accessories"));
        menu.key(Key::Right);
        assert_eq!(menu.hot_label(), Some("Notepad"));
        assert_eq!(menu.key(Key::Enter), Outcome::Chosen(Command::Run));
        menu.key(Key::Left);
        menu.key(Key::Escape);
        assert_eq!(menu.depth(), 1);
        assert_eq!(menu.key(Key::Char('u')), Outcome::Chosen(Command::ShutDown));
        assert_eq!(menu.key(Key::Escape), Outcome::Dismiss);
    }
}
