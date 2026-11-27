//! The desktop shell: desktop icons, the taskbar with its Menu button (a
//! menu with a search field) and clock, a folder (Explorer) window with a
//! menu bar, context menus, dialogs, and the file operations behind them.
//! Drawn in OTW's dark, flat theme (`win32::theme`).
//!
//! The shell is host-side Rust, independent of the display backend: it
//! takes pointer and key input, paints into a [`Surface`] the size of the
//! screen, and hands programs to launch back to the caller. Files live on a
//! [`Volume`] — normally the host directory behind guest drive C:, so the
//! files programs save appear in Explorer and vice versa.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

pub mod icons;
pub mod launch;
pub mod popup;
pub mod search;
pub mod setup;
pub mod volume;

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::{
    shell::launch::LaunchRequest,
    win32::gdi::sys_color,
    win32::lnk,
    win32::gfx::{Rect, Region, Surface},
    win32::theme,
    win32::user::{self, text_width, Painter},
};
use icons::{Bitmap, Builtin};
use popup::{Anchor, Command, Item, Menu, Outcome, Style};
use volume::{extension, join, parse_path, path_text, Entry, VolPath, Volume};

pub use crate::win32::user::TASKBAR_HEIGHT;
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const CELL_WIDTH: i32 = 75;
const CELL_HEIGHT: i32 = 70;
const ROW_HEIGHT: i32 = 17;
const CAPTION: i32 = 18;
/// Width of a flat caption button.
const CAPTION_BUTTON: i32 = 26;
const MENU_BAR: i32 = 19;
const TOOLBAR: i32 = 28;
const STATUS_BAR: i32 = 20;
const BAR_MENUS: [&str; 4] = ["&File", "&Edit", "&View", "&Help"];
/// Room for each notification-area icon.
const TRAY_SLOT: i32 = 22;

/// Width of bold text (the clock).
fn gfx_bold_width(text: &[u8]) -> i32 {
    crate::win32::gfx::pick_face(13, true).text_width(text)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Escape,
    Backspace,
    Delete,
    Tab,
    F2,
    F4,
    F5,
    F10,
    /// The context-menu key.
    Apps,
    /// The Windows logo key.
    Logo,
    Char(char),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
}

/// A program's taskbar button, supplied by the window system.
#[derive(Clone, Debug)]
pub struct Task {
    pub label: String,
    pub active: bool,
    /// File name of the program the window belongs to, for its icon.
    pub program: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerOutcome {
    /// Not the shell's: deliver it to the program under the pointer.
    Unhandled,
    Handled,
    /// A program's taskbar button was clicked.
    Task(usize),
    /// A mouse event on an icon in the notification area.
    Tray(usize, TrayEvent),
}

/// A mouse event on a notification-area icon, reported to its program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    Down(Button),
    Up(Button),
    DoubleClick(Button),
}

impl TrayEvent {
    /// The mouse message Windows reports for the event.
    pub fn message(self) -> u32 {
        let (base, button) = match self {
            Self::Down(button) => (0x201, button),
            Self::Up(button) => (0x202, button),
            Self::DoubleClick(button) => (0x203, button),
        };
        if button == Button::Right {
            base + 3
        } else {
            base
        }
    }
}

/// An icon in the taskbar notification area, supplied by the window system.
#[derive(Clone, Debug, Default)]
pub struct TrayItem {
    pub icon: Option<Bitmap>,
    pub tip: String,
}

/// Local time as (hour, minute, weekday 0 = Sunday, day, month 0-11, year).
#[cfg(not(feature = "std"))]
fn local_time() -> (i32, i32, usize, i32, usize, i32) {
    // UTC: the kernel has no time zone yet.
    let seconds = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        (of_day / 3600) as i32,
        (of_day % 3600 / 60) as i32,
        (days + 4).rem_euclid(7) as usize,
        day as i32,
        (month - 1) as usize,
        year as i32,
    )
}

#[cfg(feature = "std")]
fn local_time() -> (i32, i32, usize, i32, usize, i32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    // SAFETY: localtime_r writes only the `tm` it is given.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    (
        tm.tm_hour,
        tm.tm_min,
        tm.tm_wday as usize % 7,
        tm.tm_mday,
        tm.tm_mon as usize % 12,
        tm.tm_year + 1900,
    )
}

/// Paint passes, interleaved by the caller with program windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// The desktop and, when a program is in front, the folder window.
    Background,
    /// The folder window when it is the foreground window.
    Foreground,
    /// The taskbar, menus and dialogs.
    Top,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Desktop,
    Explorer,
    Program,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Desktop,
    Explorer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Computer,
    Path(VolPath),
}

#[derive(Clone, Debug)]
struct DesktopIcon {
    label: String,
    target: Target,
    is_dir: bool,
}

struct Explorer {
    rect: Rect,
    path: VolPath,
    entries: Vec<Entry>,
    selected: Option<usize>,
    scroll: usize,
    minimized: bool,
    /// The normal position while maximized.
    restore: Option<Rect>,
}

/// A one-line text box (rename, Run).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edit {
    pub text: String,
    caret: usize,
    all: bool,
}

impl Edit {
    fn new(text: &str, select_all: bool) -> Self {
        Self {
            text: text.to_string(),
            caret: text.len(),
            all: select_all,
        }
    }

    fn take_selection(&mut self) {
        if self.all {
            self.text.clear();
            self.caret = 0;
            self.all = false;
        }
    }

    fn prev(&self) -> usize {
        self.text[..self.caret]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next(&self) -> usize {
        self.text[self.caret..]
            .chars()
            .next()
            .map(|c| self.caret + c.len_utf8())
            .unwrap_or(self.caret)
    }

    /// Editing keys; returns whether the key was an editing key.
    fn key(&mut self, key: Key, mods: Mods) -> bool {
        match key {
            Key::Char('a') if mods.ctrl => self.all = true,
            Key::Char(ch) if !mods.ctrl && !mods.alt && (ch as u32) >= 32 && (ch as u32) < 256 => {
                self.take_selection();
                self.text.insert(self.caret, ch);
                self.caret += ch.len_utf8();
            }
            Key::Backspace => {
                if self.all {
                    self.take_selection();
                } else if self.caret > 0 {
                    let start = self.prev();
                    self.text.replace_range(start..self.caret, "");
                    self.caret = start;
                }
            }
            Key::Delete => {
                if self.all {
                    self.take_selection();
                } else {
                    let end = self.next();
                    self.text.replace_range(self.caret..end, "");
                }
            }
            Key::Left => {
                self.caret = if self.all { 0 } else { self.prev() };
                self.all = false;
            }
            Key::Right => {
                self.caret = if self.all {
                    self.text.len()
                } else {
                    self.next()
                };
                self.all = false;
            }
            Key::Home => {
                self.caret = 0;
                self.all = false;
            }
            Key::End => {
                self.caret = self.text.len();
                self.all = false;
            }
            _ => return false,
        }
        true
    }
}

struct Rename {
    place: Place,
    index: usize,
    path: VolPath,
    edit: Edit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Confirmed {
    Delete(VolPath),
    ShutDown,
}

enum DialogKind {
    Message,
    Confirm(Confirmed),
    Run(Edit),
}

struct Dialog {
    title: String,
    lines: Vec<String>,
    kind: DialogKind,
    rect: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogButton {
    Ok,
    Cancel,
    Yes,
    No,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuOwner {
    Start,
    Bar(usize),
    Context(Place),
    /// The folder window's window menu.
    Window,
}

struct OpenMenu {
    menu: Menu,
    owner: MenuOwner,
}

struct Clip {
    paths: Vec<VolPath>,
    cut: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ClickTarget {
    Icon(usize),
    Row(usize),
    Caption,
    Tray(usize),
}

pub struct Shell {
    volume: Option<Volume>,
    /// Extra desktop programs (a disk image has no desktop folder).
    programs: Vec<(String, VolPath)>,
    icons: Vec<DesktopIcon>,
    desktop_selected: Option<usize>,
    explorer: Option<Explorer>,
    focus: Focus,
    menu: Option<OpenMenu>,
    dialog: Option<Dialog>,
    rename: Option<Rename>,
    clip: Option<Clip>,
    drag: Option<(i32, i32)>,
    captured: bool,
    /// Icons in the notification area, kept up to date by the window system.
    tray: Vec<TrayItem>,
    /// A notification-area icon being pressed.
    tray_press: Option<(usize, Button)>,
    /// Last pointer position over the taskbar, for tooltips.
    hover: Option<(i32, i32)>,
    /// The minute the clock last showed.
    clock_minute: Cell<i32>,
    last_click: Option<(ClickTarget, Instant)>,
    launch: Option<LaunchRequest>,
    screen: (i32, i32),
    icon_cache: RefCell<HashMap<String, Option<Bitmap>>>,
    /// What the Menu button's search can find, gathered when first needed.
    search_index: RefCell<Option<Vec<search::Candidate>>>,
    /// Names of the programs running, kept up to date by the window system.
    pub running: Vec<String>,
    /// Set when the user confirms Shut Down.
    pub quit: bool,
}

/// Text in the font's code page (Windows-1252: dashes, quotes, the euro).
fn bytes(text: &str) -> Vec<u8> {
    crate::win32::unicode::string_to_ansi(text)
}

fn stem(name: &str) -> &str {
    name.rsplit_once('.')
        .map(|(stem, _)| stem)
        .filter(|s| !s.is_empty())
        .unwrap_or(name)
}

/// Split text into lines that fit `width` pixels.
fn wrap(text: &str, width: i32) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if text_width(&bytes(&candidate)) > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            } else {
                line = candidate;
            }
        }
        lines.push(line);
    }
    lines
}

/// Truncate text to `width` pixels with an ellipsis.
fn fit(text: &str, width: i32) -> Vec<u8> {
    let mut shown = bytes(text);
    if text_width(&shown) <= width {
        return shown;
    }
    while !shown.is_empty() && text_width(&shown) + text_width(b"...") > width {
        shown.pop();
    }
    shown.extend_from_slice(b"...");
    shown
}

fn type_name(entry: &Entry) -> String {
    if entry.is_dir {
        return "Folder".into();
    }
    match extension(&entry.name).as_str() {
        "EXE" | "COM" => "Application".into(),
        "DLL" => "Application Extension".into(),
        "LNK" => "Shortcut".into(),
        "TXT" => "Text Document".into(),
        "BMP" => "Bitmap Image".into(),
        "INI" | "INF" => "Configuration Settings".into(),
        "HLP" => "Help File".into(),
        "" => "File".into(),
        ext => format!("{ext} File"),
    }
}

fn size_text(size: u64) -> String {
    format!("{}KB", size.div_ceil(1024))
}

fn with_clip(p: &mut Painter<'_>, rect: Rect, paint: impl FnOnce(&mut Painter<'_>)) {
    let narrowed = p.clip.intersect_rect(&rect);
    let saved = std::mem::replace(&mut p.clip, narrowed);
    paint(p);
    p.clip = saved;
}

impl Shell {
    pub fn new(volume: Option<Volume>, programs: Vec<(String, VolPath)>) -> Self {
        let mut shell = Self {
            volume,
            programs,
            icons: Vec::new(),
            desktop_selected: None,
            explorer: None,
            focus: Focus::Desktop,
            menu: None,
            dialog: None,
            rename: None,
            clip: None,
            drag: None,
            captured: false,
            tray: Vec::new(),
            tray_press: None,
            hover: None,
            clock_minute: Cell::new(-1),
            last_click: None,
            launch: None,
            screen: (1024, 700),
            icon_cache: RefCell::new(HashMap::new()),
            search_index: RefCell::new(None),
            running: Vec::new(),
            quit: false,
        };
        shell.refresh();
        shell
    }

    pub fn volume(&self) -> Option<&Volume> {
        self.volume.as_ref()
    }

    pub fn set_screen(&mut self, width: i32, height: i32) {
        if self.screen != (width, height) {
            self.screen = (width, height);
            if let Some(explorer) = self.explorer.as_mut().filter(|e| e.restore.is_some()) {
                explorer.rect = Rect::new(0, 0, width, height - TASKBAR_HEIGHT);
            } else if let Some(explorer) = &mut self.explorer {
                let (w, h) = (
                    explorer.rect.width().min(width),
                    explorer.rect.height().min(height - TASKBAR_HEIGHT),
                );
                let x = explorer.rect.left.clamp(0, (width - w).max(0));
                let y = explorer
                    .rect
                    .top
                    .clamp(0, (height - TASKBAR_HEIGHT - h).max(0));
                explorer.rect = Rect::sized(x, y, w, h);
            }
        }
    }

    /// The next program the user asked to start.
    pub fn take_launch(&mut self) -> Option<LaunchRequest> {
        self.launch.take()
    }

    /// A program window was activated by the window system.
    pub fn focus_program(&mut self) {
        self.commit_rename();
        self.focus = Focus::Program;
    }

    /// The desktop (or the folder window) takes the keyboard focus, e.g.
    /// after the program in use was minimized.
    pub fn focus_desktop(&mut self) {
        self.focus = if self.explorer.as_ref().is_some_and(|e| !e.minimized) {
            Focus::Explorer
        } else {
            Focus::Desktop
        };
    }

    /// Show a message box from the system (e.g. how a program ended).
    pub fn show_message(&mut self, title: &str, text: &str) {
        self.message(title, text);
    }

    /// The running program exited: take the focus back and show any files
    /// it saved.
    pub fn program_exited(&mut self, error: Option<String>) {
        if self.focus == Focus::Program {
            self.focus = if self.explorer.is_some() {
                Focus::Explorer
            } else {
                Focus::Desktop
            };
        }
        self.refresh();
        if let Some(error) = error {
            self.message("Program Error", &error);
        }
    }

    /// Whether the desktop or the folder window (rather than a program) is
    /// in use.
    pub fn has_focus(&self) -> bool {
        self.focus != Focus::Program
    }

    pub fn foreground_explorer(&self) -> bool {
        self.focus == Focus::Explorer && self.explorer_visible()
    }

    /// The folder window is open and not minimized.
    fn explorer_visible(&self) -> bool {
        self.explorer.as_ref().is_some_and(|e| !e.minimized)
    }

    /// Whether the shell has keyboard focus (or a menu or dialog is open).
    pub fn wants_keyboard(&self) -> bool {
        self.focus != Focus::Program || self.menu.is_some() || self.dialog.is_some()
    }

    pub fn explorer_path(&self) -> Option<String> {
        self.explorer.as_ref().map(|e| path_text(&e.path))
    }

    pub fn dialog_title(&self) -> Option<&str> {
        self.dialog.as_ref().map(|d| d.title.as_str())
    }

    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    /// Screen rectangle of an open menu item, for scripted input and tests.
    pub fn menu_item_rect(&self, label: &str) -> Option<Rect> {
        self.menu.as_ref()?.menu.item_rect(label)
    }

    // ── File system views ────────────────────────────────────────────────

    fn desktop_dir(&self) -> Option<VolPath> {
        let volume = self.volume.as_ref()?;
        let dir = parse_path(setup::DESKTOP);
        (volume.writable() && volume.is_dir(&dir)).then_some(dir)
    }

    /// Reload the desktop and the folder window from the volume.
    pub fn refresh(&mut self) {
        self.icon_cache.borrow_mut().clear();
        self.search_index.borrow_mut().take();
        let mut icons = Vec::new();
        if self.volume.is_some() {
            icons.push(DesktopIcon {
                label: "My Computer".into(),
                target: Target::Computer,
                is_dir: true,
            });
        }
        if let (Some(volume), Some(dir)) = (&self.volume, self.desktop_dir()) {
            for entry in volume.list(&dir).unwrap_or_default() {
                let label = if extension(&entry.name) == "LNK" {
                    stem(&entry.name).to_string()
                } else {
                    entry.name.clone()
                };
                icons.push(DesktopIcon {
                    label,
                    target: Target::Path(join(&dir, &entry.name)),
                    is_dir: entry.is_dir,
                });
            }
        }
        for (name, path) in &self.programs {
            icons.push(DesktopIcon {
                label: name.clone(),
                target: Target::Path(path.clone()),
                is_dir: false,
            });
        }
        let selected = self
            .desktop_selected
            .and_then(|i| self.icons.get(i))
            .map(|icon| icon.target.clone());
        self.icons = icons;
        self.desktop_selected =
            selected.and_then(|target| self.icons.iter().position(|icon| icon.target == target));
        if let (Some(volume), Some(explorer)) = (&self.volume, &mut self.explorer) {
            let selected = explorer
                .selected
                .and_then(|i| explorer.entries.get(i))
                .map(|e| e.name.clone());
            explorer.entries = volume.list(&explorer.path).unwrap_or_default();
            explorer.selected =
                selected.and_then(|name| explorer.entries.iter().position(|e| e.name == name));
            explorer.scroll = explorer
                .scroll
                .min(explorer.entries.len().saturating_sub(1));
        }
    }

    fn icon_for(&self, path: &[String], is_dir: bool, size: i32, depth: usize) -> (Bitmap, bool) {
        if is_dir {
            return (icons::builtin(Builtin::Folder), false);
        }
        let name = path.last().map(String::as_str).unwrap_or("");
        let builtin = |icon| (icons::builtin(icon), false);
        match extension(name).as_str() {
            "EXE" => {
                let key = format!("{}:{size}", path_text(path).to_ascii_lowercase());
                let cached = self.icon_cache.borrow().get(&key).cloned();
                let icon = cached.unwrap_or_else(|| {
                    let icon = self
                        .volume
                        .as_ref()
                        .and_then(|v| v.read(path).ok())
                        .and_then(|b| icons::from_pe(&b, size));
                    self.icon_cache.borrow_mut().insert(key, icon.clone());
                    icon
                });
                (
                    icon.unwrap_or_else(|| icons::builtin(Builtin::Program)),
                    false,
                )
            }
            "LNK" if depth < 4 => match self.resolve_link(path) {
                Some((target, _)) => {
                    let dir = self.volume.as_ref().is_some_and(|v| v.is_dir(&target));
                    (self.icon_for(&target, dir, size, depth + 1).0, true)
                }
                None => builtin(Builtin::Document),
            },
            "TXT" | "INI" | "INF" | "LOG" | "BAT" => builtin(Builtin::TextDocument),
            "BMP" => builtin(Builtin::Picture),
            "COM" | "PIF" => builtin(Builtin::Program),
            _ => builtin(Builtin::Document),
        }
    }

    /// Target and arguments of a shortcut file.
    fn resolve_link(&self, path: &[String]) -> Option<(VolPath, String)> {
        let link = lnk::read(&self.volume.as_ref()?.read(path).ok()?)?;
        let target = if link.target.contains(':') || link.target.starts_with('\\') {
            parse_path(&link.target)
        } else {
            let mut base = path[..path.len().saturating_sub(1)].to_vec();
            base.extend(parse_path(&format!("\\{}", link.target)));
            parse_path(&path_text(&base))
        };
        Some((target, link.arguments))
    }

    // ── Opening things ───────────────────────────────────────────────────

    fn message(&mut self, title: &str, text: &str) {
        self.dialog = Some(self.make_dialog(title, text, DialogKind::Message));
    }

    fn make_dialog(&self, title: &str, text: &str, kind: DialogKind) -> Dialog {
        let width = if text_width(&bytes(text)) > 600 {
            420
        } else {
            340
        };
        let lines = wrap(text, width - 40);
        let extra = if matches!(kind, DialogKind::Run(_)) {
            34
        } else {
            0
        };
        let height = 3 + CAPTION + 16 + lines.len() as i32 * 15 + extra + 14 + 23 + 12 + 3;
        let x = (self.screen.0 - width) / 2;
        let y = ((self.screen.1 - TASKBAR_HEIGHT - height) / 2).max(0);
        Dialog {
            title: title.into(),
            lines,
            kind,
            rect: Rect::sized(x, y, width, height),
        }
    }

    fn explore(&mut self, path: VolPath) {
        let Some(volume) = &self.volume else {
            return;
        };
        let entries = match volume.list(&path) {
            Ok(entries) => entries,
            Err(error) => return self.message("Explorer", &error),
        };
        let restore = self.explorer.as_ref().and_then(|e| e.restore);
        let rect = self.explorer.as_ref().map(|e| e.rect).unwrap_or_else(|| {
            let (w, h) = (
                560.min(self.screen.0),
                380.min(self.screen.1 - TASKBAR_HEIGHT),
            );
            Rect::sized(
                ((self.screen.0 - w) / 2).min(140).max(0),
                40.min((self.screen.1 - TASKBAR_HEIGHT - h).max(0)),
                w,
                h,
            )
        });
        self.explorer = Some(Explorer {
            rect,
            path,
            entries,
            selected: None,
            scroll: 0,
            minimized: false,
            restore,
        });
        self.focus = Focus::Explorer;
    }

    fn open_path(&mut self, path: VolPath, depth: usize) {
        let Some(volume) = self.volume.clone() else {
            return;
        };
        let name = path.last().cloned().unwrap_or_default();
        let Some(entry) = volume.entry(&path) else {
            return self.message(
                &name,
                &format!("Cannot find the file '{}' (or one of its components). Make sure the path and filename are correct.", volume.guest_path(&path)),
            );
        };
        if entry.is_dir {
            return self.explore(path);
        }
        match extension(&name).as_str() {
            "LNK" if depth < 4 => match self.resolve_link(&path) {
                Some((target, arguments)) if extension(target.last().map(String::as_str).unwrap_or("")) == "EXE" => {
                    self.start_program(target, arguments)
                }
                Some((target, _)) => self.open_path(target, depth + 1),
                None => self.message("Problem with Shortcut", &format!("The shortcut '{}' is damaged.", stem(&name))),
            },
            "EXE" => self.start_program(path, String::new()),
            "COM" | "PIF" => self.message(&name, &format!("{name} is an MS-DOS program; this VM runs 32-bit Windows programs.")),
            ext => match setup::association(&volume, ext) {
                Some(program) => {
                    let document = volume.guest_path(&path);
                    self.start_program(program, document);
                }
                None => self.message(
                    "Open With",
                    &format!("There is no program associated with '{name}'. Associate a program with .{} files in WIN.INI [Extensions].", ext.to_ascii_lowercase()),
                ),
            },
        }
    }

    fn start_program(&mut self, program: VolPath, arguments: String) {
        let name = program.last().cloned().unwrap_or_default();
        if !self.volume.as_ref().is_some_and(|v| v.exists(&program)) {
            return self.message(
                &name,
                &format!("Cannot find the program '{}'.", path_text(&program)),
            );
        }
        self.launch = Some(LaunchRequest {
            name,
            path: path_text(&program),
            arguments,
        });
        self.focus = Focus::Program;
    }

    /// The Run dialog: a program, folder or document, with arguments.
    fn run_command(&mut self, text: &str) {
        let text = text.trim();
        let (first, rest) = if let Some(quoted) = text.strip_prefix('"') {
            quoted
                .split_once('"')
                .map(|(a, b)| (a, b.trim()))
                .unwrap_or((quoted, ""))
        } else {
            text.split_once(' ')
                .map(|(a, b)| (a, b.trim()))
                .unwrap_or((text, ""))
        };
        if first.is_empty() {
            return;
        }
        let Some(volume) = self.volume.clone() else {
            return;
        };
        // An unquoted path with spaces names one file or folder when it exists.
        if !text.starts_with('"') && text.contains(' ') && volume.exists(&parse_path(text)) {
            let path = volume.canonical(&parse_path(text));
            return self.open_path(path, 0);
        }
        let mut candidates = vec![parse_path(first)];
        if !first.contains(['\\', ':', '/']) {
            for dir in ["\\WINDOWS", "\\WINDOWS\\SYSTEM", "\\"] {
                candidates.push(join(&parse_path(dir), first));
                if extension(first).is_empty() {
                    candidates.push(join(&parse_path(dir), &format!("{first}.exe")));
                }
            }
        }
        match candidates.into_iter().find(|path| volume.exists(path)).map(|path| volume.canonical(&path)) {
            Some(path) if extension(path.last().map(String::as_str).unwrap_or("")) == "EXE" => self.start_program(path, rest.to_string()),
            Some(path) => self.open_path(path, 0),
            None => self.message(first, &format!("Cannot find the file '{first}' (or one of its components). Make sure the path and filename are correct.")),
        }
    }

    // ── Selection and commands ───────────────────────────────────────────

    fn place_dir(&self, place: Place) -> Option<VolPath> {
        match place {
            Place::Desktop => self.desktop_dir(),
            Place::Explorer => self.explorer.as_ref().map(|e| e.path.clone()),
        }
    }

    fn selection(&self, place: Place) -> Option<Target> {
        match place {
            Place::Desktop => self
                .desktop_selected
                .and_then(|i| self.icons.get(i))
                .map(|icon| icon.target.clone()),
            Place::Explorer => {
                let explorer = self.explorer.as_ref()?;
                let entry = explorer.entries.get(explorer.selected?)?;
                Some(Target::Path(join(&explorer.path, &entry.name)))
            }
        }
    }

    fn selected_path(&self, place: Place) -> Option<VolPath> {
        match self.selection(place)? {
            Target::Path(path) => Some(path),
            Target::Computer => None,
        }
    }

    fn writable(&self) -> bool {
        self.volume.as_ref().is_some_and(Volume::writable)
    }

    fn select_name(&mut self, place: Place, name: &str) -> Option<usize> {
        match place {
            Place::Desktop => {
                let index = self.icons.iter().position(|icon| matches!(&icon.target, Target::Path(p) if p.last().is_some_and(|n| n == name)))?;
                self.desktop_selected = Some(index);
                Some(index)
            }
            Place::Explorer => {
                let explorer = self.explorer.as_mut()?;
                let index = explorer.entries.iter().position(|e| e.name == name)?;
                explorer.selected = Some(index);
                Some(index)
            }
        }
    }

    fn create(&mut self, place: Place, folder: bool) {
        let (Some(volume), Some(dir)) = (self.volume.clone(), self.place_dir(place)) else {
            return;
        };
        let name = if folder {
            volume.unique_name(&dir, "New Folder", "")
        } else {
            volume.unique_name(&dir, "New Text Document", "txt")
        };
        let result = if folder {
            volume.create_dir(&join(&dir, &name))
        } else {
            volume.write(&join(&dir, &name), b"")
        };
        if let Err(error) = result {
            return self.message(
                "Error Creating File",
                &format!("Unable to create '{name}': {error}"),
            );
        }
        self.refresh();
        if self.select_name(place, &name).is_some() {
            self.ensure_visible();
            self.begin_rename(place);
        }
    }

    fn begin_rename(&mut self, place: Place) {
        if !self.writable() {
            return;
        }
        let Some(path) = self.selected_path(place) else {
            return;
        };
        let index = match place {
            Place::Desktop => self.desktop_selected,
            Place::Explorer => self.explorer.as_ref().and_then(|e| e.selected),
        };
        let Some(index) = index else {
            return;
        };
        let name = path.last().cloned().unwrap_or_default();
        let shown = if place == Place::Desktop && extension(&name) == "LNK" {
            stem(&name).to_string()
        } else {
            name
        };
        self.rename = Some(Rename {
            place,
            index,
            path,
            edit: Edit::new(&shown, true),
        });
    }

    fn commit_rename(&mut self) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let Some(volume) = self.volume.clone() else {
            return;
        };
        let old = rename.path.last().cloned().unwrap_or_default();
        let mut new = rename.edit.text.trim().to_string();
        if rename.place == Place::Desktop && extension(&old) == "LNK" {
            new.push_str(".lnk");
        }
        if new == old {
            return;
        }
        match volume.rename(&rename.path, &new) {
            Ok(target) => {
                self.refresh();
                if let Some(name) = target.last() {
                    self.select_name(rename.place, name);
                }
            }
            Err(error) => self.message("Error Renaming File", &error),
        }
    }

    fn delete_confirmed(&mut self, path: VolPath) {
        if let Some(volume) = self.volume.clone() {
            if let Err(error) = volume.delete(&path) {
                self.message(
                    "Error Deleting File",
                    &format!(
                        "Cannot delete {}: {error}",
                        path.last().cloned().unwrap_or_default()
                    ),
                );
            }
        }
        self.refresh();
    }

    fn paste(&mut self, place: Place) {
        let (Some(volume), Some(dir), Some(clip)) =
            (self.volume.clone(), self.place_dir(place), self.clip.take())
        else {
            return;
        };
        let mut keep = !clip.cut;
        for source in &clip.paths {
            let name = source.last().cloned().unwrap_or_default();
            let same_dir = source[..source.len() - 1]
                .iter()
                .zip(&dir)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
                && source.len() - 1 == dir.len();
            let result = if clip.cut {
                if same_dir {
                    keep = false;
                    continue;
                }
                if volume.exists(&join(&dir, &name)) {
                    Err(format!("Cannot move {name}: a file with that name already exists in the destination folder."))
                } else {
                    volume.move_to(source, &join(&dir, &name))
                }
            } else {
                let copy = volume.copy_name(&dir, &name);
                volume.copy(source, &join(&dir, &copy))
            };
            if let Err(error) = result {
                keep = true;
                self.message("Error Copying File", &error);
                break;
            }
        }
        if keep {
            self.clip = Some(clip);
        }
        self.refresh();
    }

    fn properties(&mut self, place: Place) {
        let Some(volume) = self.volume.clone() else {
            return;
        };
        match self.selection(place) {
            Some(Target::Computer) | None if place == Place::Desktop => {
                let text = format!(
                    "Drive C: {}\nFolders: \\WINDOWS, \\My Documents",
                    volume.label()
                );
                self.message("My Computer", &text);
            }
            Some(Target::Path(path)) => {
                let Some(entry) = volume.entry(&path) else {
                    return;
                };
                let location = volume.guest_path(&path[..path.len() - 1]);
                let mut text = format!(
                    "{}\nType: {}\nLocation: {location}",
                    entry.name,
                    type_name(&entry)
                );
                if !entry.is_dir {
                    text.push_str(&format!(
                        "\nSize: {} ({} bytes)",
                        size_text(entry.size),
                        entry.size
                    ));
                } else {
                    let count = volume.list(&path).map(|e| e.len()).unwrap_or(0);
                    text.push_str(&format!("\nContains: {count} object(s)"));
                }
                if let Some((target, arguments)) = (extension(&entry.name) == "LNK")
                    .then(|| self.resolve_link(&path))
                    .flatten()
                {
                    text.push_str(&format!(
                        "\nTarget: {} {arguments}",
                        volume.guest_path(&target)
                    ));
                }
                self.message(&format!("{} Properties", entry.name), &text);
            }
            _ => {
                let dir = self.place_dir(place).unwrap_or_default();
                let count = volume.list(&dir).map(|e| e.len()).unwrap_or(0);
                self.message(
                    "Properties",
                    &format!("{}\nContains: {count} object(s)", volume.guest_path(&dir)),
                );
            }
        }
    }

    fn execute(&mut self, command: Command, place: Place) {
        match command {
            Command::Open(path) => self.open_path(path, 0),
            Command::Explore(path) => self.explore(path),
            Command::OpenSelection => match self.selection(place) {
                Some(Target::Computer) => self.explore(Vec::new()),
                Some(Target::Path(path)) => self.open_path(path, 0),
                None => {}
            },
            Command::NewFolder => self.create(place, true),
            Command::NewTextDocument => self.create(place, false),
            Command::Delete => {
                if let (true, Some(path)) = (self.writable(), self.selected_path(place)) {
                    let name = path.last().cloned().unwrap_or_default();
                    let folder = self.volume.as_ref().is_some_and(|v| v.is_dir(&path));
                    let (title, text) = if folder {
                        ("Confirm Folder Delete", format!("Are you sure you want to remove the folder '{name}' and all its contents?"))
                    } else {
                        ("Confirm File Delete", format!("Are you sure you want to delete '{name}'?"))
                    };
                    self.dialog = Some(self.make_dialog(title, &text, DialogKind::Confirm(Confirmed::Delete(path))));
                }
            }
            Command::Rename => self.begin_rename(place),
            Command::Cut | Command::Copy => {
                if let Some(path) = self.selected_path(place) {
                    self.clip = Some(Clip { paths: vec![path], cut: command == Command::Cut });
                }
            }
            Command::Paste => self.paste(place),
            Command::Properties => self.properties(place),
            Command::Refresh => self.refresh(),
            Command::Up => {
                if let Some(explorer) = &self.explorer {
                    if let Some((_, parent)) = explorer.path.split_last() {
                        let (child, parent) = (explorer.path.last().cloned().unwrap_or_default(), parent.to_vec());
                        self.explore(parent);
                        self.select_name(Place::Explorer, &child);
                        self.ensure_visible();
                    }
                }
            }
            Command::Close => {
                self.explorer = None;
                self.focus = Focus::Desktop;
            }
            Command::Minimize => {
                if let Some(explorer) = self.explorer.as_mut() {
                    explorer.minimized = true;
                    self.focus = Focus::Desktop;
                }
            }
            Command::Maximize => {
                let full = Rect::new(0, 0, self.screen.0, self.screen.1 - TASKBAR_HEIGHT);
                if let Some(explorer) = self.explorer.as_mut() {
                    if explorer.restore.is_none() {
                        explorer.restore = Some(explorer.rect);
                        explorer.rect = full;
                    }
                    explorer.minimized = false;
                    self.focus = Focus::Explorer;
                }
                self.ensure_visible();
            }
            Command::Restore => {
                if let Some(explorer) = self.explorer.as_mut() {
                    if explorer.minimized {
                        explorer.minimized = false;
                    } else if let Some(rect) = explorer.restore.take() {
                        explorer.rect = rect;
                    }
                    self.focus = Focus::Explorer;
                }
                self.ensure_visible();
            }
            Command::Run => {
                let dialog = self.make_dialog(
                    "Run",
                    "Type the name of a program, folder, or document, and it will be opened for you.",
                    DialogKind::Run(Edit::default()),
                );
                self.dialog = Some(dialog);
            }
            Command::ShutDown => {
                if !self.running.is_empty() {
                    let names = self.running.join(", ");
                    self.message("Shut Down", &format!("Close the programs that are still running before shutting down: {names}."));
                } else {
                    let dialog = self.make_dialog("Shut Down", "Are you sure you want to shut down the computer?", DialogKind::Confirm(Confirmed::ShutDown));
                    self.dialog = Some(dialog);
                }
            }
            Command::About => self.message(
                "About OTW Desktop",
                "OTW Desktop\nA Windows 95 style shell for the OTW virtual machine. Programs run on the genuine Win32 binaries; files live on drive C:.",
            ),
        }
    }

    // ── Menus ────────────────────────────────────────────────────────────

    fn folder_menu(&self, dir: &[String], depth: usize) -> Vec<Item> {
        let Some(volume) = &self.volume else {
            return Vec::new();
        };
        let mut items = Vec::new();
        for entry in volume.list(dir).unwrap_or_default() {
            let path = join(dir, &entry.name);
            if entry.is_dir {
                if depth < 4 {
                    items.push(
                        Item::submenu(&entry.name, self.folder_menu(&path, depth + 1))
                            .icon(icons::builtin(Builtin::Folder), false),
                    );
                }
                continue;
            }
            let label = if matches!(extension(&entry.name).as_str(), "LNK" | "EXE") {
                stem(&entry.name).to_string()
            } else {
                entry.name.clone()
            };
            let (icon, _) = self.icon_for(&path, false, 16, 0);
            items.push(
                Item::command(&label.replace('&', "&&"), Command::Open(path)).icon(icon, false),
            );
        }
        items
    }

    fn start_items(&self) -> Vec<Item> {
        let mut programs = match &self.volume {
            Some(volume) if volume.writable() => self.folder_menu(&parse_path(setup::PROGRAMS), 0),
            _ => self
                .programs
                .iter()
                .map(|(name, path)| {
                    Item::command(stem(name), Command::Open(path.clone()))
                        .icon(self.icon_for(path, false, 16, 0).0, false)
                })
                .collect(),
        };
        if self.volume.is_some() {
            programs.push(
                Item::command("Windows Explorer", Command::Explore(Vec::new()))
                    .icon(icons::builtin(Builtin::Computer), false),
            );
        }
        let documents: Vec<Item> = self
            .volume
            .as_ref()
            .and_then(|v| v.list(&parse_path(setup::DOCUMENTS)).ok())
            .unwrap_or_default()
            .into_iter()
            .take(15)
            .map(|entry| {
                let path = join(&parse_path(setup::DOCUMENTS), &entry.name);
                let icon = self.icon_for(&path, entry.is_dir, 16, 0).0;
                Item::command(&entry.name.replace('&', "&&"), Command::Open(path)).icon(icon, false)
            })
            .collect();
        vec![
            Item::submenu("&Programs", programs).icon(icons::builtin(Builtin::Folder), false),
            Item::submenu("&Documents", documents).icon(icons::builtin(Builtin::Document), false),
            Item::command("&Help", Command::About)
                .icon(icons::builtin(Builtin::TextDocument), false),
            Item::command("&Run...", Command::Run).icon(icons::builtin(Builtin::Run), false),
            Item::separator(),
            Item::command("Sh&ut Down...", Command::ShutDown)
                .icon(icons::builtin(Builtin::ShutDown), false),
        ]
    }

    fn new_submenu(&self) -> Item {
        let writable = self.writable();
        Item::submenu(
            "Ne&w",
            vec![
                Item::command("&Folder", Command::NewFolder)
                    .enabled(writable)
                    .icon(icons::builtin(Builtin::Folder), false),
                Item::separator(),
                Item::command("Text Document", Command::NewTextDocument)
                    .enabled(writable)
                    .icon(icons::builtin(Builtin::TextDocument), false),
            ],
        )
        .enabled(writable)
    }

    fn bar_items(&self, index: usize) -> Vec<Item> {
        let selected = self.selected_path(Place::Explorer).is_some();
        let writable = self.writable();
        let at_root = self.explorer.as_ref().map_or(true, |e| e.path.is_empty());
        match index {
            0 => vec![
                self.new_submenu(),
                Item::separator(),
                Item::command("&Open", Command::OpenSelection)
                    .enabled(selected)
                    .accel("Enter"),
                Item::separator(),
                Item::command("&Delete", Command::Delete)
                    .enabled(selected && writable)
                    .accel("Del"),
                Item::command("Rena&me", Command::Rename)
                    .enabled(selected && writable)
                    .accel("F2"),
                Item::command("P&roperties", Command::Properties).accel("Alt+Enter"),
                Item::separator(),
                Item::command("&Close", Command::Close).accel("Alt+F4"),
            ],
            1 => vec![
                Item::command("Cu&t", Command::Cut)
                    .enabled(selected && writable)
                    .accel("Ctrl+X"),
                Item::command("&Copy", Command::Copy)
                    .enabled(selected)
                    .accel("Ctrl+C"),
                Item::command("&Paste", Command::Paste)
                    .enabled(self.clip.is_some() && writable)
                    .accel("Ctrl+V"),
            ],
            2 => vec![
                Item::command("&Refresh", Command::Refresh).accel("F5"),
                Item::command("&Up One Level", Command::Up)
                    .enabled(!at_root)
                    .accel("Backspace"),
            ],
            _ => vec![Item::command("&About OTW Desktop", Command::About)],
        }
    }

    fn context_items(&self, place: Place) -> Vec<Item> {
        let writable = self.writable();
        match self.selection(place) {
            Some(Target::Computer) => vec![
                Item::command("&Open", Command::OpenSelection).default_item(),
                Item::command("&Explore", Command::Explore(Vec::new())),
                Item::separator(),
                Item::command("P&roperties", Command::Properties),
            ],
            Some(Target::Path(path)) => {
                let folder = self.volume.as_ref().is_some_and(|v| v.is_dir(&path));
                let mut items = vec![Item::command("&Open", Command::OpenSelection).default_item()];
                if folder {
                    items.push(Item::command("&Explore", Command::Explore(path)));
                }
                items.extend([
                    Item::separator(),
                    Item::command("Cu&t", Command::Cut).enabled(writable),
                    Item::command("&Copy", Command::Copy),
                    Item::separator(),
                    Item::command("&Delete", Command::Delete).enabled(writable),
                    Item::command("Rena&me", Command::Rename).enabled(writable),
                    Item::separator(),
                    Item::command("P&roperties", Command::Properties),
                ]);
                items
            }
            None => vec![
                Item::command("R&efresh", Command::Refresh),
                Item::separator(),
                Item::command("&Paste", Command::Paste).enabled(self.clip.is_some() && writable),
                Item::separator(),
                self.new_submenu(),
                Item::separator(),
                Item::command("P&roperties", Command::Properties),
            ],
        }
    }

    /// Open the Menu button's menu with `query` in its search field: the
    /// usual items while it is empty, else the best matches with the first
    /// one highlighted (Enter starts it).
    fn open_start(&mut self, query: String) {
        let searching = !query.trim().is_empty();
        let items = if searching {
            self.search_items(&query)
        } else {
            self.start_items()
        };
        let mut menu = Menu::open(items, Style::Start, self.start_anchor(), self.screen);
        menu.search = query;
        if searching {
            menu.highlight_first();
        }
        self.menu = Some(OpenMenu {
            menu,
            owner: MenuOwner::Start,
        });
    }

    fn start_anchor(&self) -> Anchor {
        Anchor::Above(self.start_rect().left, self.screen.1 - TASKBAR_HEIGHT - 4)
    }

    /// What is typed in the Menu button's search field, if it is open.
    pub fn search_text(&self) -> Option<&str> {
        self.menu
            .as_ref()
            .filter(|m| m.owner == MenuOwner::Start)
            .map(|m| m.menu.search.as_str())
    }

    fn open_menu(&mut self, owner: MenuOwner, at: Option<(i32, i32)>, highlight_first: bool) {
        self.commit_rename();
        if owner == MenuOwner::Start {
            self.open_start(String::new());
            if highlight_first {
                if let Some(open) = &mut self.menu {
                    open.menu.key(Key::Down);
                }
            }
            return;
        }
        let (items, style, anchor) = match owner {
            MenuOwner::Start => unreachable!(),
            MenuOwner::Bar(index) => {
                let rect = self.bar_rects().get(index).copied().unwrap_or_default();
                (self.bar_items(index), Style::Plain, Anchor::Below(rect))
            }
            MenuOwner::Context(place) => {
                let (x, y) = at.unwrap_or(self.context_anchor(place));
                (self.context_items(place), Style::Icons, Anchor::Point(x, y))
            }
            MenuOwner::Window => {
                let (x, y) = at.unwrap_or_else(|| {
                    let e = self.explorer.as_ref().map(|e| e.rect).unwrap_or_default();
                    (e.left + 3, e.top + 3 + CAPTION)
                });
                (self.window_menu_items(), Style::Plain, Anchor::Point(x, y))
            }
        };
        let mut menu = Menu::open(items, style, anchor, self.screen);
        if highlight_first {
            menu.key(Key::Down);
        }
        self.menu = Some(OpenMenu { menu, owner });
    }

    /// Where a keyboard-opened context menu appears: at the selection.
    fn context_anchor(&self, place: Place) -> (i32, i32) {
        match place {
            Place::Desktop => self.desktop_selected.map(|i| {
                let cell = self.icon_cell(i);
                (cell.left + 40, cell.top + 40)
            }),
            Place::Explorer => self.explorer.as_ref().map(|e| {
                let rows = self.rows_rect(e);
                let row = e.selected.unwrap_or(0).saturating_sub(e.scroll) as i32;
                (rows.left + 40, rows.top + row * ROW_HEIGHT + ROW_HEIGHT)
            }),
        }
        .unwrap_or((self.screen.0 / 2, self.screen.1 / 2))
    }

    fn menu_outcome(&mut self, outcome: Outcome) {
        let Some(owner) = self.menu.as_ref().map(|m| m.owner) else {
            return;
        };
        match outcome {
            Outcome::Chosen(command) => {
                self.menu = None;
                let place = match owner {
                    MenuOwner::Bar(_) | MenuOwner::Window => Place::Explorer,
                    MenuOwner::Context(place) => place,
                    MenuOwner::Start => Place::Desktop,
                };
                self.execute(command, place);
            }
            Outcome::Dismiss => self.menu = None,
            Outcome::Next | Outcome::Previous => {
                if let MenuOwner::Bar(index) = owner {
                    let next = if outcome == Outcome::Next {
                        (index + 1) % BAR_MENUS.len()
                    } else {
                        (index + BAR_MENUS.len() - 1) % BAR_MENUS.len()
                    };
                    self.open_menu(MenuOwner::Bar(next), None, true);
                }
            }
            Outcome::None | Outcome::Changed => {}
        }
    }

    // ── Geometry ─────────────────────────────────────────────────────────

    fn rows_per_column(&self) -> usize {
        ((self.screen.1 - TASKBAR_HEIGHT - 8) / CELL_HEIGHT).max(1) as usize
    }

    fn icon_cell(&self, index: usize) -> Rect {
        let rows = self.rows_per_column();
        let (column, row) = ((index / rows) as i32, (index % rows) as i32);
        Rect::sized(
            4 + column * CELL_WIDTH,
            6 + row * CELL_HEIGHT,
            CELL_WIDTH,
            CELL_HEIGHT,
        )
    }

    /// Hit area of a desktop icon: the image and its label.
    fn icon_hit(&self, index: usize) -> Rect {
        let cell = self.icon_cell(index);
        Rect::new(cell.left + 8, cell.top, cell.right - 8, cell.top + 64)
    }

    fn label_lines(label: &str) -> Vec<String> {
        let mut lines = wrap(label, CELL_WIDTH - 6);
        if lines.len() > 2 {
            lines.truncate(2);
            let last = fit(&format!("{}...", lines[1]), CELL_WIDTH - 6);
            lines[1] = String::from_utf8_lossy(&last).into_owned();
        }
        lines
            .into_iter()
            .map(|line| String::from_utf8_lossy(&fit(&line, CELL_WIDTH - 6)).into_owned())
            .collect()
    }

    fn caption_rect(e: &Explorer) -> Rect {
        Rect::new(
            e.rect.left + 3,
            e.rect.top + 3,
            e.rect.right - 3,
            e.rect.top + 3 + CAPTION,
        )
    }

    fn close_rect(e: &Explorer) -> Rect {
        Rect::sized(
            e.rect.right - 3 - CAPTION_BUTTON,
            e.rect.top + 3,
            CAPTION_BUTTON,
            CAPTION,
        )
    }

    /// Maximize (or restore) and minimize buttons, left of close.
    fn max_rect(e: &Explorer) -> Rect {
        Self::close_rect(e).offset(-CAPTION_BUTTON, 0)
    }

    fn min_rect(e: &Explorer) -> Rect {
        Self::max_rect(e).offset(-CAPTION_BUTTON, 0)
    }

    /// The caption icon, which opens the window menu.
    fn icon_rect(e: &Explorer) -> Rect {
        Rect::sized(e.rect.left + 5, e.rect.top + 4, 16, 16)
    }

    fn window_menu_items(&self) -> Vec<Item> {
        let maximized = self.explorer.as_ref().is_some_and(|e| e.restore.is_some());
        vec![
            Item::command("&Restore", Command::Restore).enabled(maximized),
            Item::command("Mi&nimize", Command::Minimize),
            Item::command("Ma&ximize", Command::Maximize).enabled(!maximized),
            Item::separator(),
            Item::command("&Close", Command::Close)
                .accel("Alt+F4")
                .default_item(),
        ]
    }

    fn bar_rects(&self) -> Vec<Rect> {
        let Some(e) = &self.explorer else {
            return Vec::new();
        };
        let top = e.rect.top + 3 + CAPTION;
        let mut x = e.rect.left + 3;
        BAR_MENUS
            .iter()
            .map(|label| {
                let width = text_width(&crate::win32::unicode::string_to_ansi(label)) + 12;
                let rect = Rect::new(x, top + 1, x + width, top + MENU_BAR - 1);
                x += width;
                rect
            })
            .collect()
    }

    fn up_rect(e: &Explorer) -> Rect {
        Rect::sized(
            e.rect.left + 8,
            e.rect.top + 3 + CAPTION + MENU_BAR + 3,
            24,
            22,
        )
    }

    fn address_rect(e: &Explorer) -> Rect {
        let top = e.rect.top + 3 + CAPTION + MENU_BAR + 4;
        Rect::new(e.rect.left + 94, top, e.rect.right - 10, top + 20)
    }

    fn list_rect(e: &Explorer) -> Rect {
        Rect::new(
            e.rect.left + 3,
            e.rect.top + 3 + CAPTION + MENU_BAR + TOOLBAR,
            e.rect.right - 3,
            e.rect.bottom - 3 - STATUS_BAR - 2,
        )
    }

    fn visible_rows(&self, e: &Explorer) -> usize {
        (self.rows_rect_unclipped(e).height() / ROW_HEIGHT).max(1) as usize
    }

    fn rows_rect_unclipped(&self, e: &Explorer) -> Rect {
        let inner = Self::list_rect(e).inflate(-2, -2);
        Rect::new(
            inner.left,
            inner.top + ROW_HEIGHT,
            inner.right,
            inner.bottom,
        )
    }

    fn overflows(&self, e: &Explorer) -> bool {
        e.entries.len() > self.visible_rows(e)
    }

    fn rows_rect(&self, e: &Explorer) -> Rect {
        let rows = self.rows_rect_unclipped(e);
        if self.overflows(e) {
            Rect::new(
                rows.left,
                rows.top,
                rows.right - user::SCROLL_SIZE,
                rows.bottom,
            )
        } else {
            rows
        }
    }

    fn scroll_rect(&self, e: &Explorer) -> Rect {
        let rows = self.rows_rect_unclipped(e);
        let inner = Self::list_rect(e).inflate(-2, -2);
        Rect::new(
            rows.right - user::SCROLL_SIZE,
            inner.top,
            rows.right,
            rows.bottom,
        )
    }

    fn name_width(&self, e: &Explorer) -> i32 {
        (self.rows_rect(e).width() - 190).max(100)
    }

    fn ensure_visible(&mut self) {
        let Some(e) = &self.explorer else {
            return;
        };
        let visible = self.visible_rows(e);
        let Some(explorer) = self.explorer.as_mut() else {
            return;
        };
        if let Some(selected) = explorer.selected {
            if selected < explorer.scroll {
                explorer.scroll = selected;
            } else if selected >= explorer.scroll + visible {
                explorer.scroll = selected + 1 - visible;
            }
        }
    }

    /// The notification area: icons, then the clock (date and time), at
    /// the right.
    fn tray_rect(&self) -> Rect {
        let width = 8 + self.tray.len() as i32 * TRAY_SLOT + Self::clock_width();
        Rect::new(
            self.screen.0 - 4 - width,
            self.screen.1 - TASKBAR_HEIGHT + 3,
            self.screen.0 - 4,
            self.screen.1 - 3,
        )
    }

    /// Room for the clock: the widest short date, a gap, the time.
    fn clock_width() -> i32 {
        text_width(b"Wed 30 May") + 10 + gfx_bold_width(b"88:88") + 16
    }

    fn tray_icon_rect(&self, index: usize) -> Rect {
        let tray = self.tray_rect();
        Rect::sized(
            tray.left + 6 + index as i32 * TRAY_SLOT,
            tray.top + (tray.height() - 16) / 2,
            16,
            16,
        )
    }

    fn clock_rect(&self) -> Rect {
        let tray = self.tray_rect();
        Rect::new(tray.right - Self::clock_width(), tray.top, tray.right, tray.bottom)
    }

    /// Replace the notification-area icons.
    pub fn set_tray(&mut self, tray: Vec<TrayItem>) {
        self.tray = tray;
        if self
            .tray_press
            .is_some_and(|(index, _)| index >= self.tray.len())
        {
            self.tray_press = None;
        }
    }

    /// Whether the clock needs repainting (the minute changed).
    pub fn clock_changed(&self) -> bool {
        local_time().1 != self.clock_minute.get()
    }

    fn task_rects(&self, count: usize) -> Vec<Rect> {
        let top = self.screen.1 - TASKBAR_HEIGHT + 3;
        let first = self.start_rect().right + 8;
        let available = self.tray_rect().left - 8 - first;
        let width = if count == 0 {
            0
        } else {
            (available / count as i32).min(180)
        };
        (0..count)
            .map(|i| Rect::sized(first + i as i32 * width, top, width - 4, TASKBAR_HEIGHT - 6))
            .collect()
    }

    /// The Menu button, at the left of the taskbar.
    fn start_rect(&self) -> Rect {
        Rect::sized(
            4,
            self.screen.1 - TASKBAR_HEIGHT + 3,
            76,
            TASKBAR_HEIGHT - 6,
        )
    }

    fn dialog_buttons(dialog: &Dialog) -> Vec<(DialogButton, Rect)> {
        let labels: &[DialogButton] = match dialog.kind {
            DialogKind::Message => &[DialogButton::Ok],
            DialogKind::Confirm(_) => &[DialogButton::Yes, DialogButton::No],
            DialogKind::Run(_) => &[DialogButton::Ok, DialogButton::Cancel],
        };
        let r = dialog.rect;
        let total = labels.len() as i32 * 81 - 6;
        let start = if matches!(dialog.kind, DialogKind::Run(_)) {
            r.right - 12 - total
        } else {
            r.left + (r.width() - total) / 2
        };
        let top = r.bottom - 12 - 23;
        labels
            .iter()
            .enumerate()
            .map(|(i, b)| (*b, Rect::sized(start + i as i32 * 81, top, 75, 23)))
            .collect()
    }

    fn dialog_close_rect(dialog: &Dialog) -> Rect {
        Rect::sized(
            dialog.rect.right - 3 - CAPTION_BUTTON,
            dialog.rect.top + 3,
            CAPTION_BUTTON,
            CAPTION,
        )
    }

    fn dialog_edit_rect(dialog: &Dialog) -> Rect {
        let top = dialog.rect.top + 3 + CAPTION + 14 + dialog.lines.len() as i32 * 15 + 8;
        Rect::new(dialog.rect.left + 60, top, dialog.rect.right - 14, top + 21)
    }

    fn rename_rect(&self, rename: &Rename) -> Option<Rect> {
        let width = text_width(&bytes(&rename.edit.text)) + 8;
        match rename.place {
            Place::Explorer => {
                let e = self.explorer.as_ref()?;
                let rows = self.rows_rect(e);
                let row = rename.index.checked_sub(e.scroll)? as i32;
                let left = rows.left + 20;
                Some(Rect::new(
                    left,
                    rows.top + row * ROW_HEIGHT,
                    (left + width.max(40)).min(rows.left + self.name_width(e)),
                    rows.top + (row + 1) * ROW_HEIGHT,
                ))
            }
            Place::Desktop => {
                let cell = self.icon_cell(rename.index);
                let width = width.clamp(40, CELL_WIDTH + 30);
                let left = cell.left + (CELL_WIDTH - width) / 2;
                Some(Rect::sized(left, cell.top + 38, width, 17))
            }
        }
    }

    // ── Input ────────────────────────────────────────────────────────────

    fn double_click(&mut self, target: ClickTarget) -> bool {
        let now = Instant::now();
        let double = matches!(&self.last_click, Some((last, time)) if *last == target && now.duration_since(*time) < DOUBLE_CLICK);
        self.last_click = if double { None } else { Some((target, now)) };
        double
    }

    fn in_explorer(&self, x: i32, y: i32) -> bool {
        self.explorer
            .as_ref()
            .is_some_and(|e| !e.minimized && e.rect.contains(x, y))
    }

    /// A button press. `program_hit` says whether a program window is under
    /// the pointer (the shell's folder window may still be in front of it).
    pub fn pointer_down(
        &mut self,
        x: i32,
        y: i32,
        button: Button,
        program_hit: bool,
        tasks: &[Task],
    ) -> PointerOutcome {
        self.captured = false;
        let outcome = self.pointer_down_inner(x, y, button, program_hit, tasks);
        self.captured = outcome != PointerOutcome::Unhandled;
        outcome
    }

    fn pointer_down_inner(
        &mut self,
        x: i32,
        y: i32,
        button: Button,
        program_hit: bool,
        tasks: &[Task],
    ) -> PointerOutcome {
        if let Some(dialog) = &self.dialog {
            if button == Button::Left && dialog.rect.contains(x, y) {
                if let Some((choice, _)) = Self::dialog_buttons(dialog)
                    .into_iter()
                    .find(|(_, r)| r.contains(x, y))
                {
                    self.dialog_choice(choice);
                } else if Self::dialog_close_rect(dialog).contains(x, y) {
                    let cancel = if matches!(dialog.kind, DialogKind::Message) {
                        DialogButton::Ok
                    } else {
                        DialogButton::No
                    };
                    self.dialog_choice(cancel);
                }
            }
            return PointerOutcome::Handled;
        }
        if let Some(open) = &mut self.menu {
            if open.menu.contains(x, y) {
                if button == Button::Left {
                    let outcome = open.menu.click(x, y);
                    self.menu_outcome(outcome);
                }
                return PointerOutcome::Handled;
            }
            let owner = open.owner;
            self.menu = None;
            // Clicking the button that opened a menu just closes it.
            if owner == MenuOwner::Start && self.start_rect().contains(x, y) {
                return PointerOutcome::Handled;
            }
            if let MenuOwner::Bar(index) = owner {
                if self
                    .bar_rects()
                    .get(index)
                    .is_some_and(|r| r.contains(x, y))
                {
                    return PointerOutcome::Handled;
                }
            }
        }
        if let Some(rename) = &self.rename {
            if self.rename_rect(rename).is_some_and(|r| r.contains(x, y)) {
                return PointerOutcome::Handled;
            }
            self.commit_rename();
        }
        if y >= self.screen.1 - TASKBAR_HEIGHT {
            return self.taskbar_click(x, y, button, tasks);
        }
        let explorer_hit = self.in_explorer(x, y);
        if explorer_hit && (self.focus == Focus::Explorer || !program_hit) {
            self.focus = Focus::Explorer;
            self.explorer_down(x, y, button);
            return PointerOutcome::Handled;
        }
        if program_hit {
            self.focus = Focus::Program;
            return PointerOutcome::Unhandled;
        }
        self.focus = Focus::Desktop;
        let hit = (0..self.icons.len()).find(|&i| self.icon_hit(i).contains(x, y));
        self.desktop_selected = hit;
        if let (Some(index), Button::Left) = (hit, button) {
            if self.double_click(ClickTarget::Icon(index)) {
                self.execute(Command::OpenSelection, Place::Desktop);
            }
        }
        PointerOutcome::Handled
    }

    fn taskbar_click(&mut self, x: i32, y: i32, button: Button, tasks: &[Task]) -> PointerOutcome {
        if let Some(index) = (0..self.tray.len()).find(|&i| self.tray_icon_rect(i).contains(x, y)) {
            self.tray_press = Some((index, button));
            let target = ClickTarget::Tray(index);
            let event = if button == Button::Left && self.double_click(target) {
                TrayEvent::DoubleClick(button)
            } else {
                TrayEvent::Down(button)
            };
            return PointerOutcome::Tray(index, event);
        }
        if button != Button::Left {
            return PointerOutcome::Handled;
        }
        if self.start_rect().contains(x, y) {
            self.open_menu(MenuOwner::Start, None, false);
            return PointerOutcome::Handled;
        }
        let explorer = usize::from(self.explorer.is_some());
        let rects = self.task_rects(explorer + tasks.len());
        match rects.iter().position(|r| r.contains(x, y)) {
            Some(0) if explorer == 1 => {
                if self.foreground_explorer() {
                    self.execute(Command::Minimize, Place::Explorer);
                } else {
                    if let Some(explorer) = self.explorer.as_mut() {
                        explorer.minimized = false;
                    }
                    self.focus = Focus::Explorer;
                }
                PointerOutcome::Handled
            }
            Some(index) => {
                self.focus = Focus::Program;
                PointerOutcome::Task(index - explorer)
            }
            None => PointerOutcome::Handled,
        }
    }

    fn explorer_down(&mut self, x: i32, y: i32, button: Button) {
        let Some(e) = &self.explorer else {
            return;
        };
        if button == Button::Left {
            if Self::close_rect(e).contains(x, y) {
                return self.execute(Command::Close, Place::Explorer);
            }
            if Self::max_rect(e).contains(x, y) {
                let command = if e.restore.is_some() {
                    Command::Restore
                } else {
                    Command::Maximize
                };
                return self.execute(command, Place::Explorer);
            }
            if Self::min_rect(e).contains(x, y) {
                return self.execute(Command::Minimize, Place::Explorer);
            }
            if Self::icon_rect(e).contains(x, y) {
                let at = (e.rect.left + 3, e.rect.top + 3 + CAPTION);
                return self.open_menu(MenuOwner::Window, Some(at), false);
            }
            if Self::caption_rect(e).contains(x, y) {
                let (maximized, origin) = (e.restore.is_some(), (e.rect.left, e.rect.top));
                if self.double_click(ClickTarget::Caption) {
                    let command = if maximized {
                        Command::Restore
                    } else {
                        Command::Maximize
                    };
                    return self.execute(command, Place::Explorer);
                }
                // A maximized window does not move.
                if !maximized {
                    self.drag = Some((x - origin.0, y - origin.1));
                }
                return;
            }
            if let Some(index) = self.bar_rects().iter().position(|r| r.contains(x, y)) {
                return self.open_menu(MenuOwner::Bar(index), None, false);
            }
            if Self::up_rect(e).contains(x, y) {
                return self.execute(Command::Up, Place::Explorer);
            }
            if self.overflows(e) && self.scroll_rect(e).contains(x, y) {
                return self.scroll_click(y);
            }
        }
        let rows = self.rows_rect(e);
        if !rows.contains(x, y) {
            return;
        }
        let index = e.scroll + ((y - rows.top) / ROW_HEIGHT) as usize;
        let hit = (index < e.entries.len()).then_some(index);
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.selected = hit;
        }
        if let (Some(index), Button::Left) = (hit, button) {
            if self.double_click(ClickTarget::Row(index)) {
                self.execute(Command::OpenSelection, Place::Explorer);
            }
        }
    }

    fn scroll_click(&mut self, y: i32) {
        let Some(e) = &self.explorer else {
            return;
        };
        let bar = self.scroll_rect(e);
        let visible = self.visible_rows(e);
        let max = e.entries.len().saturating_sub(visible);
        let thumb = user::scroll_thumb(
            bar.height(),
            0,
            e.entries.len() as i32 - 1,
            e.scroll as i32,
            visible as u32,
        );
        let offset = y - bar.top;
        let delta: isize = if offset < user::SCROLL_SIZE {
            -1
        } else if offset >= bar.height() - user::SCROLL_SIZE {
            1
        } else {
            match thumb {
                Some((start, _)) if offset < start => -(visible as isize),
                Some((start, size)) if offset >= start + size => visible as isize,
                _ => 0,
            }
        };
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.scroll = (explorer.scroll as isize + delta).clamp(0, max as isize) as usize;
        }
    }

    pub fn pointer_up(
        &mut self,
        x: i32,
        y: i32,
        button: Button,
        program_hit: bool,
    ) -> PointerOutcome {
        let captured = std::mem::take(&mut self.captured);
        self.drag = None;
        if let Some((index, pressed)) = self.tray_press.take() {
            if pressed == button {
                return PointerOutcome::Tray(index, TrayEvent::Up(button));
            }
        }
        if button == Button::Right
            && self.dialog.is_none()
            && self.menu.is_none()
            && y < self.screen.1 - TASKBAR_HEIGHT
        {
            let explorer_hit =
                self.in_explorer(x, y) && (self.focus == Focus::Explorer || !program_hit);
            if explorer_hit {
                let e = self.explorer.as_ref().unwrap();
                if self.rows_rect(e).contains(x, y) {
                    self.open_menu(MenuOwner::Context(Place::Explorer), Some((x, y)), false);
                }
                return PointerOutcome::Handled;
            }
            if !program_hit {
                self.open_menu(MenuOwner::Context(Place::Desktop), Some((x, y)), false);
                return PointerOutcome::Handled;
            }
        }
        if captured {
            PointerOutcome::Handled
        } else {
            PointerOutcome::Unhandled
        }
    }

    /// Pointer motion; returns whether the shell consumed it.
    pub fn pointer_move(&mut self, x: i32, y: i32) -> bool {
        // Tooltips over the notification area.
        let over_tray = self.tray_rect().contains(x, y);
        let previous = self.hover_target();
        self.hover = over_tray.then_some((x, y));
        if self.hover_target() != previous {
            return true;
        }
        if let Some((grab_x, grab_y)) = self.drag {
            let screen = self.screen;
            if let Some(explorer) = self.explorer.as_mut() {
                let (w, h) = (explorer.rect.width(), explorer.rect.height());
                let left = (x - grab_x).clamp(-w + 40, screen.0 - 40);
                let top = (y - grab_y).clamp(0, screen.1 - TASKBAR_HEIGHT - CAPTION);
                explorer.rect = Rect::sized(left, top, w, h);
            }
            return true;
        }
        if let Some(open) = &mut self.menu {
            if let MenuOwner::Bar(index) = open.owner {
                if let Some(other) = self.bar_rects().iter().position(|r| r.contains(x, y)) {
                    if other != index {
                        self.open_menu(MenuOwner::Bar(other), None, false);
                    }
                    return true;
                }
            }
            if let Some(open) = &mut self.menu {
                open.menu.hover(x, y);
            }
            return true;
        }
        self.captured
    }

    pub fn wheel(&mut self, x: i32, y: i32, lines: i32) -> bool {
        let Some(e) = &self.explorer else {
            return false;
        };
        if !e.rect.contains(x, y) || self.focus != Focus::Explorer {
            return false;
        }
        let max = e.entries.len().saturating_sub(self.visible_rows(e)) as i32;
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.scroll = (explorer.scroll as i32 - lines * 3).clamp(0, max) as usize;
        }
        true
    }

    fn dialog_choice(&mut self, choice: DialogButton) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match (dialog.kind, choice) {
            (DialogKind::Confirm(Confirmed::Delete(path)), DialogButton::Yes) => {
                self.delete_confirmed(path)
            }
            (DialogKind::Confirm(Confirmed::ShutDown), DialogButton::Yes) => self.quit = true,
            (DialogKind::Run(edit), DialogButton::Ok) => self.run_command(&edit.text),
            _ => {}
        }
    }

    /// A key press. Returns whether the shell consumed it; when a program
    /// has the focus, only shell-wide keys are taken.
    pub fn key(&mut self, key: Key, mods: Mods) -> bool {
        if key == Key::Logo || (key == Key::Escape && mods.ctrl) {
            if self
                .menu
                .as_ref()
                .is_some_and(|m| m.owner == MenuOwner::Start)
            {
                self.menu = None;
            } else if self.dialog.is_none() {
                self.open_menu(MenuOwner::Start, None, false);
            }
            return true;
        }
        if let Some(dialog) = &mut self.dialog {
            let choice = match (&mut dialog.kind, key) {
                (_, Key::Enter) => Some(if matches!(dialog.kind, DialogKind::Confirm(_)) {
                    DialogButton::Yes
                } else {
                    DialogButton::Ok
                }),
                (DialogKind::Message, Key::Escape) => Some(DialogButton::Ok),
                (DialogKind::Confirm(_), Key::Escape | Key::Char('n') | Key::Char('N')) => {
                    Some(DialogButton::No)
                }
                (DialogKind::Confirm(_), Key::Char('y') | Key::Char('Y')) => {
                    Some(DialogButton::Yes)
                }
                (DialogKind::Run(_), Key::Escape) => Some(DialogButton::Cancel),
                (DialogKind::Run(edit), key) => {
                    edit.key(key, mods);
                    None
                }
                _ => None,
            };
            if let Some(choice) = choice {
                self.dialog_choice(choice);
            }
            return true;
        }
        if let Some(open) = self.menu.as_ref().filter(|m| m.owner == MenuOwner::Start) {
            if !mods.ctrl && !mods.alt {
                let mut query = open.menu.search.clone();
                let edited = match key {
                    Key::Char(ch) => {
                        query.push(ch);
                        true
                    }
                    Key::Backspace => query.pop().is_some(),
                    _ => false,
                };
                if edited {
                    self.open_start(query);
                    return true;
                }
                if key == Key::Backspace {
                    return true;
                }
            }
        }
        if let Some(open) = &mut self.menu {
            let outcome = if key == Key::F10 || (key == Key::Char(' ') && mods.alt) {
                Outcome::Dismiss
            } else {
                open.menu.key(key)
            };
            self.menu_outcome(outcome);
            return true;
        }
        if let Some(rename) = &mut self.rename {
            match key {
                Key::Enter => self.commit_rename(),
                Key::Escape => self.rename = None,
                key => {
                    rename.edit.key(key, mods);
                }
            }
            return true;
        }
        let place = match self.focus {
            Focus::Program => return false,
            Focus::Explorer if self.explorer_visible() => Place::Explorer,
            _ => Place::Desktop,
        };
        // Shortcut keys shared by the desktop and folder windows.
        let command = match (key, mods.ctrl, mods.alt, mods.shift) {
            (Key::Enter, _, true, _) => Some(Command::Properties),
            (Key::Enter, ..) => Some(Command::OpenSelection),
            (Key::Delete, ..) => Some(Command::Delete),
            (Key::F2, ..) => Some(Command::Rename),
            (Key::F5, ..) => Some(Command::Refresh),
            (Key::Char('c') | Key::Char('C'), true, ..) => Some(Command::Copy),
            (Key::Char('x') | Key::Char('X'), true, ..) => Some(Command::Cut),
            (Key::Char('v') | Key::Char('V'), true, ..) => Some(Command::Paste),
            (Key::F4, _, true, _) => Some(if place == Place::Explorer {
                Command::Close
            } else {
                Command::ShutDown
            }),
            (Key::Backspace, ..) if place == Place::Explorer => Some(Command::Up),
            _ => None,
        };
        if let Some(command) = command {
            self.execute(command, place);
            self.ensure_visible();
            return true;
        }
        if key == Key::Apps || (key == Key::F10 && mods.shift) {
            self.open_menu(MenuOwner::Context(place), None, false);
            return true;
        }
        if place == Place::Explorer {
            if key == Key::Char(' ') && mods.alt {
                self.open_menu(MenuOwner::Window, None, true);
                return true;
            }
            if key == Key::F10 && !mods.shift {
                self.open_menu(MenuOwner::Bar(0), None, false);
                return true;
            }
            if let (Key::Char(ch), true) = (key, mods.alt) {
                if let Some(index) = BAR_MENUS.iter().position(|label| {
                    label
                        .to_ascii_lowercase()
                        .contains(&format!("&{}", ch.to_ascii_lowercase()))
                }) {
                    self.open_menu(MenuOwner::Bar(index), None, true);
                }
                return true;
            }
        }
        self.navigate(place, key, mods);
        true
    }

    fn navigate(&mut self, place: Place, key: Key, mods: Mods) {
        let (count, current, page, column) = match place {
            Place::Desktop => (
                self.icons.len(),
                self.desktop_selected,
                self.rows_per_column(),
                self.rows_per_column(),
            ),
            Place::Explorer => {
                let e = self.explorer.as_ref().unwrap();
                (e.entries.len(), e.selected, self.visible_rows(e), 0)
            }
        };
        if count == 0 || mods.ctrl || mods.alt {
            return;
        }
        let last = count - 1;
        let at = current.unwrap_or(0);
        let target = match key {
            Key::Down => current.map_or(0, |i| (i + 1).min(last)),
            Key::Up => at.saturating_sub(1),
            Key::Right if column > 0 => current.map_or(0, |i| (i + column).min(last)),
            Key::Left if column > 0 => at.saturating_sub(column),
            Key::Home => 0,
            Key::End => last,
            Key::PageDown => (at + page).min(last),
            Key::PageUp => at.saturating_sub(page),
            Key::Char(ch) => {
                let ch = ch.to_ascii_lowercase();
                let names: Vec<String> = match place {
                    Place::Desktop => self
                        .icons
                        .iter()
                        .map(|i| i.label.to_ascii_lowercase())
                        .collect(),
                    Place::Explorer => self
                        .explorer
                        .as_ref()
                        .unwrap()
                        .entries
                        .iter()
                        .map(|e| e.name.to_ascii_lowercase())
                        .collect(),
                };
                let found = (1..=count)
                    .map(|step| (at + step) % count)
                    .find(|&i| names[i].starts_with(ch));
                match found {
                    Some(index) => index,
                    None => return,
                }
            }
            _ => return,
        };
        match place {
            Place::Desktop => self.desktop_selected = Some(target),
            Place::Explorer => {
                if let Some(explorer) = self.explorer.as_mut() {
                    explorer.selected = Some(target);
                }
                self.ensure_visible();
            }
        }
    }

    // ── Painting ─────────────────────────────────────────────────────────

    pub fn paint(&self, surface: &mut Surface, layer: Layer, tasks: &[Task]) {
        let clip = Region::from_rect(surface.bounds());
        let mut p = Painter {
            surface,
            clip,
            ox: 0,
            oy: 0,
        };
        match layer {
            Layer::Background => {
                self.paint_desktop(&mut p);
                if !self.foreground_explorer() {
                    self.paint_explorer(&mut p);
                }
            }
            Layer::Foreground => {
                if self.foreground_explorer() {
                    self.paint_explorer(&mut p);
                }
            }
            Layer::Top => {
                self.paint_taskbar(&mut p, tasks);
                self.paint_tray(&mut p);
                if let Some(open) = &self.menu {
                    popup::paint(&mut p, &open.menu);
                }
                if let Some(dialog) = &self.dialog {
                    self.paint_dialog(&mut p, dialog);
                }
            }
        }
    }

    fn paint_desktop(&self, p: &mut Painter<'_>) {
        // A quiet vertical gradient, darker towards the taskbar.
        let height = self.screen.1.max(1);
        for y in 0..self.screen.1 {
            p.hline(0, self.screen.0, y, theme::desktop_shade(y, height));
        }
        for (index, icon) in self.icons.iter().enumerate() {
            let cell = self.icon_cell(index);
            let selected = self.desktop_selected == Some(index);
            let lines = Self::label_lines(&icon.label);
            if selected {
                let hit = self.icon_hit(index);
                let bottom = cell.top + 38 + lines.len() as i32 * 13 + 4;
                theme::rounded(
                    p,
                    Rect::new(hit.left, hit.top, hit.right, bottom.min(hit.bottom)),
                    4,
                    if self.focus == Focus::Desktop {
                        theme::DESKTOP_SELECTION
                    } else {
                        theme::DESKTOP_SELECTION_INACTIVE
                    },
                );
            }
            let (image, overlay) = match &icon.target {
                Target::Computer => (icons::builtin(Builtin::Computer), false),
                Target::Path(path) => self.icon_for(path, icon.is_dir, 32, 0),
            };
            let (ix, iy) = (cell.left + (CELL_WIDTH - 32) / 2, cell.top + 2);
            icons::draw(p, &image, ix, iy, 32, false);
            if overlay {
                icons::draw_shortcut_overlay(p, ix, iy, 32);
            }
            if self
                .rename
                .as_ref()
                .is_some_and(|r| r.place == Place::Desktop && r.index == index)
            {
                continue;
            }
            for (line_index, line) in lines.iter().enumerate() {
                let text = bytes(line);
                let width = text_width(&text);
                let x = cell.left + (CELL_WIDTH - width) / 2;
                let y = cell.top + 38 + line_index as i32 * 13;
                // A shadow keeps labels readable on any background.
                p.text(x + 1, y + 1, &text, 0x000000, false);
                p.text(x, y, &text, theme::TEXT, false);
            }
        }
        if let Some(rename) = self.rename.as_ref().filter(|r| r.place == Place::Desktop) {
            self.paint_edit(p, rename, true);
        }
    }

    fn paint_edit(&self, p: &mut Painter<'_>, rename: &Rename, border: bool) {
        let Some(rect) = self.rename_rect(rename) else {
            return;
        };
        paint_edit_box(p, rect, &rename.edit, border);
    }

    /// A window's frame and caption, as the window system draws programs'
    /// windows: a one-pixel outline (the accent while in use), the caption
    /// the frame's colour, flat buttons.
    fn paint_frame(
        p: &mut Painter<'_>,
        r: Rect,
        caption: Rect,
        title: &str,
        icon: Option<&Bitmap>,
        active: bool,
        buttons: &[(Rect, u8)],
    ) {
        p.fill(r, theme::SURFACE);
        let fill = theme::caption(active);
        p.fill(
            Rect::new(r.left, r.top, r.right, caption.bottom),
            fill,
        );
        p.outline(Rect::new(r.left + 1, r.top + 1, r.right - 1, r.bottom - 1), fill);
        p.outline(Rect::new(r.left + 2, r.top + 2, r.right - 2, r.bottom - 2), fill);
        p.outline(r, theme::frame_outline(active));
        let mut text_x = caption.left + 6;
        if let Some(icon) = icon {
            icons::draw(p, icon, caption.left + 2, caption.top + 1, 16, false);
            text_x = caption.left + 23;
        }
        let limit = buttons
            .iter()
            .map(|(rect, _)| rect.left)
            .min()
            .unwrap_or(caption.right);
        let shown = fit(title, limit - text_x - 6);
        p.text(
            text_x,
            caption.top + (caption.height() - 13) / 2,
            &shown,
            theme::caption_text(active),
            false,
        );
        for (rect, glyph) in buttons {
            theme::caption_button(p, *rect, *glyph, active, false);
        }
    }

    fn paint_explorer(&self, p: &mut Painter<'_>) {
        let Some(e) = self.explorer.as_ref().filter(|e| !e.minimized) else {
            return;
        };
        let volume = self.volume.as_ref();
        let active = self.focus == Focus::Explorer;
        let r = e.rect;
        let title = match e.path.last() {
            Some(name) => name.clone(),
            None => volume.map(Volume::label).unwrap_or_default(),
        };
        Self::paint_frame(
            p,
            r,
            Self::caption_rect(e),
            &title,
            Some(&icons::builtin(Builtin::Folder)),
            active,
            &[
                (Self::close_rect(e), theme::GLYPH_CLOSE),
                (
                    Self::max_rect(e),
                    if e.restore.is_some() {
                        theme::GLYPH_RESTORE
                    } else {
                        theme::GLYPH_MAXIMIZE
                    },
                ),
                (Self::min_rect(e), theme::GLYPH_MINIMIZE),
            ],
        );
        // Menu bar.
        let hot_bar = match self.menu.as_ref().map(|m| m.owner) {
            Some(MenuOwner::Bar(index)) => Some(index),
            _ => None,
        };
        for (index, (rect, label)) in self.bar_rects().iter().zip(BAR_MENUS).enumerate() {
            if hot_bar == Some(index) {
                theme::rounded(p, *rect, 3, theme::MENU_HOT);
            }
            p.mnemonic_text(
                rect.left + 6,
                rect.top + 2,
                &crate::win32::unicode::string_to_ansi(label),
                theme::TEXT,
            );
        }
        // Up button and address box.
        let up = Self::up_rect(e);
        let can_go_up = !e.path.is_empty();
        theme::rounded(p, up, 3, theme::RAISED);
        let arrow = if can_go_up {
            theme::TEXT
        } else {
            theme::TEXT_DISABLED
        };
        let (ax, ay) = (up.left + up.width() / 2, up.top + 6);
        for i in 0..5 {
            p.pixel(ax - i, ay + i, arrow);
            p.pixel(ax + i, ay + i, arrow);
        }
        p.vline(ax, ay, ay + 11, arrow);
        p.text(r.left + 40, up.top + 4, b"Address", theme::TEXT_DIM, false);
        let address = Self::address_rect(e);
        theme::rounded_box(p, address, 3, theme::WELL, theme::BORDER);
        let address_text = volume.map(|v| v.guest_path(&e.path)).unwrap_or_default();
        with_clip(p, address.inflate(-3, -2), |p| {
            icons::draw(
                p,
                &icons::builtin(if e.path.is_empty() {
                    Builtin::Drive
                } else {
                    Builtin::Folder
                }),
                address.left + 4,
                address.top + 2,
                16,
                false,
            );
            p.text(
                address.left + 24,
                address.top + 4,
                &bytes(&address_text),
                theme::TEXT,
                false,
            );
        });
        // List view.
        let list = Self::list_rect(e);
        p.fill(list, theme::WELL);
        p.hline(list.left, list.right, list.top, theme::BORDER);
        let inner = list.inflate(-2, -2);
        let rows = self.rows_rect(e);
        let name_width = self.name_width(e);
        let header = Rect::new(inner.left, inner.top, rows.right, inner.top + ROW_HEIGHT);
        let columns = [
            ("Name", rows.left, rows.left + name_width),
            ("Size", rows.left + name_width, rows.left + name_width + 80),
            ("Type", rows.left + name_width + 80, rows.right),
        ];
        with_clip(p, inner, |p| {
            for (index, (label, left, _right)) in columns.into_iter().enumerate() {
                if index > 0 {
                    p.vline(left, header.top + 3, header.bottom - 3, theme::BORDER);
                }
                p.text(
                    left + 6,
                    header.top + 2,
                    &crate::win32::unicode::string_to_ansi(label),
                    theme::TEXT_DIM,
                    false,
                );
            }
            p.hline(header.left, header.right, header.bottom - 1, theme::BORDER);
        });
        with_clip(p, rows, |p| {
            for (row, index) in (e.scroll..e.entries.len()).enumerate() {
                let entry = &e.entries[index];
                let top = rows.top + row as i32 * ROW_HEIGHT;
                if top >= rows.bottom {
                    break;
                }
                let renaming = self
                    .rename
                    .as_ref()
                    .is_some_and(|r| r.place == Place::Explorer && r.index == index);
                let selected = e.selected == Some(index);
                if selected {
                    // The whole row is selected.
                    theme::rounded(
                        p,
                        Rect::new(rows.left, top, rows.right - 2, top + ROW_HEIGHT),
                        3,
                        if active {
                            theme::SELECTION
                        } else {
                            theme::SELECTION_INACTIVE
                        },
                    );
                }
                let path = join(&e.path, &entry.name);
                let (icon, overlay) = self.icon_for(&path, entry.is_dir, 16, 0);
                icons::draw(p, &icon, rows.left + 3, top, 16, false);
                if overlay {
                    icons::draw_shortcut_overlay(p, rows.left + 3, top, 16);
                }
                let name = fit(&entry.name, name_width - 26);
                if !renaming {
                    p.text(rows.left + 23, top + 2, &name, theme::TEXT, false);
                }
                let dim = if selected { theme::TEXT } else { theme::TEXT_DIM };
                if !entry.is_dir {
                    let size = bytes(&size_text(entry.size));
                    p.text(
                        rows.left + name_width + 74 - text_width(&size),
                        top + 2,
                        &size,
                        dim,
                        false,
                    );
                }
                p.text(
                    rows.left + name_width + 86,
                    top + 2,
                    &fit(&type_name(entry), rows.right - rows.left - name_width - 90),
                    dim,
                    false,
                );
            }
        });
        if let Some(rename) = self.rename.as_ref().filter(|r| r.place == Place::Explorer) {
            with_clip(p, rows, |p| self.paint_edit(p, rename, true));
        }
        if self.overflows(e) {
            let bar = self.scroll_rect(e);
            let visible = self.visible_rows(e) as u32;
            user::draw_scroll_bar(
                p,
                bar,
                true,
                0,
                e.entries.len() as i32 - 1,
                e.scroll as i32,
                visible,
            );
        }
        // Status bar.
        let status = Rect::new(
            r.left + 3,
            r.bottom - 3 - STATUS_BAR,
            r.right - 3,
            r.bottom - 3,
        );
        p.hline(status.left, status.right, status.top, theme::BORDER);
        let left = Rect::new(
            status.left,
            status.top + 2,
            status.left + status.width() * 2 / 3,
            status.bottom,
        );
        let right = Rect::new(left.right + 2, status.top + 2, status.right, status.bottom);
        p.vline(right.left - 1, right.top + 2, right.bottom - 2, theme::BORDER);
        let (count_text, size) = match e.selected.and_then(|i| e.entries.get(i)) {
            Some(entry) => (
                "1 object(s) selected".to_string(),
                if entry.is_dir {
                    String::new()
                } else {
                    size_text(entry.size)
                },
            ),
            None => (
                format!("{} object(s)", e.entries.len()),
                size_text(e.entries.iter().map(|e| e.size).sum()),
            ),
        };
        let count_text = if volume.is_some_and(|v| !v.writable()) {
            format!("{count_text} (read-only)")
        } else {
            count_text
        };
        with_clip(p, left.inflate(-2, -1), |p| {
            p.text(
                left.left + 6,
                left.top + 2,
                &bytes(&count_text),
                theme::TEXT_DIM,
                false,
            );
        });
        with_clip(p, right.inflate(-2, -1), |p| {
            p.text(
                right.left + 6,
                right.top + 2,
                &bytes(&size),
                theme::TEXT_DIM,
                false,
            );
        });
    }

    /// What the pointer rests on in the notification area: an icon, or the
    /// clock (`usize::MAX`).
    fn hover_target(&self) -> Option<usize> {
        let (x, y) = self.hover?;
        (0..self.tray.len())
            .find(|&i| self.tray_icon_rect(i).contains(x, y))
            .or_else(|| self.clock_rect().contains(x, y).then_some(usize::MAX))
    }

    fn paint_tray(&self, p: &mut Painter<'_>) {
        let tray = self.tray_rect();
        for (index, item) in self.tray.iter().enumerate() {
            let rect = self.tray_icon_rect(index);
            match &item.icon {
                Some(icon) => icons::draw(p, icon, rect.left, rect.top, 16, false),
                None => icons::draw(
                    p,
                    &icons::builtin(Builtin::Program),
                    rect.left,
                    rect.top,
                    16,
                    false,
                ),
            }
        }
        let (hour, minute, weekday, day, month, year) = local_time();
        self.clock_minute.set(minute);
        let clock = self.clock_rect();
        let (time, date) = clock_texts(hour, minute, weekday, day, month);
        let time = bytes(&time);
        let date = bytes(&date);
        let y = clock.top + (clock.height() - 13) / 2;
        let time_x = clock.right - 8 - text_width(&time);
        p.text(time_x, y, &time, theme::TEXT, true);
        p.text(
            time_x - 10 - text_width(&date),
            y,
            &date,
            theme::TEXT_DIM,
            false,
        );

        // Tooltip: the icon's tip, or the date over the clock.
        let tip = match self.hover_target() {
            Some(usize::MAX) => long_date(weekday, day, month, year),
            Some(index) => self
                .tray
                .get(index)
                .map(|item| item.tip.clone())
                .unwrap_or_default(),
            None => String::new(),
        };
        if let (false, Some((x, _))) = (tip.is_empty(), self.hover) {
            let text = bytes(&tip);
            let width = text_width(&text) + 16;
            let left = (x - width / 2).clamp(2, self.screen.0 - width - 2);
            let rect = Rect::sized(left, tray.top - 30, width, 23);
            theme::rounded_box(p, rect, 4, sys_color(24), theme::BORDER);
            p.text(rect.left + 8, rect.top + 5, &text, sys_color(23), false);
        }
    }

    fn paint_taskbar(&self, p: &mut Painter<'_>, tasks: &[Task]) {
        let top = self.screen.1 - TASKBAR_HEIGHT;
        p.fill(
            Rect::new(0, top, self.screen.0, self.screen.1),
            theme::TASKBAR,
        );
        p.hline(0, self.screen.0, top, theme::TASKBAR_EDGE);
        // The Menu button: four squares and its name.
        let start = self.start_rect();
        let open = self
            .menu
            .as_ref()
            .is_some_and(|m| m.owner == MenuOwner::Start);
        if open {
            theme::rounded(p, start, 4, theme::TASKBAR_ACTIVE);
        }
        let (gx, gy) = (start.left + 9, start.top + start.height() / 2 - 5);
        for (dx, dy) in [(0, 0), (6, 0), (0, 6), (6, 6)] {
            p.fill(Rect::sized(gx + dx, gy + dy, 5, 5), theme::ACCENT);
        }
        p.text(
            start.left + 28,
            start.top + (start.height() - 13) / 2,
            b"Menu",
            theme::TEXT,
            true,
        );

        let mut buttons: Vec<(String, bool, Bitmap)> = Vec::new();
        if let Some(e) = &self.explorer {
            let title = e
                .path
                .last()
                .cloned()
                .unwrap_or_else(|| self.volume.as_ref().map(Volume::label).unwrap_or_default());
            buttons.push((
                title,
                self.focus == Focus::Explorer,
                icons::builtin(Builtin::Folder),
            ));
        }
        for task in tasks {
            let program = if task.program.is_empty() {
                icons::builtin(Builtin::Program)
            } else {
                self.program_icon(&task.program)
            };
            buttons.push((
                task.label.clone(),
                task.active && self.focus == Focus::Program,
                program,
            ));
        }
        // Each program has a window in use of its own; only the front one
        // (the last) is the window in use on the screen.
        if let Some(front) = buttons.iter().rposition(|(_, active, _)| *active) {
            for (index, button) in buttons.iter_mut().enumerate() {
                button.1 &= index == front;
            }
        }
        for ((label, active, icon), rect) in buttons.iter().zip(self.task_rects(buttons.len())) {
            if *active {
                theme::rounded(p, rect, 4, theme::TASKBAR_ACTIVE);
                // The window in use: an accent line under its button.
                let mid = rect.left + rect.width() / 2;
                p.fill(
                    Rect::new(mid - 8, rect.bottom - 2, mid + 8, rect.bottom),
                    theme::ACCENT,
                );
            } else {
                // Running, not in use: a short grey line.
                let mid = rect.left + rect.width() / 2;
                p.fill(
                    Rect::new(mid - 3, rect.bottom - 2, mid + 3, rect.bottom),
                    theme::TEXT_DISABLED,
                );
            }
            icons::draw(
                p,
                icon,
                rect.left + 7,
                rect.top + (rect.height() - 16) / 2,
                16,
                false,
            );
            with_clip(p, rect.inflate(-3, 0), |p| {
                p.text(
                    rect.left + 29,
                    rect.top + (rect.height() - 13) / 2,
                    &fit(label, rect.width() - 36),
                    if *active { theme::TEXT } else { theme::TEXT_DIM },
                    false,
                );
            });
        }
    }

    /// Icon of the running program, found on the volume by file name.
    fn program_icon(&self, name: &str) -> Bitmap {
        let path = join(&parse_path(setup::WINDOWS), name);
        let path = if self.volume.as_ref().is_some_and(|v| v.exists(&path)) {
            path
        } else {
            vec![name.to_string()]
        };
        self.icon_for(&path, false, 16, 0).0
    }

    fn paint_dialog(&self, p: &mut Painter<'_>, dialog: &Dialog) {
        let r = dialog.rect;
        let caption = Rect::new(r.left + 3, r.top + 3, r.right - 3, r.top + 3 + CAPTION);
        Self::paint_frame(
            p,
            r,
            caption,
            &dialog.title,
            None,
            true,
            &[(Self::dialog_close_rect(dialog), theme::GLYPH_CLOSE)],
        );
        let mut y = r.top + 3 + CAPTION + 14;
        for line in &dialog.lines {
            p.text(r.left + 16, y, &bytes(line), theme::TEXT, false);
            y += 15;
        }
        if let DialogKind::Run(edit) = &dialog.kind {
            let rect = Self::dialog_edit_rect(dialog);
            p.mnemonic_text(r.left + 16, rect.top + 4, b"&Open:", theme::TEXT);
            paint_edit_box(p, rect, edit, false);
        }
        for (index, (button, rect)) in Self::dialog_buttons(dialog).into_iter().enumerate() {
            let label: &[u8] = match button {
                DialogButton::Ok => b"OK",
                DialogButton::Cancel => b"Cancel",
                DialogButton::Yes => b"&Yes",
                DialogButton::No => b"&No",
            };
            // The default button is the accent; the others are quiet.
            let (fill, text) = if index == 0 {
                (theme::ACCENT_STRONG, 0xFFFFFF)
            } else {
                (theme::RAISED, theme::TEXT)
            };
            if index == 0 {
                theme::rounded(p, rect, 4, fill);
            } else {
                theme::rounded_box(p, rect, 4, fill, theme::BORDER);
            }
            let width = text_width(label);
            p.mnemonic_text(
                rect.left + (rect.width() - width) / 2,
                rect.top + 5,
                label,
                text,
            );
        }
    }
}

/// The clock's texts: the time, and the short date beside it.
fn clock_texts(hour: i32, minute: i32, weekday: usize, day: i32, month: usize) -> (String, String) {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    (
        format!("{hour:02}:{minute:02}"),
        format!("{} {day} {}", DAYS[weekday % 7], MONTHS[month % 12]),
    )
}

/// The date in full, for the clock's tooltip.
fn long_date(weekday: usize, day: i32, month: usize, year: i32) -> String {
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    format!("{}, {} {day}, {year}", DAYS[weekday % 7], MONTHS[month % 12])
}

/// A dark text box with a caret; selected text is highlighted.
fn paint_edit_box(p: &mut Painter<'_>, rect: Rect, edit: &Edit, border: bool) {
    let radius = if border { 2 } else { 3 };
    theme::rounded_box(p, rect, radius, theme::WELL, theme::ACCENT);
    let text = bytes(&edit.text);
    let x = rect.left + 5;
    let y = rect.top + (rect.height() - 13) / 2;
    with_clip(p, rect.inflate(-2, -1), |p| {
        if edit.all && !text.is_empty() {
            p.fill(
                Rect::new(x - 1, y, x + text_width(&text) + 1, y + 13),
                theme::SELECTION,
            );
            p.text(x, y, &text, theme::TEXT, false);
        } else {
            p.text(x, y, &text, theme::TEXT, false);
            let caret = x + text_width(&bytes(&edit.text[..edit.caret]));
            p.vline(caret, y - 1, y + 14, theme::TEXT);
        }
    });
}

#[cfg(test)]
mod tests;
