//! USER32: windows, messages, painting and input.
//!
//! This is a host-side window manager in the spirit of the Win95 USER
//! module. Windows form a tree; each top-level window owns a backing surface
//! (in `GdiState::surfaces`) that GDI draws into and the host display
//! composites. Messages are delivered synchronously to guest window
//! procedures through [`GuestMemory::call_guest`], raw host input is routed
//! into the queue at retrieval time, and WM_PAINT/WM_TIMER are synthesised
//! from update regions and timers exactly when the queue is otherwise empty.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::{
    kernel::pe::ImportName,
    win32::gdi::{self, read_rect, sys_color, write_rect, DcTarget, SYSBRUSH_BASE},
    win32::gfx::{self, Rect, Region, Rgb, Surface, Target},
    win32::{
        name_of, read_cstr, write_cstr, ApiContext, ApiOutcome, GuestClass, GuestHook, GuestMemory,
        GuestMessage, GuestTimer, GuestWindow, GuiState, RawInput, SharedState,
    },
};

pub const DESKTOP_HWND: u32 = 0x0000_0010;

// Window styles.
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_MINIMIZE: u32 = 0x2000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_DISABLED: u32 = 0x0800_0000;
pub const WS_CLIPSIBLINGS: u32 = 0x0400_0000;
pub const WS_CLIPCHILDREN: u32 = 0x0200_0000;
pub const WS_MAXIMIZE: u32 = 0x0100_0000;
pub const WS_CAPTION: u32 = 0x00C0_0000;
pub const WS_BORDER: u32 = 0x0080_0000;
pub const WS_DLGFRAME: u32 = 0x0040_0000;
pub const WS_VSCROLL: u32 = 0x0020_0000;
pub const WS_HSCROLL: u32 = 0x0010_0000;
pub const WS_SYSMENU: u32 = 0x0008_0000;
pub const WS_THICKFRAME: u32 = 0x0004_0000;
pub const WS_MINIMIZEBOX: u32 = 0x0002_0000;
pub const WS_MAXIMIZEBOX: u32 = 0x0001_0000;
pub const WS_EX_DLGMODALFRAME: u32 = 0x0000_0001;
pub const WS_EX_NOPARENTNOTIFY: u32 = 0x0000_0004;
pub const WS_EX_TOPMOST: u32 = 0x0000_0008;
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
pub const WS_EX_WINDOWEDGE: u32 = 0x0000_0100;
pub const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
pub const WS_EX_STATICEDGE: u32 = 0x0002_0000;

// Messages.
pub const WM_CREATE: u32 = 0x0001;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_MOVE: u32 = 0x0003;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_ACTIVATE: u32 = 0x0006;
pub const WM_SETFOCUS: u32 = 0x0007;
pub const WM_KILLFOCUS: u32 = 0x0008;
pub const WM_ENABLE: u32 = 0x000A;
pub const WM_SETTEXT: u32 = 0x000C;
pub const WM_GETTEXT: u32 = 0x000D;
pub const WM_GETTEXTLENGTH: u32 = 0x000E;
pub const WM_PAINT: u32 = 0x000F;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_QUIT: u32 = 0x0012;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_SHOWWINDOW: u32 = 0x0018;
pub const WM_ACTIVATEAPP: u32 = 0x001C;
pub const WM_SETCURSOR: u32 = 0x0020;
pub const WM_MOUSEACTIVATE: u32 = 0x0021;
pub const WM_GETMINMAXINFO: u32 = 0x0024;
pub const WM_SETFONT: u32 = 0x0030;
pub const WM_GETFONT: u32 = 0x0031;
pub const WM_WINDOWPOSCHANGING: u32 = 0x0046;
pub const WM_WINDOWPOSCHANGED: u32 = 0x0047;
pub const WM_CONTEXTMENU: u32 = 0x007B;
pub const WM_NCCREATE: u32 = 0x0081;
pub const WM_NCDESTROY: u32 = 0x0082;
pub const WM_NCCALCSIZE: u32 = 0x0083;
pub const WM_NCHITTEST: u32 = 0x0084;
pub const WM_NCPAINT: u32 = 0x0085;
pub const WM_NCACTIVATE: u32 = 0x0086;
pub const WM_GETDLGCODE: u32 = 0x0087;
pub const WM_NCMOUSEMOVE: u32 = 0x00A0;
pub const WM_NCLBUTTONDOWN: u32 = 0x00A1;
pub const WM_NCLBUTTONUP: u32 = 0x00A2;
pub const WM_NCLBUTTONDBLCLK: u32 = 0x00A3;
pub const WM_NCRBUTTONDOWN: u32 = 0x00A4;
pub const WM_NCRBUTTONUP: u32 = 0x00A5;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_KEYUP: u32 = 0x0101;
pub const WM_CHAR: u32 = 0x0102;
pub const WM_SYSKEYDOWN: u32 = 0x0104;
pub const WM_SYSKEYUP: u32 = 0x0105;
pub const WM_SYSCHAR: u32 = 0x0106;
pub const WM_INITDIALOG: u32 = 0x0110;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_SYSCOMMAND: u32 = 0x0112;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_HSCROLL: u32 = 0x0114;
pub const WM_VSCROLL: u32 = 0x0115;
pub const WM_INITMENU: u32 = 0x0116;
pub const WM_INITMENUPOPUP: u32 = 0x0117;
pub const WM_MENUSELECT: u32 = 0x011F;
pub const WM_ENTERIDLE: u32 = 0x0121;
pub const WM_CTLCOLORMSGBOX: u32 = 0x0132;
pub const WM_CTLCOLOREDIT: u32 = 0x0133;
pub const WM_CTLCOLORLISTBOX: u32 = 0x0134;
pub const WM_CTLCOLORBTN: u32 = 0x0135;
pub const WM_CTLCOLORDLG: u32 = 0x0136;
pub const WM_CTLCOLORSCROLLBAR: u32 = 0x0137;
pub const WM_CTLCOLORSTATIC: u32 = 0x0138;
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_LBUTTONDBLCLK: u32 = 0x0203;
pub const WM_RBUTTONDOWN: u32 = 0x0204;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_PARENTNOTIFY: u32 = 0x0210;
pub const WM_ENTERMENULOOP: u32 = 0x0211;
pub const WM_EXITMENULOOP: u32 = 0x0212;
pub const WM_CAPTURECHANGED: u32 = 0x0215;

// Hit-test codes.
pub const HTERROR: i32 = -2;
pub const HTTRANSPARENT: i32 = -1;
pub const HTNOWHERE: i32 = 0;
pub const HTCLIENT: i32 = 1;
pub const HTCAPTION: i32 = 2;
pub const HTSYSMENU: i32 = 3;
pub const HTMENU: i32 = 5;
pub const HTHSCROLL: i32 = 6;
pub const HTVSCROLL: i32 = 7;
pub const HTMINBUTTON: i32 = 8;
pub const HTMAXBUTTON: i32 = 9;
pub const HTLEFT: i32 = 10;
pub const HTRIGHT: i32 = 11;
pub const HTTOP: i32 = 12;
pub const HTTOPLEFT: i32 = 13;
pub const HTTOPRIGHT: i32 = 14;
pub const HTBOTTOM: i32 = 15;
pub const HTBOTTOMLEFT: i32 = 16;
pub const HTBOTTOMRIGHT: i32 = 17;
pub const HTBORDER: i32 = 18;
pub const HTCLOSE: i32 = 20;

pub const CAPTION_HEIGHT: i32 = 18;
pub const MENU_HEIGHT: i32 = 19;
pub const SCROLL_SIZE: i32 = 16;
/// Height of the shell's taskbar along the bottom of the screen: the work
/// area (SPI_GETWORKAREA, GetMonitorInfo, maximized windows) ends above it.
pub const TASKBAR_HEIGHT: i32 = 28;

const CW_USEDEFAULT: i32 = i32::MIN;

pub fn lo(value: u32) -> i32 {
    value as u16 as i16 as i32
}

pub fn hi(value: u32) -> i32 {
    (value >> 16) as u16 as i16 as i32
}

pub fn make_long(low: i32, high: i32) -> u32 {
    (low as u16 as u32) | ((high as u16 as u32) << 16)
}

// ── Geometry ──────────────────────────────────────────────────────────────

pub fn has_menu_bar(window: &GuestWindow) -> bool {
    window.menu != 0 && window.style & WS_CHILD == 0
}

/// Frame width for a style: thick frame, dialog frame or thin border.
pub fn frame_width(style: u32, ex_style: u32) -> i32 {
    if style & WS_THICKFRAME != 0 {
        4
    } else if style & WS_DLGFRAME != 0 || ex_style & WS_EX_DLGMODALFRAME != 0 {
        3
    } else if style & WS_BORDER != 0 {
        1
    } else {
        0
    }
}

/// Non-client insets (left, top, right, bottom) of a window.
pub fn nc_insets(window: &GuestWindow) -> (i32, i32, i32, i32) {
    if let Some(insets) = window.nc_override {
        return insets;
    }
    nc_insets_for(scroll_style(window), window.ex_style, has_menu_bar(window))
}

/// Ask a 64-bit program's window for its client area (WM_NCCALCSIZE with
/// the window rectangle): a program may claim the caption for itself.
/// The standard frame is what DefWindowProc gives.
pub fn calc_nc_size<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    if !ctx.shared.borrow().x64 {
        return;
    }
    let Some((width, height, style)) = ctx.shared.borrow().gui.window(hwnd).map(|w| (w.width, w.height, w.style)) else {
        return;
    };
    if style & WS_CHILD != 0 {
        return;
    }
    // NCCALCSIZE_PARAMS: three RECTs and a WINDOWPOS pointer, then the
    // WINDOWPOS (hwnd, insert after, x, y, cx, cy, flags).
    let params = scratch(ctx, 128);
    let pos = params + 56;
    let mut bytes = Vec::new();
    for _ in 0..3 {
        for value in [0, 0, width, height] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&(pos as u64).to_le_bytes());
    bytes.extend_from_slice(&(hwnd as u64).to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    for value in [0, 0, width, height, 0] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.resize(128, 0);
    let _ = memory.write_bytes(params, &bytes);
    send(ctx, memory, hwnd, WM_NCCALCSIZE, 1, params);
    let rect: Vec<i32> = memory
        .read_bytes(params, 16)
        .unwrap_or_default()
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    free_scratch(ctx, params);
    if rect.len() != 4 {
        return;
    }
    let insets = (rect[0], rect[1], width - rect[2], height - rect[3]);
    let mut shared = ctx.shared.borrow_mut();
    if let Some(window) = shared.gui.window_mut(hwnd) {
        let standard = nc_insets_for(scroll_style(window), window.ex_style, has_menu_bar(window));
        let chosen = (insets != standard).then_some(insets);
        if window.nc_override != chosen {
            window.nc_override = chosen;
            window.nc_dirty = true;
        }
    }
}

/// A window's style as far as its own scroll bars go: a combo box passes
/// WS_VSCROLL/WS_HSCROLL on to its drop-down list and shows none itself.
fn scroll_style(window: &GuestWindow) -> u32 {
    if crate::win32::controls::is_combo_class(&window.class) {
        window.style & !(WS_VSCROLL | WS_HSCROLL)
    } else {
        window.style
    }
}

pub fn nc_insets_for(style: u32, ex_style: u32, menu: bool) -> (i32, i32, i32, i32) {
    let frame = frame_width(style, ex_style);
    let mut insets = (frame, frame, frame, frame);
    if style & WS_CAPTION == WS_CAPTION {
        insets.1 += if ex_style & WS_EX_TOOLWINDOW != 0 {
            15
        } else {
            CAPTION_HEIGHT
        };
    }
    if menu {
        insets.1 += MENU_HEIGHT;
    }
    let edge = if ex_style & WS_EX_CLIENTEDGE != 0 {
        2
    } else if ex_style & WS_EX_STATICEDGE != 0 {
        1
    } else {
        0
    };
    insets.0 += edge;
    insets.1 += edge;
    insets.2 += edge;
    insets.3 += edge;
    if style & WS_VSCROLL != 0 {
        insets.2 += SCROLL_SIZE;
    }
    if style & WS_HSCROLL != 0 {
        insets.3 += SCROLL_SIZE;
    }
    insets
}

/// Client rectangle size of a window.
pub fn client_size(window: &GuestWindow) -> (i32, i32) {
    let (l, t, r, b) = nc_insets(window);
    (
        (window.width - l - r).max(0),
        (window.height - t - b).max(0),
    )
}

/// Position of a window's top-left corner inside its top-level window, and
/// that top-level window's handle.
pub fn origin_in_top(gui: &GuiState, hwnd: u32) -> Option<(u32, i32, i32)> {
    let window = gui.window(hwnd)?;
    if window.parent == 0 || window.style & WS_CHILD == 0 {
        return Some((hwnd, 0, 0));
    }
    let parent = gui.window(window.parent)?;
    let (top, px, py) = origin_in_top(gui, window.parent)?;
    let (l, t, _, _) = nc_insets(parent);
    Some((top, px + l + window.x, py + t + window.y))
}

/// Screen position of a window's top-left corner.
pub fn window_screen_origin(gui: &GuiState, hwnd: u32) -> (i32, i32) {
    match origin_in_top(gui, hwnd) {
        Some((top, x, y)) => {
            let top = gui.window(top);
            (
                top.map(|w| w.x).unwrap_or(0) + x,
                top.map(|w| w.y).unwrap_or(0) + y,
            )
        }
        None => (0, 0),
    }
}

/// Screen position of a window's client origin.
pub fn client_screen_origin(gui: &GuiState, hwnd: u32) -> (i32, i32) {
    if hwnd == 0 || hwnd == DESKTOP_HWND {
        return (0, 0);
    }
    let (x, y) = window_screen_origin(gui, hwnd);
    let (l, t, _, _) = gui.window(hwnd).map(nc_insets).unwrap_or((0, 0, 0, 0));
    (x + l, y + t)
}

pub fn window_screen_rect(gui: &GuiState, hwnd: u32) -> Rect {
    if hwnd == DESKTOP_HWND || hwnd == 0 {
        return Rect::new(0, 0, gui.screen.0, gui.screen.1);
    }
    let (x, y) = window_screen_origin(gui, hwnd);
    let (w, h) = gui
        .window(hwnd)
        .map(|w| (w.width, w.height))
        .unwrap_or((0, 0));
    Rect::sized(x, y, w, h)
}

/// A window and all its ancestors are visible.
pub fn is_visible_chain(gui: &GuiState, hwnd: u32) -> bool {
    let mut current = hwnd;
    for _ in 0..32 {
        let Some(window) = gui.window(current) else {
            return false;
        };
        if !window.visible || window.minimized {
            return false;
        }
        if window.parent == 0 || window.style & WS_CHILD == 0 {
            return true;
        }
        current = window.parent;
    }
    false
}

/// Children of `parent` in z-order from bottom to top.
pub fn children(gui: &GuiState, parent: u32) -> Vec<u32> {
    gui.windows
        .iter()
        .filter(|w| w.parent == parent && w.style & WS_CHILD != 0)
        .map(|w| w.hwnd)
        .collect()
}

/// Geometry of a DC on `hwnd`: top-level window, device origin inside its
/// surface and the visible clip region in surface coordinates.
pub fn dc_geometry(gui: &GuiState, hwnd: u32, client: bool) -> Option<(u32, i32, i32, Region)> {
    let window = gui.window(hwnd)?;
    let (top, wx, wy) = origin_in_top(gui, hwnd)?;
    let (l, t, r, b) = nc_insets(window);
    let area = if client {
        Rect::new(
            wx + l,
            wy + t,
            wx + window.width - r,
            wy + window.height - b,
        )
    } else {
        Rect::sized(wx, wy, window.width, window.height)
    };
    let (ox, oy) = (area.left, area.top);
    if !is_visible_chain(gui, hwnd) {
        return Some((top, ox, oy, Region::empty()));
    }
    let mut clip = Region::from_rect(area);
    // Clip to every ancestor's client area.
    let mut current = window;
    while current.parent != 0 && current.style & WS_CHILD != 0 {
        let Some(parent) = gui.window(current.parent) else {
            break;
        };
        let (_, px, py) = origin_in_top(gui, parent.hwnd)?;
        let (pl, pt, pr, pb) = nc_insets(parent);
        clip = clip.intersect_rect(&Rect::new(
            px + pl,
            py + pt,
            px + parent.width - pr,
            py + parent.height - pb,
        ));
        // Ancestors with WS_CLIPSIBLINGS also hide siblings above them.
        current = parent;
    }
    if client && window.style & WS_CLIPCHILDREN != 0 {
        for child in children(gui, hwnd) {
            let Some(child_window) = gui.window(child) else {
                continue;
            };
            if !child_window.visible {
                continue;
            }
            let (_, cx, cy) = origin_in_top(gui, child)?;
            clip = clip.subtract_rect(&Rect::sized(
                cx,
                cy,
                child_window.width,
                child_window.height,
            ));
        }
    }
    if window.style & WS_CHILD != 0 && window.style & WS_CLIPSIBLINGS != 0 {
        let siblings = children(gui, window.parent);
        let position = siblings.iter().position(|s| *s == hwnd).unwrap_or(0);
        for sibling in &siblings[position + 1..] {
            let Some(sibling_window) = gui.window(*sibling) else {
                continue;
            };
            if !sibling_window.visible {
                continue;
            }
            let (_, sx, sy) = origin_in_top(gui, *sibling)?;
            clip = clip.subtract_rect(&Rect::sized(
                sx,
                sy,
                sibling_window.width,
                sibling_window.height,
            ));
        }
    }
    Some((top, ox, oy, clip))
}

/// Deepest visible child of `hwnd` under the screen point, or `hwnd`.
pub fn child_at(gui: &GuiState, hwnd: u32, sx: i32, sy: i32) -> u32 {
    let mut current = hwnd;
    'outer: for _ in 0..32 {
        let (cx, cy) = client_screen_origin(gui, current);
        let Some(window) = gui.window(current) else {
            return current;
        };
        let (cw, ch) = client_size(window);
        if !Rect::sized(cx, cy, cw, ch).contains(sx, sy) {
            return current;
        }
        for child in children(gui, current).into_iter().rev() {
            let Some(child_window) = gui.window(child) else {
                continue;
            };
            if !child_window.visible {
                continue;
            }
            let rect = Rect::sized(
                cx + child_window.x,
                cy + child_window.y,
                child_window.width,
                child_window.height,
            );
            if rect.contains(sx, sy) {
                current = child;
                continue 'outer;
            }
        }
        return current;
    }
    current
}

/// Topmost visible top-level window under a screen point.
pub fn top_level_at(gui: &GuiState, sx: i32, sy: i32) -> Option<u32> {
    gui.windows
        .iter()
        .rev()
        .filter(|w| (w.parent == 0 || w.style & WS_CHILD == 0) && w.visible && !w.minimized)
        .find(|w| Rect::sized(w.x, w.y, w.width, w.height).contains(sx, sy))
        .map(|w| w.hwnd)
}

/// Rectangles of the caption buttons in window coordinates:
/// (close, maximize, minimize).
pub fn caption_buttons(window: &GuestWindow) -> (Option<Rect>, Option<Rect>, Option<Rect>) {
    if window.style & WS_CAPTION != WS_CAPTION || window.style & WS_SYSMENU == 0 {
        return (None, None, None);
    }
    let frame = frame_width(window.style, window.ex_style);
    let tool = window.ex_style & WS_EX_TOOLWINDOW != 0;
    // Flat buttons as tall as the caption, side by side at its right.
    let (bw, bh) = if tool { (18, 14) } else { (26, CAPTION_HEIGHT - 1) };
    let top = frame;
    let close = Rect::sized(window.width - frame - bw, top, bw, bh);
    if tool || window.style & (WS_MAXIMIZEBOX | WS_MINIMIZEBOX) == 0 {
        return (Some(close), None, None);
    }
    let max = Rect::sized(close.left - bw, top, bw, bh);
    let min = Rect::sized(max.left - bw, top, bw, bh);
    (Some(close), Some(max), Some(min))
}

/// Default non-client hit test in window coordinates.
pub fn nc_hit_test(window: &GuestWindow, x: i32, y: i32) -> i32 {
    if x < 0 || y < 0 || x >= window.width || y >= window.height {
        return HTNOWHERE;
    }
    if let Some((l, t, r, b)) = window.nc_override {
        // The program drew its own frame: outside its client area is
        // border, or caption above it.
        if x >= l && y >= t && x < window.width - r && y < window.height - b {
            return HTCLIENT;
        }
        return if y < t && x >= l && x < window.width - r { HTCAPTION } else { HTBORDER };
    }
    let frame = frame_width(window.style, window.ex_style);
    if window.style & WS_THICKFRAME != 0 && !window.maximized {
        let corner = 16;
        let on_left = x < frame;
        let on_right = x >= window.width - frame;
        let on_top = y < frame;
        let on_bottom = y >= window.height - frame;
        if on_top || on_bottom || on_left || on_right {
            let near_left = x < corner;
            let near_right = x >= window.width - corner;
            let near_top = y < corner;
            let near_bottom = y >= window.height - corner;
            return match (
                on_top || (near_top && (on_left || on_right)),
                on_bottom || (near_bottom && (on_left || on_right)),
                on_left || (near_left && (on_top || on_bottom)),
                on_right || (near_right && (on_top || on_bottom)),
            ) {
                (true, _, true, _) => HTTOPLEFT,
                (true, _, _, true) => HTTOPRIGHT,
                (_, true, true, _) => HTBOTTOMLEFT,
                (_, true, _, true) => HTBOTTOMRIGHT,
                (true, _, _, _) => HTTOP,
                (_, true, _, _) => HTBOTTOM,
                (_, _, true, _) => HTLEFT,
                _ => HTRIGHT,
            };
        }
    } else if x < frame || y < frame || x >= window.width - frame || y >= window.height - frame {
        return HTBORDER;
    }
    let mut top = frame;
    if window.style & WS_CAPTION == WS_CAPTION {
        let caption = if window.ex_style & WS_EX_TOOLWINDOW != 0 {
            15
        } else {
            CAPTION_HEIGHT
        };
        if y < top + caption {
            let (close, max, min) = caption_buttons(window);
            if close.is_some_and(|r| r.contains(x, y)) {
                return HTCLOSE;
            }
            if max.is_some_and(|r| r.contains(x, y)) {
                return HTMAXBUTTON;
            }
            if min.is_some_and(|r| r.contains(x, y)) {
                return HTMINBUTTON;
            }
            if window.style & WS_SYSMENU != 0
                && x < frame + 18
                && window.ex_style & WS_EX_TOOLWINDOW == 0
            {
                return HTSYSMENU;
            }
            return HTCAPTION;
        }
        top += caption;
    }
    if has_menu_bar(window) {
        if y < top + MENU_HEIGHT {
            return HTMENU;
        }
    }
    let (l, t, r, b) = nc_insets(window);
    if scroll_style(window) & WS_VSCROLL != 0
        && x >= window.width - r
        && x < window.width - r + SCROLL_SIZE
        && y >= t
        && y < window.height - b
    {
        return HTVSCROLL;
    }
    if scroll_style(window) & WS_HSCROLL != 0
        && y >= window.height - b
        && y < window.height - b + SCROLL_SIZE
        && x >= l
        && x < window.width - r
    {
        return HTHSCROLL;
    }
    if x >= l && y >= t && x < window.width - r && y < window.height - b {
        return HTCLIENT;
    }
    HTBORDER
}

// ── Painting helpers ─────────────────────────────────────────────────────

/// Draw directly into the area of a window (bypassing guest DCs). Used for
/// non-client frames, menus and the built-in controls.
pub struct Painter<'a> {
    pub surface: &'a mut Surface,
    pub clip: Region,
    pub ox: i32,
    pub oy: i32,
}

impl Painter<'_> {
    fn target(&mut self) -> Target<'_> {
        Target {
            surface: self.surface,
            clip: &self.clip,
        }
    }

    pub fn fill(&mut self, rect: Rect, color: Rgb) {
        let rect = rect.offset(self.ox, self.oy);
        let mono = gfx::MonoColors {
            foreground: 0,
            background: 0xFFFFFF,
        };
        self.target().fill_rect(
            rect,
            &gfx::Fill::Solid(color),
            gfx::R2_COPYPEN,
            (0, 0),
            mono,
        );
    }

    pub fn pixel(&mut self, x: i32, y: i32, color: Rgb) {
        let (ox, oy) = (self.ox, self.oy);
        self.target().plot(x + ox, y + oy, color, gfx::R2_COPYPEN);
    }

    pub fn hline(&mut self, x0: i32, x1: i32, y: i32, color: Rgb) {
        self.fill(Rect::new(x0, y, x1, y + 1), color);
    }

    pub fn vline(&mut self, x: i32, y0: i32, y1: i32, color: Rgb) {
        self.fill(Rect::new(x, y0, x + 1, y1), color);
    }

    /// Two-pixel 3D edge: outer top-left, inner top-left, outer
    /// bottom-right, inner bottom-right colours.
    pub fn edge(&mut self, rect: Rect, tl_outer: Rgb, tl_inner: Rgb, br_outer: Rgb, br_inner: Rgb) {
        let Rect {
            left,
            top,
            right,
            bottom,
        } = rect;
        self.hline(left, right - 1, top, tl_outer);
        self.vline(left, top, bottom - 1, tl_outer);
        self.hline(left, right, bottom - 1, br_outer);
        self.vline(right - 1, top, bottom, br_outer);
        self.hline(left + 1, right - 2, top + 1, tl_inner);
        self.vline(left + 1, top + 1, bottom - 2, tl_inner);
        self.hline(left + 1, right - 1, bottom - 2, br_inner);
        self.vline(right - 2, top + 1, bottom - 1, br_inner);
    }

    /// A flat one-pixel outline in `color` just inside `rect`.
    pub fn outline(&mut self, rect: Rect, color: Rgb) {
        crate::win32::theme::outline(self, rect, color);
    }

    /// The edge of a panel. Flat: a one-pixel outline.
    pub fn raised(&mut self, rect: Rect) {
        self.outline(rect, sys_color(16));
    }

    /// The edge of a button. Flat: a one-pixel outline.
    pub fn raised_button(&mut self, rect: Rect) {
        self.outline(rect, sys_color(16));
    }

    /// The edge of a field. Flat: a one-pixel outline, darker than a
    /// button's.
    pub fn sunken(&mut self, rect: Rect) {
        self.outline(rect, 0x7A7A7A);
    }

    pub fn text(&mut self, x: i32, y: i32, text: &[u8], color: Rgb, bold: bool) -> i32 {
        let face = gfx::pick_face(13, bold);
        let (ox, oy) = (self.ox, self.oy);
        let mut target = self.target();
        gfx::draw_text(&mut target, face, x + ox, y + oy, text, color, false)
    }

    /// Text with `&` mnemonics underlined.
    pub fn mnemonic_text(&mut self, x: i32, y: i32, text: &[u8], color: Rgb) -> i32 {
        let face = gfx::pick_face(13, false);
        let mut pen = x;
        let mut index = 0;
        while index < text.len() {
            let byte = text[index];
            if byte == b'&' && index + 1 < text.len() {
                index += 1;
                let ch = text[index];
                let w = face.advance(ch);
                if ch != b'&' {
                    self.hline(pen, pen + w, y + face.ascent + 1, color);
                }
                self.text(pen, y, &[ch], color, false);
                pen += w;
            } else {
                pen += self.text(pen, y, &[byte], color, false);
            }
            index += 1;
        }
        pen - x
    }

    /// Disabled (greyed) text: flat, in the grey-text colour.
    pub fn disabled_text(&mut self, x: i32, y: i32, text: &[u8]) {
        self.mnemonic_text(x, y, text, sys_color(17));
    }
}

pub fn text_width(text: &[u8]) -> i32 {
    let face = gfx::pick_face(13, false);
    text.iter()
        .filter(|b| **b != b'&')
        .map(|b| face.advance(*b))
        .sum()
}

/// Run `paint` with a painter on `hwnd` (client or whole window area).
pub fn with_painter<R>(
    shared: &mut SharedState,
    hwnd: u32,
    client: bool,
    paint: impl FnOnce(&mut Painter<'_>) -> R,
) -> Option<R> {
    let (top, ox, oy, clip) = dc_geometry(&shared.gui, hwnd, client)?;
    ensure_surface(shared, top);
    let mut surface = shared.gdi.surfaces.remove(&top)?;
    let result = {
        let mut painter = Painter {
            surface: &mut surface,
            clip,
            ox,
            oy,
        };
        paint(&mut painter)
    };
    shared.gdi.surfaces.insert(top, surface);
    shared.gui.dirty = true;
    Some(result)
}

/// Make sure a top-level window's backing surface matches its size.
pub fn ensure_surface(shared: &mut SharedState, top: u32) {
    let Some(window) = shared.gui.window(top) else {
        return;
    };
    let (w, h) = (window.width.max(1), window.height.max(1));
    match shared.gdi.surfaces.get_mut(&top) {
        Some(surface) => {
            if surface.width != w || surface.height != h {
                surface.resize(w, h, sys_color(15));
                shared.gui.dirty = true;
            }
        }
        None => {
            let mut surface = Surface::new(w, h, 24);
            surface.pixels.fill(sys_color(15));
            shared.gdi.surfaces.insert(top, surface);
            shared.gui.dirty = true;
        }
    }
}

/// Draw the non-client area: frame, caption, menu bar, edges and scroll bars.
pub fn paint_nc(shared: &mut SharedState, hwnd: u32) {
    let Some(window) = shared.gui.window(hwnd).cloned() else {
        return;
    };
    let active = {
        let top = shared.gui.top_level_for(hwnd).unwrap_or(hwnd);
        shared.gui.active_hwnd == top
            || shared
                .gui
                .windows
                .iter()
                .any(|w| w.hwnd == shared.gui.active_hwnd && w.owner == top)
    };
    let menu_items = if has_menu_bar(&window) {
        shared
            .gui
            .menus
            .get(&window.menu)
            .map(|menu| menu.items.clone())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let hot = shared
        .gui
        .menu_bar_hot
        .filter(|(h, _)| *h == hwnd)
        .map(|(_, i)| i);
    // The caption shows the window class's icon.
    let class_icon = shared
        .gui
        .classes
        .iter()
        .find(|class| class.name.eq_ignore_ascii_case(&window.class))
        .and_then(|class| shared.gui.small_icon(class.icon))
        .filter(|icon| icon.width == 16 && icon.height == 16)
        .cloned();
    if window.nc_override.is_some() {
        // The program draws its own frame.
        return;
    }
    with_painter(shared, hwnd, false, |p| {
        let (w, h) = (window.width, window.height);
        let frame = frame_width(window.style, window.ex_style);
        let mut y = 0;
        if frame >= 3 {
            // Flat and dark: a one-pixel outline (the accent while the
            // window is in use), the rest of the frame the caption's colour.
            let fill = crate::win32::theme::caption(active);
            for inset in 1..frame {
                p.outline(Rect::new(inset, inset, w - inset, h - inset), fill);
            }
            p.outline(
                Rect::new(0, 0, w, h),
                crate::win32::theme::frame_outline(active),
            );
            y = frame;
        } else if frame == 1 {
            p.hline(0, w, 0, sys_color(6));
            p.hline(0, w, h - 1, sys_color(6));
            p.vline(0, 0, h, sys_color(6));
            p.vline(w - 1, 0, h, sys_color(6));
            y = 1;
        }
        if window.style & WS_CAPTION == WS_CAPTION {
            let tool = window.ex_style & WS_EX_TOOLWINDOW != 0;
            let caption = if tool { 15 } else { CAPTION_HEIGHT };
            let bar = Rect::new(frame, y, w - frame, y + caption);
            p.fill(bar, if active { sys_color(2) } else { sys_color(3) });
            let text_color = if active { sys_color(9) } else { sys_color(19) };
            let mut text_x = frame + 3;
            if window.style & WS_SYSMENU != 0 && !tool {
                match &class_icon {
                    Some(icon) => {
                        for (index, pixel) in icon.pixels.iter().enumerate() {
                            if let Some(rgb) = pixel {
                                let (ix, iy) =
                                    (index as i32 % icon.width, index as i32 / icon.width);
                                p.pixel(frame + 2 + ix, y + 1 + iy, *rgb);
                            }
                        }
                    }
                    None => draw_small_icon(p, frame + 2, y + 1),
                }
                text_x = frame + 20;
            }
            let (close, max, min) = caption_buttons(&window);
            let limit = min
                .or(max)
                .or(close)
                .map(|r| r.left - 2)
                .unwrap_or(w - frame);
            let title = &crate::win32::unicode::string_to_ansi(&window.title)[..];
            let face = gfx::pick_face(13, false);
            let mut shown = title.to_vec();
            while !shown.is_empty() && text_x + face.text_width(&shown) > limit {
                shown.pop();
            }
            if shown.len() < title.len() && shown.len() > 3 {
                shown.truncate(shown.len() - 3);
                shown.extend_from_slice(b"...");
            }
            p.text(
                text_x,
                y + (caption - face.height) / 2,
                &shown,
                text_color,
                false,
            );
            for (rect, glyph) in [
                (close, 0u8),
                (max, if window.maximized { 3 } else { 1 }),
                (min, 2),
            ] {
                if let Some(rect) = rect {
                    crate::win32::theme::caption_button(p, rect, glyph, active, false);
                }
            }
            y += caption;
        }
        if has_menu_bar(&window) {
            let bar = Rect::new(frame, y, w - frame, y + MENU_HEIGHT);
            p.fill(bar, sys_color(4));
            let mut x = frame + 6;
            for (index, item) in menu_items.iter().enumerate() {
                let label = item
                    .text
                    .split(|b| *b == b'\t')
                    .next()
                    .unwrap_or(&[])
                    .to_vec();
                let width = text_width(&label) + 12;
                let item_rect = Rect::new(x - 6, y + 1, x - 6 + width, y + MENU_HEIGHT - 1);
                let disabled = item.flags & 0x3 != 0;
                if hot == Some(index as i32) {
                    crate::win32::theme::rounded(p, item_rect, 2, crate::win32::theme::MENU_HOT);
                    p.mnemonic_text(x, y + 3, &label, sys_color(7));
                } else if disabled {
                    p.mnemonic_text(x, y + 3, &label, crate::win32::theme::TEXT_DISABLED);
                } else {
                    p.mnemonic_text(x, y + 3, &label, sys_color(7));
                }
                x += width;
            }
            p.hline(frame, w - frame, y + MENU_HEIGHT - 1, crate::win32::theme::BORDER);
            y += MENU_HEIGHT;
        }
        let edge_rect = Rect::new(frame, y, w - frame, h - frame);
        if window.ex_style & WS_EX_CLIENTEDGE != 0 {
            // Two pixels: a top-level window's in its frame's colour; a
            // control's a grey line and its own (window) colour inside.
            let (outer, inner) = if window.style & WS_CHILD == 0 {
                let fill = crate::win32::theme::caption(active);
                (fill, fill)
            } else {
                (0x7A7A7A, sys_color(5))
            };
            p.outline(edge_rect, outer);
            let inside = edge_rect.inflate(-1, -1);
            p.outline(inside, inner);
            // Scroll bars run up to the grey line.
            let style = scroll_style(&window);
            if style & WS_VSCROLL != 0 {
                p.vline(inside.right - 1, inside.top, inside.bottom, crate::win32::theme::SCROLL_TRACK);
            }
            if style & WS_HSCROLL != 0 {
                p.hline(inside.left, inside.right, inside.bottom - 1, crate::win32::theme::SCROLL_TRACK);
            }
        } else if window.ex_style & WS_EX_STATICEDGE != 0 {
            p.hline(
                edge_rect.left,
                edge_rect.right,
                edge_rect.top,
                sys_color(16),
            );
            p.vline(
                edge_rect.left,
                edge_rect.top,
                edge_rect.bottom,
                sys_color(16),
            );
            p.hline(
                edge_rect.left,
                edge_rect.right,
                edge_rect.bottom - 1,
                sys_color(20),
            );
            p.vline(
                edge_rect.right - 1,
                edge_rect.top,
                edge_rect.bottom,
                sys_color(20),
            );
        }
        let (l, t, r, b) = nc_insets(&window);
        let style = scroll_style(&window);
        if style & WS_VSCROLL != 0 {
            let rect = Rect::new(w - r, t, w - r + SCROLL_SIZE, h - b);
            let (min, max, pos, page) = window.scroll[1];
            draw_scroll_bar(p, rect, true, min, max, pos, page);
        }
        if style & WS_HSCROLL != 0 {
            let rect = Rect::new(l, h - b, w - r, h - b + SCROLL_SIZE);
            let (min, max, pos, page) = window.scroll[0];
            draw_scroll_bar(p, rect, false, min, max, pos, page);
        }
        if style & WS_VSCROLL != 0 && style & WS_HSCROLL != 0 {
            p.fill(
                Rect::new(w - r, h - b, w - r + SCROLL_SIZE, h - b + SCROLL_SIZE),
                crate::win32::theme::SCROLL_TRACK,
            );
        }
    });
}

fn draw_small_icon(p: &mut Painter<'_>, x: i32, y: i32) {
    // A generic application glyph: a window outline with an accent bar.
    let window = Rect::sized(x + 2, y + 3, 12, 10);
    p.outline(window, crate::win32::theme::TEXT_DIM);
    p.fill(
        Rect::sized(window.left, window.top, window.width(), 3),
        crate::win32::theme::ACCENT,
    );
}

/// Draw a caption button of an active window. Glyphs: 0 close,
/// 1 maximize, 2 minimize, 3 restore.
pub fn caption_button(p: &mut Painter<'_>, rect: Rect, glyph: u8) {
    crate::win32::theme::caption_button(p, rect, glyph, true, false);
}

/// Scroll bar geometry: (arrow size, thumb start, thumb size) along the axis.
pub fn scroll_thumb(length: i32, min: i32, max: i32, pos: i32, page: u32) -> Option<(i32, i32)> {
    let arrow = SCROLL_SIZE.min(length / 2);
    let track = length - 2 * arrow;
    let range = max - min + 1;
    if max <= min || track < 8 {
        return None;
    }
    let size = if page == 0 {
        SCROLL_SIZE.min(track)
    } else {
        ((track as i64 * page as i64 / range.max(1) as i64) as i32).clamp(8, track)
    };
    let span = (range - page.max(1) as i32 + if page == 0 { 0 } else { 1 }).max(1);
    let offset =
        ((pos - min).clamp(0, span) as i64 * (track - size) as i64 / span.max(1) as i64) as i32;
    Some((arrow + offset, size))
}

pub fn draw_scroll_bar(
    p: &mut Painter<'_>,
    rect: Rect,
    vertical: bool,
    min: i32,
    max: i32,
    pos: i32,
    page: u32,
) {
    let length = if vertical {
        rect.height()
    } else {
        rect.width()
    };
    let arrow_size = SCROLL_SIZE.min(length / 2);
    crate::win32::theme::scroll_bar(
        p,
        rect,
        vertical,
        arrow_size,
        scroll_thumb(length, min, max, pos, page),
        max > min,
    );
}

// ── Messaging ────────────────────────────────────────────────────────────

/// Call a window procedure. Rust-backed procedures are dispatched directly;
/// everything else runs as guest code.
pub fn call_wndproc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    proc_va: u32,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    if proc_va == 0 {
        return 0;
    }
    if let Some(name) = ctx.procs.get(&proc_va).cloned() {
        let args = [hwnd, msg, wparam, lparam];
        return builtin_proc(ctx, memory, &name, &args);
    }
    crate::win32::unicode::call_proc(ctx, memory, proc_va, hwnd, msg, wparam, lparam).unwrap_or(0)
}

fn print_client<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, hwnd: u32, target: u32) {
    let Some((width, height)) = ctx.shared.borrow().gui.window(hwnd).map(client_size) else { return };
    {
        let mut shared = ctx.shared.borrow_mut();
        invalidate(&mut shared.gui, hwnd, None, true, false);
    }
    builtin_proc(ctx, memory, name, &[hwnd, WM_PAINT, 0, 0]);
    let call = |memory: &mut M, dll: &str, function: &str, args: &[u32]| {
        crate::win32::dispatch_with_memory(ctx, memory, dll, &crate::kernel::pe::ImportName::ByName(function.into()), args, 0)
            .map(|o| o.value)
            .unwrap_or(0)
    };
    let dc = call(memory, "USER32.dll", "GetDC", &[hwnd]);
    if dc != 0 {
        call(memory, "GDI32.dll", "BitBlt", &[target, 0, 0, width as u32, height as u32, dc, 0, 0, 0x00CC_0020]);
        call(memory, "USER32.dll", "ReleaseDC", &[hwnd, dc]);
    }
}

fn builtin_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
) -> u32 {
    let (hwnd, msg, wparam, lparam) = (args[0], args[1], args[2], args[3]);
    // WM_PRINT / WM_PRINTCLIENT to a built-in control: paint it, then copy
    // its client area into the caller's DC (double-buffered painting).
    if matches!(msg, 0x0317 | 0x0318)
        && name.starts_with("OTW_")
        && !matches!(name, "OTW_SubclassProc" | "OTW_MenuProc")
        && (msg == 0x0318 || lparam & 0x4 != 0)
    {
        print_client(ctx, memory, name, hwnd, wparam);
        return 0;
    }
    match name {
        "DefWindowProcA" | "DefMDIChildProcA" => {
            def_window_proc(ctx, memory, hwnd, msg, wparam, lparam)
        }
        "DefFrameProcA" => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "DefDlgProcA" => {
            crate::win32::controls::def_dlg_proc(ctx, memory, hwnd, msg, wparam, lparam)
        }
        "OTW_MenuProc" => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_PropSheetProc" => crate::win32::comctl32::sheet_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_TabProc" => crate::win32::comctl32::tab_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_ListViewProc" => crate::win32::comctl32::listview_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_ComboBoxExProc" => crate::win32::controls::combo_ex_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_ProgressProc" => crate::win32::controls::progress_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_TreeViewProc" => crate::win32::treeview::treeview_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_RebarProc" => crate::win32::rebar::rebar_proc(ctx, memory, hwnd, msg, wparam, lparam),
        // Tooltips: tools are accepted and never shown.
        "OTW_TooltipProc" => match msg {
            0x0404 | 0x0432 => 1,     // TTM_ADDTOOLA/W
            0x0400..=0x04FF => 0,     // the other TTM_* messages
            _ => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
        },
        "OTW_SubclassProc" => crate::win32::commctrl::subclass_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_ToolbarProc" => crate::win32::commctrl::toolbar_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_StatusProc" => crate::win32::commctrl::status_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_MDIClientProc" => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
        other => {
            crate::win32::controls::control_proc(ctx, memory, other, hwnd, msg, wparam, lparam)
        }
    }
}

/// Synchronously send a message to a window.
pub fn send<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    if hwnd == 0xFFFF {
        let tops: Vec<u32> = {
            let shared = ctx.shared.borrow();
            shared
                .gui
                .windows
                .iter()
                .filter(|w| w.parent == 0 || w.style & WS_CHILD == 0)
                .map(|w| w.hwnd)
                .collect()
        };
        for top in tops {
            send(ctx, memory, top, msg, wparam, lparam);
        }
        return 1;
    }
    let proc_va = ctx.shared.borrow().gui.wndproc_for(hwnd);
    if std::env::var_os("OTW_TRACE_MSG").is_some() {
        eprintln!("[msg] send {hwnd:#x} {msg:#06x} w={wparam:#x} l={lparam:#x} proc={proc_va:x?}");
    }
    match proc_va {
        Some(proc_va) => call_wndproc(ctx, memory, proc_va, hwnd, msg, wparam, lparam),
        None => 0,
    }
}

pub fn post(ctx: &ApiContext, hwnd: u32, msg: u32, wparam: u32, lparam: u32) {
    ctx.shared.borrow_mut().gui.posted.push_back(GuestMessage {
        hwnd,
        message: msg,
        wparam,
        lparam,
    });
    crate::win32::sync::wake_key(ctx, crate::win32::kobj::KEY_MESSAGES);
}

/// Call the hook chain of `kind` starting after `after` (0 = from the top).
fn call_hooks<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    kind: i32,
    after: u32,
    code: u32,
    wparam: u32,
    lparam: u32,
) -> Option<u32> {
    let hook = {
        let shared = ctx.shared.borrow();
        let hooks: Vec<GuestHook> = shared
            .gui
            .hooks
            .iter()
            .rev()
            .filter(|h| h.kind == kind)
            .copied()
            .collect();
        let start = if after == 0 {
            0
        } else {
            hooks
                .iter()
                .position(|h| h.handle == after)
                .map(|p| p + 1)
                .unwrap_or(hooks.len())
        };
        hooks.get(start).copied()
    };
    let hook = hook?;
    if ctx.shared.borrow().x64 {
        return crate::win32::x64::call_hook(ctx, memory, hook.proc_va, kind, code, wparam, lparam);
    }
    memory.call_guest(hook.proc_va, &[code, wparam, lparam])
}

// ── Invalidation ─────────────────────────────────────────────────────────

/// Add `rect` (client coordinates, `None` = whole client) to the update
/// region. Children overlapping the area are invalidated too unless the
/// window clips its children.
pub fn invalidate(
    gui: &mut GuiState,
    hwnd: u32,
    rect: Option<Rect>,
    erase: bool,
    all_children: bool,
) {
    let Some(window) = gui.window(hwnd) else {
        return;
    };
    let (cw, ch) = client_size(window);
    let client = Rect::new(0, 0, cw, ch);
    let area = rect
        .map(|r| r.normalized().intersect(&client))
        .unwrap_or(client);
    let clip_children = window.style & WS_CLIPCHILDREN != 0;
    if let Some(window) = gui.window_mut(hwnd) {
        if !area.is_empty() {
            window.update = window.update.union_rect(&area);
            window.erase |= erase;
        }
        if rect.is_none() {
            window.nc_dirty = true;
        }
    }
    gui.dirty = true;
    if area.is_empty() {
        return;
    }
    if all_children || !clip_children {
        for child in children(gui, hwnd) {
            let Some(child_window) = gui.window(child) else {
                continue;
            };
            if !child_window.visible {
                continue;
            }
            let child_rect = Rect::sized(
                child_window.x,
                child_window.y,
                child_window.width,
                child_window.height,
            );
            let overlap = area.intersect(&child_rect);
            if overlap.is_empty() {
                continue;
            }
            let (l, t, _, _) = nc_insets(child_window);
            let local = overlap.offset(-child_window.x - l, -child_window.y - t);
            if let Some(child_window) = gui.window_mut(child) {
                child_window.nc_dirty = true;
            }
            // A parent that paints over its children invalidates the
            // whole overlapping subtree, whatever the children clip.
            invalidate(gui, child, Some(local), erase, true);
        }
    }
}

/// Invalidate a window completely, including its frame and descendants.
pub fn invalidate_all(gui: &mut GuiState, hwnd: u32) {
    if let Some(window) = gui.window_mut(hwnd) {
        window.nc_dirty = true;
    }
    invalidate(gui, hwnd, None, true, true);
}

/// Invalidate the part of `parent`'s client area covered by `rect`
/// (parent client coordinates), e.g. after a child moved away.
fn invalidate_parent_area(gui: &mut GuiState, parent: u32, rect: Rect) {
    if parent == 0 {
        return;
    }
    invalidate(gui, parent, Some(rect), true, false);
}

fn validate(gui: &mut GuiState, hwnd: u32, rect: Option<Rect>) {
    if let Some(window) = gui.window_mut(hwnd) {
        match rect {
            Some(rect) => window.update = window.update.subtract_rect(&rect.normalized()),
            None => {
                window.update = Region::empty();
                window.erase = false;
            }
        }
    }
}

/// Windows in painting order: top-level windows bottom to top, each followed
/// by its descendants (parents before children).
fn paint_order(gui: &GuiState) -> Vec<u32> {
    fn walk(gui: &GuiState, hwnd: u32, out: &mut Vec<u32>) {
        out.push(hwnd);
        for child in children(gui, hwnd) {
            walk(gui, child, out);
        }
    }
    let mut out = Vec::new();
    for window in &gui.windows {
        if window.parent == 0 || window.style & WS_CHILD == 0 {
            walk(gui, window.hwnd, &mut out);
        }
    }
    out
}

/// Paint pending non-client areas and pick the next window that needs
/// WM_PAINT (optionally restricted to `filter` and its descendants).
fn next_paint(shared: &mut SharedState, filter: u32, thread: u32) -> Option<u32> {
    let order = paint_order(&shared.gui);
    for hwnd in order {
        if !is_visible_chain(&shared.gui, hwnd) || !owned_by(&shared.gui, hwnd, thread) {
            continue;
        }
        if filter != 0 && hwnd != filter && !is_descendant(&shared.gui, hwnd, filter) {
            continue;
        }
        let (nc_dirty, needs_paint) = match shared.gui.window(hwnd) {
            Some(w) => (w.nc_dirty, !w.update.is_empty()),
            None => continue,
        };
        if nc_dirty {
            if let Some(w) = shared.gui.window_mut(hwnd) {
                w.nc_dirty = false;
            }
            paint_nc(shared, hwnd);
        }
        if needs_paint {
            return Some(hwnd);
        }
    }
    None
}

pub fn is_descendant(gui: &GuiState, hwnd: u32, ancestor: u32) -> bool {
    let mut current = hwnd;
    for _ in 0..32 {
        let Some(window) = gui.window(current) else {
            return false;
        };
        if window.parent == 0 || window.style & WS_CHILD == 0 {
            return false;
        }
        if window.parent == ancestor {
            return true;
        }
        current = window.parent;
    }
    false
}

// ── Window lifecycle ─────────────────────────────────────────────────────

/// Register the built-in control classes once per process.
/// LOGFONTA of the system's MS Sans Serif 8 at `weight`.
fn system_logfont(weight: u32) -> Vec<u8> {
    let mut font = vec![0u8; 60];
    font[0..4].copy_from_slice(&(-11i32).to_le_bytes());
    font[16..20].copy_from_slice(&weight.to_le_bytes());
    font[28..41].copy_from_slice(b"MS Sans Serif");
    font
}

/// The common controls COMCTL32 provides; a genuine COMCTL32 registers
/// its own.
pub const COMMON_CONTROL_CLASSES: [&str; 9] = [
    "msctls_progress32",
    "ComboBoxEx32",
    "ToolbarWindow32",
    "msctls_statusbar32",
    "SysListView32",
    "SysTabControl32",
    "SysTreeView32",
    "tooltips_class32",
    "ReBarWindow32",
];

fn has_genuine_comctl32(ctx: &ApiContext) -> bool {
    ctx.modules
        .borrow()
        .iter()
        .any(|m| m.name.eq_ignore_ascii_case("comctl32.dll"))
}

/// A genuine COMCTL32 is being loaded: drop the subsystem's versions of its
/// classes so that it registers its own.
pub fn yield_common_controls(ctx: &ApiContext) {
    ctx.shared
        .borrow_mut()
        .gui
        .classes
        .retain(|c| !COMMON_CONTROL_CLASSES.iter().any(|n| c.name.eq_ignore_ascii_case(n)));
}

pub fn ensure_builtin_classes(ctx: &ApiContext) {
    let mut shared = ctx.shared.borrow_mut();
    if shared
        .gui
        .classes
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case("Button"))
    {
        return;
    }
    let builtin = |name: &str| ctx.builtin_proc(name);
    let classes: [(&str, &str, u32, u32); 21] = [
        ("msctls_progress32", "OTW_ProgressProc", 0x08, 16),
        ("ComboBoxEx32", "OTW_ComboBoxExProc", 0x08, 6),
        ("SysTreeView32", "OTW_TreeViewProc", 0x08, 0),
        ("ReBarWindow32", "OTW_RebarProc", 0x08, 0),
        ("tooltips_class32", "OTW_TooltipProc", 0x08, 0),
        ("RICHEDIT", "OTW_RichEditProc", 0x08, 6),
        ("OTW_ColorPanel", "OTW_ColorPanelProc", 0x08, 0),
        ("ToolbarWindow32", "OTW_ToolbarProc", 0x08, 0),
        ("msctls_statusbar32", "OTW_StatusProc", 0x08, 0),
        ("SysListView32", "OTW_ListViewProc", 0x08, 0),
        ("SysTabControl32", "OTW_TabProc", 0x08, 0),
        ("Button", "OTW_ButtonProc", 0x0B, 16),
        ("Edit", "OTW_EditProc", 0x08, 6),
        ("Static", "OTW_StaticProc", 0x08, 16),
        ("ListBox", "OTW_ListBoxProc", 0x08, 6),
        ("ComboBox", "OTW_ComboBoxProc", 0x08, 6),
        ("ComboLBox", "OTW_ListBoxProc", 0x08, 6),
        ("ScrollBar", "OTW_ScrollBarProc", 0x0B, 6),
        ("MDIClient", "OTW_MDIClientProc", 0, 0),
        ("#32770", "DefDlgProcA", 0x08, 30),
        ("#32768", "OTW_MenuProc", 0x08, 0),
    ];
    let genuine_comctl32 = has_genuine_comctl32(ctx);
    for (name, proc_name, style, extra) in classes {
        if genuine_comctl32 && COMMON_CONTROL_CLASSES.contains(&name) {
            continue;
        }
        let atom = 0xC000 + shared.gui.classes.len() as u32;
        shared.gui.classes.push(GuestClass {
            name: name.to_owned(),
            wndproc: builtin(proc_name),
            style,
            wnd_extra: extra,
            background: if name == "#32770" {
                SYSBRUSH_BASE + 15 * 4
            } else {
                0
            },
            atom,
            ..GuestClass::default()
        });
    }
}

fn find_class<'a>(gui: &'a GuiState, class_va_or_name: &str) -> Option<&'a GuestClass> {
    gui.classes
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(class_va_or_name))
}

fn class_name_from<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, va: u32) -> String {
    if va < 0x10000 {
        let shared = ctx.shared.borrow();
        if let Some(class) = shared.gui.classes.iter().find(|c| c.atom == va) {
            return class.name.clone();
        }
        return match va {
            0x80 => "Button".into(),
            0x81 => "Edit".into(),
            0x82 => "Static".into(),
            0x83 => "ListBox".into(),
            0x84 => "ScrollBar".into(),
            0x85 => "ComboBox".into(),
            0x8002 => "#32770".into(),
            _ => format!("#{va}"),
        };
    }
    String::from_utf8_lossy(&read_cstr(memory, va)).into_owned()
}

/// Parameters of `CreateWindowExA`.
#[derive(Clone, Debug, Default)]
pub struct CreateParams {
    pub ex_style: u32,
    pub class: String,
    pub class_va: u32,
    pub title: Vec<u8>,
    pub title_va: u32,
    pub style: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub parent: u32,
    pub menu: u32,
    pub instance: u32,
    pub param: u32,
}

/// Allocate a scratch block of guest memory for message structures.
pub fn scratch(ctx: &ApiContext, size: u32) -> u32 {
    ctx.shared.borrow_mut().heap.alloc(size).unwrap_or(0)
}

pub fn free_scratch(ctx: &ApiContext, va: u32) {
    if va != 0 {
        ctx.shared.borrow_mut().heap.free(va);
    }
}

pub fn create_window<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    params: CreateParams,
) -> u32 {
    ensure_builtin_classes(ctx);
    let CreateParams {
        ex_style,
        class,
        style,
        parent,
        ..
    } = params.clone();
    let (class_record, screen) = {
        let shared = ctx.shared.borrow();
        (find_class(&shared.gui, &class).cloned(), shared.gui.screen)
    };
    let Some(class_record) = class_record else {
        eprintln!("[user] CreateWindowExA: unknown class {class:?}");
        ctx.shared.borrow_mut().last_error = 1407; // ERROR_CANNOT_FIND_WND_CLASS
        return 0;
    };
    let child = style & WS_CHILD != 0;
    if child && parent == 0 {
        return 0;
    }
    let (mut x, mut y, mut width, mut height) = (params.x, params.y, params.width, params.height);
    if let Some(closed) = closed_combo_height(&class_record.name, style) {
        height = closed;
    }
    if !child {
        let cascade = {
            let shared = ctx.shared.borrow();
            shared
                .gui
                .windows
                .iter()
                .filter(|w| w.parent == 0 && w.style & WS_CAPTION != 0)
                .count() as i32
                % 8
        };
        if x == CW_USEDEFAULT {
            x = 24 + cascade * 26;
            y = 16 + cascade * 26;
        }
        if width == CW_USEDEFAULT {
            width = (screen.0 * 3 / 4).min(800);
            height = ((screen.1 - TASKBAR_HEIGHT) * 3 / 4).min(600);
        }
        if y == CW_USEDEFAULT {
            y = 16 + cascade * 26;
        }
    } else {
        if x == CW_USEDEFAULT {
            x = 0;
            y = 0;
        }
        if width == CW_USEDEFAULT {
            width = 0;
            height = 0;
        }
    }
    let menu_handle = if !child && params.menu == 0 && class_record.menu_name != 0 {
        crate::win32::menu::load_menu(ctx, memory, class_record.instance, class_record.menu_name)
    } else if !child {
        params.menu
    } else {
        0
    };
    let (owner, parent_hwnd) = if child {
        (0, parent)
    } else {
        let owner = if parent != 0 {
            ctx.shared
                .borrow()
                .gui
                .top_level_for(parent)
                .unwrap_or(parent)
        } else {
            0
        };
        (owner, 0)
    };

    let hwnd = {
        let mut shared = ctx.shared.borrow_mut();
        let hwnd = shared.gui.next_hwnd;
        shared.gui.next_hwnd += 2;
        let wndproc = class_record.wndproc;
        let is_edit = class.eq_ignore_ascii_case("edit");
        if is_edit {
            shared.gui.edit_hwnd = hwnd;
        }
        let window = GuestWindow {
            hwnd,
            thread: memory.current_thread(),
            class: class_record.name.clone(),
            title: crate::win32::unicode::ansi_to_string(&params.title),
            parent: parent_hwnd,
            x,
            y,
            width: width.max(0),
            height: height.max(0),
            style: style & !(WS_VISIBLE | WS_MAXIMIZE | WS_MINIMIZE),
            ex_style,
            wndproc,
            owner,
            id: if child { params.menu } else { 0 },
            menu: menu_handle,
            instance: params.instance,
            extra: vec![0; class_record.wnd_extra.min(4096) as usize],
            disabled: style & WS_DISABLED != 0,
            nc_dirty: true,
            text: if is_edit {
                params.title.clone()
            } else {
                Vec::new()
            },
            ..GuestWindow::default()
        };
        if child {
            // New child windows go to the bottom of their siblings' z-order.
            let position = shared
                .gui
                .windows
                .iter()
                .position(|w| w.parent == parent_hwnd && w.style & WS_CHILD != 0)
                .unwrap_or(shared.gui.windows.len());
            shared.gui.windows.insert(position, window);
        } else {
            shared.gui.windows.push(window);
            if shared.gui.main_hwnd == 0 && style & WS_CAPTION == WS_CAPTION {
                shared.gui.main_hwnd = hwnd;
            }
        }
        if !child {
            ensure_surface(&mut shared, hwnd);
        }
        hwnd
    };

    // CREATESTRUCT in guest memory, in the process's layout, followed by
    // the CBT_CREATEWND that points to it.
    let cs = scratch(ctx, 112);
    let cs_view = crate::win32::x64::View::new(memory, &crate::win32::x64::CREATESTRUCT, cs);
    let fields = [
        params.param,
        params.instance,
        if child { params.menu } else { menu_handle },
        parent,
        height as u32,
        width as u32,
        y as u32,
        x as u32,
        style,
        params.title_va,
        params.class_va,
        ex_style,
    ];
    for (index, value) in fields.iter().enumerate() {
        cs_view.set(memory, index, *value);
    }

    // WH_CBT HCBT_CREATEWND lets MFC subclass the window before WM_NCCREATE.
    let cbt = cs + crate::win32::x64::CREATESTRUCT.size(memory.ptr_size() == 8);
    let cbt_view = crate::win32::x64::View::new(memory, &crate::win32::x64::CBT_CREATEWND, cbt);
    cbt_view.set(memory, 0, cs);
    cbt_view.set(memory, 1, 0);
    if let Some(result) = call_hooks(ctx, memory, 5, 0, 3, hwnd, cbt) {
        if result != 0 {
            destroy_window_record(ctx, hwnd);
            free_scratch(ctx, cs);
            return 0;
        }
    }
    if memory.stopping() {
        free_scratch(ctx, cs);
        return 0;
    }

    if send(ctx, memory, hwnd, WM_NCCREATE, 0, cs) == 0 {
        send(ctx, memory, hwnd, WM_NCDESTROY, 0, 0);
        destroy_window_record(ctx, hwnd);
        free_scratch(ctx, cs);
        return 0;
    }
    calc_nc_size(ctx, memory, hwnd);
    if send(ctx, memory, hwnd, WM_CREATE, 0, cs) as i32 == -1 {
        destroy_window(ctx, memory, hwnd);
        free_scratch(ctx, cs);
        return 0;
    }
    free_scratch(ctx, cs);
    if !is_window(ctx, hwnd) {
        return 0;
    }
    let (cw, ch) = {
        let shared = ctx.shared.borrow();
        shared.gui.window(hwnd).map(client_size).unwrap_or((0, 0))
    };
    send(ctx, memory, hwnd, WM_SIZE, 0, make_long(cw, ch));
    send(ctx, memory, hwnd, WM_MOVE, 0, make_long(x, y));
    if child && ex_style & WS_EX_NOPARENTNOTIFY == 0 {
        let id = params.menu;
        send(
            ctx,
            memory,
            parent,
            WM_PARENTNOTIFY,
            make_long(WM_CREATE as i32, id as i32),
            hwnd,
        );
    }
    if style & WS_MAXIMIZE != 0 {
        show_window(ctx, memory, hwnd, 3);
    } else if style & WS_VISIBLE != 0 {
        show_window(ctx, memory, hwnd, 5);
    }
    hwnd
}

pub fn is_window(ctx: &ApiContext, hwnd: u32) -> bool {
    hwnd == DESKTOP_HWND || ctx.shared.borrow().gui.window(hwnd).is_some()
}

fn destroy_window_record(ctx: &ApiContext, hwnd: u32) {
    let mut shared = ctx.shared.borrow_mut();
    shared.gui.windows.retain(|w| w.hwnd != hwnd);
    shared.gdi.surfaces.remove(&hwnd);
    shared.gui.timers.retain(|t| t.hwnd != hwnd);
    shared.gui.dirty = true;
}

pub fn destroy_window<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
) -> bool {
    let info = {
        let shared = ctx.shared.borrow();
        shared.gui.window(hwnd).map(|w| {
            (
                w.parent,
                w.style,
                Rect::sized(w.x, w.y, w.width, w.height),
                w.visible,
                w.id,
            )
        })
    };
    let Some((parent, style, rect, visible, id)) = info else {
        return false;
    };
    let child = style & WS_CHILD != 0;
    // Owned popups are destroyed first.
    let owned: Vec<u32> = {
        let shared = ctx.shared.borrow();
        shared
            .gui
            .windows
            .iter()
            .filter(|w| w.owner == hwnd && w.style & WS_CHILD == 0)
            .map(|w| w.hwnd)
            .collect()
    };
    for popup in owned {
        destroy_window(ctx, memory, popup);
    }
    if child {
        let ex = ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.ex_style)
            .unwrap_or(0);
        if ex & WS_EX_NOPARENTNOTIFY == 0 {
            send(
                ctx,
                memory,
                parent,
                WM_PARENTNOTIFY,
                make_long(WM_DESTROY as i32, id as i32),
                hwnd,
            );
        }
    }
    if visible {
        // Hide without activation changes for children; top-level windows
        // pass activation on below.
        if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
            window.visible = false;
        }
        if child {
            invalidate_parent_area(&mut ctx.shared.borrow_mut().gui, parent, rect);
        }
    }
    {
        let mut shared = ctx.shared.borrow_mut();
        if shared.gui.capture == hwnd {
            shared.gui.capture = 0;
        }
    }
    send(ctx, memory, hwnd, WM_DESTROY, 0, 0);
    // Destroy children bottom-up after the parent saw WM_DESTROY.
    let kids = children(&ctx.shared.borrow().gui, hwnd);
    for kid in kids {
        destroy_window(ctx, memory, kid);
    }
    send(ctx, memory, hwnd, WM_NCDESTROY, 0, 0);
    let (was_active, was_focus, is_main) = {
        let shared = ctx.shared.borrow();
        let focus_inside = shared.gui.focus_hwnd == hwnd
            || is_descendant(&shared.gui, shared.gui.focus_hwnd, hwnd);
        (
            shared.gui.active_hwnd == hwnd,
            focus_inside,
            shared.gui.main_hwnd == hwnd,
        )
    };
    destroy_window_record(ctx, hwnd);
    {
        let mut shared = ctx.shared.borrow_mut();
        if was_focus {
            shared.gui.focus_hwnd = 0;
        }
        if is_main {
            shared.gui.main_hwnd = 0;
        }
        shared.gui.dirty = true;
    }
    if was_active {
        ctx.shared.borrow_mut().gui.active_hwnd = 0;
        let next = {
            let shared = ctx.shared.borrow();
            let owner = shared.gui.window(hwnd).map(|w| w.owner).unwrap_or(0);
            shared
                .gui
                .windows
                .iter()
                .rev()
                .find(|w| {
                    (w.parent == 0 || w.style & WS_CHILD == 0)
                        && w.visible
                        && !w.minimized
                        && !w.disabled
                        && (owner == 0 || w.hwnd == owner)
                        && w.class != "#32768"
                })
                .map(|w| w.hwnd)
        };
        if let Some(next) = next {
            activate(ctx, memory, next, 1);
        }
    }
    true
}

/// `ShowWindow`. Returns whether the window was previously visible.
pub fn show_window<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    cmd: u32,
) -> bool {
    let info = {
        let shared = ctx.shared.borrow();
        shared.gui.window(hwnd).map(|w| {
            (
                w.visible,
                w.style,
                w.parent,
                Rect::sized(w.x, w.y, w.width, w.height),
                w.maximized,
                w.minimized,
            )
        })
    };
    let Some((was_visible, style, parent, rect, maximized, minimized)) = info else {
        return false;
    };
    let child = style & WS_CHILD != 0;
    let show = cmd != 0;
    match cmd {
        3 if !child && !maximized => {
            // SW_SHOWMAXIMIZED: fill the work area.
            let (sw, sh) = ctx.shared.borrow().gui.screen;
            {
                let mut shared = ctx.shared.borrow_mut();
                if let Some(window) = shared.gui.window_mut(hwnd) {
                    window.maximized = true;
                    window.minimized = false;
                    window.props.retain(|(name, _)| name != "OTW_RESTORE");
                    window.props.push((
                        "OTW_RESTORE".into(),
                        ((rect.left as u32 & 0xFFFF) << 16) | (rect.top as u32 & 0xFFFF),
                    ));
                    window.props.push((
                        "OTW_RESTORE_SIZE".into(),
                        ((rect.width() as u32 & 0xFFFF) << 16) | (rect.height() as u32 & 0xFFFF),
                    ));
                }
            }
            let frame = frame_width(style, 0);
            set_window_pos(
                ctx,
                memory,
                hwnd,
                0,
                -frame,
                -frame,
                sw + 2 * frame,
                sh - TASKBAR_HEIGHT + 2 * frame,
                0x0004 | 0x0020,
            );
        }
        2 | 6 | 7 if !child => {
            if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                window.minimized = true;
            }
            ctx.shared.borrow_mut().gui.dirty = true;
            send(ctx, memory, hwnd, WM_SIZE, 1, 0);
            // A minimized window gives up the activation: the next top-level
            // window of the program gets it, or none does.
            let was_active = ctx.shared.borrow().gui.active_hwnd == hwnd;
            if cmd != 7 && was_active {
                let next = {
                    let shared = ctx.shared.borrow();
                    shared
                        .gui
                        .windows
                        .iter()
                        .rev()
                        .find(|w| {
                            w.hwnd != hwnd
                                && w.parent == 0
                                && w.owner == 0
                                && w.visible
                                && !w.minimized
                                && w.style & WS_CHILD == 0
                                && w.style & WS_CAPTION == WS_CAPTION
                        })
                        .map(|w| w.hwnd)
                };
                match next {
                    Some(next) => activate(ctx, memory, next, 1),
                    None => deactivate_program(ctx, memory),
                }
            }
            return was_visible;
        }
        1 | 9 if !child && (maximized || minimized) => {
            let restore = {
                let mut shared = ctx.shared.borrow_mut();
                let window = shared.gui.window_mut(hwnd);
                window.map(|w| {
                    w.minimized = false;
                    let was_max = w.maximized;
                    w.maximized = false;
                    let pos = w
                        .props
                        .iter()
                        .find(|(n, _)| n == "OTW_RESTORE")
                        .map(|(_, v)| *v);
                    let size = w
                        .props
                        .iter()
                        .find(|(n, _)| n == "OTW_RESTORE_SIZE")
                        .map(|(_, v)| *v);
                    (was_max, pos, size)
                })
            };
            if let Some((true, Some(pos), Some(size))) = restore {
                set_window_pos(
                    ctx,
                    memory,
                    hwnd,
                    0,
                    hi(pos),
                    lo(pos),
                    hi(size),
                    lo(size),
                    0x0004 | 0x0020,
                );
            } else {
                invalidate_all(&mut ctx.shared.borrow_mut().gui, hwnd);
            }
        }
        _ => {}
    }
    if show != was_visible {
        send(ctx, memory, hwnd, WM_SHOWWINDOW, show as u32, 0);
        // The first time a window is shown it gets its size and position
        // again, so children created after WM_CREATE can be laid out.
        let first_show = show && {
            let mut shared = ctx.shared.borrow_mut();
            match shared.gui.window_mut(hwnd) {
                Some(window) if !window.props.iter().any(|(k, _)| k == "otw_shown") => {
                    window.props.push(("otw_shown".into(), 1));
                    true
                }
                _ => false,
            }
        };
        if first_show {
            let geometry = {
                let shared = ctx.shared.borrow();
                shared.gui.window(hwnd).map(|w| {
                    let (cw, ch) = client_size(w);
                    let (l, t, _, _) = nc_insets(w);
                    (cw, ch, w.x + l, w.y + t, w.maximized)
                })
            };
            if let Some((cw, ch, x, y, maximized)) = geometry {
                send(
                    ctx,
                    memory,
                    hwnd,
                    WM_SIZE,
                    if maximized { 2 } else { 0 },
                    make_long(cw, ch),
                );
                send(ctx, memory, hwnd, WM_MOVE, 0, make_long(x, y));
            }
        }
        {
            let mut shared = ctx.shared.borrow_mut();
            if let Some(window) = shared.gui.window_mut(hwnd) {
                window.visible = show;
            }
            if show {
                invalidate_all(&mut shared.gui, hwnd);
            } else if child {
                invalidate_parent_area(&mut shared.gui, parent, rect);
            }
            shared.gui.dirty = true;
        }
        if !show {
            let (focus_inside, active) = {
                let shared = ctx.shared.borrow();
                (
                    shared.gui.focus_hwnd == hwnd
                        || is_descendant(&shared.gui, shared.gui.focus_hwnd, hwnd),
                    shared.gui.active_hwnd == hwnd,
                )
            };
            if focus_inside && child {
                set_focus(ctx, memory, parent);
            }
            if active && !child {
                let next = {
                    let shared = ctx.shared.borrow();
                    let owner = shared.gui.window(hwnd).map(|w| w.owner).unwrap_or(0);
                    if owner != 0 {
                        Some(owner)
                    } else {
                        shared
                            .gui
                            .windows
                            .iter()
                            .rev()
                            .find(|w| {
                                w.hwnd != hwnd
                                    && (w.parent == 0 || w.style & WS_CHILD == 0)
                                    && w.visible
                                    && w.class != "#32768"
                            })
                            .map(|w| w.hwnd)
                    }
                };
                ctx.shared.borrow_mut().gui.active_hwnd = 0;
                if let Some(next) = next {
                    activate(ctx, memory, next, 1);
                }
            }
        }
    }
    if show && !child && !matches!(cmd, 4 | 7 | 8) {
        activate(ctx, memory, hwnd, 1);
    }
    was_visible
}

/// Activate a top-level window: z-order, WM_NCACTIVATE, WM_ACTIVATE, focus.
pub fn activate<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, how: u32) {
    let top = {
        let shared = ctx.shared.borrow();
        shared.gui.top_level_for(hwnd)
    };
    let Some(top) = top else {
        return;
    };
    let previous = ctx.shared.borrow().gui.active_hwnd;
    // Bring the window and its owned popups to the top of the z-order.
    {
        let mut shared = ctx.shared.borrow_mut();
        if let Some(index) = shared.gui.windows.iter().position(|w| w.hwnd == top) {
            let window = shared.gui.windows.remove(index);
            shared.gui.windows.push(window);
        }
        let owned: Vec<u32> = shared
            .gui
            .windows
            .iter()
            .filter(|w| w.owner == top && w.style & WS_CHILD == 0)
            .map(|w| w.hwnd)
            .collect();
        for popup in owned {
            if let Some(index) = shared.gui.windows.iter().position(|w| w.hwnd == popup) {
                let window = shared.gui.windows.remove(index);
                shared.gui.windows.push(window);
            }
        }
        // Topmost windows stay above.
        let topmost: Vec<u32> = shared
            .gui
            .windows
            .iter()
            .filter(|w| w.ex_style & WS_EX_TOPMOST != 0 && w.parent == 0)
            .map(|w| w.hwnd)
            .collect();
        for hwnd in topmost {
            if let Some(index) = shared.gui.windows.iter().position(|w| w.hwnd == hwnd) {
                let window = shared.gui.windows.remove(index);
                shared.gui.windows.push(window);
            }
        }
        shared.gui.dirty = true;
    }
    if previous == top {
        return;
    }
    if previous != 0 && is_window(ctx, previous) {
        send(ctx, memory, previous, WM_NCACTIVATE, 0, 0);
        send(ctx, memory, previous, WM_ACTIVATE, 0, top);
    }
    ctx.shared.borrow_mut().gui.active_hwnd = top;
    if previous == 0 {
        send(ctx, memory, top, WM_ACTIVATEAPP, 1, 0);
    }
    send(ctx, memory, top, WM_NCACTIVATE, 1, 0);
    let minimized = ctx
        .shared
        .borrow()
        .gui
        .window(top)
        .map(|w| w.minimized)
        .unwrap_or(false);
    send(
        ctx,
        memory,
        top,
        WM_ACTIVATE,
        make_long(how as i32, minimized as i32),
        previous,
    );
    // DefWindowProc sets focus on activation; make sure focus is inside.
    let focus = ctx.shared.borrow().gui.focus_hwnd;
    let inside = {
        let shared = ctx.shared.borrow();
        focus == top || is_descendant(&shared.gui, focus, top)
    };
    if !inside && is_window(ctx, top) && ctx.shared.borrow().gui.active_hwnd == top {
        set_focus(ctx, memory, top);
    }
}

/// This program lost the foreground to another one: its active window
/// becomes inactive and loses the focus, as with WM_ACTIVATEAPP(FALSE).
pub fn deactivate_program<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) {
    let previous = ctx.shared.borrow().gui.active_hwnd;
    if previous == 0 || !is_window(ctx, previous) {
        return;
    }
    send(ctx, memory, previous, WM_NCACTIVATE, 0, 0);
    send(ctx, memory, previous, WM_ACTIVATE, 0, 0);
    send(ctx, memory, previous, WM_ACTIVATEAPP, 0, 0);
    let focus = ctx.shared.borrow().gui.focus_hwnd;
    if focus != 0 {
        send(ctx, memory, focus, WM_KILLFOCUS, 0, 0);
    }
    let mut shared = ctx.shared.borrow_mut();
    shared.gui.focus_hwnd = 0;
    shared.gui.active_hwnd = 0;
    // Its frame is drawn again as inactive.
    if let Some(window) = shared.gui.window_mut(previous) {
        window.nc_dirty = true;
    }
    shared.gui.dirty = true;
}

pub fn set_focus<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) -> u32 {
    let previous = ctx.shared.borrow().gui.focus_hwnd;
    if hwnd == previous {
        return previous;
    }
    if hwnd != 0 {
        let (top, disabled) = {
            let shared = ctx.shared.borrow();
            (
                shared.gui.top_level_for(hwnd),
                shared.gui.window(hwnd).map(|w| w.disabled).unwrap_or(true),
            )
        };
        let Some(top) = top else {
            return 0;
        };
        if disabled {
            return 0;
        }
        if ctx.shared.borrow().gui.active_hwnd != top {
            activate(ctx, memory, top, 1);
            if ctx.shared.borrow().gui.focus_hwnd == hwnd {
                return previous;
            }
        }
    }
    let previous = ctx.shared.borrow().gui.focus_hwnd;
    ctx.shared.borrow_mut().gui.focus_hwnd = hwnd;
    if previous != 0 && is_window(ctx, previous) {
        send(ctx, memory, previous, WM_KILLFOCUS, hwnd, 0);
    }
    if hwnd != 0 && ctx.shared.borrow().gui.focus_hwnd == hwnd {
        send(ctx, memory, hwnd, WM_SETFOCUS, previous, 0);
    }
    ctx.shared.borrow_mut().gui.dirty = true;
    previous
}

/// `SetWindowPos` core. Coordinates are parent-client (child) or screen.
#[allow(clippy::too_many_arguments)]
/// A drop-down combo box's window is its edit field only: the height it is
/// given sets how far its list drops down.
fn closed_combo_height(class: &str, style: u32) -> Option<i32> {
    const CBS_DROPDOWN: u32 = 2;
    (crate::win32::controls::is_combo_class(class) && style & 3 >= CBS_DROPDOWN).then_some(21)
}

pub fn set_window_pos<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    after: u32,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    flags: u32,
) -> bool {
    const SWP_NOSIZE: u32 = 0x1;
    const SWP_NOMOVE: u32 = 0x2;
    const SWP_NOZORDER: u32 = 0x4;
    const SWP_NOREDRAW: u32 = 0x8;
    const SWP_NOACTIVATE: u32 = 0x10;
    const SWP_FRAMECHANGED: u32 = 0x20;
    const SWP_SHOWWINDOW: u32 = 0x40;
    const SWP_HIDEWINDOW: u32 = 0x80;
    let info = {
        let shared = ctx.shared.borrow();
        shared.gui.window(hwnd).map(|w| {
            (
                Rect::sized(w.x, w.y, w.width, w.height),
                w.style,
                w.parent,
                w.visible,
            )
        })
    };
    let Some((old, style, parent, visible)) = info else {
        return false;
    };
    let child = style & WS_CHILD != 0;
    let mut new = old;
    if flags & SWP_NOMOVE == 0 {
        new = Rect::sized(x, y, new.width(), new.height());
    }
    if flags & SWP_NOSIZE == 0 {
        let class = ctx.shared.borrow().gui.window(hwnd).map(|w| w.class.clone()).unwrap_or_default();
        let cy = closed_combo_height(&class, style).unwrap_or(cy);
        new = Rect::sized(new.left, new.top, cx.max(0), cy.max(0));
    }
    // WM_WINDOWPOSCHANGING lets the window adjust the new placement.
    // WINDOWPOS in the process's layout.
    let wp = scratch(ctx, 48);
    let wp_view = crate::win32::x64::View::new(memory, &crate::win32::x64::WINDOWPOS, wp);
    let write_wp = |memory: &mut M, rect: Rect, flags: u32| {
        for (index, value) in [
            hwnd,
            after,
            rect.left as u32,
            rect.top as u32,
            rect.width() as u32,
            rect.height() as u32,
            flags,
        ]
        .iter()
        .enumerate()
        {
            wp_view.set(memory, index, *value);
        }
    };
    let moved = new.left != old.left || new.top != old.top;
    let sized = new.width() != old.width() || new.height() != old.height();
    let mut effective = flags;
    if !moved {
        effective |= SWP_NOMOVE;
    }
    if !sized {
        effective |= SWP_NOSIZE;
    }
    write_wp(memory, new, effective);
    if effective & (SWP_NOMOVE | SWP_NOSIZE) != (SWP_NOMOVE | SWP_NOSIZE)
        || effective & (SWP_SHOWWINDOW | SWP_HIDEWINDOW | SWP_FRAMECHANGED) != 0
    {
        send(ctx, memory, hwnd, WM_WINDOWPOSCHANGING, 0, wp);
        let v = |memory: &mut M, index: usize| wp_view.get32(memory, index) as i32;
        new = Rect::sized(v(memory, 2), v(memory, 3), v(memory, 4).max(0), v(memory, 5).max(0));
    }
    let moved = new.left != old.left || new.top != old.top;
    let sized = new.width() != old.width() || new.height() != old.height();
    if std::env::var_os("OTW_DEBUG_POS").is_some() {
        eprintln!("[pos] {hwnd:#x} {old:?} -> {new:?} flags={flags:#x}");
    }
    {
        let mut shared = ctx.shared.borrow_mut();
        if let Some(window) = shared.gui.window_mut(hwnd) {
            window.x = new.left;
            window.y = new.top;
            window.width = new.width();
            window.height = new.height();
            if sized || flags & SWP_FRAMECHANGED != 0 {
                window.nc_dirty = true;
            }
        }
        if flags & SWP_NOZORDER == 0 {
            if let Some(index) = shared.gui.windows.iter().position(|w| w.hwnd == hwnd) {
                let window = shared.gui.windows.remove(index);
                let position = match after {
                    1 => {
                        // HWND_BOTTOM: before the first sibling.
                        shared
                            .gui
                            .windows
                            .iter()
                            .position(|w| {
                                (w.parent == parent && child && w.style & WS_CHILD != 0)
                                    || (!child && w.style & WS_CHILD == 0)
                            })
                            .unwrap_or(0)
                    }
                    0 | 0xFFFF_FFFF | 0xFFFF_FFFE => shared.gui.windows.len(),
                    other => shared
                        .gui
                        .windows
                        .iter()
                        .position(|w| w.hwnd == other)
                        .unwrap_or(shared.gui.windows.len()),
                };
                let position = position.min(shared.gui.windows.len());
                shared.gui.windows.insert(position, window);
                if after == 0xFFFF_FFFF {
                    if let Some(window) = shared.gui.window_mut(hwnd) {
                        window.ex_style |= WS_EX_TOPMOST;
                    }
                } else if after == 0xFFFF_FFFE {
                    if let Some(window) = shared.gui.window_mut(hwnd) {
                        window.ex_style &= !WS_EX_TOPMOST;
                    }
                }
            }
        }
        if !child {
            ensure_surface(&mut shared, hwnd);
        }
        if flags & SWP_NOREDRAW == 0
            && (sized || moved || flags & (SWP_FRAMECHANGED | SWP_NOZORDER) != SWP_NOZORDER)
        {
            if child && visible && (sized || moved) {
                invalidate_parent_area(&mut shared.gui, parent, old);
                invalidate_all(&mut shared.gui, hwnd);
            } else if !child && sized {
                invalidate_all(&mut shared.gui, hwnd);
            } else if flags & SWP_FRAMECHANGED != 0 {
                invalidate_all(&mut shared.gui, hwnd);
            } else if child && visible && flags & SWP_NOZORDER == 0 {
                invalidate_all(&mut shared.gui, hwnd);
            }
        } else if sized && !child {
            invalidate_all(&mut shared.gui, hwnd);
        }
        shared.gui.dirty = true;
    }
    if flags & SWP_SHOWWINDOW != 0 && !visible {
        show_window(
            ctx,
            memory,
            hwnd,
            if flags & SWP_NOACTIVATE != 0 { 8 } else { 5 },
        );
    } else if flags & SWP_HIDEWINDOW != 0 && visible {
        show_window(ctx, memory, hwnd, 0);
    }
    let mut final_flags = flags;
    if !moved {
        final_flags |= SWP_NOMOVE;
    }
    if !sized {
        final_flags |= SWP_NOSIZE;
    }
    write_wp(memory, new, final_flags);
    if sized || flags & SWP_FRAMECHANGED != 0 {
        calc_nc_size(ctx, memory, hwnd);
    }
    if moved || sized || flags & SWP_FRAMECHANGED != 0 {
        send(ctx, memory, hwnd, WM_WINDOWPOSCHANGED, 0, wp);
    }
    free_scratch(ctx, wp);
    if flags & SWP_NOACTIVATE == 0 && !child && flags & SWP_NOZORDER == 0 && is_window(ctx, hwnd) {
        let visible = ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.visible)
            .unwrap_or(false);
        if visible {
            activate(ctx, memory, hwnd, 1);
        }
    }
    true
}

// ── DefWindowProc ────────────────────────────────────────────────────────

pub fn def_window_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    match msg {
        WM_NCCREATE => {
            if lparam != 0 {
                let title_va = crate::win32::x64::View::new(memory, &crate::win32::x64::CREATESTRUCT, lparam).get32(memory, 9);
                if title_va >= 0x10000 {
                    let title = read_cstr(memory, title_va);
                    let mut shared = ctx.shared.borrow_mut();
                    if let Some(window) = shared.gui.window_mut(hwnd) {
                        window.title = crate::win32::unicode::ansi_to_string(&title);
                    }
                }
            }
            1
        }
        WM_NCCALCSIZE => {
            // The standard frame: the proposed window rectangle less the
            // borders, caption and menu bar.
            if wparam != 0 && lparam != 0 {
                let standard = {
                    let shared = ctx.shared.borrow();
                    shared.gui.window(hwnd).map(|w| nc_insets_for(scroll_style(w), w.ex_style, has_menu_bar(w)))
                };
                if let (Some((l, t, r, b)), Some(rect)) = (standard, memory.read_bytes(lparam, 16)) {
                    let v: Vec<i32> = rect.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
                    let mut out = Vec::new();
                    for value in [v[0] + l, v[1] + t, v[2] - r, v[3] - b] {
                        out.extend_from_slice(&value.to_le_bytes());
                    }
                    let _ = memory.write_bytes(lparam, &out);
                }
            }
            0
        }
        WM_NCHITTEST => {
            let shared = ctx.shared.borrow();
            let Some(window) = shared.gui.window(hwnd) else {
                return HTNOWHERE as u32;
            };
            let (wx, wy) = window_screen_origin(&shared.gui, hwnd);
            nc_hit_test(window, lo(lparam) - wx, hi(lparam) - wy) as u32
        }
        WM_NCPAINT => {
            paint_nc(&mut ctx.shared.borrow_mut(), hwnd);
            0
        }
        WM_NCACTIVATE => {
            if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                window.nc_dirty = true;
            }
            let mut shared = ctx.shared.borrow_mut();
            if shared.gui.window(hwnd).is_some() {
                shared.gui.window_mut(hwnd).unwrap().nc_dirty = false;
                paint_nc(&mut shared, hwnd);
            }
            1
        }
        WM_ERASEBKGND => {
            let background = {
                let shared = ctx.shared.borrow();
                let class = shared
                    .gui
                    .window(hwnd)
                    .map(|w| w.class.clone())
                    .unwrap_or_default();
                shared
                    .gui
                    .classes
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(&class))
                    .map(|c| c.background)
                    .unwrap_or(0)
            };
            if background == 0 {
                return 0;
            }
            let brush = if background <= 31 {
                SYSBRUSH_BASE + (background - 1) * 4
            } else {
                background
            };
            fill_client(ctx, memory, wparam, hwnd, brush);
            1
        }
        WM_PAINT => {
            let ps = scratch(ctx, 80);
            let hdc = begin_paint(ctx, memory, hwnd, ps);
            end_paint(ctx, hwnd, hdc);
            free_scratch(ctx, ps);
            0
        }
        WM_CLOSE => {
            destroy_window(ctx, memory, hwnd);
            0
        }
        WM_SETTEXT => {
            let text = read_cstr(memory, lparam);
            let mut shared = ctx.shared.borrow_mut();
            if let Some(window) = shared.gui.window_mut(hwnd) {
                window.title = crate::win32::unicode::ansi_to_string(&text);
                window.nc_dirty = true;
            }
            shared.gui.dirty = true;
            1
        }
        WM_GETTEXT => {
            let title = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| crate::win32::unicode::string_to_ansi(&w.title))
                .unwrap_or_default();
            if wparam == 0 {
                return 0;
            }
            let mut title = title;
            title.truncate(wparam as usize - 1);
            let _ = write_cstr(memory, lparam, &title);
            title.len() as u32
        }
        WM_GETTEXTLENGTH => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| crate::win32::unicode::string_to_ansi(&w.title).len() as u32)
            .unwrap_or(0),
        WM_SETFONT => {
            if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                window.font = wparam;
            }
            0
        }
        WM_GETFONT => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.font)
            .unwrap_or(0),
        WM_MOUSEACTIVATE => {
            let parent = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .filter(|w| w.style & WS_CHILD != 0)
                .map(|w| w.parent)
                .unwrap_or(0);
            if parent != 0 {
                let result = send(ctx, memory, parent, WM_MOUSEACTIVATE, wparam, lparam);
                if result != 0 {
                    return result;
                }
            }
            1 // MA_ACTIVATE
        }
        WM_SETCURSOR => {
            let parent = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .filter(|w| w.style & WS_CHILD != 0)
                .map(|w| w.parent)
                .unwrap_or(0);
            if parent != 0 && send(ctx, memory, parent, WM_SETCURSOR, wparam, lparam) != 0 {
                return 1;
            }
            0
        }
        WM_ACTIVATE => {
            if lo(wparam) != 0 && hi(wparam) == 0 {
                let focus = ctx.shared.borrow().gui.focus_hwnd;
                let inside = focus == hwnd || is_descendant(&ctx.shared.borrow().gui, focus, hwnd);
                if !inside {
                    set_focus(ctx, memory, hwnd);
                }
            }
            0
        }
        WM_WINDOWPOSCHANGED => {
            if lparam != 0 {
                let flags = crate::win32::x64::View::new(memory, &crate::win32::x64::WINDOWPOS, lparam).get32(memory, 6);
                let (cw, ch, x, y) = {
                    let shared = ctx.shared.borrow();
                    match shared.gui.window(hwnd) {
                        Some(w) => {
                            let (cw, ch) = client_size(w);
                            let (l, t, _, _) = nc_insets(w);
                            let (x, y) = if w.style & WS_CHILD != 0 {
                                (w.x + l, w.y + t)
                            } else {
                                (w.x + l, w.y + t)
                            };
                            (cw, ch, x, y)
                        }
                        None => return 0,
                    }
                };
                if flags & 0x2 == 0 {
                    send(ctx, memory, hwnd, WM_MOVE, 0, make_long(x, y));
                }
                if flags & 0x1 == 0 || flags & 0x20 != 0 {
                    let (maximized, minimized) = ctx
                        .shared
                        .borrow()
                        .gui
                        .window(hwnd)
                        .map(|w| (w.maximized, w.minimized))
                        .unwrap_or((false, false));
                    let kind = if minimized {
                        1
                    } else if maximized {
                        2
                    } else {
                        0
                    };
                    send(ctx, memory, hwnd, WM_SIZE, kind, make_long(cw, ch));
                }
            }
            0
        }
        WM_GETMINMAXINFO => 0,
        WM_SYSCOMMAND => {
            match wparam & 0xFFF0 {
                0xF060 => {
                    // SC_CLOSE
                    send(ctx, memory, hwnd, WM_CLOSE, 0, 0);
                }
                0xF020 => {
                    show_window(ctx, memory, hwnd, 6);
                }
                0xF030 => {
                    show_window(ctx, memory, hwnd, 3);
                }
                0xF120 => {
                    show_window(ctx, memory, hwnd, 9);
                }
                0xF100 if lparam == 0x20 => {
                    // SC_KEYMENU with a space (Alt+Space): the window menu.
                    crate::win32::menu::track_system_menu(ctx, memory, hwnd, None);
                }
                0xF100 => {
                    // SC_KEYMENU: Alt+letter or F10 opens the menu bar.
                    crate::win32::menu::key_menu(ctx, memory, hwnd, lparam);
                }
                0xF090 => {
                    // SC_MOUSEMENU: the window menu from the caption icon.
                    crate::win32::menu::track_system_menu(ctx, memory, hwnd, None);
                }
                0xF010 => keyboard_move_size(ctx, memory, hwnd, false),
                0xF000 => keyboard_move_size(ctx, memory, hwnd, true),
                _ => {}
            }
            0
        }
        WM_NCLBUTTONDOWN => {
            match wparam as i32 {
                HTMENU => {
                    crate::win32::menu::track_menu_bar(ctx, memory, hwnd, lo(lparam), hi(lparam))
                }
                HTCLOSE => {
                    if track_caption_button(ctx, memory, hwnd, HTCLOSE) {
                        send(ctx, memory, hwnd, WM_SYSCOMMAND, 0xF060, lparam);
                    }
                }
                HTMAXBUTTON => {
                    if track_caption_button(ctx, memory, hwnd, HTMAXBUTTON) {
                        let maximized = ctx
                            .shared
                            .borrow()
                            .gui
                            .window(hwnd)
                            .map(|w| w.maximized)
                            .unwrap_or(false);
                        send(
                            ctx,
                            memory,
                            hwnd,
                            WM_SYSCOMMAND,
                            if maximized { 0xF120 } else { 0xF030 },
                            lparam,
                        );
                    }
                }
                HTMINBUTTON => {
                    if track_caption_button(ctx, memory, hwnd, HTMINBUTTON) {
                        send(ctx, memory, hwnd, WM_SYSCOMMAND, 0xF020, lparam);
                    }
                }
                HTSYSMENU => {
                    send(ctx, memory, hwnd, WM_SYSCOMMAND, 0xF090, lparam);
                }
                HTCAPTION => move_size_loop(ctx, memory, hwnd, HTCAPTION, lo(lparam), hi(lparam)),
                HTLEFT..=HTBOTTOMRIGHT => {
                    move_size_loop(ctx, memory, hwnd, wparam as i32, lo(lparam), hi(lparam))
                }
                HTHSCROLL | HTVSCROLL => scroll_track(
                    ctx,
                    memory,
                    hwnd,
                    wparam as i32 == HTVSCROLL,
                    lo(lparam),
                    hi(lparam),
                ),
                _ => {}
            }
            0
        }
        WM_NCLBUTTONDBLCLK => {
            if wparam as i32 == HTSYSMENU {
                send(ctx, memory, hwnd, WM_SYSCOMMAND, 0xF060, lparam);
            }
            if wparam as i32 == HTCAPTION {
                let (style, maximized) = ctx
                    .shared
                    .borrow()
                    .gui
                    .window(hwnd)
                    .map(|w| (w.style, w.maximized))
                    .unwrap_or((0, false));
                if style & WS_MAXIMIZEBOX != 0 {
                    send(
                        ctx,
                        memory,
                        hwnd,
                        WM_SYSCOMMAND,
                        if maximized { 0xF120 } else { 0xF030 },
                        lparam,
                    );
                }
            }
            0
        }
        WM_SYSKEYDOWN => {
            // Alt or F10 alone arms the menu bar; any other key disarms it.
            ctx.shared.borrow_mut().gui.alt_alone = (wparam == 0x12 || wparam == 0x79)
                && lparam & 0x4000_0000 == 0
                || (wparam == 0x12 && ctx.shared.borrow().gui.alt_alone);
            if wparam == 0x73 && lparam & (1 << 29) != 0 {
                // Alt+F4
                send(ctx, memory, hwnd, WM_SYSCOMMAND, 0xF060, 0);
            } else if wparam == 0x12 || wparam == 0x79 {
                // Alt or F10 alone: arm the menu bar (handled on key up).
            }
            0
        }
        WM_SYSKEYUP => {
            let armed = std::mem::take(&mut ctx.shared.borrow_mut().gui.alt_alone);
            if armed && (wparam == 0x12 || wparam == 0x79) {
                let top = ctx.shared.borrow().gui.top_level_for(hwnd).unwrap_or(hwnd);
                send(ctx, memory, top, WM_SYSCOMMAND, 0xF100, 0);
            }
            0
        }
        WM_SYSCHAR => {
            let top = ctx.shared.borrow().gui.top_level_for(hwnd).unwrap_or(hwnd);
            if wparam != 0x0D {
                send(ctx, memory, top, WM_SYSCOMMAND, 0xF100, wparam);
            }
            0
        }
        WM_CTLCOLORMSGBOX..=WM_CTLCOLORSTATIC => {
            let hdc = wparam;
            let (text, back, brush) = match msg {
                WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => (8, 5, SYSBRUSH_BASE + 5 * 4),
                WM_CTLCOLORSCROLLBAR => (8, 15, SYSBRUSH_BASE + 15 * 4),
                _ => (18, 15, SYSBRUSH_BASE + 15 * 4),
            };
            let mut shared = ctx.shared.borrow_mut();
            if let Some(dc) = shared.gdi.dcs.get_mut(&hdc) {
                dc.attrs.text_color = gfx::rgb_to_colorref(sys_color(text));
                dc.attrs.bk_color = gfx::rgb_to_colorref(sys_color(back));
            }
            brush
        }
        WM_NCRBUTTONUP => {
            send(ctx, memory, hwnd, WM_CONTEXTMENU, hwnd, lparam);
            0
        }
        WM_CONTEXTMENU if is_caption_point(ctx, hwnd, lo(lparam), hi(lparam)) => {
            crate::win32::menu::track_system_menu(
                ctx,
                memory,
                hwnd,
                Some((lo(lparam), hi(lparam))),
            );
            0
        }
        WM_CONTEXTMENU | WM_PARENTNOTIFY => {
            if msg == WM_CONTEXTMENU {
                let parent = ctx
                    .shared
                    .borrow()
                    .gui
                    .window(hwnd)
                    .filter(|w| w.style & WS_CHILD != 0)
                    .map(|w| w.parent)
                    .unwrap_or(0);
                if parent != 0 {
                    send(ctx, memory, parent, msg, wparam, lparam);
                }
            }
            0
        }
        WM_RBUTTONUP => {
            let (sx, sy) = {
                let shared = ctx.shared.borrow();
                let (cx, cy) = client_screen_origin(&shared.gui, hwnd);
                (cx + lo(lparam), cy + hi(lparam))
            };
            send(ctx, memory, hwnd, WM_CONTEXTMENU, hwnd, make_long(sx, sy));
            0
        }
        WM_QUERYENDSESSION | WM_QUERYOPEN => 1,
        _ => 0,
    }
}

const WM_QUERYENDSESSION: u32 = 0x0011;
const WM_QUERYOPEN: u32 = 0x0013;

fn fill_client<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hdc: u32,
    hwnd: u32,
    brush: u32,
) {
    let (cw, ch) = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(client_size)
        .unwrap_or((0, 0));
    let fill = {
        let shared = ctx.shared.borrow();
        let attrs = shared
            .gdi
            .dcs
            .get(&hdc)
            .map(|dc| dc.attrs.clone())
            .unwrap_or_default();
        shared.gdi.fill_of(&attrs, brush)
    };
    gdi::with_dc(ctx, memory, hdc, |target, dctx| {
        let rect = dctx.rect(0, 0, cw, ch);
        target.fill_rect(rect, &fill, gfx::R2_COPYPEN, dctx.brush_origin(), dctx.mono);
    });
}

pub fn begin_paint<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    ps: u32,
) -> u32 {
    let (update, erase, top_offset) = {
        let mut shared = ctx.shared.borrow_mut();
        let shared = &mut *shared;
        let Some(window) = shared.gui.window(hwnd) else {
            return 0;
        };
        let update = window.update.clone();
        let erase = window.erase;
        let offset = dc_geometry(&shared.gui, hwnd, true)
            .map(|(_, ox, oy, _)| (ox, oy))
            .unwrap_or((0, 0));
        if let Some(window) = shared.gui.window_mut(hwnd) {
            window.update = Region::empty();
            window.erase = false;
        }
        (update, erase, offset)
    };
    let mut paint_region = update.clone();
    paint_region.offset(top_offset.0, top_offset.1);
    let hdc = ctx.shared.borrow_mut().gdi.new_dc(DcTarget::Window {
        hwnd,
        client: true,
        paint: Some(paint_region),
    });
    let erased = if erase {
        send(ctx, memory, hwnd, WM_ERASEBKGND, hdc, 0) != 0
    } else {
        true
    };
    if ps != 0 {
        // PAINTSTRUCT in the process's layout: hdc, fErase, rcPaint, and
        // the reserved rest zeroed.
        let bounds = update.bounds();
        let view = crate::win32::x64::View::new(memory, &crate::win32::x64::PAINTSTRUCT, ps);
        view.set(memory, 0, hdc);
        view.set(memory, 1, (!erased) as u32);
        let _ = write_rect(memory, view.at(2), bounds);
        let _ = memory.write_bytes(view.at(3), &[0u8; 40]);
    }
    hdc
}

pub fn end_paint(ctx: &ApiContext, _hwnd: u32, hdc: u32) {
    ctx.shared.borrow_mut().gdi.dcs.remove(&hdc);
}

// ── Input routing ────────────────────────────────────────────────────────

fn key_flags(gui: &GuiState) -> u32 {
    let mut flags = 0;
    if gui.keys[0x01] & 0x80 != 0 {
        flags |= 0x1;
    }
    if gui.keys[0x02] & 0x80 != 0 {
        flags |= 0x2;
    }
    if gui.keys[0x10] & 0x80 != 0 {
        flags |= 0x4;
    }
    if gui.keys[0x11] & 0x80 != 0 {
        flags |= 0x8;
    }
    if gui.keys[0x04] & 0x80 != 0 {
        flags |= 0x10;
    }
    flags
}

/// The enabled top-level window that should receive input instead of a
/// window disabled by a modal dialog.
fn modal_popup_for(gui: &GuiState, top: u32) -> Option<u32> {
    gui.windows
        .iter()
        .rev()
        .find(|w| w.owner == top && w.visible && w.style & WS_CHILD == 0 && !w.disabled)
        .map(|w| w.hwnd)
}

/// Convert one raw input event into a queued message. Returns `true` when a
/// message was produced.
fn route_input<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, event: RawInput) -> bool {
    match event {
        RawInput::Move { x, y } | RawInput::Button { x, y, .. } => {
            ctx.shared.borrow_mut().gui.cursor = (x, y);
            let (button, down) = match event {
                RawInput::Button { button, down, .. } => (Some(button), down),
                _ => (None, false),
            };
            if let Some(button) = button {
                let vk = [0x01u32, 0x02, 0x04][button.min(2) as usize] as usize;
                let mut shared = ctx.shared.borrow_mut();
                if down {
                    shared.gui.keys[vk] |= 0x80;
                } else {
                    shared.gui.keys[vk] &= !0x80;
                }
            }
            let capture = ctx.shared.borrow().gui.capture;
            let (target, hit) = if capture != 0 && is_window(ctx, capture) {
                (capture, HTCLIENT)
            } else {
                let top = top_level_at(&ctx.shared.borrow().gui, x, y);
                let Some(top) = top else {
                    return false;
                };
                let disabled = ctx
                    .shared
                    .borrow()
                    .gui
                    .window(top)
                    .map(|w| w.disabled)
                    .unwrap_or(false);
                if disabled {
                    if button.is_some() && down {
                        let popup = modal_popup_for(&ctx.shared.borrow().gui, top);
                        if let Some(popup) = popup {
                            activate(ctx, memory, popup, 2);
                        }
                    }
                    return false;
                }
                let mut target = child_at(&ctx.shared.borrow().gui, top, x, y);
                let mut hit;
                loop {
                    let disabled = ctx
                        .shared
                        .borrow()
                        .gui
                        .window(target)
                        .map(|w| w.disabled)
                        .unwrap_or(false);
                    if disabled {
                        return false;
                    }
                    hit = send(ctx, memory, target, WM_NCHITTEST, 0, make_long(x, y)) as i32;
                    if hit == HTTRANSPARENT {
                        let parent = ctx
                            .shared
                            .borrow()
                            .gui
                            .window(target)
                            .filter(|w| w.style & WS_CHILD != 0)
                            .map(|w| w.parent)
                            .unwrap_or(0);
                        if parent == 0 {
                            return false;
                        }
                        target = parent;
                        continue;
                    }
                    break;
                }
                (target, hit)
            };
            let msg_base = match (button, down) {
                (None, _) => 0,
                (Some(0), true) => 1,
                (Some(0), false) => 2,
                (Some(1), true) => 4,
                (Some(1), false) => 5,
                (Some(_), true) => 7,
                (Some(_), false) => 8,
            };
            if down && button.is_some() {
                // Mouse activation.
                let top = ctx
                    .shared
                    .borrow()
                    .gui
                    .top_level_for(target)
                    .unwrap_or(target);
                let active = ctx.shared.borrow().gui.active_hwnd;
                let is_menu = ctx
                    .shared
                    .borrow()
                    .gui
                    .window(top)
                    .map(|w| w.class == "#32768")
                    .unwrap_or(false);
                if top != active && !is_menu {
                    let result = send(
                        ctx,
                        memory,
                        target,
                        WM_MOUSEACTIVATE,
                        top,
                        make_long(hit, (WM_MOUSEMOVE + msg_base) as i32),
                    );
                    if result != 3 && result != 4 {
                        activate(ctx, memory, top, 2);
                    }
                    if result == 4 || result == 2 {
                        return false;
                    }
                }
            }
            let _ = send(
                ctx,
                memory,
                target,
                WM_SETCURSOR,
                target,
                make_long(hit, (WM_MOUSEMOVE + msg_base) as i32),
            );
            let mut shared = ctx.shared.borrow_mut();
            let flags = key_flags(&shared.gui);
            let mut message = if hit == HTCLIENT {
                let (cx, cy) = client_screen_origin(&shared.gui, target);
                GuestMessage {
                    hwnd: target,
                    message: WM_MOUSEMOVE + msg_base,
                    wparam: flags,
                    lparam: make_long(x - cx, y - cy),
                }
            } else {
                GuestMessage {
                    hwnd: target,
                    message: WM_NCMOUSEMOVE + msg_base,
                    wparam: hit as u32,
                    lparam: make_long(x, y),
                }
            };
            if down && button.is_some() {
                let now = shared.ticks();
                let (last_hwnd, last_msg, lx, ly, time) = shared.gui.last_click;
                let class_dbl = {
                    let class = shared
                        .gui
                        .window(target)
                        .map(|w| w.class.clone())
                        .unwrap_or_default();
                    shared
                        .gui
                        .classes
                        .iter()
                        .find(|c| c.name.eq_ignore_ascii_case(&class))
                        .map(|c| c.style & 0x8 != 0)
                        .unwrap_or(false)
                };
                let double = last_hwnd == target
                    && last_msg == message.message
                    && (x - lx).abs() <= 4
                    && (y - ly).abs() <= 4
                    && now.saturating_sub(time) <= 500;
                if double && (class_dbl || hit != HTCLIENT) {
                    message.message += 2;
                    shared.gui.last_click = (0, 0, 0, 0, 0);
                } else {
                    shared.gui.last_click = (target, message.message, x, y, now);
                }
            }
            if message.message == WM_MOUSEMOVE || message.message == WM_NCMOUSEMOVE {
                // Coalesce consecutive moves to the same window.
                if let Some(last) = shared.gui.queue.back_mut() {
                    if last.hwnd == message.hwnd && last.message == message.message {
                        *last = message;
                        return true;
                    }
                }
            }
            shared.gui.message_pos = (x, y);
            shared.gui.queue.push_back(message);
            true
        }
        RawInput::Key { vk, down, ch } => {
            let vk = vk & 0xFF;
            let mut shared = ctx.shared.borrow_mut();
            if down {
                shared.gui.keys[vk as usize] |= 0x80;
                shared.gui.keys[vk as usize] ^= 0x01;
            } else {
                shared.gui.keys[vk as usize] &= !0x80;
            }
            // Generic shift/ctrl/alt also update the left/right variants.
            let alt = shared.gui.keys[0x12] & 0x80 != 0;
            let ctrl = shared.gui.keys[0x11] & 0x80 != 0;
            let target = if shared.gui.focus_hwnd != 0 {
                shared.gui.focus_hwnd
            } else {
                shared.gui.active_hwnd
            };
            if target == 0 {
                return false;
            }
            let sys = (alt && !ctrl) || vk == 0x79 || (vk == 0x12);
            if down && vk != 0x12 && vk != 0x79 {
                shared.gui.alt_alone = false;
            }
            if down && vk == 0x12 && shared.gui.keys[0x12] & 0x80 != 0 && !shared.gui.alt_alone {
                // First Alt press: armed by DefWindowProc on WM_SYSKEYDOWN.
            }
            let message = match (down, sys) {
                (true, false) => WM_KEYDOWN,
                (false, false) => WM_KEYUP,
                (true, true) => WM_SYSKEYDOWN,
                (false, true) => WM_SYSKEYUP,
            };
            let mut lparam = 1 | (vk & 0xFF) << 16;
            if alt {
                lparam |= 1 << 29;
            }
            if !down {
                lparam |= 0xC000_0000;
            }
            shared.gui.queue.push_back(GuestMessage {
                hwnd: target,
                message,
                wparam: vk,
                lparam,
            });
            if down {
                if let Some(ch) = ch {
                    let char_message = if sys { WM_SYSCHAR } else { WM_CHAR };
                    shared.gui.pending_chars.push_back(GuestMessage {
                        hwnd: target,
                        message: char_message,
                        wparam: ch,
                        lparam,
                    });
                }
            }
            true
        }
    }
}

// ── Message retrieval ────────────────────────────────────────────────────

/// Whether a window's messages go to `thread`'s message loop: each thread
/// gets the messages of the windows it created.
pub fn owned_by(gui: &GuiState, hwnd: u32, thread: u32) -> bool {
    hwnd == 0 || gui.window(hwnd).is_none_or(|w| w.thread == 0 || w.thread == thread)
}

/// Whether `thread` has messages waiting (posted, queued, quit, a due
/// timer or a window to paint).
pub fn messages_for(gui: &GuiState, now: u64, thread: u32) -> bool {
    gui.posted.iter().chain(&gui.queue).any(|m| owned_by(gui, m.hwnd, thread))
        || !gui.input.is_empty()
        || (gui.quit && (gui.quit_thread == 0 || gui.quit_thread == thread))
        || gui.timers.iter().any(|t| t.due <= now && timer_for(gui, t, thread))
}

fn timer_for(gui: &GuiState, timer: &GuestTimer, thread: u32) -> bool {
    (timer.thread == 0 || timer.thread == thread) && owned_by(gui, timer.hwnd, thread)
}

fn matches_filter(message: &GuestMessage, hwnd: u32, min: u32, max: u32, gui: &GuiState) -> bool {
    if hwnd != 0 && message.hwnd != hwnd && !is_descendant(gui, message.hwnd, hwnd) {
        return false;
    }
    if min == 0 && max == 0 {
        return true;
    }
    message.message >= min && message.message <= max
}

/// Core of GetMessageA/PeekMessageA. Returns the message, or `None` when
/// nothing is available (only for non-blocking peeks).
pub fn fetch_message<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    min: u32,
    max: u32,
    remove: bool,
    block: bool,
) -> Option<GuestMessage> {
    let mut idle_rounds = 0u32;
    let thread = memory.current_thread();
    loop {
        advance_timed_script(ctx, memory);
        if memory.stopping() {
            return Some(GuestMessage {
                hwnd: 0,
                message: WM_QUIT,
                wparam: 0,
                lparam: 0,
            });
        }
        // Another program came to the front.
        let deactivate = std::mem::take(&mut ctx.shared.borrow_mut().gui.pending_deactivate);
        if deactivate {
            deactivate_program(ctx, memory);
        }
        // Pending activation requested by the host (taskbar).
        let pending = ctx.shared.borrow_mut().gui.pending_activate.take();
        if let Some(target) = pending {
            if is_window(ctx, target) {
                let minimized = ctx
                    .shared
                    .borrow()
                    .gui
                    .window(target)
                    .map(|w| w.minimized)
                    .unwrap_or(false);
                if minimized {
                    show_window(ctx, memory, target, 9);
                }
                activate(ctx, memory, target, 1);
            }
        }
        {
            let mut shared = ctx.shared.borrow_mut();
            let gui = &mut shared.gui;
            for queue_index in 0..2 {
                let queue = if queue_index == 0 {
                    &gui.posted
                } else {
                    &gui.queue
                };
                let position = queue
                    .iter()
                    .position(|m| matches_filter(m, hwnd, min, max, gui) && owned_by(gui, m.hwnd, thread));
                if let Some(position) = position {
                    let message = if queue_index == 0 {
                        gui.posted[position]
                    } else {
                        gui.queue[position]
                    };
                    if remove {
                        if queue_index == 0 {
                            gui.posted.remove(position);
                        } else {
                            gui.queue.remove(position);
                        }
                    }
                    return Some(message);
                }
            }
        }
        // Raw input is converted lazily, one event at a time.
        let event = ctx.shared.borrow_mut().gui.input.pop_front();
        if let Some(event) = event {
            route_input(ctx, memory, event);
            continue;
        }
        let quit = {
            let shared = ctx.shared.borrow();
            let mine = shared.gui.quit_thread == 0 || shared.gui.quit_thread == thread;
            (shared.gui.quit && mine).then_some(shared.gui.quit_code)
        };
        if let Some(code) = quit {
            if remove {
                ctx.shared.borrow_mut().gui.quit = false;
            }
            return Some(GuestMessage {
                hwnd: 0,
                message: WM_QUIT,
                wparam: code,
                lparam: 0,
            });
        }
        let wants_paint = min == 0 && max == 0 || (min..=max).contains(&WM_PAINT);
        if wants_paint {
            let target = next_paint(&mut ctx.shared.borrow_mut(), hwnd, thread);
            if let Some(target) = target {
                return Some(GuestMessage {
                    hwnd: target,
                    message: WM_PAINT,
                    wparam: 0,
                    lparam: 0,
                });
            }
        }
        let wants_timer = min == 0 && max == 0 || (min..=max).contains(&WM_TIMER);
        if wants_timer {
            let mut shared = ctx.shared.borrow_mut();
            let now = shared.ticks();
            let due = shared
                .gui
                .timers
                .iter()
                .position(|t| t.due <= now && (hwnd == 0 || t.hwnd == hwnd) && timer_for(&shared.gui, t, thread));
            if let Some(index) = due {
                let timer = shared.gui.timers[index];
                if remove {
                    shared.gui.timers[index].due = now + timer.interval.max(10) as u64;
                }
                return Some(GuestMessage {
                    hwnd: timer.hwnd,
                    message: WM_TIMER,
                    wparam: timer.id,
                    lparam: timer.proc_va,
                });
            }
        }
        if !block {
            if idle_rounds == 0 {
                memory.pump(false);
                idle_rounds += 1;
                let has_more = {
                    let shared = ctx.shared.borrow();
                    !shared.gui.input.is_empty()
                        || !shared.gui.queue.is_empty()
                        || !shared.gui.posted.is_empty()
                        || shared.gui.pending_activate.is_some()
                        || shared.gui.pending_deactivate
                };
                if has_more {
                    continue;
                }
            }
            return None;
        }
        // Nothing to do: other threads of the program run, then wait for
        // host input or the next timer.
        if crate::win32::sync::run_ready_threads(ctx, memory) {
            idle_rounds = 0;
            continue;
        }
        if !memory.has_host() && !memory.is_worker() && feed_script(ctx) {
            idle_rounds = 0;
            continue;
        }
        if !memory.has_host() && !memory.is_worker() && !ctx.shared.borrow().alongside {
            let idle_limit = std::env::var("OTW_IDLE_EXIT")
                .ok()
                .and_then(|v| v.parse::<u32>().ok());
            idle_rounds += 1;
            if let Some(limit) = idle_limit {
                if idle_rounds >= limit {
                    let mut shared = ctx.shared.borrow_mut();
                    shared.gui.idle_exits += 1;
                    drop(shared);
                    crate::win32::user::dump_screenshot(ctx);
                    return Some(GuestMessage {
                        hwnd: 0,
                        message: WM_QUIT,
                        wparam: 0,
                        lparam: 0,
                    });
                }
            }
        }
        let started = std::time::Instant::now();
        memory.sync_memory();
        memory.pump(true);
        // The X11 pump can return at once; never spin while idle. Wake up in
        // time for the next timer.
        let idle = {
            let shared = ctx.shared.borrow();
            shared.gui.input.is_empty()
                && shared.gui.queue.is_empty()
                && shared.gui.posted.is_empty()
                && !shared.gui.dirty
        };
        if idle && started.elapsed() < std::time::Duration::from_millis(5) {
            let next_timer = {
                let shared = ctx.shared.borrow();
                let now = shared.ticks();
                shared
                    .gui
                    .timers
                    .iter()
                    .map(|t| t.due.saturating_sub(now))
                    .min()
            };
            let wait = next_timer.unwrap_or(10).clamp(1, 10);
            std::thread::sleep(std::time::Duration::from_millis(wait));
        }
    }
}

/// A timed scripted wait (`t n`) that is over lets the script go on even
/// while the program keeps busy (timers, painting).
fn advance_timed_script<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &M) {
    let timed_out = ctx.shared.borrow().gui.script_until.is_some_and(|until| std::time::Instant::now() >= until);
    if timed_out && !memory.has_host() {
        while feed_script(ctx) {
            let shared = ctx.shared.borrow();
            if shared.gui.script_wait > 0 || shared.gui.script_until.is_some() {
                break;
            }
        }
    }
}

/// Scripted input for headless runs (`OTW_INPUT`), e.g.
/// `m 100 120; d; m 200 220; u; k 65 a; w 2; s /tmp/shot.ppm`:
/// `t n` waits n seconds (and goes on even while the program is busy),
/// `m x y` move, `d`/`u` left button, `rd`/`ru` right button, `k vk [ch]`
/// key press and release, `w n` wait n idle rounds, `s path` screenshot,
/// `q` end OTW (every program) at once.
/// Returns true if an event was fed.
pub fn feed_script(ctx: &ApiContext) -> bool {
    let mut shared = ctx.shared.borrow_mut();
    if shared.gui.script.is_none() {
        let text = std::env::var("OTW_INPUT").unwrap_or_default();
        shared.gui.script = Some(
            text.split(';')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect(),
        );
    }
    if shared.gui.script_wait > 0 {
        shared.gui.script_wait -= 1;
        return true;
    }
    if let Some(until) = shared.gui.script_until {
        if std::time::Instant::now() < until {
            drop(shared);
            std::thread::sleep(std::time::Duration::from_millis(5));
            return true;
        }
        shared.gui.script_until = None;
    }
    let Some(command) = shared.gui.script.as_mut().and_then(|s| s.pop_front()) else {
        return false;
    };
    let parts: Vec<&str> = command.split_whitespace().collect();
    let num = |i: usize| -> i32 {
        parts
            .get(i)
            .map(|p| {
                if let Some(h) = p.strip_prefix("0x") {
                    i32::from_str_radix(h, 16).unwrap_or(0)
                } else {
                    p.parse().unwrap_or(0)
                }
            })
            .unwrap_or(0)
    };
    let (x, y) = shared.gui.cursor;
    match parts.first().copied().unwrap_or("") {
        "m" => shared.gui.input.push_back(RawInput::Move {
            x: num(1),
            y: num(2),
        }),
        "d" => shared.gui.input.push_back(RawInput::Button {
            x,
            y,
            button: 0,
            down: true,
        }),
        "u" => shared.gui.input.push_back(RawInput::Button {
            x,
            y,
            button: 0,
            down: false,
        }),
        "rd" => shared.gui.input.push_back(RawInput::Button {
            x,
            y,
            button: 1,
            down: true,
        }),
        "ru" => shared.gui.input.push_back(RawInput::Button {
            x,
            y,
            button: 1,
            down: false,
        }),
        "kd" | "ku" | "k" => {
            let vk = num(1) as u32;
            let ch = parts.get(2).and_then(|c| match c.strip_prefix('#') {
                Some(code) => code.parse::<u32>().ok(),
                None => c.chars().next().map(|c| c as u32),
            });
            if parts[0] != "ku" {
                shared
                    .gui
                    .input
                    .push_back(RawInput::Key { vk, down: true, ch });
            }
            if parts[0] != "kd" {
                shared.gui.input.push_back(RawInput::Key {
                    vk,
                    down: false,
                    ch: None,
                });
            }
        }
        "w" => shared.gui.script_wait = num(1).max(0) as u32,
        "t" => {
            shared.gui.script_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(num(1).max(0) as u64));
        }
        "s" => {
            let path = parts.get(1).map(|p| p.to_string()).unwrap_or_default();
            drop(shared);
            write_screenshot(ctx, &path);
            return true;
        }
        "q" => {
            // End OTW itself, with every program (a headless run done).
            drop(shared);
            crate::win32::registry::flush();
            eprintln!("[script] quit");
            std::process::exit(0);
        }
        _ => {}
    }
    // Let the cursor follow scripted moves for button events.
    if let Some(RawInput::Move { x, y }) = shared.gui.input.back().copied() {
        shared.gui.cursor = (x, y);
    }
    true
}

/// Write a PPM of all top-level windows composited on a teal desktop when
/// `OTW_SCREENSHOT` names a file. Used for headless verification.
pub fn dump_screenshot(ctx: &ApiContext) {
    if std::env::var_os("OTW_DEBUG_WINDOWS").is_some() {
        let shared = ctx.shared.borrow();
        for w in &shared.gui.windows {
            eprintln!(
                "[window] {:#x} class={:?} title={:?} parent={:#x} owner={:#x} rect=({},{} {}x{}) style={:#010x} ex={:#x} visible={} min={} max={} menu={:#x} proc={:#x} update={:?}",
                w.hwnd, w.class, w.title, w.parent, w.owner, w.x, w.y, w.width, w.height, w.style, w.ex_style, w.visible, w.minimized, w.maximized, w.menu, w.wndproc, w.update.bounds()
            );
        }
        for class in &shared.gui.classes {
            eprintln!("[class] {:?} proc={:#x}", class.name, class.wndproc);
        }
        let mut menus: Vec<_> = shared.gui.menus.iter().collect();
        menus.sort_by_key(|(handle, _)| **handle);
        for (handle, menu) in menus {
            let items: Vec<String> = menu
                .items
                .iter()
                .map(|item| format!("{}#{:x}{}", String::from_utf8_lossy(&item.text), item.id, if item.submenu != 0 { format!(">{:#x}", item.submenu) } else { String::new() }))
                .collect();
            eprintln!("[menu] {handle:#x} {items:?}");
        }
    }
    let Ok(path) = std::env::var("OTW_SCREENSHOT") else {
        return;
    };
    write_screenshot(ctx, &path);
}

/// Composite all visible top-level windows into a PPM file.
pub fn write_screenshot(ctx: &ApiContext, path: &str) {
    write_screenshot_of(&ctx.shared.borrow(), path);
}

/// [`write_screenshot`] of a program's state.
pub fn write_screenshot_of(shared: &SharedState, path: &str) {
    let (w, h) = shared.gui.screen;
    let mut pixels = vec![0x008080u32; (w * h) as usize];
    for window in &shared.gui.windows {
        if !(window.parent == 0 || window.style & WS_CHILD == 0)
            || !window.visible
            || window.minimized
        {
            continue;
        }
        let Some(surface) = shared.gdi.surfaces.get(&window.hwnd) else {
            continue;
        };
        for y in 0..surface.height {
            for x in 0..surface.width {
                let (sx, sy) = (window.x + x, window.y + y);
                if sx >= 0 && sy >= 0 && sx < w && sy < h {
                    pixels[(sy * w + sx) as usize] =
                        surface.pixels[(y * surface.width + x) as usize];
                }
            }
        }
    }
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for pixel in pixels {
        out.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
    }
    let _ = std::fs::write(path, out);
}

/// Dispatch one message to its window procedure.
pub fn dispatch_message<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    message: GuestMessage,
) -> u32 {
    if std::env::var_os("OTW_TRACE_MSG").is_some() {
        eprintln!(
            "[msg] dispatch {:#x} {:#06x} w={:#x} l={:#x}",
            message.hwnd, message.message, message.wparam, message.lparam
        );
    }
    if message.message == WM_TIMER && message.lparam != 0 {
        let now = ctx.shared.borrow().ticks() as u32;
        return memory
            .call_guest(
                message.lparam,
                &[message.hwnd, WM_TIMER, message.wparam, now],
            )
            .unwrap_or(0);
    }
    if message.hwnd == 0 {
        return 0;
    }
    let result = send(
        ctx,
        memory,
        message.hwnd,
        message.message,
        message.wparam,
        message.lparam,
    );
    if message.message == WM_PAINT {
        // A window procedure that ignored WM_PAINT must not loop forever.
        let pending = ctx
            .shared
            .borrow()
            .gui
            .window(message.hwnd)
            .map(|w| !w.update.is_empty())
            .unwrap_or(false);
        if pending {
            let ps = scratch(ctx, 80);
            let hdc = begin_paint(ctx, memory, message.hwnd, ps);
            end_paint(ctx, message.hwnd, hdc);
            free_scratch(ctx, ps);
        }
    }
    result
}

// ── Modal tracking loops ─────────────────────────────────────────────────

/// Wait for the next raw input event while a modal loop owns the mouse.
/// Returns `None` if the process is stopping.
pub fn next_raw_input<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
) -> Option<RawInput> {
    loop {
        if memory.stopping() {
            return None;
        }
        let event = ctx.shared.borrow_mut().gui.input.pop_front();
        if let Some(event) = event {
            let mut shared = ctx.shared.borrow_mut();
            match event {
                RawInput::Move { x, y } => shared.gui.cursor = (x, y),
                RawInput::Button { x, y, button, down } => {
                    shared.gui.cursor = (x, y);
                    let vk = [0x01usize, 0x02, 0x04][button.min(2) as usize];
                    if down {
                        shared.gui.keys[vk] |= 0x80;
                    } else {
                        shared.gui.keys[vk] &= !0x80;
                    }
                }
                RawInput::Key { vk, down, .. } => {
                    if down {
                        shared.gui.keys[(vk & 0xFF) as usize] |= 0x80;
                    } else {
                        shared.gui.keys[(vk & 0xFF) as usize] &= !0x80;
                    }
                }
            }
            return Some(event);
        }
        // Keep painting while the modal loop runs.
        flush_paints(ctx, memory);
        if !memory.has_host() {
            if feed_script(ctx) {
                continue;
            }
            return None;
        }
        memory.pump(true);
    }
}

/// A host display is attached, or scripted input is still pending.
pub fn interactive<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &M) -> bool {
    if memory.has_host() {
        return true;
    }
    let shared = ctx.shared.borrow();
    let pending = match &shared.gui.script {
        Some(script) => !script.is_empty() || shared.gui.script_wait > 0 || shared.gui.script_until.is_some(),
        None => std::env::var_os("OTW_INPUT").is_some(),
    };
    pending || !shared.gui.input.is_empty()
}

/// Deliver all pending WM_PAINT messages synchronously.
pub fn flush_paints<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) {
    for _ in 0..64 {
        let target = next_paint(&mut ctx.shared.borrow_mut(), 0, memory.current_thread());
        let Some(target) = target else {
            break;
        };
        dispatch_message(
            ctx,
            memory,
            GuestMessage {
                hwnd: target,
                message: WM_PAINT,
                wparam: 0,
                lparam: 0,
            },
        );
    }
}

fn track_caption_button<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    which: i32,
) -> bool {
    let rect_of = |ctx: &ApiContext| {
        let shared = ctx.shared.borrow();
        let window = shared.gui.window(hwnd)?;
        let (close, max, min) = caption_buttons(window);
        let rect = match which {
            HTCLOSE => close,
            HTMAXBUTTON => max,
            _ => min,
        }?;
        let (wx, wy) = window_screen_origin(&shared.gui, hwnd);
        Some((rect, rect.offset(wx, wy)))
    };
    let Some((local, screen)) = rect_of(ctx) else {
        return false;
    };
    let draw = |ctx: &ApiContext, pressed: bool| {
        let mut shared = ctx.shared.borrow_mut();
        let maximized = shared
            .gui
            .window(hwnd)
            .map(|w| w.maximized)
            .unwrap_or(false);
        with_painter(&mut shared, hwnd, false, |p| {
            let glyph = match which {
                HTCLOSE => 0,
                HTMAXBUTTON => {
                    if maximized {
                        3
                    } else {
                        1
                    }
                }
                _ => 2,
            };
            crate::win32::theme::caption_button(p, local, glyph, true, pressed);
        });
    };
    draw(ctx, true);
    let mut inside = true;
    if !interactive(ctx, memory) {
        draw(ctx, false);
        return true;
    }
    while let Some(event) = next_raw_input(ctx, memory) {
        match event {
            RawInput::Move { x, y } => {
                let now = screen.contains(x, y);
                if now != inside {
                    inside = now;
                    draw(ctx, inside);
                }
            }
            RawInput::Button {
                x,
                y,
                button: 0,
                down: false,
            } => {
                draw(ctx, false);
                return screen.contains(x, y);
            }
            _ => {}
        }
    }
    draw(ctx, false);
    false
}

/// Modal move/size loop started from a caption or border press.
/// Whether a screen point is on a top-level window's caption or icon.
fn is_caption_point(ctx: &ApiContext, hwnd: u32, x: i32, y: i32) -> bool {
    let shared = ctx.shared.borrow();
    let Some(window) = shared.gui.window(hwnd) else {
        return false;
    };
    if window.style & WS_CHILD != 0 {
        return false;
    }
    let (wx, wy) = window_screen_origin(&shared.gui, hwnd);
    matches!(nc_hit_test(window, x - wx, y - wy), HTCAPTION | HTSYSMENU)
}

/// The window menu's Move and Size: arrow keys (or the mouse) move the
/// window or its bottom-right corner; Enter or a click accepts, Escape
/// cancels.
fn keyboard_move_size<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    size: bool,
) {
    let Some((start, maximized, child, sizable)) = ({
        let shared = ctx.shared.borrow();
        shared.gui.window(hwnd).map(|w| {
            (
                Rect::sized(w.x, w.y, w.width, w.height),
                w.maximized,
                w.style & WS_CHILD != 0,
                w.style & WS_THICKFRAME != 0,
            )
        })
    }) else {
        return;
    };
    if maximized || child || (size && !sizable) || !interactive(ctx, memory) {
        return;
    }
    let mut rect = start;
    let mut anchor: Option<(i32, i32)> = None;
    let apply = |ctx: &ApiContext, memory: &mut M, rect: Rect| {
        set_window_pos(
            ctx,
            memory,
            hwnd,
            0,
            rect.left,
            rect.top,
            rect.width(),
            rect.height(),
            0x4 | 0x10,
        );
        flush_paints(ctx, memory);
    };
    let adjust = |rect: Rect, dx: i32, dy: i32| {
        if size {
            Rect::new(
                rect.left,
                rect.top,
                (rect.right + dx).max(rect.left + 112),
                (rect.bottom + dy).max(rect.top + 27),
            )
        } else {
            rect.offset(dx, dy)
        }
    };
    while let Some(event) = next_raw_input(ctx, memory) {
        match event {
            RawInput::Key { vk, down: true, .. } => match vk {
                0x25..=0x28 => {
                    let (dx, dy) = match vk {
                        0x25 => (-8, 0),
                        0x26 => (0, -8),
                        0x27 => (8, 0),
                        _ => (0, 8),
                    };
                    rect = adjust(rect, dx, dy);
                    apply(ctx, memory, rect);
                }
                0x0D => break,
                0x1B => {
                    apply(ctx, memory, start);
                    break;
                }
                _ => {}
            },
            RawInput::Move { x, y } => {
                let (ax, ay) = *anchor.get_or_insert((x, y));
                let moved = adjust(start, x - ax, y - ay);
                if moved != rect {
                    rect = moved;
                    apply(ctx, memory, rect);
                }
            }
            RawInput::Button { down: true, .. } => break,
            _ => {}
        }
    }
}

fn move_size_loop<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    hit: i32,
    sx: i32,
    sy: i32,
) {
    let Some((start, maximized, child)) = ({
        let shared = ctx.shared.borrow();
        shared.gui.window(hwnd).map(|w| {
            (
                Rect::sized(w.x, w.y, w.width, w.height),
                w.maximized,
                w.style & WS_CHILD != 0,
            )
        })
    }) else {
        return;
    };
    if maximized || child || !interactive(ctx, memory) {
        return;
    }
    let (screen_w, screen_h) = ctx.shared.borrow().gui.screen;
    while let Some(event) = next_raw_input(ctx, memory) {
        match event {
            RawInput::Move { x, y } => {
                let (dx, dy) = (x - sx, y - sy);
                let mut rect = start;
                match hit {
                    HTCAPTION => rect = rect.offset(dx, dy),
                    _ => {
                        if matches!(hit, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT) {
                            rect.left = (start.left + dx).min(start.right - 112);
                        }
                        if matches!(hit, HTRIGHT | HTTOPRIGHT | HTBOTTOMRIGHT) {
                            rect.right = (start.right + dx).max(start.left + 112);
                        }
                        if matches!(hit, HTTOP | HTTOPLEFT | HTTOPRIGHT) {
                            rect.top = (start.top + dy).min(start.bottom - 27);
                        }
                        if matches!(hit, HTBOTTOM | HTBOTTOMLEFT | HTBOTTOMRIGHT) {
                            rect.bottom = (start.bottom + dy).max(start.top + 27);
                        }
                    }
                }
                // Keep the caption reachable.
                let rect = if hit == HTCAPTION {
                    let x = rect.left.clamp(-rect.width() + 40, screen_w - 40);
                    let y = rect.top.clamp(0, (screen_h - TASKBAR_HEIGHT - 20).max(0));
                    Rect::sized(x, y, rect.width(), rect.height())
                } else {
                    rect
                };
                let flags = if hit == HTCAPTION {
                    0x1 | 0x4 | 0x10
                } else {
                    0x4 | 0x10
                };
                set_window_pos(
                    ctx,
                    memory,
                    hwnd,
                    0,
                    rect.left,
                    rect.top,
                    rect.width(),
                    rect.height(),
                    flags,
                );
                flush_paints(ctx, memory);
            }
            RawInput::Button {
                button: 0,
                down: false,
                ..
            } => break,
            RawInput::Key {
                vk: 0x1B,
                down: true,
                ..
            } => {
                set_window_pos(
                    ctx,
                    memory,
                    hwnd,
                    0,
                    start.left,
                    start.top,
                    start.width(),
                    start.height(),
                    0x4 | 0x10,
                );
                break;
            }
            _ => {}
        }
    }
}

/// Track a press on a window's standard scroll bar.
fn scroll_track<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    vertical: bool,
    sx: i32,
    sy: i32,
) {
    let geometry = |ctx: &ApiContext| {
        let shared = ctx.shared.borrow();
        let window = shared.gui.window(hwnd)?.clone();
        let (wx, wy) = window_screen_origin(&shared.gui, hwnd);
        let (l, t, r, b) = nc_insets(&window);
        let rect = if vertical {
            Rect::new(
                wx + window.width - r,
                wy + t,
                wx + window.width - r + SCROLL_SIZE,
                wy + window.height - b,
            )
        } else {
            Rect::new(
                wx + l,
                wy + window.height - b,
                wx + window.width - r,
                wy + window.height - b + SCROLL_SIZE,
            )
        };
        Some((rect, window.scroll[vertical as usize]))
    };
    let Some((rect, (min, max, pos, page))) = geometry(ctx) else {
        return;
    };
    let message = if vertical { WM_VSCROLL } else { WM_HSCROLL };
    let length = if vertical {
        rect.height()
    } else {
        rect.width()
    };
    let along = if vertical {
        sy - rect.top
    } else {
        sx - rect.left
    };
    let arrow = SCROLL_SIZE.min(length / 2);
    let thumb = scroll_thumb(length, min, max, pos, page);
    let code = if along < arrow {
        0 // SB_LINEUP
    } else if along >= length - arrow {
        1 // SB_LINEDOWN
    } else if let Some((start, size)) = thumb {
        if along < start {
            2
        } else if along >= start + size {
            3
        } else {
            5 // SB_THUMBTRACK
        }
    } else {
        return;
    };
    if code != 5 {
        send(ctx, memory, hwnd, message, code, 0);
        let mut last = ctx.shared.borrow().ticks();
        let mut delay = 350;
        if interactive(ctx, memory) {
            loop {
                let now = ctx.shared.borrow().ticks();
                if now - last >= delay {
                    send(ctx, memory, hwnd, message, code, 0);
                    last = now;
                    delay = 50;
                    flush_paints(ctx, memory);
                }
                let event = ctx.shared.borrow_mut().gui.input.pop_front();
                match event {
                    Some(RawInput::Button {
                        button: 0,
                        down: false,
                        ..
                    }) => break,
                    Some(RawInput::Move { x, y }) => ctx.shared.borrow_mut().gui.cursor = (x, y),
                    Some(_) => {}
                    None => {
                        if memory.stopping() {
                            return;
                        }
                        flush_paints(ctx, memory);
                        memory.pump(true);
                    }
                }
            }
        }
        send(ctx, memory, hwnd, message, 8, 0); // SB_ENDSCROLL
        return;
    }
    let (start, size) = thumb.unwrap();
    let grab = along - start;
    let mut current = pos;
    while let Some(event) = next_raw_input(ctx, memory) {
        match event {
            RawInput::Move { x, y } => {
                let along = if vertical {
                    y - rect.top
                } else {
                    x - rect.left
                };
                let track = length - 2 * arrow - size;
                let offset = (along - grab - arrow).clamp(0, track.max(0));
                let span =
                    (max - min + 1 - page.max(1) as i32 + if page == 0 { 0 } else { 1 }).max(1);
                let value = min
                    + if track > 0 {
                        (offset as i64 * span as i64 / track as i64) as i32
                    } else {
                        0
                    };
                if value != current {
                    current = value;
                    send(ctx, memory, hwnd, message, make_long(5, value), 0);
                    flush_paints(ctx, memory);
                }
            }
            RawInput::Button {
                button: 0,
                down: false,
                ..
            } => break,
            _ => {}
        }
    }
    send(ctx, memory, hwnd, message, make_long(4, current), 0);
    send(ctx, memory, hwnd, message, 8, 0);
}

// ── Resources ────────────────────────────────────────────────────────────

fn mem_u16<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> Option<u16> {
    let bytes = memory.read_bytes(va, 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Locate a PE32 module's resource directory in guest memory.
pub fn resource_directory<M: GuestMemory + ?Sized>(memory: &mut M, module: u32) -> Option<u32> {
    let pe_offset = memory.read_u32(module.wrapping_add(0x3c))?;
    let pe = module.wrapping_add(pe_offset);
    if memory.read_bytes(pe, 4)? != *b"PE\0\0" {
        return None;
    }
    let optional = pe.wrapping_add(24);
    // PE32 or PE32+ (whose data directories start 16 bytes later).
    let table = match mem_u16(memory, optional)? {
        0x10b => optional.wrapping_add(96),
        0x20b => optional.wrapping_add(112),
        _ => return None,
    };
    let directories = memory.read_u32(table.wrapping_sub(4))?;
    if directories <= 2 {
        return None;
    }
    let rva = memory.read_u32(table.wrapping_add(2 * 8))?;
    if rva == 0 {
        return None;
    }
    Some(module.wrapping_add(rva))
}

/// A resource name or type: integer id or (upper-cased) string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResName {
    Id(u32),
    Name(String),
}

pub fn res_name<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> ResName {
    if va < 0x10000 {
        return ResName::Id(va);
    }
    let text = read_cstr(memory, va);
    if text.first() == Some(&b'#') {
        if let Ok(id) = String::from_utf8_lossy(&text[1..]).parse::<u32>() {
            return ResName::Id(id);
        }
    }
    ResName::Name(String::from_utf8_lossy(&text).to_ascii_uppercase())
}

/// Find a resource: returns (data VA, size).
pub fn find_resource<M: GuestMemory + ?Sized>(
    memory: &mut M,
    module: u32,
    kind: &ResName,
    name: &ResName,
) -> Option<(u32, u32)> {
    let root = resource_directory(memory, module)?;
    let mut directory = root;
    for (level, wanted) in [Some(kind), Some(name), None].into_iter().enumerate() {
        let named = mem_u16(memory, directory + 12)? as u32;
        let ids = mem_u16(memory, directory + 14)? as u32;
        let mut found = None;
        let mut first = None;
        for index in 0..named + ids {
            let entry = directory + 16 + index * 8;
            let id = memory.read_u32(entry)?;
            let value = memory.read_u32(entry + 4)?;
            if first.is_none() {
                first = Some(value);
            }
            let matches = match wanted {
                None => true,
                Some(ResName::Id(wanted)) => id & 0x8000_0000 == 0 && id == *wanted,
                Some(ResName::Name(wanted)) => {
                    if id & 0x8000_0000 == 0 {
                        false
                    } else {
                        let string = root + (id & 0x7FFF_FFFF);
                        let length = mem_u16(memory, string)? as usize;
                        let bytes = memory.read_bytes(string + 2, length * 2)?;
                        let text: String = bytes
                            .chunks_exact(2)
                            .map(|c| {
                                char::from_u32(u16::from_le_bytes([c[0], c[1]]) as u32)
                                    .unwrap_or('?')
                            })
                            .collect();
                        text.to_ascii_uppercase() == *wanted
                    }
                }
            };
            if matches {
                found = Some(value);
                break;
            }
        }
        let value = if level == 2 { found.or(first)? } else { found? };
        if value & 0x8000_0000 != 0 {
            directory = root + (value & 0x7FFF_FFFF);
        } else {
            let data = root + value;
            let rva = memory.read_u32(data)?;
            let size = memory.read_u32(data + 4)?;
            return Some((module + rva, size));
        }
    }
    None
}

/// Find a resource in `module`, or in any loaded module when `module` is 0.
pub fn find_resource_any<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    module: u32,
    kind: &ResName,
    name: &ResName,
) -> Option<(u32, u32)> {
    if module != 0 {
        if let Some(found) = find_resource(memory, module, kind, name) {
            return Some(found);
        }
    }
    let candidates: Vec<u32> = std::iter::once(ctx.image_base)
        .chain(ctx.modules.borrow().iter().map(|m| m.image_base))
        .collect();
    for candidate in candidates {
        if candidate == module {
            continue;
        }
        if let Some(found) = find_resource(memory, candidate, kind, name) {
            return Some(found);
        }
    }
    None
}

/// Read one ANSI string from a module's RT_STRING table.
pub fn resource_string<M: GuestMemory + ?Sized>(
    memory: &mut M,
    module: u32,
    id: u32,
) -> Option<Vec<u8>> {
    let (va, size) = find_resource(memory, module, &ResName::Id(6), &ResName::Id(id / 16 + 1))?;
    let bytes = memory.read_bytes(va, size.min(0x1_0000) as usize)?;
    let index = (id % 16) as usize;
    let mut offset = 0usize;
    for current in 0..=index {
        if offset + 2 > bytes.len() {
            return None;
        }
        let units = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        offset += 2;
        let end = offset.checked_add(units * 2)?;
        if end > bytes.len() {
            return None;
        }
        if current == index {
            if units == 0 {
                return None;
            }
            let units: Vec<u16> = bytes[offset..end].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            return Some(crate::win32::unicode::wide_to_ansi(&units));
        }
        offset = end;
    }
    None
}

/// Load a bitmap resource as a GDI bitmap handle.
/// `LoadIconA`: a handle whose images come from the module's icon
/// resources, or the system icons for a null instance. A handle is returned
/// even when no image is found, as programs rarely check.
pub fn load_icon<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    instance: u32,
    name_va: u32,
) -> u32 {
    let images = if instance == 0 {
        crate::win32::icon::system_icon(name_va)
    } else {
        let name = res_name(memory, name_va);
        let large = crate::win32::icon::load(ctx, memory, instance, &name, 32);
        let small = crate::win32::icon::load(ctx, memory, instance, &name, 16);
        large
            .zip(small)
            .map(|(large, small)| crate::win32::icon::Icon { large, small })
    };
    let mut shared = ctx.shared.borrow_mut();
    let handle = shared.alloc_handle() | 0x0050_0000;
    if let Some(images) = images {
        shared.gui.icons.insert(handle, images);
    }
    handle
}

pub fn load_bitmap<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    instance: u32,
    name_va: u32,
) -> u32 {
    let name = res_name(memory, name_va);
    if instance == 0 {
        return 0;
    }
    let Some((va, _size)) = find_resource_any(ctx, memory, instance, &ResName::Id(2), &name) else {
        return 0;
    };
    let palette = gdi::default_palette();
    let Some(info) = gdi::read_dib_info(memory, va, 0, &palette) else {
        return 0;
    };
    let table = if info.bpp <= 8 {
        info.color_count as u32 * if info.header_size == 12 { 3 } else { 4 }
    } else {
        0
    };
    let mask_bytes = if info.masks.is_some() { 12 } else { 0 };
    let bits_va = va + info.header_size + mask_bytes + table;
    let bits = memory
        .read_bytes(bits_va, info.image_size())
        .unwrap_or_default();
    let mono =
        info.bpp == 1 && info.colors.first() == Some(&0) && info.colors.get(1) == Some(&0xFFFFFF);
    let mut surface = Surface::new(
        info.width.abs(),
        info.height.abs(),
        if mono { 1 } else { 24 },
    );
    gdi::decode_dib(
        &info,
        &bits,
        &mut surface,
        0,
        info.height.unsigned_abs() as usize,
    );
    ctx.shared
        .borrow_mut()
        .gdi
        .insert(gdi::GdiObject::Bitmap(gdi::Bitmap { surface, dib: None }))
}

// ── API surface ──────────────────────────────────────────────────────────

pub fn arg_count(name: &str) -> Option<usize> {
    if crate::win32::menu::arg_count(name).is_some() {
        return crate::win32::menu::arg_count(name);
    }
    if crate::win32::controls::arg_count(name).is_some() {
        return crate::win32::controls::arg_count(name);
    }
    if crate::win32::clipboard::arg_count(name).is_some() {
        return crate::win32::clipboard::arg_count(name);
    }
    Some(match name {
        "GetFocus"
        | "GetActiveWindow"
        | "GetForegroundWindow"
        | "GetShellWindow"
        | "GetCapture"
        | "ReleaseCapture"
        | "GetDesktopWindow"
        | "GetMessagePos"
        | "GetMessageTime"
        | "InSendMessage"
        | "WaitMessage"
        | "GetCursor"
        | "GetDoubleClickTime"
        | "GetKBCodePage" => 0,
        "GetMessageA" => 4,
        "PeekMessageA" | "MsgWaitForMultipleObjects" => 5,
        "CreateWindowExA" => 12,
        "TabbedTextOutA" => 8,
        "DrawTextA" | "SetRect" | "GetTabbedTextExtentA" => 5,
        "DrawTextExA" | "LoadImageA" => 6,
        "MessageBoxA" | "SendMessageA" | "PostMessageA" | "WinHelpA" | "PostThreadMessageA"
        | "CallMsgFilterA" | "SetWindowsHookExA" | "CallNextHookEx" | "SetTimer"
        | "RedrawWindow" | "UnpackDDElParam" | "ReuseDDElParam" => {
            if name == "RedrawWindow" || name == "UnpackDDElParam" {
                4
            } else if name == "ReuseDDElParam" {
                5
            } else if name == "CallMsgFilterA" {
                2
            } else {
                4
            }
        }
        "DefWindowProcA" | "DefMDIChildProcA" | "DefDlgProcA" => 4,
        "OTW_ButtonProc" | "OTW_EditProc" | "OTW_StaticProc" | "OTW_ListBoxProc"
        | "OTW_ComboBoxProc" | "OTW_ScrollBarProc" | "OTW_MDIClientProc" | "OTW_MenuProc"
        | "OTW_ColorPanelProc" | "OTW_RichEditProc" => 4,
        "DefFrameProcA" | "CallWindowProcA" => 5,
        "RegisterClassA" | "RegisterClassExA" => 1,
        "UnregisterClassA" => 2,
        "GetClassNameA" | "GetWindowTextA" | "GetClassInfoA" | "GetClassInfoExA" => 3,
        "SetWindowLongA"
        | "InvalidateRect"
        | "ChildWindowFromPoint"
        | "ValidateRect"
        | "GetUpdateRect"
        | "ScrollWindowEx" => {
            if name == "ValidateRect" {
                2
            } else if name == "ScrollWindowEx" {
                8
            } else {
                3
            }
        }
        "MoveWindow" => 6,
        "SetWindowPos" => 7,
        "ScrollWindow" => 5,
        "LoadStringA" => 4,
        "GetSystemMetrics"
        | "UpdateWindow"
        | "GetDC"
        | "GetDlgCtrlID"
        | "GetCursorPos"
        | "PostQuitMessage"
        | "IsIconic"
        | "IsZoomed"
        | "DestroyWindow"
        | "MessageBeep"
        | "GetLastActivePopup"
        | "SetFocus"
        | "SetForegroundWindow"
        | "SetActiveWindow"
        | "DispatchMessageA"
        | "TranslateMessage"
        | "GetParent"
        | "IsWindow"
        | "IsWindowEnabled"
        | "IsWindowVisible"
        | "GetWindowDC"
        | "GetSysColor"
        | "SetCapture"
        | "BringWindowToTop"
        | "GetWindowTextLengthA"
        | "UnhookWindowsHookEx"
        | "SetMessageQueue"
        | "GetKeyState"
        | "GetAsyncKeyState"
        | "DestroyCursor"
        | "DestroyIcon"
        | "RegisterClipboardFormatA"
        | "CharUpperA"
        | "CharLowerA"
        | "LockWindowUpdate"
        | "GetCaretPos"
        | "ClipCursor"
        | "CharNextA"
        | "RegisterWindowMessageA"
        | "GetTopWindow"
        | "IsRectEmpty"
        | "SetRectEmpty"
        | "GetSysColorBrush"
        | "ShowCaret"
        | "HideCaret"
        | "DestroyCaret"
        | "SetCursor"
        | "GetKeyboardState" => 1,
        "GetWindowLongA"
        | "GetClientRect"
        | "GetWindowRect"
        | "ShowWindow"
        | "EnableWindow"
        | "ReleaseDC"
        | "ScreenToClient"
        | "ClientToScreen"
        | "WindowFromPoint"
        | "GetWindow"
        | "IsChild"
        | "SetParent"
        | "CharPrevA"
        | "LoadCursorA"
        | "LoadIconA"
        | "LoadBitmapA"
        | "GetPropA"
        | "RemovePropA"
        | "SetCursorPos"
        | "CopyRect"
        | "EqualRect"
        | "OemToCharA"
        | "CharToOemA"
        | "CharLowerBuffA"
        | "CharUpperBuffA"
        | "GetWindowPlacement"
        | "SetWindowPlacement"
        | "ShowOwnedPopups"
        | "GetWindowThreadProcessId"
        | "KillTimer"
        | "SetWindowTextA"
        | "SetCaretPos" => 2,
        "SetPropA" | "GetDCEx" | "FillRect" | "FrameRect" | "InflateRect" | "IntersectRect"
        | "OffsetRect" | "PtInRect" | "UnionRect" | "SubtractRect" | "DrawFocusRect"
        | "InvertRect" | "wvsprintfA" | "DrawIcon" | "MapWindowPoints" | "CreateCaret"
        | "DrawEdge" => match name {
            "DrawFocusRect" | "InvertRect" => 2,
            "DrawIcon" | "PtInRect" | "OffsetRect" | "InflateRect" | "SetPropA" | "GetDCEx"
            | "FillRect" | "FrameRect" | "IntersectRect" | "UnionRect" | "SubtractRect"
            | "wvsprintfA" => 3,
            "MapWindowPoints" | "CreateCaret" | "DrawEdge" => 4,
            _ => 3,
        },
        "GrayStringA" => 9,
        "wsprintfA" => 8,
        "SystemParametersInfoA" => 4,
        "BeginPaint" | "EndPaint" => 2,
        "BeginDeferWindowPos" => 1,
        "DeferWindowPos" => 8,
        "EndDeferWindowPos" => 1,
        "ShowScrollBar" | "EnableScrollBar" | "GetScrollPos" | "SetScrollPos"
        | "GetScrollRange" | "SetScrollRange" | "SetScrollInfo" | "GetScrollInfo" => match name {
            "GetScrollPos" => 2,
            "ShowScrollBar" | "EnableScrollBar" | "GetScrollInfo" => 3,
            "SetScrollPos" | "GetScrollRange" | "SetScrollInfo" => 4,
            _ => 5,
        },
        "AdjustWindowRect" => 3,
        "AdjustWindowRectEx" => 4,
        "TranslateMDISysAccel" => 2,
        "CopyAcceleratorTableA" => 3,
        "EnumWindows" | "EnumChildWindows" | "EnumThreadWindows" => {
            if name == "EnumWindows" {
                2
            } else {
                3
            }
        }
        "FindWindowA" => 2,
        "CreateIconIndirect" | "CreateIcon" => {
            if name == "CreateIcon" {
                7
            } else {
                1
            }
        }
        _ => return None,
    })
}

#[allow(clippy::cognitive_complexity)]
pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &ImportName,
    args: &[u32],
    caller: u32,
) -> Option<ApiOutcome> {
    let name = name_of(name)?;
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    if crate::win32::menu::arg_count(name).is_some() {
        return crate::win32::menu::dispatch(ctx, memory, name, args, count);
    }
    if crate::win32::controls::arg_count(name).is_some() {
        return crate::win32::controls::dispatch(ctx, memory, name, args, count, caller);
    }
    if crate::win32::clipboard::arg_count(name).is_some() {
        return crate::win32::clipboard::dispatch(ctx, memory, name, args, count);
    }
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    let a = |i: usize| args[i] as i32;
    ensure_builtin_classes(ctx);

    match name {
        "DefWindowProcA" | "DefMDIChildProcA" | "DefDlgProcA" | "OTW_ButtonProc"
        | "OTW_EditProc" | "OTW_StaticProc" | "OTW_ListBoxProc" | "OTW_ComboBoxProc"
        | "OTW_ScrollBarProc" | "OTW_MDIClientProc" | "OTW_MenuProc" | "OTW_ColorPanelProc"
        | "OTW_RichEditProc" => {
            ok(builtin_proc(ctx, memory, name, args))
        }
        "DefFrameProcA" => ok(def_window_proc(
            ctx, memory, args[0], args[2], args[3], args[4],
        )),
        "CallWindowProcA" => ok(call_wndproc(
            ctx, memory, args[0], args[1], args[2], args[3], args[4],
        )),

        // ── Classes ──
        "RegisterClassA" | "RegisterClassExA" => {
            // WNDCLASS, or WNDCLASSEX (cbSize first), in the process's layout.
            let (layout, first) = crate::win32::x64::window_class(name == "RegisterClassExA");
            let view = crate::win32::x64::View::new(memory, &layout, args[0]);
            let field = |memory: &mut M, index: usize| view.get32(memory, first + index);
            let style = field(memory, 0);
            let wndproc = field(memory, 1);
            let cls_extra = field(memory, 2);
            let wnd_extra = field(memory, 3);
            let instance = field(memory, 4);
            let icon = field(memory, 5);
            let cursor = field(memory, 6);
            let background = field(memory, 7);
            let menu_name = field(memory, 8);
            let class_va = field(memory, 9);
            let class_name = class_name_from(ctx, memory, class_va);
            let mut shared = ctx.shared.borrow_mut();
            if let Some(existing) = shared
                .gui
                .classes
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&class_name))
            {
                // Re-registering a built-in class name is allowed for apps
                // that supersede it; others fail with ERROR_CLASS_ALREADY_EXISTS.
                if existing.instance != 0 || existing.wndproc == wndproc {
                    shared.last_error = 1410;
                    return ok(0);
                }
            }
            shared
                .gui
                .classes
                .retain(|c| !c.name.eq_ignore_ascii_case(&class_name));
            let atom = 0xC100 + shared.gui.classes.len() as u32 + shared.gui.atoms.len() as u32;
            shared.gui.classes.push(GuestClass {
                name: class_name,
                wndproc,
                style,
                cls_extra,
                wnd_extra,
                instance,
                icon,
                cursor,
                background,
                menu_name,
                atom,
            });
            ok(atom)
        }
        "UnregisterClassA" => {
            let class_name = class_name_from(ctx, memory, args[0]);
            ctx.shared
                .borrow_mut()
                .gui
                .classes
                .retain(|c| !c.name.eq_ignore_ascii_case(&class_name));
            ok(1)
        }
        "GetClassInfoA" | "GetClassInfoExA" => {
            let class_name = class_name_from(ctx, memory, args[1]);
            let class = ctx
                .shared
                .borrow()
                .gui
                .classes
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&class_name))
                .cloned();
            match class {
                Some(class) => {
                    let (layout, first) = crate::win32::x64::window_class(name == "GetClassInfoExA");
                    let view = crate::win32::x64::View::new(memory, &layout, args[2]);
                    let values = [
                        class.style,
                        class.wndproc,
                        class.cls_extra,
                        class.wnd_extra,
                        if class.instance == 0 {
                            args[0]
                        } else {
                            class.instance
                        },
                        class.icon,
                        class.cursor,
                        class.background,
                        class.menu_name,
                        args[1],
                    ];
                    for (index, value) in values.iter().enumerate() {
                        view.set(memory, first + index, *value);
                    }
                    ok(1)
                }
                None => {
                    ctx.shared.borrow_mut().last_error = 1411;
                    ok(0)
                }
            }
        }
        "GetClassNameA" => {
            let class = ctx
                .shared
                .borrow()
                .gui
                .window(args[0])
                .map(|w| w.class.as_bytes().to_vec())
                .unwrap_or_default();
            let mut class = class;
            class.truncate(args[2].saturating_sub(1) as usize);
            let _ = write_cstr(memory, args[1], &class);
            ok(class.len() as u32)
        }

        // ── Window lifecycle ──
        "CreateWindowExA" => {
            let class = class_name_from(ctx, memory, args[1]);
            let title = if args[2] >= 0x10000 {
                read_cstr(memory, args[2])
            } else {
                Vec::new()
            };
            let params = CreateParams {
                ex_style: args[0],
                class,
                class_va: args[1],
                title,
                title_va: args[2],
                style: args[3],
                x: a(4),
                y: a(5),
                width: a(6),
                height: a(7),
                parent: args[8],
                menu: args[9],
                instance: args[10],
                param: args[11],
            };
            ok(create_window(ctx, memory, params))
        }
        "DestroyWindow" => ok(destroy_window(ctx, memory, args[0]) as u32),
        "ShowWindow" => ok(show_window(ctx, memory, args[0], args[1]) as u32),
        "IsWindow" => ok(is_window(ctx, args[0]) as u32),
        "IsWindowVisible" => ok(is_visible_chain(&ctx.shared.borrow().gui, args[0]) as u32),
        "IsWindowEnabled" => ok(ctx
            .shared
            .borrow()
            .gui
            .window(args[0])
            .map(|w| !w.disabled)
            .unwrap_or(false) as u32),
        "IsIconic" => ok(ctx
            .shared
            .borrow()
            .gui
            .window(args[0])
            .map(|w| w.minimized)
            .unwrap_or(false) as u32),
        "IsZoomed" => ok(ctx
            .shared
            .borrow()
            .gui
            .window(args[0])
            .map(|w| w.maximized)
            .unwrap_or(false) as u32),
        "EnableWindow" => {
            let previous = {
                let mut shared = ctx.shared.borrow_mut();
                let Some(window) = shared.gui.window_mut(args[0]) else {
                    return ok(0);
                };
                let previous = window.disabled;
                window.disabled = args[1] == 0;
                if previous != window.disabled {
                    window.nc_dirty = true;
                }
                previous
            };
            if previous != (args[1] == 0) {
                send(ctx, memory, args[0], WM_ENABLE, args[1], 0);
                invalidate(&mut ctx.shared.borrow_mut().gui, args[0], None, true, false);
                if args[1] == 0 {
                    let focus = ctx.shared.borrow().gui.focus_hwnd;
                    if focus == args[0] || is_descendant(&ctx.shared.borrow().gui, focus, args[0]) {
                        set_focus(ctx, memory, 0);
                    }
                }
            }
            ok(previous as u32)
        }
        "SetWindowTextA" => {
            let result = send(ctx, memory, args[0], WM_SETTEXT, 0, args[1]);
            ok(result.max(1))
        }
        "GetWindowTextA" => {
            if args[2] == 0 {
                return ok(0);
            }
            ok(send(ctx, memory, args[0], WM_GETTEXT, args[2], args[1]))
        }
        "GetWindowTextLengthA" => ok(send(ctx, memory, args[0], WM_GETTEXTLENGTH, 0, 0)),
        "GetParent" => {
            let shared = ctx.shared.borrow();
            ok(shared
                .gui
                .window(args[0])
                .map(|w| {
                    if w.style & WS_CHILD != 0 {
                        w.parent
                    } else {
                        w.owner
                    }
                })
                .unwrap_or(0))
        }
        "SetParent" => {
            let mut shared = ctx.shared.borrow_mut();
            let Some(window) = shared.gui.window_mut(args[0]) else {
                return ok(0);
            };
            let previous = window.parent;
            window.parent = args[1];
            invalidate_all(&mut shared.gui, args[0]);
            ok(previous)
        }
        "IsChild" => ok(is_descendant(&ctx.shared.borrow().gui, args[1], args[0]) as u32),
        "GetDesktopWindow" => ok(DESKTOP_HWND),
        "GetTopWindow" => {
            let shared = ctx.shared.borrow();
            if args[0] == 0 || args[0] == DESKTOP_HWND {
                ok(shared
                    .gui
                    .windows
                    .iter()
                    .rev()
                    .find(|w| w.style & WS_CHILD == 0 && w.visible)
                    .map(|w| w.hwnd)
                    .unwrap_or(0))
            } else {
                ok(children(&shared.gui, args[0]).last().copied().unwrap_or(0))
            }
        }
        "GetWindow" => {
            let shared = ctx.shared.borrow();
            let gui = &shared.gui;
            let Some(window) = gui.window(args[0]) else {
                return ok(0);
            };
            let siblings: Vec<u32> = if window.style & WS_CHILD != 0 {
                children(gui, window.parent)
            } else {
                gui.windows
                    .iter()
                    .filter(|w| w.style & WS_CHILD == 0)
                    .map(|w| w.hwnd)
                    .collect()
            };
            let position = siblings.iter().position(|h| *h == args[0]);
            ok(match args[1] {
                0 => siblings.last().copied().unwrap_or(0),
                1 => siblings.first().copied().unwrap_or(0),
                2 => position
                    .and_then(|p| p.checked_sub(1))
                    .map(|p| siblings[p])
                    .unwrap_or(0),
                3 => position
                    .and_then(|p| siblings.get(p + 1))
                    .copied()
                    .unwrap_or(0),
                4 => window.owner,
                5 => children(gui, args[0]).last().copied().unwrap_or(0),
                _ => 0,
            })
        }
        "GetLastActivePopup" => {
            let shared = ctx.shared.borrow();
            ok(shared
                .gui
                .windows
                .iter()
                .rev()
                .find(|w| {
                    w.owner == args[0]
                        && w.visible
                        && w.style & WS_CHILD == 0
                        && w.class != "#32768"
                })
                .map(|w| w.hwnd)
                .unwrap_or(args[0]))
        }
        "GetWindowThreadProcessId" => {
            if args[1] != 0 {
                let _ = memory.write_u32(args[1], 1);
            }
            ok(1)
        }
        "EnumWindows" | "EnumChildWindows" | "EnumThreadWindows" => {
            let (proc_va, lparam, list) = {
                let shared = ctx.shared.borrow();
                match name {
                    "EnumChildWindows" => {
                        let mut all = Vec::new();
                        fn walk(gui: &GuiState, hwnd: u32, out: &mut Vec<u32>) {
                            for child in children(gui, hwnd).into_iter().rev() {
                                out.push(child);
                                walk(gui, child, out);
                            }
                        }
                        walk(&shared.gui, args[0], &mut all);
                        (args[1], args[2], all)
                    }
                    "EnumThreadWindows" => (
                        args[1],
                        args[2],
                        shared
                            .gui
                            .windows
                            .iter()
                            .rev()
                            .filter(|w| w.style & WS_CHILD == 0)
                            .map(|w| w.hwnd)
                            .collect(),
                    ),
                    _ => (
                        args[0],
                        args[1],
                        shared
                            .gui
                            .windows
                            .iter()
                            .rev()
                            .filter(|w| w.style & WS_CHILD == 0)
                            .map(|w| w.hwnd)
                            .collect(),
                    ),
                }
            };
            for hwnd in list {
                if memory.call_guest(proc_va, &[hwnd, lparam]).unwrap_or(0) == 0 {
                    break;
                }
            }
            ok(1)
        }
        "FindWindowA" => {
            let class = if args[0] != 0 {
                Some(class_name_from(ctx, memory, args[0]))
            } else {
                None
            };
            let title = if args[1] != 0 {
                Some(crate::win32::unicode::ansi_to_string(&read_cstr(memory, args[1])))
            } else {
                None
            };
            let shared = ctx.shared.borrow();
            ok(shared
                .gui
                .windows
                .iter()
                .filter(|w| w.style & WS_CHILD == 0)
                .find(|w| {
                    class
                        .as_ref()
                        .map(|c| w.class.eq_ignore_ascii_case(c))
                        .unwrap_or(true)
                        && title.as_ref().map(|t| w.title == *t).unwrap_or(true)
                })
                .map(|w| w.hwnd)
                .unwrap_or(0))
        }

        // ── Window data ──
        "GetWindowLongA" => {
            let shared = ctx.shared.borrow();
            let Some(window) = shared.gui.window(args[0]) else {
                return ok(0);
            };
            let index = a(1);
            ok(match index {
                -4 => window.wndproc,
                -6 => window.instance,
                -8 => {
                    if window.style & WS_CHILD != 0 {
                        window.parent
                    } else {
                        window.owner
                    }
                }
                -12 => window.id,
                -16 => {
                    window.style
                        | if window.visible { WS_VISIBLE } else { 0 }
                        | if window.disabled { WS_DISABLED } else { 0 }
                        | if window.maximized { WS_MAXIMIZE } else { 0 }
                        | if window.minimized { WS_MINIMIZE } else { 0 }
                }
                -20 => window.ex_style,
                -21 => window.user_data,
                i if i >= 0 => {
                    let i = i as usize;
                    if i + 4 <= window.extra.len() {
                        u32::from_le_bytes([
                            window.extra[i],
                            window.extra[i + 1],
                            window.extra[i + 2],
                            window.extra[i + 3],
                        ])
                    } else {
                        0
                    }
                }
                _ => 0,
            })
        }
        "SetWindowLongA" => {
            let index = a(1);
            let value = args[2];
            let mut shared = ctx.shared.borrow_mut();
            let Some(window) = shared.gui.window_mut(args[0]) else {
                return ok(0);
            };
            let previous = match index {
                -4 => std::mem::replace(&mut window.wndproc, value),
                -6 => std::mem::replace(&mut window.instance, value),
                -8 => std::mem::replace(&mut window.owner, value),
                -12 => std::mem::replace(&mut window.id, value),
                -16 => {
                    let previous = window.style;
                    window.style = value & !(WS_VISIBLE | WS_DISABLED | WS_MAXIMIZE | WS_MINIMIZE);
                    window.visible = value & WS_VISIBLE != 0;
                    window.disabled = value & WS_DISABLED != 0;
                    window.nc_dirty = true;
                    previous
                }
                -20 => {
                    window.nc_dirty = true;
                    std::mem::replace(&mut window.ex_style, value)
                }
                -21 => std::mem::replace(&mut window.user_data, value),
                i if i >= 0 => {
                    let i = i as usize;
                    if i + 4 <= window.extra.len() {
                        let previous = u32::from_le_bytes([
                            window.extra[i],
                            window.extra[i + 1],
                            window.extra[i + 2],
                            window.extra[i + 3],
                        ]);
                        window.extra[i..i + 4].copy_from_slice(&value.to_le_bytes());
                        if i == 4 && window.class == "#32770" {
                            window.dlg_proc = value;
                        }
                        previous
                    } else {
                        0
                    }
                }
                _ => 0,
            };
            ok(previous)
        }
        "GetPropA" | "RemovePropA" => {
            let key = prop_key(ctx, memory, args[1]);
            let mut shared = ctx.shared.borrow_mut();
            let Some(window) = shared.gui.window_mut(args[0]) else {
                return ok(0);
            };
            let value = window
                .props
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| *v)
                .unwrap_or(0);
            if name == "RemovePropA" {
                window.props.retain(|(k, _)| *k != key);
            }
            ok(value)
        }
        "SetPropA" => {
            let key = prop_key(ctx, memory, args[1]);
            let mut shared = ctx.shared.borrow_mut();
            let Some(window) = shared.gui.window_mut(args[0]) else {
                return ok(0);
            };
            window.props.retain(|(k, _)| *k != key);
            window.props.push((key, args[2]));
            ok(1)
        }
        "GetDlgCtrlID" => ok(ctx
            .shared
            .borrow()
            .gui
            .window(args[0])
            .map(|w| w.id)
            .unwrap_or(0)),

        // ── Geometry ──
        "GetClientRect" => {
            let size = if args[0] == DESKTOP_HWND || args[0] == 0 {
                Some(ctx.shared.borrow().gui.screen)
            } else {
                ctx.shared.borrow().gui.window(args[0]).map(client_size)
            };
            let Some((w, h)) = size else {
                return ok(0);
            };
            let _ = write_rect(memory, args[1], Rect::new(0, 0, w, h));
            ok(1)
        }
        "GetWindowRect" => {
            if !is_window(ctx, args[0]) {
                return ok(0);
            }
            let rect = window_screen_rect(&ctx.shared.borrow().gui, args[0]);
            let _ = write_rect(memory, args[1], rect);
            ok(1)
        }
        "ClientToScreen" | "ScreenToClient" => {
            let (cx, cy) = client_screen_origin(&ctx.shared.borrow().gui, args[0]);
            let x = memory.read_u32(args[1]).unwrap_or(0) as i32;
            let y = memory.read_u32(args[1] + 4).unwrap_or(0) as i32;
            let (nx, ny) = if name == "ClientToScreen" {
                (x + cx, y + cy)
            } else {
                (x - cx, y - cy)
            };
            let _ = memory.write_u32(args[1], nx as u32);
            let _ = memory.write_u32(args[1] + 4, ny as u32);
            ok(1)
        }
        "MapWindowPoints" => {
            let (fx, fy) = client_screen_origin(&ctx.shared.borrow().gui, args[0]);
            let (tx, ty) = client_screen_origin(&ctx.shared.borrow().gui, args[1]);
            let (dx, dy) = (fx - tx, fy - ty);
            for index in 0..args[3].min(4096) {
                let va = args[2] + index * 8;
                let x = memory.read_u32(va).unwrap_or(0) as i32;
                let y = memory.read_u32(va + 4).unwrap_or(0) as i32;
                let _ = memory.write_u32(va, (x + dx) as u32);
                let _ = memory.write_u32(va + 4, (y + dy) as u32);
            }
            ok(make_long(dx, dy))
        }
        "WindowFromPoint" => {
            let shared = ctx.shared.borrow();
            let (x, y) = (a(0), a(1));
            ok(match top_level_at(&shared.gui, x, y) {
                Some(top) => child_at(&shared.gui, top, x, y),
                None => DESKTOP_HWND,
            })
        }
        "ChildWindowFromPoint" => {
            let shared = ctx.shared.borrow();
            let (cx, cy) = client_screen_origin(&shared.gui, args[0]);
            let (sx, sy) = (cx + a(1), cy + a(2));
            let Some(window) = shared.gui.window(args[0]) else {
                return ok(0);
            };
            let (cw, ch) = client_size(window);
            if !Rect::new(0, 0, cw, ch).contains(a(1), a(2)) {
                return ok(0);
            }
            for child in children(&shared.gui, args[0]).into_iter().rev() {
                let Some(w) = shared.gui.window(child) else {
                    continue;
                };
                if Rect::sized(cx + w.x, cy + w.y, w.width, w.height).contains(sx, sy) {
                    return ok(child);
                }
            }
            ok(args[0])
        }
        "MoveWindow" => {
            let flags = 0x4 | 0x10 | if args[5] == 0 { 0x8 } else { 0 };
            ok(set_window_pos(ctx, memory, args[0], 0, a(1), a(2), a(3), a(4), flags) as u32)
        }
        "SetWindowPos" => ok(set_window_pos(
            ctx,
            memory,
            args[0],
            args[1],
            a(2),
            a(3),
            a(4),
            a(5),
            args[6],
        ) as u32),
        "BringWindowToTop" => {
            ok(set_window_pos(ctx, memory, args[0], 0, 0, 0, 0, 0, 0x1 | 0x2) as u32)
        }
        "BeginDeferWindowPos" => {
            let mut shared = ctx.shared.borrow_mut();
            let handle = shared.alloc_handle();
            shared.gui.deferred.insert(handle, Vec::new());
            ok(handle)
        }
        "DeferWindowPos" => {
            let mut shared = ctx.shared.borrow_mut();
            if let Some(list) = shared.gui.deferred.get_mut(&args[0]) {
                list.push([
                    args[1], args[2], args[3], args[4], args[5], args[6], args[7],
                ]);
                ok(args[0])
            } else {
                ok(0)
            }
        }
        "EndDeferWindowPos" => {
            let list = ctx
                .shared
                .borrow_mut()
                .gui
                .deferred
                .remove(&args[0])
                .unwrap_or_default();
            for entry in list {
                set_window_pos(
                    ctx,
                    memory,
                    entry[0],
                    entry[1],
                    entry[2] as i32,
                    entry[3] as i32,
                    entry[4] as i32,
                    entry[5] as i32,
                    entry[6],
                );
            }
            ok(1)
        }
        "GetWindowPlacement" => {
            let shared = ctx.shared.borrow();
            let Some(window) = shared.gui.window(args[0]) else {
                return ok(0);
            };
            let show = if window.minimized {
                2
            } else if window.maximized {
                3
            } else {
                1
            };
            let normal = Rect::sized(window.x, window.y, window.width, window.height);
            let values = [
                44u32,
                0,
                show,
                u32::MAX,
                u32::MAX,
                u32::MAX,
                u32::MAX,
                normal.left as u32,
                normal.top as u32,
                normal.right as u32,
                normal.bottom as u32,
            ];
            drop(shared);
            for (index, value) in values.iter().enumerate() {
                let _ = memory.write_u32(args[1] + index as u32 * 4, *value);
            }
            ok(1)
        }
        "SetWindowPlacement" => {
            let show = memory.read_u32(args[1] + 8).unwrap_or(1);
            let rect = read_rect(memory, args[1] + 28).unwrap_or_default();
            // USER keeps restored placements on the work area: a rectangle
            // that would be off screen keeps the current position.
            let (sw, sh) = ctx.shared.borrow().gui.screen;
            let work = Rect::new(0, 0, sw, sh - TASKBAR_HEIGHT);
            let current = ctx
                .shared
                .borrow()
                .gui
                .window(args[0])
                .map(|w| Rect::sized(w.x, w.y, w.width, w.height));
            if let Some(current) = current {
                let sane_size = rect.width() > 0
                    && rect.height() > 0
                    && rect.width() <= 4 * sw
                    && rect.height() <= 4 * sh;
                let (width, height) = if sane_size {
                    (rect.width().min(sw), rect.height().min(sh - TASKBAR_HEIGHT))
                } else {
                    (current.width(), current.height())
                };
                let visible =
                    rect.intersect(&work).width() >= 32 && rect.intersect(&work).height() >= 16;
                let (x, y) = if visible && sane_size {
                    (rect.left, rect.top)
                } else {
                    (current.left, current.top)
                };
                let x = x.clamp(work.left, (work.right - width).max(work.left));
                let y = y.clamp(work.top, (work.bottom - height).max(work.top));
                set_window_pos(ctx, memory, args[0], 0, x, y, width, height, 0x4 | 0x10);
            }
            let visible = ctx
                .shared
                .borrow()
                .gui
                .window(args[0])
                .map(|w| w.visible)
                .unwrap_or(false);
            if visible || show != 0 {
                show_window(ctx, memory, args[0], show);
            }
            ok(1)
        }
        "AdjustWindowRect" | "AdjustWindowRectEx" => {
            let Some(rect) = read_rect(memory, args[0]) else {
                return ok(0);
            };
            let ex = if name == "AdjustWindowRectEx" {
                args[3]
            } else {
                0
            };
            let (l, t, r, b) =
                nc_insets_for(args[1] & !(WS_VSCROLL | WS_HSCROLL), ex, args[2] != 0);
            let _ = write_rect(
                memory,
                args[0],
                Rect::new(rect.left - l, rect.top - t, rect.right + r, rect.bottom + b),
            );
            ok(1)
        }
        "ShowOwnedPopups" => ok(1),
        "LockWindowUpdate" => ok(1),

        // ── Painting ──
        "BeginPaint" => ok(begin_paint(ctx, memory, args[0], args[1])),
        "EndPaint" => {
            let hdc = crate::win32::x64::View::new(memory, &crate::win32::x64::PAINTSTRUCT, args[1]).get32(memory, 0);
            end_paint(ctx, args[0], hdc);
            ok(1)
        }
        "GetDC" | "GetWindowDC" | "GetDCEx" => {
            let window_dc = name == "GetWindowDC" || (name == "GetDCEx" && args[2] & 1 != 0);
            let target = if args[0] == 0 || args[0] == DESKTOP_HWND {
                DcTarget::Screen
            } else if is_window(ctx, args[0]) {
                DcTarget::Window {
                    hwnd: args[0],
                    client: !window_dc,
                    paint: None,
                }
            } else {
                return ok(0);
            };
            ok(ctx.shared.borrow_mut().gdi.new_dc(target))
        }
        "ReleaseDC" => {
            ctx.shared.borrow_mut().gdi.dcs.remove(&args[1]);
            ok(1)
        }
        "InvalidateRect" => {
            let rect = read_rect(memory, args[1]);
            let mut shared = ctx.shared.borrow_mut();
            if args[0] == 0 {
                let tops: Vec<u32> = shared
                    .gui
                    .windows
                    .iter()
                    .filter(|w| w.style & WS_CHILD == 0)
                    .map(|w| w.hwnd)
                    .collect();
                for top in tops {
                    invalidate_all(&mut shared.gui, top);
                }
            } else {
                invalidate(&mut shared.gui, args[0], rect, args[2] != 0, false);
            }
            ok(1)
        }
        "ValidateRect" => {
            let rect = read_rect(memory, args[1]);
            validate(&mut ctx.shared.borrow_mut().gui, args[0], rect);
            ok(1)
        }
        "RedrawWindow" => {
            const RDW_INVALIDATE: u32 = 0x1;
            const RDW_ERASE: u32 = 0x4;
            const RDW_VALIDATE: u32 = 0x8;
            const RDW_FRAME: u32 = 0x400;
            const RDW_ALLCHILDREN: u32 = 0x80;
            const RDW_UPDATENOW: u32 = 0x100;
            let flags = args[3];
            let rect = read_rect(memory, args[1]);
            let rect = if args[2] != 0 {
                match ctx.shared.borrow().gdi.objects.get(&args[2]) {
                    Some(gdi::GdiObject::Region(region)) => Some(region.bounds()),
                    _ => rect,
                }
            } else {
                rect
            };
            let hwnd = if args[0] == 0 {
                ctx.shared.borrow().gui.main_hwnd
            } else {
                args[0]
            };
            {
                let mut shared = ctx.shared.borrow_mut();
                if flags & RDW_INVALIDATE != 0 {
                    invalidate(
                        &mut shared.gui,
                        hwnd,
                        rect,
                        flags & RDW_ERASE != 0,
                        flags & RDW_ALLCHILDREN != 0,
                    );
                }
                if flags & RDW_FRAME != 0 {
                    if let Some(window) = shared.gui.window_mut(hwnd) {
                        window.nc_dirty = true;
                    }
                }
                if flags & RDW_VALIDATE != 0 {
                    validate(&mut shared.gui, hwnd, rect);
                }
            }
            if flags & RDW_UPDATENOW != 0 {
                update_window(ctx, memory, hwnd);
            }
            ok(1)
        }
        "UpdateWindow" => {
            update_window(ctx, memory, args[0]);
            ok(1)
        }
        "GetUpdateRect" => {
            let bounds = ctx
                .shared
                .borrow()
                .gui
                .window(args[0])
                .map(|w| w.update.bounds())
                .unwrap_or_default();
            if args[1] != 0 {
                let _ = write_rect(memory, args[1], bounds);
            }
            if args[2] != 0 && !bounds.is_empty() {
                let erase = ctx
                    .shared
                    .borrow()
                    .gui
                    .window(args[0])
                    .map(|w| w.erase)
                    .unwrap_or(false);
                if erase {
                    let hdc = ctx.shared.borrow_mut().gdi.new_dc(DcTarget::Window {
                        hwnd: args[0],
                        client: true,
                        paint: None,
                    });
                    send(ctx, memory, args[0], WM_ERASEBKGND, hdc, 0);
                    ctx.shared.borrow_mut().gdi.dcs.remove(&hdc);
                    if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(args[0]) {
                        w.erase = false;
                    }
                }
            }
            ok((!bounds.is_empty()) as u32)
        }
        "ScrollWindow" | "ScrollWindowEx" => {
            let (hwnd, dx, dy) = (args[0], a(1), a(2));
            let rect = read_rect(memory, args[3]);
            scroll_window(ctx, hwnd, dx, dy, rect);
            ok(1)
        }

        // ── Messages ──
        "GetMessageA" => {
            // On a shared desktop, wait by yielding to the other programs:
            // the call is made again when this program next runs.
            if memory.can_yield() {
                if let Some(message) =
                    fetch_message(ctx, memory, args[1], args[2], args[3], true, false)
                {
                    write_message(ctx, memory, args[0], message);
                    return ok((message.message != WM_QUIT) as u32);
                }
                if memory.yield_now() {
                    return ok(0);
                }
            }
            let message = fetch_message(ctx, memory, args[1], args[2], args[3], true, true);
            let message = message.unwrap_or(GuestMessage {
                hwnd: 0,
                message: WM_QUIT,
                wparam: 0,
                lparam: 0,
            });
            write_message(ctx, memory, args[0], message);
            ok((message.message != WM_QUIT) as u32)
        }
        "PeekMessageA" => {
            let remove = args[4] & 1 != 0;
            match fetch_message(ctx, memory, args[1], args[2], args[3], remove, false) {
                Some(message) => {
                    write_message(ctx, memory, args[0], message);
                    ok(1)
                }
                None => ok(0),
            }
        }
        "WaitMessage" => {
            loop {
                advance_timed_script(ctx, memory);
                let thread = memory.current_thread();
                let ready = {
                    let mut shared = ctx.shared.borrow_mut();
                    let now = shared.ticks();
                    messages_for(&shared.gui, now, thread)
                        || shared.gui.pending_activate.is_some()
                        || shared.gui.pending_deactivate
                        // Only windows that are shown get painted.
                        || paint_order(&shared.gui).iter().any(|h| {
                            owned_by(&shared.gui, *h, thread)
                                && is_visible_chain(&shared.gui, *h)
                                && shared
                                    .gui
                                    .window(*h)
                                    .map(|w| !w.update.is_empty() || w.nc_dirty)
                                    .unwrap_or(false)
                        })
                        || {
                            let _ = &mut shared;
                            false
                        }
                };
                if ready || memory.stopping() {
                    break;
                }
                // Other threads of the program run while this one waits.
                if crate::win32::sync::run_ready_threads(ctx, memory) {
                    continue;
                }
                if !memory.has_host() {
                    // Headless: wait as GetMessage does (scripted input, idle
                    // exit), then leave the message for the PeekMessage that
                    // follows.
                    if let Some(message) = fetch_message(ctx, memory, 0, 0, 0, true, true) {
                        // A program whose loop ignores the idle WM_QUIT ends
                        // once it has been idle a few times over.
                        if message.message == WM_QUIT && ctx.shared.borrow().gui.idle_exits >= 3 {
                            return Some(ApiOutcome {
                                wide: None,
                                value: 0,
                                arg_count: count,
                                exit_code: Some(0),
                            });
                        }
                        ctx.shared.borrow_mut().gui.posted.push_front(message);
                    }
                    break;
                }
                if memory.yield_now() {
                    return ok(0);
                }
                memory.pump(true);
            }
            ok(1)
        }
        "MsgWaitForMultipleObjects" => {
            let messages = {
                let shared = ctx.shared.borrow();
                !shared.gui.queue.is_empty()
                    || !shared.gui.posted.is_empty()
                    || !shared.gui.input.is_empty()
            };
            if !messages {
                if memory.yield_now() {
                    return ok(0);
                }
                memory.pump(true);
            }
            ok(if messages { args[0] } else { 0x0102 })
        }
        "TranslateMessage" => {
            let message = read_message(memory, args[0]);
            if message.message == WM_KEYDOWN || message.message == WM_SYSKEYDOWN {
                let mut shared = ctx.shared.borrow_mut();
                let position = shared.gui.pending_chars.iter().position(|m| {
                    m.hwnd == message.hwnd && m.lparam & 0x00FF_0000 == message.lparam & 0x00FF_0000
                });
                if let Some(position) = position {
                    let char_message = shared.gui.pending_chars.remove(position).unwrap();
                    shared.gui.posted.push_front(char_message);
                    return ok(1);
                }
            }
            ok(0)
        }
        "DispatchMessageA" => {
            let message = read_message(memory, args[0]);
            ok(dispatch_message(ctx, memory, message))
        }
        "SendMessageA" => {
            let debug = std::env::var_os("OTW_DEBUG_REBAR").is_some() && matches!(args[1], 0x401 | 0x406 | 0x40A | 0x40B);
            if debug {
                let words: Vec<u32> = (0..20).map(|i| memory.read_u32(args[3] + i * 4).unwrap_or(0)).collect();
                eprintln!("[rebar] msg={:#x} band={} info={:x?}", args[1], args[2], words);
            }
            let result = send(ctx, memory, args[0], args[1], args[2], args[3]);
            if let Ok(target) = std::env::var("OTW_DEBUG_SEND") {
                if u32::from_str_radix(target.trim_start_matches("0x"), 16).ok() == Some(args[0]) {
                    let after: Vec<u32> = (0..4).map(|i| memory.read_u32(args[3].wrapping_add(i * 4)).unwrap_or(0)).collect();
                    eprintln!("[send] {:#x} msg={:#x} w={:#x} l={:#x} -> {:#x} l*={:x?}", args[0], args[1], args[2], args[3], result, after);
                }
            }
            ok(result)
        }
        "PostMessageA" => {
            if args[0] == 0xFFFF {
                let tops: Vec<u32> = ctx
                    .shared
                    .borrow()
                    .gui
                    .windows
                    .iter()
                    .filter(|w| w.style & WS_CHILD == 0)
                    .map(|w| w.hwnd)
                    .collect();
                for top in tops {
                    post(ctx, top, args[1], args[2], args[3]);
                }
            } else {
                post(ctx, args[0], args[1], args[2], args[3]);
            }
            ok(1)
        }
        "PostThreadMessageA" => {
            post(ctx, 0, args[1], args[2], args[3]);
            ok(1)
        }
        "PostQuitMessage" => {
            let mut shared = ctx.shared.borrow_mut();
            shared.gui.quit = true;
            shared.gui.quit_thread = memory.current_thread();
            shared.gui.quit_code = args[0];
            ok(0)
        }
        "InSendMessage" => ok(0),
        "SetMessageQueue" => ok(1),
        "GetMessagePos" => {
            let (x, y) = ctx.shared.borrow().gui.message_pos;
            ok(make_long(x, y))
        }
        "GetMessageTime" => ok(ctx.shared.borrow().gui.message_time),
        "SetWindowsHookExA" => {
            let mut shared = ctx.shared.borrow_mut();
            let handle = shared.alloc_handle();
            shared.gui.hooks.push(GuestHook {
                handle,
                kind: a(0),
                proc_va: args[1],
            });
            ok(handle)
        }
        "UnhookWindowsHookEx" => {
            ctx.shared
                .borrow_mut()
                .gui
                .hooks
                .retain(|h| h.handle != args[0]);
            ok(1)
        }
        "CallNextHookEx" => {
            let kind = ctx
                .shared
                .borrow()
                .gui
                .hooks
                .iter()
                .find(|h| h.handle == args[0])
                .map(|h| h.kind);
            match kind {
                Some(kind) => ok(
                    call_hooks(ctx, memory, kind, args[0], args[1], args[2], args[3]).unwrap_or(0),
                ),
                None => ok(0),
            }
        }
        "CallMsgFilterA" => {
            let result = call_hooks(ctx, memory, -1, 0, args[1], 0, args[0]).unwrap_or(0);
            ok(result)
        }
        "SetTimer" => {
            let mut shared = ctx.shared.borrow_mut();
            let now = shared.ticks();
            let id = if args[0] == 0 {
                0x7000 + shared.gui.timers.len() as u32
            } else {
                args[1]
            };
            shared
                .gui
                .timers
                .retain(|t| !(t.hwnd == args[0] && t.id == id));
            shared.gui.timers.push(GuestTimer {
                hwnd: args[0],
                thread: memory.current_thread(),
                id,
                interval: args[2],
                proc_va: args[3],
                due: now + args[2].max(10) as u64,
            });
            ok(id)
        }
        "KillTimer" => {
            let mut shared = ctx.shared.borrow_mut();
            let before = shared.gui.timers.len();
            shared
                .gui
                .timers
                .retain(|t| !(t.hwnd == args[0] && t.id == args[1]));
            ok((before != shared.gui.timers.len()) as u32)
        }

        // ── Input state ──
        "GetFocus" => ok(ctx.shared.borrow().gui.focus_hwnd),
        "SetFocus" => ok(set_focus(ctx, memory, args[0])),
        "GetActiveWindow" | "GetForegroundWindow" => ok(ctx.shared.borrow().gui.active_hwnd),
        "SetActiveWindow" | "SetForegroundWindow" => {
            let previous = ctx.shared.borrow().gui.active_hwnd;
            activate(ctx, memory, args[0], 1);
            ok(if name == "SetActiveWindow" {
                previous
            } else {
                1
            })
        }
        "GetCapture" => ok(ctx.shared.borrow().gui.capture),
        "SetCapture" => {
            let mut shared = ctx.shared.borrow_mut();
            let previous = shared.gui.capture;
            shared.gui.capture = args[0];
            ok(previous)
        }
        "ReleaseCapture" => {
            let previous = std::mem::take(&mut ctx.shared.borrow_mut().gui.capture);
            if previous != 0 {
                send(ctx, memory, previous, WM_CAPTURECHANGED, 0, 0);
            }
            ok(1)
        }
        "GetKeyState" | "GetAsyncKeyState" => {
            let shared = ctx.shared.borrow();
            let vk = (args[0] & 0xFF) as usize;
            let state = match vk {
                0xA0 | 0xA1 => shared.gui.keys[0x10],
                0xA2 | 0xA3 => shared.gui.keys[0x11],
                0xA4 | 0xA5 => shared.gui.keys[0x12],
                _ => shared.gui.keys[vk],
            };
            let value: u32 = if state & 0x80 != 0 { 0xFFFF_8000 } else { 0 } | (state & 1) as u32;
            ok(if name == "GetKeyState" {
                value
            } else {
                value & 0xFFFF_8000
            })
        }
        "GetKeyboardState" => {
            let keys = ctx.shared.borrow().gui.keys.clone();
            let _ = memory.write_bytes(args[0], &keys);
            ok(1)
        }
        "GetCursorPos" => {
            let (x, y) = ctx.shared.borrow().gui.cursor;
            let _ = memory.write_u32(args[0], x as u32);
            let _ = memory.write_u32(args[0] + 4, y as u32);
            ok(1)
        }
        "SetCursorPos" => {
            ctx.shared.borrow_mut().gui.cursor = (a(0), a(1));
            ok(1)
        }
        "ClipCursor" => ok(1),
        "SetCursor" => ok(0),
        "GetCursor" => ok(0),
        "GetDoubleClickTime" => ok(500),
        "GetKBCodePage" => ok(437),

        // ── Caret ──
        "CreateCaret" => {
            ctx.shared.borrow_mut().gui.caret = (args[0], 0, 0, a(2).max(1), a(3).max(1), false);
            ok(1)
        }
        "SetCaretPos" => {
            let mut shared = ctx.shared.borrow_mut();
            shared.gui.caret.1 = a(0);
            shared.gui.caret.2 = a(1);
            ok(1)
        }
        "GetCaretPos" => {
            let (x, y) = {
                let shared = ctx.shared.borrow();
                (shared.gui.caret.1, shared.gui.caret.2)
            };
            let _ = memory.write_u32(args[0], x as u32);
            let _ = memory.write_u32(args[0] + 4, y as u32);
            ok(1)
        }
        "ShowCaret" | "HideCaret" | "DestroyCaret" => ok(1),

        // ── Rectangles ──
        "SetRect" => {
            let _ = write_rect(memory, args[0], Rect::new(a(1), a(2), a(3), a(4)));
            ok(1)
        }
        "SetRectEmpty" => {
            let _ = write_rect(memory, args[0], Rect::default());
            ok(1)
        }
        "CopyRect" => {
            let rect = read_rect(memory, args[1]).unwrap_or_default();
            let _ = write_rect(memory, args[0], rect);
            ok(1)
        }
        "EqualRect" => ok((read_rect(memory, args[0]) == read_rect(memory, args[1])) as u32),
        "IsRectEmpty" => ok(read_rect(memory, args[0])
            .map(|r| r.is_empty())
            .unwrap_or(true) as u32),
        "InflateRect" => {
            let rect = read_rect(memory, args[0]).unwrap_or_default();
            let _ = write_rect(
                memory,
                args[0],
                Rect::new(
                    rect.left - a(1),
                    rect.top - a(2),
                    rect.right + a(1),
                    rect.bottom + a(2),
                ),
            );
            ok(1)
        }
        "OffsetRect" => {
            let rect = read_rect(memory, args[0]).unwrap_or_default();
            let _ = write_rect(memory, args[0], rect.offset(a(1), a(2)));
            ok(1)
        }
        "PtInRect" => ok(read_rect(memory, args[0])
            .map(|r| r.contains(a(1), a(2)))
            .unwrap_or(false) as u32),
        "IntersectRect" => {
            let r1 = read_rect(memory, args[1]).unwrap_or_default();
            let r2 = read_rect(memory, args[2]).unwrap_or_default();
            let result = r1.intersect(&r2);
            let _ = write_rect(memory, args[0], result);
            ok((!result.is_empty()) as u32)
        }
        "UnionRect" => {
            let r1 = read_rect(memory, args[1]).unwrap_or_default();
            let r2 = read_rect(memory, args[2]).unwrap_or_default();
            let result = r1.union(&r2);
            let _ = write_rect(memory, args[0], result);
            ok((!result.is_empty()) as u32)
        }
        "SubtractRect" => {
            let r1 = read_rect(memory, args[1]).unwrap_or_default();
            let r2 = read_rect(memory, args[2]).unwrap_or_default();
            let region = Region::from_rect(r1).subtract_rect(&r2);
            let result = if region.rects.len() == 1 {
                region.rects[0]
            } else {
                r1
            };
            let _ = write_rect(memory, args[0], result);
            ok((!result.is_empty()) as u32)
        }
        "FillRect" | "FrameRect" | "InvertRect" | "DrawFocusRect" => {
            let rect = read_rect(memory, args[1]).unwrap_or_default();
            let brush = if count > 2 { args[2] } else { 0 };
            let brush = if brush != 0 && brush <= 31 {
                SYSBRUSH_BASE + (brush - 1) * 4
            } else {
                brush
            };
            let fill = {
                let shared = ctx.shared.borrow();
                let attrs = shared
                    .gdi
                    .dcs
                    .get(&args[0])
                    .map(|dc| dc.attrs.clone())
                    .unwrap_or_default();
                shared.gdi.fill_of(&attrs, brush)
            };
            gdi::with_dc(ctx, memory, args[0], |target, dctx| {
                let device = dctx.rect(rect.left, rect.top, rect.right, rect.bottom);
                match name {
                    "FillRect" => target.fill_rect(
                        device,
                        &fill,
                        gfx::R2_COPYPEN,
                        dctx.brush_origin(),
                        dctx.mono,
                    ),
                    "FrameRect" => {
                        let band = Region::from_rect(device).subtract_rect(&device.inflate(-1, -1));
                        for piece in band.rects {
                            target.fill_rect(
                                piece,
                                &fill,
                                gfx::R2_COPYPEN,
                                dctx.brush_origin(),
                                dctx.mono,
                            );
                        }
                    }
                    "InvertRect" => {
                        target.pat_blt(device, &gfx::Fill::Solid(0), 0x55, (0, 0), dctx.mono)
                    }
                    _ => {
                        // Dotted XOR rectangle.
                        let mut points = Vec::new();
                        for x in device.left..device.right {
                            points.push((x, device.top));
                            points.push((x, device.bottom - 1));
                        }
                        for y in device.top + 1..device.bottom - 1 {
                            points.push((device.left, y));
                            points.push((device.right - 1, y));
                        }
                        for (x, y) in points {
                            if (x + y) % 2 == 0 {
                                target.plot(x, y, 0xFFFFFF, 7);
                            }
                        }
                    }
                }
            });
            ok(1)
        }
        "DrawEdge" => {
            let rect = read_rect(memory, args[1]).unwrap_or_default();
            let edge = args[2];
            let colors = |outer_raised: bool| {
                if outer_raised {
                    (sys_color(22), sys_color(21))
                } else {
                    (sys_color(16), sys_color(20))
                }
            };
            gdi::with_dc(ctx, memory, args[0], |target, dctx| {
                let device = dctx.rect(rect.left, rect.top, rect.right, rect.bottom);
                let mut painter = Painter {
                    surface: target.surface,
                    clip: target.clip.clone(),
                    ox: 0,
                    oy: 0,
                };
                let (otl, obr) = colors(edge & 0x1 != 0);
                let (itl, ibr) = if edge & 0x4 != 0 {
                    (sys_color(20), sys_color(16))
                } else {
                    (sys_color(21), sys_color(22))
                };
                painter.edge(device, otl, itl, obr, ibr);
            });
            ok(1)
        }
        "DrawIcon" => ok(1),

        // ── Text ──
        "DrawTextA" | "DrawTextExA" => {
            let length = a(2);
            let text = if length < 0 {
                read_cstr(memory, args[1])
            } else {
                memory
                    .read_bytes(args[1], length as usize)
                    .unwrap_or_default()
            };
            ok(gdi::draw_text_api(ctx, memory, args[0], &text, args[3], args[4]) as u32)
        }
        "GrayStringA" => {
            let (hdc, output_proc, data, length, x, y) =
                (args[0], args[2], args[3], a(4), a(5), a(6));
            if output_proc != 0 {
                let result = memory
                    .call_guest(output_proc, &[hdc, data, length.max(0) as u32])
                    .unwrap_or(0);
                return ok(result);
            }
            let text = if length <= 0 {
                read_cstr(memory, data)
            } else {
                memory.read_bytes(data, length as usize).unwrap_or_default()
            };
            gdi::with_dc(ctx, memory, hdc, |target, dctx| {
                let (sx, sy) = dctx.pt(x, y);
                gfx::draw_text(target, dctx.face, sx, sy, &text, sys_color(17), false);
            });
            ok(1)
        }
        "TabbedTextOutA" | "GetTabbedTextExtentA" => {
            let (hdc, text_va, length) = if name == "TabbedTextOutA" {
                (args[0], args[3], a(4))
            } else {
                (args[0], args[1], a(2))
            };
            let (tab_count, tabs_va) = if name == "TabbedTextOutA" {
                (args[5], args[6])
            } else {
                (args[3], args[4])
            };
            let text = memory
                .read_bytes(text_va, length.max(0) as usize)
                .unwrap_or_default();
            let tabs: Vec<i32> = (0..tab_count.min(64))
                .map(|i| memory.read_u32(tabs_va + i * 4).unwrap_or(0) as i32)
                .collect();
            let (_, height) = gdi::text_extent(ctx, hdc, b"X");
            let mut x = 0;
            let mut segments = Vec::new();
            for (index, part) in text.split(|b| *b == b'\t').enumerate() {
                if index > 0 {
                    let stop = if tabs.is_empty() {
                        (x / 64 + 1) * 64
                    } else if tabs.len() == 1 {
                        let step = tabs[0].max(1);
                        (x / step + 1) * step
                    } else {
                        tabs.iter().copied().find(|t| *t > x).unwrap_or(x + 8)
                    };
                    x = stop;
                }
                segments.push((x, part.to_vec()));
                x += gdi::text_extent(ctx, hdc, part).0;
            }
            if name == "TabbedTextOutA" {
                let (ox, oy) = (a(1), a(2));
                gdi::with_dc(ctx, memory, hdc, |target, dctx| {
                    for (offset, part) in &segments {
                        let (sx, sy) = dctx.pt(ox + offset, oy);
                        gdi::text_run(target, dctx, sx, sy, part, None, None);
                    }
                });
            }
            ok(make_long(x, height))
        }

        // ── Resources ──
        "LoadStringA" => {
            let module = if args[0] != 0 {
                args[0]
            } else {
                ctx.modules
                    .borrow().iter()
                    .find(|m| caller >= m.image_base && caller < m.image_base.wrapping_add(m.size))
                    .map(|m| m.image_base)
                    .unwrap_or(ctx.image_base)
            };
            let mut text = resource_string(memory, module, args[1]).unwrap_or_default();
            if text.is_empty() {
                for candidate in ctx.modules.borrow().iter().map(|m| m.image_base).collect::<Vec<_>>() {
                    if let Some(found) = resource_string(memory, candidate, args[1]) {
                        text = found;
                        break;
                    }
                }
            }
            if args[2] == 0 || args[3] == 0 {
                return ok(0);
            }
            let copied = text.len().min(args[3] as usize - 1);
            let _ = write_cstr(memory, args[2], &text[..copied]);
            ok(copied as u32)
        }
        "LoadCursorA" => {
            let mut shared = ctx.shared.borrow_mut();
            let handle = shared.alloc_handle();
            ok(handle | 0x0050_0000)
        }
        "LoadIconA" => ok(load_icon(ctx, memory, args[0], args[1])),
        "LoadImageA" => {
            if args[2] == 0 {
                let handle = load_bitmap(ctx, memory, args[0], args[1]);
                return ok(handle);
            }
            if args[2] == 1 {
                // IMAGE_ICON
                return ok(load_icon(ctx, memory, args[0], args[1]));
            }
            let mut shared = ctx.shared.borrow_mut();
            let handle = shared.alloc_handle();
            ok(handle | 0x0050_0000)
        }
        "LoadBitmapA" => ok(load_bitmap(ctx, memory, args[0], args[1])),
        "DestroyCursor" | "DestroyIcon" => ok(1),
        "CreateIcon" | "CreateIconIndirect" => {
            let mut shared = ctx.shared.borrow_mut();
            let handle = shared.alloc_handle();
            ok(handle | 0x0050_0000)
        }

        // ── System ──
        "GetSystemMetrics" => {
            let (sw, sh) = ctx.shared.borrow().gui.screen;
            ok(system_metric(args[0], sw, sh) as u32)
        }
        "GetSysColor" => ok(gfx::rgb_to_colorref(sys_color(args[0]))),
        "GetSysColorBrush" => ok(SYSBRUSH_BASE + args[0].min(30) * 4),
        "SystemParametersInfoA" => {
            match args[0] {
                0x30 => {
                    let (sw, sh) = ctx.shared.borrow().gui.screen;
                    let _ = write_rect(memory, args[2], Rect::new(0, 0, sw, sh - TASKBAR_HEIGHT));
                }
                // SPI_GETICONTITLELOGFONT
                0x1F => {
                    let _ = memory.write_bytes(args[2], &system_logfont(400));
                }
                // SPI_GETNONCLIENTMETRICS: Windows 95's standard sizes, with
                // MS Sans Serif 8 (bold for captions).
                0x29 => {
                    let mut metrics = Vec::with_capacity(340);
                    metrics.extend_from_slice(&340u32.to_le_bytes());
                    for value in [1u32, 16, 16, 18, 18] {
                        metrics.extend_from_slice(&value.to_le_bytes());
                    }
                    metrics.extend(system_logfont(700));
                    for value in [12u32, 14] {
                        metrics.extend_from_slice(&value.to_le_bytes());
                    }
                    metrics.extend(system_logfont(700));
                    for value in [18u32, 18] {
                        metrics.extend_from_slice(&value.to_le_bytes());
                    }
                    for _ in 0..3 {
                        metrics.extend(system_logfont(400));
                    }
                    if args[2] != 0 {
                        let _ = memory.write_bytes(args[2], &metrics);
                    }
                }
                0x05 => {
                    if args[2] != 0 {
                        let _ = memory.write_u32(args[2], 1);
                    }
                }
                0x26 | 0x2A => {
                    if args[2] != 0 {
                        let _ = memory.write_u32(args[2], 0);
                    }
                }
                _ => {
                    if args[2] != 0 && args[0] & 0x1 == 0 && args[0] < 0x1000 {
                        let _ = memory.write_u32(args[2], 0);
                    }
                }
            }
            ok(1)
        }
        "MessageBeep" => ok(1),
        "MessageBoxA" => {
            let text = read_cstr(memory, args[1]);
            let caption = read_cstr(memory, args[2]);
            ok(crate::win32::controls::message_box(
                ctx, memory, args[0], &text, &caption, args[3],
            ))
        }
        // No help engine: HELP_QUIT (closing a help window that is not
        // open) succeeds; requests to show help fail.
        "WinHelpA" => ok(u32::from(args[2] == 2)),
        "GetShellWindow" => ok(0),
        "RegisterWindowMessageA" | "RegisterClipboardFormatA" => {
            let text = String::from_utf8_lossy(&read_cstr(memory, args[0])).to_ascii_lowercase();
            let mut shared = ctx.shared.borrow_mut();
            let index = match shared.gui.atoms.iter().position(|a| *a == text) {
                Some(index) => index,
                None => {
                    shared.gui.atoms.push(text);
                    shared.gui.atoms.len() - 1
                }
            };
            ok(0xC000 + index as u32)
        }

        // ── Strings ──
        "CharNextA" => {
            let byte = memory.read_bytes(args[0], 1).map(|b| b[0]).unwrap_or(0);
            ok(if byte == 0 { args[0] } else { args[0] + 1 })
        }
        "CharPrevA" => ok(if args[1] > args[0] {
            args[1] - 1
        } else {
            args[0]
        }),
        "CharUpperA" | "CharLowerA" => {
            if args[0] < 0x10000 {
                let ch = args[0] as u8;
                return ok(if name == "CharUpperA" {
                    ch.to_ascii_uppercase()
                } else {
                    ch.to_ascii_lowercase()
                } as u32);
            }
            let text = read_cstr(memory, args[0]);
            let converted = if name == "CharUpperA" {
                text.to_ascii_uppercase()
            } else {
                text.to_ascii_lowercase()
            };
            let _ = memory.write_bytes(args[0], &converted);
            ok(args[0])
        }
        "CharLowerBuffA" | "CharUpperBuffA" => {
            let bytes = memory
                .read_bytes(args[0], args[1] as usize)
                .unwrap_or_default();
            let converted = if name == "CharLowerBuffA" {
                bytes.to_ascii_lowercase()
            } else {
                bytes.to_ascii_uppercase()
            };
            let _ = memory.write_bytes(args[0], &converted);
            ok(args[1])
        }
        "OemToCharA" | "CharToOemA" => {
            let text = read_cstr(memory, args[0]);
            let _ = write_cstr(memory, args[1], &text);
            ok(1)
        }
        "wsprintfA" => {
            let format = read_cstr(memory, args[1]);
            let text = crate::win32::controls::format_printf(memory, &format, &args[2..], None);
            let _ = write_cstr(memory, args[0], &text);
            Some(ApiOutcome::new(text.len() as u32, 0))
        }
        "wvsprintfA" => {
            let format = read_cstr(memory, args[1]);
            let text = crate::win32::controls::format_printf(memory, &format, &[], Some(args[2]));
            let _ = write_cstr(memory, args[0], &text);
            ok(text.len() as u32)
        }

        // ── Scroll bars ──
        "SetScrollRange" | "SetScrollPos" | "GetScrollPos" | "GetScrollRange" | "ShowScrollBar"
        | "EnableScrollBar" | "SetScrollInfo" | "GetScrollInfo" => {
            scroll_api(ctx, memory, name, args, count)
        }

        // ── DDE / MDI ──
        "UnpackDDElParam" => {
            if args[2] != 0 {
                let _ = memory.write_u32(args[2], args[1] & 0xFFFF);
            }
            if args[3] != 0 {
                let _ = memory.write_u32(args[3], args[1] >> 16);
            }
            ok(1)
        }
        "ReuseDDElParam" => ok(make_long(args[3] as i32, args[4] as i32)),
        "TranslateMDISysAccel" => ok(0),
        "CopyAcceleratorTableA" => crate::win32::menu::dispatch(ctx, memory, name, args, count),
        _ => None,
    }
}

/// A window property's name. A global atom names the same property as its
/// string.
fn prop_key<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, va: u32) -> String {
    if va < 0x10000 {
        let atom = va.checked_sub(0xC000).and_then(|index| ctx.shared.borrow().global_atoms.get(index as usize).cloned());
        match atom {
            Some(name) => name.to_ascii_lowercase(),
            None => format!("#{va}"),
        }
    } else {
        String::from_utf8_lossy(&read_cstr(memory, va)).to_ascii_lowercase()
    }
}

pub fn write_message<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    va: u32,
    message: GuestMessage,
) {
    if va == 0 {
        return;
    }
    let (time, (x, y)) = {
        let mut shared = ctx.shared.borrow_mut();
        let time = shared.ticks() as u32;
        shared.gui.message_time = time;
        (time, shared.gui.cursor)
    };
    // MSG in the process's layout (pointer-sized hwnd, wParam, lParam).
    let view = crate::win32::x64::View::new(memory, &crate::win32::x64::MSG, va);
    let values = [message.hwnd, message.message, message.wparam, message.lparam, time, x as u32, y as u32];
    for (index, value) in values.into_iter().enumerate() {
        view.set(memory, index, value);
    }
}

pub fn read_message<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> GuestMessage {
    let view = crate::win32::x64::View::new(memory, &crate::win32::x64::MSG, va);
    GuestMessage {
        hwnd: view.get32(memory, 0),
        message: view.get32(memory, 1),
        wparam: view.get32(memory, 2),
        lparam: view.get32(memory, 3),
    }
}

/// Send WM_PAINT now if the window has a pending update.
pub fn update_window<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let needs = {
        let mut shared = ctx.shared.borrow_mut();
        let nc = shared.gui.window(hwnd).map(|w| w.nc_dirty).unwrap_or(false);
        if nc && is_visible_chain(&shared.gui, hwnd) {
            if let Some(w) = shared.gui.window_mut(hwnd) {
                w.nc_dirty = false;
            }
            paint_nc(&mut shared, hwnd);
        }
        shared
            .gui
            .window(hwnd)
            .map(|w| !w.update.is_empty())
            .unwrap_or(false)
            && is_visible_chain(&shared.gui, hwnd)
    };
    if needs {
        dispatch_message(
            ctx,
            memory,
            GuestMessage {
                hwnd,
                message: WM_PAINT,
                wparam: 0,
                lparam: 0,
            },
        );
    }
}

/// Scroll a window's client area by moving surface pixels and child
/// windows, invalidating what was uncovered.
fn scroll_window(ctx: &ApiContext, hwnd: u32, dx: i32, dy: i32, rect: Option<Rect>) {
    let mut shared = ctx.shared.borrow_mut();
    let shared = &mut *shared;
    let Some(window) = shared.gui.window(hwnd).cloned() else {
        return;
    };
    let (cw, ch) = client_size(&window);
    let area = rect
        .unwrap_or(Rect::new(0, 0, cw, ch))
        .intersect(&Rect::new(0, 0, cw, ch));
    if rect.is_none() {
        for child in children(&shared.gui, hwnd) {
            if let Some(child_window) = shared.gui.window_mut(child) {
                child_window.x += dx;
                child_window.y += dy;
            }
        }
    }
    if let Some((top, ox, oy, clip)) = dc_geometry(&shared.gui, hwnd, true) {
        if let Some(surface) = shared.gdi.surfaces.get_mut(&top) {
            let source = surface.clone();
            let dest = area.offset(dx, dy).intersect(&area).offset(ox, oy);
            let clip = clip.intersect_rect(&dest);
            for band in &clip.rects {
                for y in band.top..band.bottom {
                    for x in band.left..band.right {
                        if let Some(pixel) = source.get(x - dx, y - dy) {
                            surface.put(x, y, pixel);
                        }
                    }
                }
            }
            shared.gui.dirty = true;
        }
    }
    // Invalidate the uncovered strips.
    let moved = area.offset(dx, dy).intersect(&area);
    let exposed = Region::from_rect(area).subtract_rect(&moved);
    for piece in exposed.rects {
        invalidate(&mut shared.gui, hwnd, Some(piece), true, false);
    }
    // Pending update regions move with the content.
    if let Some(w) = shared.gui.window_mut(hwnd) {
        w.update.offset(dx, dy);
    }
    if rect.is_none() {
        for child in children(&shared.gui, hwnd) {
            invalidate_all(&mut shared.gui, child);
        }
    }
    shared.gui.dirty = true;
}

fn scroll_api<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
    count: usize,
) -> Option<ApiOutcome> {
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    let hwnd = args[0];
    let bar = args[1];
    let is_ctl = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.class.eq_ignore_ascii_case("ScrollBar"))
        .unwrap_or(false);
    let index = if bar == 2 || is_ctl {
        0
    } else {
        bar.min(1) as usize
    };
    let redraw = |ctx: &ApiContext| {
        let mut shared = ctx.shared.borrow_mut();
        if is_ctl {
            invalidate(&mut shared.gui, hwnd, None, true, false);
        } else if let Some(w) = shared.gui.window_mut(hwnd) {
            w.nc_dirty = true;
            shared.gui.dirty = true;
        }
    };
    match name {
        "SetScrollRange" => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                let entry = &mut w.scroll[index];
                entry.0 = args[2] as i32;
                entry.1 = args[3] as i32;
                entry.2 = entry.2.clamp(entry.0, entry.1.max(entry.0));
            }
            if args[4] != 0 {
                redraw(ctx);
            }
            // Win95 shows or hides standard bars when the range changes.
            if !is_ctl && bar != 2 {
                let show = args[3] as i32 > args[2] as i32;
                show_scroll_bar(ctx, memory, hwnd, bar, show);
            }
            ok(1)
        }
        "SetScrollPos" => {
            let previous = {
                let mut shared = ctx.shared.borrow_mut();
                let Some(w) = shared.gui.window_mut(hwnd) else {
                    return ok(0);
                };
                let entry = &mut w.scroll[index];
                let previous = entry.2;
                entry.2 = (args[2] as i32).clamp(entry.0, entry.1.max(entry.0));
                previous
            };
            if args[3] != 0 {
                redraw(ctx);
            }
            ok(previous as u32)
        }
        "GetScrollPos" => ok(ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.scroll[index].2 as u32)
            .unwrap_or(0)),
        "GetScrollRange" => {
            let (min, max) = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| (w.scroll[index].0, w.scroll[index].1))
                .unwrap_or((0, 0));
            let _ = memory.write_u32(args[2], min as u32);
            let _ = memory.write_u32(args[3], max as u32);
            ok(1)
        }
        "ShowScrollBar" => {
            if bar == 3 {
                show_scroll_bar(ctx, memory, hwnd, 0, args[2] != 0);
                show_scroll_bar(ctx, memory, hwnd, 1, args[2] != 0);
            } else if bar == 2 {
                show_window(ctx, memory, hwnd, if args[2] != 0 { 5 } else { 0 });
            } else {
                show_scroll_bar(ctx, memory, hwnd, bar, args[2] != 0);
            }
            ok(1)
        }
        "EnableScrollBar" => ok(1),
        "SetScrollInfo" => {
            let mask = memory.read_u32(args[2] + 4).unwrap_or(0);
            let min = memory.read_u32(args[2] + 8).unwrap_or(0) as i32;
            let max = memory.read_u32(args[2] + 12).unwrap_or(0) as i32;
            let page = memory.read_u32(args[2] + 16).unwrap_or(0);
            let pos = memory.read_u32(args[2] + 20).unwrap_or(0) as i32;
            let position = {
                let mut shared = ctx.shared.borrow_mut();
                let Some(w) = shared.gui.window_mut(hwnd) else {
                    return ok(0);
                };
                let entry = &mut w.scroll[index];
                if mask & 0x1 != 0 {
                    entry.0 = min;
                    entry.1 = max;
                }
                if mask & 0x2 != 0 {
                    entry.3 = page;
                }
                if mask & 0x4 != 0 {
                    entry.2 = pos;
                }
                entry.2 = entry.2.clamp(entry.0, entry.1.max(entry.0));
                entry.2
            };
            if !is_ctl && bar != 2 && mask & 0x3 != 0 {
                let (min, max, _, page) = ctx
                    .shared
                    .borrow()
                    .gui
                    .window(hwnd)
                    .map(|w| w.scroll[index])
                    .unwrap_or((0, 0, 0, 0));
                let needed = if page > 0 {
                    max - min + 1 > page as i32
                } else {
                    max > min
                };
                // SIF_DISABLENOSCROLL keeps an unneeded bar visible.
                show_scroll_bar(ctx, memory, hwnd, bar, needed || mask & 0x8 != 0);
            }
            if args[3] != 0 {
                redraw(ctx);
            }
            ok(position as u32)
        }
        "GetScrollInfo" => {
            let (min, max, pos, page) = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| w.scroll[index])
                .unwrap_or((0, 0, 0, 0));
            let values = [min as u32, max as u32, page, pos as u32, pos as u32];
            for (offset, value) in values.iter().enumerate() {
                let _ = memory.write_u32(args[2] + 8 + offset as u32 * 4, *value);
            }
            ok(1)
        }
        _ => None,
    }
}

pub(crate) fn show_scroll_bar<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    bar: u32,
    show: bool,
) {
    let bit = if bar == 1 { WS_VSCROLL } else { WS_HSCROLL };
    let changed = {
        let mut shared = ctx.shared.borrow_mut();
        let Some(w) = shared.gui.window_mut(hwnd) else {
            return;
        };
        let had = w.style & bit != 0;
        if had == show {
            false
        } else {
            if show {
                w.style |= bit;
            } else {
                w.style &= !bit;
            }
            true
        }
    };
    if changed {
        // The client area changed size: behave like SWP_FRAMECHANGED.
        set_window_pos(
            ctx,
            memory,
            hwnd,
            0,
            0,
            0,
            0,
            0,
            0x1 | 0x2 | 0x4 | 0x10 | 0x20,
        );
    }
}

pub fn system_metric(index: u32, sw: i32, sh: i32) -> i32 {
    match index {
        0 => sw,
        1 => sh,
        2 | 3 | 9 | 10 | 20 | 21 => SCROLL_SIZE,
        4 => CAPTION_HEIGHT,
        5 | 6 => 1,
        7 | 8 => 3,
        11 | 12 | 13 | 14 => 32,
        15 => MENU_HEIGHT,
        16 => sw,
        17 => sh - TASKBAR_HEIGHT - CAPTION_HEIGHT,
        19 => 1,
        28 => 112,
        29 => 27,
        30 | 31 => 18,
        32 | 33 => 4,
        34 => 112,
        35 => 27,
        36 | 37 => 4,
        38 | 39 => 75,
        43 => 3,
        45 | 46 => 2,
        49 | 50 => 16,
        51 => 15,
        52 | 53 => 15,
        54 | 55 => 18,
        57 => 160,
        58 => 24,
        59 => sw + 8,
        60 => sh + 8,
        61 => sw + 8,
        62 => sh - TASKBAR_HEIGHT + 8,
        68 | 69 => 4,
        71 | 72 => 13,
        // The virtual screen (all monitors: the one): Firefox takes the
        // part of it no window covers for where its windows show, and with
        // none stops drawing them as hidden.
        78 => sw,
        79 => sh,
        // Monitors; all of the same colour format.
        80 | 81 => 1,
        _ => 0,
    }
}
