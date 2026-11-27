//! The App Center window: one custom-painted top-level window. It turns
//! Win32 messages into calls on [`App`], carries out the [`Effect`]s that
//! come back, and paints the views [`App`] describes.

use crate::gdi::{Align, Fonts, Painter};
use crate::shell::{self, wide};
use crate::worker::{Workers, WM_APP_EVENT};
use app_center::select::Preferences;
use app_center::source::{default_cache_dir, Http, Source, DEFAULT_SOURCE};
use app_center::ui::app::{
    Action, App, ButtonStyle, ButtonView, DetailsView, Effect, Event, Kind, PackageView,
    ProgressView, Timer,
};
use app_center::ui::format;
use app_center::ui::geom::{HitMap, Rect};
use app_center::ui::layout::{self, Layout};
use app_center::ui::theme::{self as t, Font};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver};
use std::time::Instant;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    EndPaint, GetDC, InvalidateRect, ReleaseDC, ScreenToClient, SelectObject, UpdateWindow,
    PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN,
    VK_END, VK_ESCAPE, VK_F5, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect,
    GetMessageW, KillTimer, LoadCursorW, LoadIconW, MessageBoxW, PostQuitMessage, RegisterClassExW,
    SetCursor, SetTimer, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    HCURSOR, HTCLIENT, IDC_ARROW, IDC_HAND, IDC_IBEAM, IDI_APPLICATION, MB_ICONERROR, MINMAXINFO,
    MSG, SW_SHOW, WM_CHAR, WM_DESTROY, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SETCURSOR, WM_SETFOCUS,
    WM_SIZE, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};

const TIMER_SEARCH: usize = 1;
const TIMER_DETAILS: usize = 2;
const TIMER_CARET: usize = 3;
const TIMER_ANIM: usize = 4;
const CARET_BLINK_MS: u32 = 530;
const ANIM_MS: u32 = 120;
const CF_UNICODETEXT: u32 = 13;
const WHITE: u32 = t::rgb(0xFF, 0xFF, 0xFF);

/// What is under the mouse.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Search,
    Row(usize),
    Button(Action),
    Link(Action),
    ListThumb,
    ListTrack,
    DetailsThumb,
    DetailsTrack,
}

/// A scroll bar thumb being dragged; `grab` is where in the thumb it was
/// grabbed.
#[derive(Debug, Clone, Copy)]
enum Drag {
    List { grab: i32 },
    Details { grab: i32 },
}

struct Cursors {
    arrow: HCURSOR,
    hand: HCURSOR,
    ibeam: HCURSOR,
}

struct Gui {
    hwnd: HWND,
    app: App,
    workers: Workers,
    rx: Receiver<Event>,
    fonts: Fonts,
    cursors: Cursors,
    layout: Layout,
    hits: HitMap<Target>,
    hover: Option<Target>,
    pressed: Option<Target>,
    drag: Option<Drag>,
    caret_on: bool,
    focused: bool,
    started: Instant,
    phase: u32,
    anim_running: bool,
    /// The first character of the search text that is drawn.
    search_start: usize,
    high_surrogate: Option<u16>,
    wraps: WrapCache,
}

thread_local! {
    static GUI: RefCell<Option<Gui>> = const { RefCell::new(None) };
}

fn timer_id(t: Timer) -> usize {
    match t {
        Timer::Search => TIMER_SEARCH,
        Timer::Details => TIMER_DETAILS,
    }
}

fn lparam_point(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam & 0xFFFF) as u16 as i16 as i32,
        ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32,
    )
}

fn ctrl_down() -> bool {
    // SAFETY: reads keyboard state only.
    unsafe { GetKeyState(VK_CONTROL as i32) < 0 }
}

fn clipboard_text(hwnd: HWND) -> Option<String> {
    // SAFETY: the clipboard is opened, read under GlobalLock, and closed.
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return None;
        }
        let mut out = None;
        let h = GetClipboardData(CF_UNICODETEXT);
        if h != 0 {
            let p = GlobalLock(h as _) as *const u16;
            if !p.is_null() {
                let mut n = 0;
                while n < 4096 && *p.add(n) != 0 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                GlobalUnlock(h as _);
            }
        }
        CloseClipboard();
        out
    }
}

/// Wrapped lines of recently drawn paragraphs, so that repainting (e.g. for
/// download progress) does not measure every word again.
#[derive(Default)]
struct WrapCache {
    map: HashMap<(String, i32, Font, usize), Vec<String>>,
}

impl WrapCache {
    fn wrap(&mut self, p: &Painter, text: &str, width: i32, f: Font, max: usize) -> Vec<String> {
        let key = (text.to_string(), width, f, max);
        if let Some(v) = self.map.get(&key) {
            return v.clone();
        }
        if self.map.len() > 200 {
            self.map.clear();
        }
        let v = format::wrap(text, width, max, |s| p.text_width(s, f));
        self.map.insert(key, v.clone());
        v
    }
}

impl Gui {
    fn invalidate(&self) {
        // SAFETY: plain call on our window.
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    fn resize(&mut self) {
        let mut r = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: plain call on our window.
        unsafe {
            GetClientRect(self.hwnd, &mut r);
        }
        self.layout = layout::compute(r.right - r.left, r.bottom - r.top);
        self.app.list.set_viewport(self.layout.list.height());
        self.app
            .details_scroll
            .set_viewport(self.layout.details_panel.height());
        self.invalidate();
    }

    fn restart_caret(&mut self) {
        self.caret_on = true;
        // SAFETY: plain call on our window.
        unsafe {
            SetTimer(self.hwnd, TIMER_CARET, CARET_BLINK_MS, None);
        }
    }

    fn run(&mut self, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::LoadIndex { refresh } => self.workers.load_index(refresh),
                Effect::StartTimer(t, ms) => {
                    // SAFETY: plain call; SetTimer with the same id restarts it.
                    unsafe {
                        SetTimer(self.hwnd, timer_id(t), ms, None);
                    }
                }
                Effect::FetchDetails { package } => self
                    .workers
                    .fetch_details(package, self.app.prefs().clone()),
                Effect::Download {
                    id,
                    installer,
                    dir,
                    fallback_name,
                } => self.workers.download(id, installer, dir, fallback_name),
                Effect::CancelDownload => self.workers.cancel(),
                Effect::Open(path) => {
                    if let Err(e) = shell::open_file(self.hwnd, &path) {
                        self.app.report(Kind::Error, e);
                    }
                }
                Effect::ShowInFolder(path) => {
                    if let Err(e) = shell::show_in_folder(self.hwnd, &path) {
                        self.app.report(Kind::Error, e);
                    }
                }
                Effect::OpenUrl(url) => {
                    if let Err(e) = shell::open_url(self.hwnd, &url) {
                        self.app.report(Kind::Error, e);
                    }
                }
            }
        }
        self.update_animation();
        self.invalidate();
    }

    fn update_animation(&mut self) {
        let busy = self.app.busy();
        // SAFETY: plain timer calls on our window.
        unsafe {
            if busy && !self.anim_running {
                SetTimer(self.hwnd, TIMER_ANIM, ANIM_MS, None);
                self.anim_running = true;
            } else if !busy && self.anim_running {
                KillTimer(self.hwnd, TIMER_ANIM);
                self.anim_running = false;
            }
        }
    }

    fn edited(&mut self) {
        let fx = self.app.query_edited();
        self.restart_caret();
        self.run(fx);
    }

    fn key_down(&mut self, vk: u16) -> bool {
        let ctrl = ctrl_down();
        let app = &mut self.app;
        let fx = match vk {
            VK_UP => app.move_selection(-1),
            VK_DOWN => app.move_selection(1),
            VK_PRIOR => app.page_selection(-1),
            VK_NEXT => app.page_selection(1),
            VK_HOME if ctrl => app.select_edge(false),
            VK_END if ctrl => app.select_edge(true),
            VK_HOME => {
                app.search.home();
                Vec::new()
            }
            VK_END => {
                app.search.end();
                Vec::new()
            }
            VK_LEFT => {
                if ctrl {
                    app.search.word_left();
                } else {
                    app.search.left();
                }
                Vec::new()
            }
            VK_RIGHT => {
                if ctrl {
                    app.search.word_right();
                } else {
                    app.search.right();
                }
                Vec::new()
            }
            VK_BACK => {
                let changed = if ctrl {
                    app.search.delete_word_back()
                } else {
                    app.search.backspace()
                };
                if changed {
                    self.edited();
                }
                return true;
            }
            VK_DELETE => {
                if app.search.delete() {
                    self.edited();
                }
                return true;
            }
            VK_RETURN => app.submit(),
            VK_ESCAPE => app.clear_search(),
            VK_F5 => app.action(&Action::RefreshIndex),
            0x56 if ctrl => {
                // Ctrl+V
                if let Some(text) = clipboard_text(self.hwnd) {
                    if self.app.search.insert(&text) {
                        self.edited();
                    }
                }
                return true;
            }
            _ => return false,
        };
        self.restart_caret();
        self.run(fx);
        true
    }

    fn char_typed(&mut self, unit: u16) {
        let c = match unit {
            0xD800..=0xDBFF => {
                self.high_surrogate = Some(unit);
                return;
            }
            0xDC00..=0xDFFF => match self.high_surrogate.take() {
                Some(hi) => String::from_utf16_lossy(&[hi, unit]),
                None => return,
            },
            // Control characters (Enter, Backspace, Ctrl+letters) are
            // handled in WM_KEYDOWN.
            u if u < 0x20 || u == 0x7F => return,
            u => String::from_utf16_lossy(&[u]),
        };
        if self.app.search.insert(&c) {
            self.edited();
        }
    }

    fn set_hover(&mut self, x: i32, y: i32) {
        let hover = self.hits.hit(x, y);
        let row = match hover {
            Some(Target::Row(i)) => Some(i),
            _ => None,
        };
        if hover != self.hover || row != self.app.list.hover {
            self.hover = hover;
            self.app.list.hover = row;
            self.invalidate();
        }
    }

    fn mouse_move(&mut self, x: i32, y: i32) {
        if let Some(drag) = self.drag {
            let l = self.layout;
            let changed = match drag {
                Drag::List { grab } => {
                    let s = &mut self.app.list.scroll;
                    let off = s.offset_for_thumb(l.list_scrollbar, y - grab);
                    s.scroll_to(off)
                }
                Drag::Details { grab } => {
                    let s = &mut self.app.details_scroll;
                    let off = s.offset_for_thumb(l.details_scrollbar, y - grab);
                    s.scroll_to(off)
                }
            };
            if changed {
                self.invalidate();
            }
            return;
        }
        self.set_hover(x, y);
    }

    fn button_down(&mut self, x: i32, y: i32) {
        // SAFETY: plain calls on our window.
        unsafe {
            SetFocus(self.hwnd);
            SetCapture(self.hwnd);
        }
        let target = self.hits.hit(x, y);
        let l = self.layout;
        match &target {
            Some(Target::Row(i)) => {
                let fx = self.app.select_row(*i);
                self.run(fx);
            }
            Some(Target::Search) => self.place_caret(x),
            Some(Target::ListThumb) => {
                if let Some(th) = self.app.list.scroll.thumb(l.list_scrollbar) {
                    self.drag = Some(Drag::List { grab: y - th.top });
                }
            }
            Some(Target::DetailsThumb) => {
                if let Some(th) = self.app.details_scroll.thumb(l.details_scrollbar) {
                    self.drag = Some(Drag::Details { grab: y - th.top });
                }
            }
            Some(Target::ListTrack) => {
                let s = &mut self.app.list.scroll;
                if let Some(th) = s.thumb(l.list_scrollbar) {
                    let page = s.viewport.max(1);
                    s.scroll_by(if y < th.top { -page } else { page });
                }
            }
            Some(Target::DetailsTrack) => {
                let s = &mut self.app.details_scroll;
                if let Some(th) = s.thumb(l.details_scrollbar) {
                    let page = s.viewport.max(1);
                    s.scroll_by(if y < th.top { -page } else { page });
                }
            }
            Some(Target::Button(_)) | Some(Target::Link(_)) | None => {}
        }
        self.pressed = target;
        self.invalidate();
    }

    fn button_up(&mut self, x: i32, y: i32) {
        // SAFETY: plain call.
        unsafe {
            ReleaseCapture();
        }
        self.drag = None;
        let pressed = self.pressed.take();
        let here = self.hits.hit(x, y);
        if let Some(Target::Button(a) | Target::Link(a)) = &pressed {
            if pressed == here {
                let fx = self.app.action(a);
                self.run(fx);
            }
        }
        self.set_hover(x, y);
        self.invalidate();
    }

    fn wheel(&mut self, delta: i32, x: i32, y: i32) {
        let l = self.layout;
        let changed = if l.list_panel.contains(x, y) {
            self.app.list.scroll.wheel(delta)
        } else if l.details_panel.contains(x, y) {
            self.app.details_scroll.wheel(delta)
        } else {
            false
        };
        if changed {
            self.set_hover(x, y);
            self.invalidate();
        }
    }

    fn place_caret(&mut self, x: i32) {
        // SAFETY: a screen DC is borrowed for measuring and released.
        unsafe {
            let dc = GetDC(self.hwnd);
            {
                let p = Painter::new(dc, &self.fonts);
                let inner = search_inner(self.layout.search);
                let start = self
                    .search_start
                    .min(self.app.search.text().chars().count());
                let skipped = p.text_width(&self.app.search.slice(0, start), Font::Body);
                self.app
                    .search
                    .caret_from_x(x - inner.left + skipped, |s| p.text_width(s, Font::Body));
            }
            ReleaseDC(self.hwnd, dc);
        }
        self.restart_caret();
    }

    fn cursor(&self) -> HCURSOR {
        match &self.hover {
            Some(Target::Search) => self.cursors.ibeam,
            Some(Target::Button(_) | Target::Link(_)) => self.cursors.hand,
            _ => self.cursors.arrow,
        }
    }

    fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        self.app.set_time(self.started.elapsed().as_millis() as u64);
        match msg {
            WM_PAINT => {
                self.paint_window();
                Some(0)
            }
            WM_ERASEBKGND => Some(1),
            WM_SIZE => {
                self.resize();
                Some(0)
            }
            WM_GETMINMAXINFO => {
                let mut r = RECT {
                    left: 0,
                    top: 0,
                    right: layout::MIN_WIDTH,
                    bottom: layout::MIN_HEIGHT,
                };
                // SAFETY: lparam points to a MINMAXINFO for this message.
                unsafe {
                    AdjustWindowRectEx(&mut r, WS_OVERLAPPEDWINDOW, 0, 0);
                    let info = lparam as *mut MINMAXINFO;
                    if !info.is_null() {
                        (*info).ptMinTrackSize.x = r.right - r.left;
                        (*info).ptMinTrackSize.y = r.bottom - r.top;
                    }
                }
                Some(0)
            }
            WM_APP_EVENT => {
                let mut fx = Vec::new();
                while let Ok(ev) = self.rx.try_recv() {
                    fx.extend(self.app.on_event(ev));
                }
                self.run(fx);
                Some(0)
            }
            WM_TIMER => {
                match wparam {
                    TIMER_SEARCH | TIMER_DETAILS => {
                        // SAFETY: plain call; the timers are one-shot.
                        unsafe {
                            KillTimer(self.hwnd, wparam);
                        }
                        let t = if wparam == TIMER_SEARCH {
                            Timer::Search
                        } else {
                            Timer::Details
                        };
                        let fx = self.app.timer(t);
                        self.run(fx);
                    }
                    TIMER_CARET => {
                        self.caret_on = !self.caret_on;
                        self.invalidate();
                    }
                    TIMER_ANIM => {
                        self.phase = self.phase.wrapping_add(1);
                        self.update_animation();
                        self.invalidate();
                    }
                    _ => {}
                }
                Some(0)
            }
            WM_KEYDOWN => self.key_down(wparam as u16).then_some(0),
            WM_CHAR => {
                self.char_typed(wparam as u16);
                Some(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_point(lparam);
                self.mouse_move(x, y);
                Some(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_point(lparam);
                self.button_down(x, y);
                Some(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = lparam_point(lparam);
                self.button_up(x, y);
                Some(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam >> 16) & 0xFFFF) as u16 as i16 as i32;
                let (sx, sy) = lparam_point(lparam);
                let mut pt = POINT { x: sx, y: sy };
                // SAFETY: plain call.
                unsafe {
                    ScreenToClient(self.hwnd, &mut pt);
                }
                self.wheel(delta, pt.x, pt.y);
                Some(0)
            }
            WM_SETCURSOR if (lparam & 0xFFFF) as u32 == HTCLIENT => {
                // SAFETY: plain call.
                unsafe {
                    SetCursor(self.cursor());
                }
                Some(1)
            }
            WM_SETFOCUS => {
                self.focused = true;
                self.restart_caret();
                self.invalidate();
                Some(0)
            }
            WM_KILLFOCUS => {
                self.focused = false;
                self.invalidate();
                Some(0)
            }
            WM_DESTROY => {
                self.workers.cancel();
                // SAFETY: plain call.
                unsafe {
                    PostQuitMessage(0);
                }
                Some(0)
            }
            _ => None,
        }
    }

    // ── Painting ────────────────────────────────────────────────────────

    fn paint_window(&mut self) {
        // SAFETY: standard BeginPaint/EndPaint with an off-screen bitmap
        // that is selected out and deleted before returning.
        unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(self.hwnd, &mut ps);
            let (w, h) = (self.layout.client.width(), self.layout.client.height());
            if w > 0 && h > 0 {
                let mem = CreateCompatibleDC(dc);
                let bmp = CreateCompatibleBitmap(dc, w, h);
                if mem != 0 && bmp != 0 {
                    let old = SelectObject(mem, bmp);
                    self.paint(mem);
                    BitBlt(dc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
                    SelectObject(mem, old);
                } else {
                    self.paint(dc);
                }
                if bmp != 0 {
                    DeleteObject(bmp);
                }
                if mem != 0 {
                    DeleteDC(mem);
                }
            }
            EndPaint(self.hwnd, &ps);
        }
    }

    fn paint(&mut self, dc: isize) {
        let Gui {
            app,
            fonts,
            layout,
            hits,
            hover,
            pressed,
            caret_on,
            focused,
            phase,
            search_start,
            wraps,
            ..
        } = self;
        let p = Painter::new(dc, fonts);
        hits.clear();
        let mut c = Canvas {
            p: &p,
            l: *layout,
            hits,
            hover: hover.clone(),
            pressed: pressed.clone(),
            phase: *phase,
            wraps,
            clip: layout.client,
        };
        c.p.fill(c.l.client, t::BACKGROUND);
        c.list(app);
        c.details(app);
        // Drawn last, over anything scrolled under them.
        c.list_caption(app);
        c.header(app, *focused, *caret_on, search_start);
        c.status(app);
    }
}

/// Where the search text goes inside the search box.
fn search_inner(search: Rect) -> Rect {
    Rect::new(
        search.left + 42,
        search.top,
        search.right - 14,
        search.bottom,
    )
}

/// One paint pass.
struct Canvas<'a, 'p> {
    p: &'a Painter<'p>,
    l: Layout,
    hits: &'a mut HitMap<Target>,
    hover: Option<Target>,
    pressed: Option<Target>,
    phase: u32,
    wraps: &'a mut WrapCache,
    /// Clickable areas are cut to this, so that what is scrolled out of a
    /// panel cannot be clicked through whatever covers it.
    clip: Rect,
}

fn kind_color(k: Kind) -> u32 {
    match k {
        Kind::Info => t::ACCENT,
        Kind::Success => t::SUCCESS,
        Kind::Warning => t::WARNING,
        Kind::Error => t::DANGER,
    }
}

impl Canvas<'_, '_> {
    fn wrap(&mut self, text: &str, width: i32, f: Font, max: usize) -> Vec<String> {
        self.wraps.wrap(self.p, text, width, f, max)
    }

    fn header(&mut self, app: &App, focused: bool, caret_on: bool, search_start: &mut usize) {
        let (p, l) = (self.p, self.l);
        p.fill(l.header, t::SURFACE);
        p.fill(
            Rect::new(0, l.header.bottom - 1, l.header.right, l.header.bottom),
            t::BORDER,
        );
        // Logo: a 2×2 grid of tiles, then the name.
        let (lx, ly) = (l.brand.left, l.brand.top + (l.brand.height() - 24) / 2);
        for (i, color) in [t::ACCENT, t::ACCENT, t::ACCENT_HOVER, t::ACCENT]
            .iter()
            .enumerate()
        {
            let (dx, dy) = ((i % 2) as i32 * 13, (i / 2) as i32 * 13);
            p.round_rect(Rect::xywh(lx + dx, ly + dy, 11, 11), 3, *color, None);
        }
        p.text_in(
            Rect::new(lx + 38, l.brand.top, l.brand.right - 8, l.brand.bottom),
            "App Center",
            Font::Brand,
            t::TEXT,
            Align::Left,
        );

        // Search box.
        let r = l.search;
        let border = if focused { t::ACCENT } else { t::BORDER };
        p.round_rect(r, 10, t::RAISED, Some(border));
        let icon = Rect::xywh(r.left + 15, r.top + 11, 14, 14);
        p.ring(icon, 2, t::TEXT_MUTED);
        for i in 0..3 {
            p.fill(
                Rect::xywh(icon.right - 2 + i * 2, icon.bottom - 2 + i * 2, 3, 3),
                t::TEXT_MUTED,
            );
        }
        let inner = search_inner(r);
        let lh = p.line_height(Font::Body);
        let ty = r.top + (r.height() - lh) / 2;
        let input = &app.search;
        let mut caret_x = inner.left;
        if input.is_empty() {
            *search_start = 0;
            p.text_in(
                inner,
                "Search apps, e.g. firefox, 7-zip, vlc",
                Font::Body,
                t::TEXT_FAINT,
                Align::Left,
            );
        } else {
            let measure = |s: &str| p.text_width(s, Font::Body);
            let (start, end) = input.visible_range(*search_start, inner.width() - 2, measure);
            *search_start = start;
            p.text(
                inner.left,
                ty,
                &input.slice(start, end),
                Font::Body,
                t::TEXT,
            );
            caret_x = inner.left + p.text_width(&input.slice(start, input.caret()), Font::Body);
        }
        if focused && caret_on {
            p.fill(
                Rect::new(caret_x, ty + 1, caret_x + 2, ty + lh - 1),
                t::ACCENT,
            );
        }
        self.hits.add(r, Target::Search);

        // Refresh.
        let busy = app.refreshing()
            || matches!(
                app.index_state(),
                app_center::ui::app::IndexState::Loading(_)
            );
        let b = ButtonView {
            label: if busy { "Updating…" } else { "Refresh" }.into(),
            action: Action::RefreshIndex,
            style: ButtonStyle::Secondary,
            enabled: !busy,
        };
        self.button(l.refresh, &b);
    }

    fn list_caption(&mut self, app: &App) {
        let (p, l) = (self.p, self.l);
        p.fill(l.list_caption, t::SURFACE);
        p.text_in(
            Rect::new(
                l.list_caption.left + 22,
                l.list_caption.top + 8,
                l.list_caption.right - 16,
                l.list_caption.bottom,
            ),
            &app.list_caption(),
            Font::Small,
            t::TEXT_MUTED,
            Align::Left,
        );
        // Divider between the columns.
        p.fill(
            Rect::new(
                l.list_panel.right,
                l.list_panel.top,
                l.list_panel.right + 1,
                l.list_panel.bottom,
            ),
            t::BORDER,
        );
    }

    fn list(&mut self, app: &App) {
        let (p, l) = (self.p, self.l);
        p.fill(l.list_panel, t::SURFACE);
        if app.row_count() == 0 {
            if let Some((title, detail, progress)) = app.list_placeholder() {
                let x = l.list.left + 24;
                let w = l.list.width() - 48;
                let mut y = l.list.top + 40;
                for line in self.wrap(&title, w, Font::Strong, 3) {
                    p.text(x, y, &line, Font::Strong, t::TEXT);
                    y += p.line_height(Font::Strong);
                }
                y += 6;
                let lines = self.wrap(&detail, w, Font::Small, 8);
                y += p.lines(x, y, &lines, Font::Small, t::TEXT_MUTED) + 16;
                if let Some(pv) = progress {
                    self.progress_bar(x, y, w, &pv, false);
                }
            }
            return;
        }
        let scrollable = app.list.scroll.scrollable();
        let right_pad = if scrollable { 22 } else { 18 };
        let (strong_h, small_h) = (p.line_height(Font::Strong), p.line_height(Font::Small));
        for i in app.list.visible() {
            let Some(row) = app.row(i) else { continue };
            let r = app.list.row_rect(l.list, i);
            let hovered = app.list.hover == Some(i);
            let card = Rect::new(r.left + 8, r.top + 2, r.right - 8, r.bottom - 2);
            if row.selected {
                p.round_rect(card, 8, t::SELECTED, None);
                p.round_rect(
                    Rect::new(card.left, card.top + 14, card.left + 3, card.bottom - 14),
                    1,
                    t::ACCENT,
                    None,
                );
            } else if hovered {
                p.round_rect(card, 8, t::HOVER, None);
            }
            let av = Rect::xywh(r.left + 22, r.top + (r.height() - 38) / 2, 38, 38);
            p.round_rect(av, 9, t::avatar_color(&row.id), None);
            p.text_in(
                av,
                &t::avatar_letter(&row.name),
                Font::Strong,
                WHITE,
                Align::Center,
            );
            let tx = av.right + 14;
            let right = r.right - right_pad;
            let vw = p.text_width(&row.version, Font::Small).min(110);
            let top = r.top + 10;
            p.text_in(
                Rect::new(right - vw, top, right, top + strong_h),
                &row.version,
                Font::Small,
                t::TEXT_MUTED,
                Align::Right,
            );
            p.text_in(
                Rect::new(tx, top, right - vw - 12, top + strong_h),
                &row.name,
                Font::Strong,
                t::TEXT,
                Align::Left,
            );
            let (badge, badge_color) = if row.downloading {
                ("Downloading", t::ACCENT)
            } else if row.downloaded {
                ("Downloaded", t::SUCCESS)
            } else {
                ("", 0)
            };
            let bw = if badge.is_empty() {
                0
            } else {
                p.text_width(badge, Font::Small) + 12
            };
            let sub_top = top + strong_h + 2;
            if bw > 0 {
                p.text_in(
                    Rect::new(right - bw + 12, sub_top, right, sub_top + small_h),
                    badge,
                    Font::Small,
                    badge_color,
                    Align::Right,
                );
            }
            let sub = if row.publisher.is_empty() {
                row.id.clone()
            } else {
                format!("{} · {}", row.publisher, row.id)
            };
            p.text_in(
                Rect::new(tx, sub_top, right - bw, sub_top + small_h),
                &sub,
                Font::Small,
                t::TEXT_MUTED,
                Align::Left,
            );
            self.hits.add(r.intersect(&l.list), Target::Row(i));
        }
        self.scrollbar(
            &app.list.scroll,
            l.list_scrollbar,
            Target::ListTrack,
            Target::ListThumb,
        );
    }

    fn scrollbar(
        &mut self,
        s: &app_center::ui::scroll::Scroll,
        track: Rect,
        track_target: Target,
        thumb_target: Target,
    ) {
        let Some(thumb) = s.thumb(track) else { return };
        self.hits.add(track, track_target);
        let active = self.hover.as_ref() == Some(&thumb_target)
            || self.pressed.as_ref() == Some(&thumb_target);
        let color = if active {
            t::mix(t::SCROLL_THUMB, t::TEXT, 60)
        } else {
            t::SCROLL_THUMB
        };
        self.p.round_rect(thumb, 3, color, None);
        self.hits.add(thumb, thumb_target);
    }

    fn button(&mut self, r: Rect, b: &ButtonView) {
        let p = self.p;
        let target = Target::Button(b.action.clone());
        let hot = b.enabled && self.hover.as_ref() == Some(&target);
        let down = hot && self.pressed.as_ref() == Some(&target);
        let (fill, border, text) = match (b.style, b.enabled) {
            (ButtonStyle::Primary, true) => (
                if down {
                    t::ACCENT
                } else if hot {
                    t::ACCENT_HOVER
                } else {
                    t::ACCENT
                },
                None,
                t::ACCENT_TEXT,
            ),
            (ButtonStyle::Primary, false) => (
                t::mix(t::ACCENT, t::BACKGROUND, 150),
                None,
                t::mix(t::ACCENT_TEXT, t::BACKGROUND, 110),
            ),
            (ButtonStyle::Secondary, enabled) => (
                if hot {
                    t::mix(t::RAISED, t::TEXT, 18)
                } else {
                    t::RAISED
                },
                Some(t::BORDER),
                if enabled { t::TEXT } else { t::TEXT_FAINT },
            ),
        };
        p.round_rect(r, 8, fill, border);
        p.text_in(r, &b.label, Font::Button, text, Align::Center);
        if b.enabled {
            self.hits.add(r.intersect(&self.clip), target);
        }
    }

    /// A progress bar and its label at `y`; returns the height used.
    fn progress_bar(&mut self, x: i32, y: i32, w: i32, pv: &ProgressView, big: bool) -> i32 {
        let p = self.p;
        let h = if big { 8 } else { 6 };
        let track = Rect::xywh(x, y, w, h);
        p.round_rect(track, h / 2, t::TRACK, None);
        match pv.fraction {
            Some(f) => {
                let fw = ((w as f32) * f) as i32;
                if fw > 0 {
                    p.round_rect(Rect::xywh(x, y, fw.max(h), h), h / 2, t::ACCENT, None);
                }
            }
            None => {
                // A segment sweeping across.
                let seg = (w / 4).max(24);
                let span = w + seg;
                let pos = (self.phase as i32 * (w / 30).max(6)) % span - seg;
                let left = pos.max(0);
                let right = (pos + seg).min(w);
                if right > left {
                    p.round_rect(
                        Rect::new(x + left, y, x + right, y + h),
                        h / 2,
                        t::ACCENT,
                        None,
                    );
                }
            }
        }
        let mut used = h;
        if !pv.label.is_empty() {
            p.text(x, y + h + 8, &pv.label, Font::Small, t::TEXT_MUTED);
            used += 8 + p.line_height(Font::Small);
        }
        used
    }

    fn details(&mut self, app: &mut App) {
        let l = self.l;
        self.clip = l.details_panel;
        let top = l.details_panel.top + 32 - app.details_scroll.offset;
        let height = match app.details_view() {
            DetailsView::Placeholder {
                title,
                detail,
                progress,
                button,
            } => {
                app.details_scroll.scroll_to(0);
                self.placeholder(&title, &detail, progress.as_ref(), button.as_ref());
                0
            }
            DetailsView::Package(v) => self.package(&v, top) - top + 32,
        };
        self.clip = l.client;
        app.details_scroll.set_viewport(l.details_panel.height());
        app.details_scroll.set_content(height);
        self.scrollbar(
            &app.details_scroll.clone(),
            l.details_scrollbar,
            Target::DetailsTrack,
            Target::DetailsThumb,
        );
    }

    fn placeholder(
        &mut self,
        title: &str,
        detail: &str,
        progress: Option<&ProgressView>,
        button: Option<&ButtonView>,
    ) {
        let (p, l) = (self.p, self.l);
        let w = l.details.width().min(520);
        let x = l.details.left + (l.details.width() - w) / 2;
        let mut y = l.details_panel.top + l.details_panel.height() / 4;
        let center = |p: &Painter, y: i32, s: &str, f: Font, color: u32| {
            p.text_in(
                Rect::new(x, y, x + w, y + p.line_height(f)),
                s,
                f,
                color,
                Align::Center,
            );
        };
        for line in self.wrap(title, w, Font::Title, 2) {
            center(p, y, &line, Font::Title, t::TEXT);
            y += p.line_height(Font::Title);
        }
        y += 10;
        for line in self.wrap(detail, w, Font::Body, 8) {
            center(p, y, &line, Font::Body, t::TEXT_MUTED);
            y += p.line_height(Font::Body);
        }
        y += 24;
        if let Some(pv) = progress {
            let bw = w.min(360);
            self.progress_bar(x + (w - bw) / 2, y, bw, pv, true);
            y += 40;
        }
        if let Some(b) = button {
            let bw = p.text_width(&b.label, Font::Button) + 2 * layout::BUTTON_PADDING;
            let r = Rect::xywh(x + (w - bw) / 2, y, bw, layout::BUTTON_HEIGHT);
            self.button(r, b);
        }
    }

    /// Paints a package's details from `y`; returns the bottom.
    fn package(&mut self, v: &PackageView, mut y: i32) -> i32 {
        let (p, l) = (self.p, self.l);
        let x = l.details.left;
        let w = l.details.width();
        let panel = l.details_panel;

        // Title block: a big tile, the name and publisher · version.
        let tile = Rect::xywh(x, y, 72, 72);
        p.round_rect(tile, 16, t::avatar_color(&v.name), None);
        p.text_in(
            tile,
            &t::avatar_letter(&v.name),
            Font::Title,
            WHITE,
            Align::Center,
        );
        let tx = tile.right + 20;
        let title_h = p.line_height(Font::Title);
        p.text_in(
            Rect::new(tx, y + 4, x + w, y + 4 + title_h),
            &v.name,
            Font::Title,
            t::TEXT,
            Align::Left,
        );
        p.text_in(
            Rect::new(
                tx,
                y + 8 + title_h,
                x + w,
                y + 8 + title_h + p.line_height(Font::Body),
            ),
            &v.subtitle,
            Font::Body,
            t::TEXT_MUTED,
            Align::Left,
        );
        y = tile.bottom + 20;

        if let Some(s) = &v.summary {
            let lines = self.wrap(s, w, Font::Body, 4);
            y += p.lines(x, y, &lines, Font::Body, t::TEXT) + 20;
        }

        // Buttons.
        if !v.buttons.is_empty() {
            let widths: Vec<i32> = v
                .buttons
                .iter()
                .map(|b| p.text_width(&b.label, Font::Button))
                .collect();
            let rects = layout::button_row(x, y, x + w, &widths);
            for (r, b) in rects.iter().zip(&v.buttons) {
                // Scrolled under the header: the header is painted (and
                // hit-tested) on top, so only the visible part is clickable.
                self.button(*r, b);
            }
            y = rects.last().map_or(y, |r| r.bottom) + 18;
        }
        if let Some(pv) = &v.progress {
            y += self.progress_bar(x, y, w.min(560), pv, true) + 18;
        }
        if v.loading {
            let pv = ProgressView {
                fraction: None,
                label: "Loading details…".into(),
            };
            y += self.progress_bar(x, y, w.min(360), &pv, false) + 22;
        }

        // Notice card.
        if let Some(n) = &v.notice {
            let inner_w = w - 40;
            let detail = self.wrap(&n.detail, inner_w, Font::Small, 8);
            let th = p.line_height(Font::Strong);
            let dh = p.line_height(Font::Small) * detail.len() as i32;
            let h = 16 + th + if detail.is_empty() { 0 } else { 4 + dh } + 16;
            let card = Rect::xywh(x, y, w, h);
            p.round_rect(card, 10, t::mix(t::RAISED, kind_color(n.kind), 22), None);
            p.round_rect(
                Rect::new(card.left, card.top, card.left + 4, card.bottom),
                2,
                kind_color(n.kind),
                None,
            );
            p.text_in(
                Rect::new(x + 20, y + 16, x + w - 16, y + 16 + th),
                &n.title,
                Font::Strong,
                t::TEXT,
                Align::Left,
            );
            p.lines(x + 20, y + 20 + th, &detail, Font::Small, t::TEXT_MUTED);
            y = card.bottom + 24;
        }

        // About.
        if let Some(d) = &v.description {
            p.text(x, y, "About", Font::Strong, t::TEXT);
            y += p.line_height(Font::Strong) + 8;
            let lines = self.wrap(d, w, Font::Body, 60);
            y += p.lines(x, y, &lines, Font::Body, t::mix(t::TEXT, t::TEXT_MUTED, 90)) + 24;
        }

        // Facts.
        if !v.facts.is_empty() {
            p.text(x, y, "Information", Font::Strong, t::TEXT);
            y += p.line_height(Font::Strong) + 10;
            p.fill(Rect::new(x, y, x + w, y + 1), t::BORDER);
            y += 12;
            let label_w = 150.min(w / 3);
            let vx = x + label_w;
            let vw = w - label_w;
            let lh = p.line_height(Font::Body);
            for f in &v.facts {
                let lines = self.wrap(&f.value, vw, Font::Body, 4);
                let target = f.link.clone().map(Target::Link);
                let hot = target.is_some() && self.hover == target;
                let color = match (&f.link, hot) {
                    (Some(_), true) => t::ACCENT_HOVER,
                    (Some(_), false) => t::ACCENT,
                    (None, _) => t::TEXT,
                };
                p.text(x, y + 1, &f.label, Font::Small, t::TEXT_MUTED);
                let h = p.lines(vx, y, &lines, Font::Body, color).max(lh);
                if let Some(target) = target {
                    let tw = lines
                        .iter()
                        .map(|s| p.text_width(s, Font::Body))
                        .max()
                        .unwrap_or(0);
                    let r = Rect::xywh(vx, y, tw, h).intersect(&panel);
                    if hot {
                        p.fill(Rect::new(vx, y + h - 1, vx + tw, y + h), t::ACCENT_HOVER);
                    }
                    self.hits.add(r, target);
                }
                y += h + 10;
            }
        }
        y
    }

    fn status(&mut self, app: &App) {
        let (p, l) = (self.p, self.l);
        p.fill(l.status, t::SURFACE);
        p.fill(
            Rect::new(0, l.status.top, l.status.right, l.status.top + 1),
            t::BORDER,
        );
        let s = app.status_line();
        let cy = l.status.top + l.status.height() / 2;
        p.ellipse(
            Rect::xywh(layout::PADDING, cy - 4, 8, 8),
            kind_color(s.kind),
        );
        let hint = "Enter: search   F5: refresh   Esc: clear";
        let hw = p.text_width(hint, Font::Small);
        let show_hint = l.status.width() > hw + 400;
        let right = if show_hint {
            l.status.right - layout::PADDING - hw - 24
        } else {
            l.status.right - layout::PADDING
        };
        p.text_in(
            Rect::new(layout::PADDING + 18, l.status.top, right, l.status.bottom),
            &s.text,
            Font::Small,
            t::TEXT_MUTED,
            Align::Left,
        );
        if show_hint {
            p.text_in(
                Rect::new(
                    right,
                    l.status.top,
                    l.status.right - layout::PADDING,
                    l.status.bottom,
                ),
                hint,
                Font::Small,
                t::TEXT_FAINT,
                Align::Right,
            );
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handled = GUI.with(|g| match g.try_borrow_mut() {
        Ok(mut guard) => guard
            .as_mut()
            .filter(|gui| gui.hwnd == hwnd)
            .and_then(|gui| gui.handle(msg, wparam, lparam)),
        // Re-entered while handling another message (e.g. a message sent
        // by a call we made): let Windows handle it.
        Err(_) => None,
    });
    match handled {
        Some(r) => r,
        None => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn fail(text: &str) {
    let (t, c) = (wide(text), wide("App Center"));
    // SAFETY: NUL-terminated strings.
    unsafe {
        MessageBoxW(0, t.as_ptr(), c.as_ptr(), MB_ICONERROR);
    }
}

pub fn run() {
    let class = wide("OTWAppCenter");
    let title = wide("App Center");
    // SAFETY: standard window class registration, window creation and
    // message loop; all strings outlive the calls that use them.
    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let arrow = LoadCursorW(0, IDC_ARROW);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: LoadIconW(0, IDI_APPLICATION),
            hCursor: 0,
            hbrBackground: 0,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: 0,
        };
        if RegisterClassExW(&wc) == 0 {
            fail("Could not register the window class.");
            return;
        }
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1120,
            740,
            0,
            0,
            hinstance,
            std::ptr::null(),
        );
        if hwnd == 0 {
            fail("Could not create the App Center window.");
            return;
        }

        let (tx, rx) = channel();
        let base = std::env::var("APP_CENTER_SOURCE").unwrap_or_else(|_| DEFAULT_SOURCE.into());
        let source = Source::new(&base, Http::new(), default_cache_dir());
        let (app, effects) = App::new(shell::downloads_dir(), Preferences::default());
        let gui = Gui {
            hwnd,
            app,
            workers: Workers::new(hwnd, tx, source),
            rx,
            fonts: Fonts::new(),
            cursors: Cursors {
                arrow,
                hand: LoadCursorW(0, IDC_HAND),
                ibeam: LoadCursorW(0, IDC_IBEAM),
            },
            layout: Layout::default(),
            hits: HitMap::new(),
            hover: None,
            pressed: None,
            drag: None,
            caret_on: true,
            focused: true,
            started: Instant::now(),
            phase: 0,
            anim_running: false,
            search_start: 0,
            high_surrogate: None,
            wraps: WrapCache::default(),
        };
        GUI.with(|g| *g.borrow_mut() = Some(gui));
        GUI.with(|g| {
            if let Some(gui) = g.borrow_mut().as_mut() {
                gui.resize();
                gui.run(effects);
                gui.restart_caret();
            }
        });

        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetFocus(hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // Worker threads may still be running; they stop with the process.
        GUI.with(|g| g.borrow_mut().take());
    }
}
