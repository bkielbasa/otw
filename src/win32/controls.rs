//! USER32 built-in control classes, dialog boxes and message boxes.

use crate::{
    win32::gdi::{self, sys_color, GdiObject, LogFont},
    win32::gfx::{self, Rect},
    win32::user::{
        self, client_size, def_window_proc, hi, is_descendant, is_window, lo, make_long, send,
        set_focus, show_window, with_painter, CreateParams, Painter, WM_CHAR, WM_CLOSE, WM_COMMAND,
        WM_CTLCOLORBTN, WM_CTLCOLORDLG, WM_CTLCOLOREDIT, WM_CTLCOLORLISTBOX, WM_CTLCOLORSTATIC,
        WM_ENABLE, WM_ERASEBKGND, WM_GETDLGCODE, WM_GETFONT, WM_GETTEXT, WM_GETTEXTLENGTH,
        WM_INITDIALOG, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCHITTEST, WM_PAINT, WM_SETFOCUS, WM_SETFONT, WM_SETTEXT,
        WM_SYSCHAR, WS_CHILD, WS_POPUP, WS_VISIBLE,
    },
    win32::{read_cstr, write_cstr, ApiContext, ApiOutcome, GuestMemory, GuestMessage, RawInput},
};

const WS_TABSTOP: u32 = 0x0001_0000;
const WS_GROUP: u32 = 0x0002_0000;

const BN_CLICKED: u32 = 0;
const EN_SETFOCUS: u32 = 0x100;
const EN_KILLFOCUS: u32 = 0x200;
const EN_CHANGE: u32 = 0x300;
const EN_UPDATE: u32 = 0x400;
const LBN_SELCHANGE: u32 = 1;
const LBN_DBLCLK: u32 = 2;
const CBN_SELCHANGE: u32 = 1;
const CBN_EDITCHANGE: u32 = 5;
const CBN_DROPDOWN: u32 = 7;
const CBN_CLOSEUP: u32 = 8;
const CBN_SELENDOK: u32 = 9;

fn window_style(ctx: &ApiContext, hwnd: u32) -> u32 {
    ctx.shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.style)
        .unwrap_or(0)
}

fn notify_parent<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, code: u32) {
    let (parent, id) = {
        let shared = ctx.shared.borrow();
        match shared.gui.window(hwnd) {
            Some(w) => (w.parent, w.id),
            None => return,
        }
    };
    if parent != 0 {
        send(
            ctx,
            memory,
            parent,
            WM_COMMAND,
            make_long(id as i32, code as i32),
            hwnd,
        );
    }
}

fn redraw(ctx: &ApiContext, hwnd: u32) {
    user::invalidate(&mut ctx.shared.borrow_mut().gui, hwnd, None, true, false);
}

/// Ask the parent for control colours through WM_CTLCOLOR*; returns
/// (text, background, brush fill).
fn control_colors<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    message: u32,
    hdc: u32,
) -> (u32, u32, gfx::Fill) {
    let parent = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.parent)
        .unwrap_or(0);
    let brush = if parent != 0 {
        send(ctx, memory, parent, message, hdc, hwnd)
    } else {
        0
    };
    let brush = if brush == 0 {
        user::def_window_proc(ctx, memory, hwnd, message, hdc, hwnd)
    } else {
        brush
    };
    let shared = ctx.shared.borrow();
    let attrs = shared
        .gdi
        .dcs
        .get(&hdc)
        .map(|dc| dc.attrs.clone())
        .unwrap_or_default();
    let fill = shared.gdi.fill_of(&attrs, brush);
    (attrs.text_color, attrs.bk_color, fill)
}

/// Draw `text` with the control's font into `hdc` at client coordinates.
fn control_text<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hdc: u32,
    rect: Rect,
    text: &[u8],
    format: u32,
) {
    let va = user::scratch(ctx, 16 + text.len() as u32 + 1);
    let _ = gdi::write_rect(memory, va, rect);
    let _ = memory.write_bytes(va + 16, text);
    gdi::draw_text_api(ctx, memory, hdc, text, va, format);
    user::free_scratch(ctx, va);
}

fn select_font(ctx: &ApiContext, hwnd: u32, hdc: u32) {
    let font = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.font)
        .unwrap_or(0);
    let mut shared = ctx.shared.borrow_mut();
    let font = if font == 0 {
        gdi::STOCK_BASE + 17 * 4
    } else {
        font
    };
    if let Some(dc) = shared.gdi.dcs.get_mut(&hdc) {
        dc.attrs.font = font;
        dc.attrs.bk_mode = 1;
    }
}

fn with_client<R>(
    ctx: &ApiContext,
    hwnd: u32,
    paint: impl FnOnce(&mut Painter<'_>) -> R,
) -> Option<R> {
    let mut shared = ctx.shared.borrow_mut();
    with_painter(&mut shared, hwnd, true, paint)
}

// ── Button ───────────────────────────────────────────────────────────────

fn paint_button<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, hdc: u32) {
    let Some(window) = ctx.shared.borrow().gui.window(hwnd).cloned() else {
        return;
    };
    let kind = window.style & 0xF;
    let (cw, ch) = client_size(&window);
    let rect = Rect::new(0, 0, cw, ch);
    let focused = ctx.shared.borrow().gui.focus_hwnd == hwnd;
    let disabled = window.disabled;
    let text = window.title.as_bytes().to_vec();
    select_font(ctx, hwnd, hdc);
    match kind {
        0 | 1 => {
            let pressed = window.control.pressed || window.control.check & 0x4 != 0;
            let default = kind == 1;
            with_client(ctx, hwnd, |p| {
                p.fill(rect, sys_color(15));
                let mut face = rect;
                if default || focused {
                    p.hline(0, cw, 0, 0);
                    p.hline(0, cw, ch - 1, 0);
                    p.vline(0, 0, ch, 0);
                    p.vline(cw - 1, 0, ch, 0);
                    face = rect.inflate(-1, -1);
                }
                if pressed {
                    p.hline(face.left, face.right, face.top, sys_color(16));
                    p.hline(face.left, face.right, face.bottom - 1, sys_color(16));
                    p.vline(face.left, face.top, face.bottom, sys_color(16));
                    p.vline(face.right - 1, face.top, face.bottom, sys_color(16));
                } else {
                    p.raised_button(face);
                }
            });
            let offset = if pressed { 1 } else { 0 };
            let color = if disabled {
                sys_color(16)
            } else {
                sys_color(18)
            };
            if let Some(dc) = ctx.shared.borrow_mut().gdi.dcs.get_mut(&hdc) {
                dc.attrs.text_color = gfx::rgb_to_colorref(color);
            }
            control_text(
                ctx,
                memory,
                hdc,
                rect.offset(offset, offset),
                &text,
                0x1 | 0x4 | 0x20,
            );
            if focused {
                with_client(ctx, hwnd, |p| focus_rect(p, rect.inflate(-4, -4)));
            }
        }
        7 => {
            let (text_color, _, fill) = control_colors(ctx, memory, hwnd, WM_CTLCOLORBTN, hdc);
            let _ = text_color;
            let face = gfx::pick_face(13, false);
            let top = face.height / 2;
            let _ = fill;
            with_client(ctx, hwnd, |p| {
                // Group boxes are transparent: only the frame and caption.
                let frame = Rect::new(0, top, cw, ch);
                p.hline(frame.left, frame.right - 1, frame.top, sys_color(16));
                p.vline(frame.left, frame.top, frame.bottom - 1, sys_color(16));
                p.hline(frame.left, frame.right - 1, frame.bottom - 2, sys_color(16));
                p.vline(frame.right - 2, frame.top, frame.bottom - 1, sys_color(16));
                p.hline(
                    frame.left + 1,
                    frame.right - 2,
                    frame.top + 1,
                    sys_color(20),
                );
                p.vline(
                    frame.left + 1,
                    frame.top + 1,
                    frame.bottom - 2,
                    sys_color(20),
                );
                p.hline(frame.left, frame.right, frame.bottom - 1, sys_color(20));
                p.vline(frame.right - 1, frame.top, frame.bottom, sys_color(20));
                let width = user::text_width(&text);
                if width > 0 {
                    p.fill(Rect::new(7, 0, 9 + width + 2, face.height), sys_color(15));
                    if disabled {
                        p.disabled_text(9, 0, &text);
                    } else {
                        p.mnemonic_text(9, 0, &text, sys_color(18));
                    }
                }
            });
        }
        2 | 3 | 4 | 5 | 6 | 9 => {
            let (_, _, fill) = control_colors(ctx, memory, hwnd, WM_CTLCOLORBTN, hdc);
            let radio = kind == 4 || kind == 9;
            let check = window.control.check & 0x3;
            let pressed = window.control.pressed;
            let box_y = (ch - 13) / 2;
            with_client(ctx, hwnd, |p| {
                if let gfx::Fill::Solid(color) = fill {
                    p.fill(rect, color);
                }
                let b = Rect::sized(0, box_y, 13, 13);
                if radio {
                    let face = if pressed || disabled {
                        sys_color(15)
                    } else {
                        sys_color(5)
                    };
                    let rows = [
                        (4, 8),
                        (2, 10),
                        (1, 11),
                        (1, 11),
                        (0, 12),
                        (0, 12),
                        (0, 12),
                        (0, 12),
                        (1, 11),
                        (1, 11),
                        (2, 10),
                        (4, 8),
                    ];
                    for (dy, (x0, x1)) in rows.iter().enumerate() {
                        p.hline(b.left + x0, b.left + x1, b.top + dy as i32, face);
                    }
                    p.hline(b.left + 4, b.left + 8, b.top, sys_color(16));
                    p.hline(b.left + 2, b.left + 4, b.top + 1, sys_color(16));
                    p.hline(b.left + 8, b.left + 10, b.top + 1, sys_color(16));
                    p.vline(b.left, b.top + 4, b.top + 8, sys_color(16));
                    p.vline(b.left + 1, b.top + 2, b.top + 4, sys_color(16));
                    p.vline(b.left + 1, b.top + 8, b.top + 10, sys_color(16));
                    p.hline(b.left + 4, b.left + 8, b.top + 11, sys_color(20));
                    p.vline(b.left + 11, b.top + 4, b.top + 8, sys_color(20));
                    if check != 0 {
                        p.fill(
                            Rect::sized(b.left + 4, b.top + 4, 4, 4),
                            if disabled { sys_color(16) } else { 0 },
                        );
                        p.hline(
                            b.left + 5,
                            b.left + 7,
                            b.top + 3,
                            if disabled { sys_color(16) } else { 0 },
                        );
                        p.hline(
                            b.left + 5,
                            b.left + 7,
                            b.top + 8,
                            if disabled { sys_color(16) } else { 0 },
                        );
                    }
                } else {
                    p.fill(
                        b,
                        if pressed || disabled {
                            sys_color(15)
                        } else {
                            sys_color(5)
                        },
                    );
                    p.sunken(b);
                    if check != 0 {
                        let color = if disabled || check == 2 {
                            sys_color(16)
                        } else {
                            0
                        };
                        for i in 0..3 {
                            p.vline(b.left + 3 + i, b.top + 5 + i, b.top + 8 + i, color);
                        }
                        for i in 0..4 {
                            p.vline(b.left + 6 + i, b.top + 6 - i, b.top + 9 - i, color);
                        }
                    }
                }
                let face = gfx::pick_face(13, false);
                let ty = (ch - face.height) / 2;
                if disabled {
                    p.disabled_text(17, ty, &text);
                } else {
                    p.mnemonic_text(17, ty, &text, sys_color(18));
                }
                if focused {
                    focus_rect(
                        p,
                        Rect::new(
                            15,
                            ty - 1,
                            19 + user::text_width(&text),
                            ty + face.height + 1,
                        ),
                    );
                }
            });
        }
        0xB => {
            // Owner-draw: WM_DRAWITEM to the parent.
            let state = (window.control.pressed as u32)
                | if focused { 0x10 } else { 0 }
                | if disabled { 0x4 } else { 0 };
            send_draw_item(ctx, memory, window.parent, [4, window.id, 0, 1, state, hwnd, hdc], (cw, ch));
        }
        _ => {}
    }
}

fn focus_rect(p: &mut Painter<'_>, rect: Rect) {
    for x in rect.left..rect.right {
        if (x + rect.top) % 2 == 0 {
            p.pixel(x, rect.top, 0);
        }
        if (x + rect.bottom - 1) % 2 == 0 {
            p.pixel(x, rect.bottom - 1, 0);
        }
    }
    for y in rect.top..rect.bottom {
        if (rect.left + y) % 2 == 0 {
            p.pixel(rect.left, y, 0);
        }
        if (rect.right - 1 + y) % 2 == 0 {
            p.pixel(rect.right - 1, y, 0);
        }
    }
}

fn button_click<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let (style, parent) = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| (w.style, w.parent))
        .unwrap_or((0, 0));
    match style & 0xF {
        3 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.check = if w.control.check & 3 == 0 { 1 } else { 0 };
            }
        }
        6 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.check = (w.control.check + 1) % 3;
            }
        }
        9 => {
            // Uncheck the other auto radio buttons of the group.
            let group = radio_group(ctx, parent, hwnd);
            for other in group {
                if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(other) {
                    if w.style & 0xF == 9 {
                        w.control.check = (other == hwnd) as u32;
                    }
                }
                redraw(ctx, other);
            }
        }
        _ => {}
    }
    redraw(ctx, hwnd);
    notify_parent(ctx, memory, hwnd, BN_CLICKED);
}

/// The WS_GROUP-delimited group containing `hwnd` among its siblings.
fn radio_group(ctx: &ApiContext, parent: u32, hwnd: u32) -> Vec<u32> {
    let shared = ctx.shared.borrow();
    let siblings: Vec<u32> = user::children(&shared.gui, parent)
        .into_iter()
        .rev()
        .collect();
    let Some(position) = siblings.iter().position(|h| *h == hwnd) else {
        return vec![hwnd];
    };
    let style = |h: u32| shared.gui.window(h).map(|w| w.style).unwrap_or(0);
    let mut start = position;
    while start > 0 && style(siblings[start]) & WS_GROUP == 0 {
        start -= 1;
    }
    let mut end = position + 1;
    while end < siblings.len() && style(siblings[end]) & WS_GROUP == 0 {
        end += 1;
    }
    siblings[start..end].to_vec()
}

fn button_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    let kind = window_style(ctx, hwnd) & 0xF;
    match msg {
        WM_PAINT => {
            let ps = user::scratch(ctx, 80);
            let hdc = user::begin_paint(ctx, memory, hwnd, ps);
            paint_button(ctx, memory, hwnd, hdc);
            user::end_paint(ctx, hwnd, hdc);
            user::free_scratch(ctx, ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            if kind == 7 {
                return 0;
            }
            ctx.shared.borrow_mut().gui.capture = hwnd;
            set_focus(ctx, memory, hwnd);
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.pressed = true;
            }
            redraw(ctx, hwnd);
            0
        }
        WM_MOUSEMOVE => {
            if ctx.shared.borrow().gui.capture != hwnd {
                return 0;
            }
            let (cw, ch) = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(client_size)
                .unwrap_or((0, 0));
            let inside = Rect::new(0, 0, cw, ch).contains(lo(lparam), hi(lparam));
            let changed = {
                let mut shared = ctx.shared.borrow_mut();
                let w = shared.gui.window_mut(hwnd).unwrap();
                let changed = w.control.pressed != inside;
                w.control.pressed = inside;
                changed
            };
            if changed {
                redraw(ctx, hwnd);
            }
            0
        }
        WM_LBUTTONUP => {
            if ctx.shared.borrow().gui.capture != hwnd {
                return 0;
            }
            ctx.shared.borrow_mut().gui.capture = 0;
            let pressed = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| w.control.pressed)
                .unwrap_or(false);
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.pressed = false;
            }
            redraw(ctx, hwnd);
            if pressed {
                button_click(ctx, memory, hwnd);
            }
            0
        }
        WM_KEYDOWN if wparam == 0x20 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.pressed = true;
            }
            redraw(ctx, hwnd);
            0
        }
        WM_KEYUP if wparam == 0x20 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.pressed = false;
            }
            button_click(ctx, memory, hwnd);
            0
        }
        WM_SETFOCUS | WM_KILLFOCUS | WM_ENABLE => {
            redraw(ctx, hwnd);
            0
        }
        WM_GETDLGCODE => match kind {
            1 => 0x2000 | 0x10,
            0 => 0x2000 | 0x20,
            4 | 9 => 0x2000 | 0x40,
            7 => 0x100,
            _ => 0x2000,
        },
        0x00F0 => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.control.check & 3)
            .unwrap_or(0),
        0x00F1 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.check = (w.control.check & !3) | (wparam & 3);
            }
            redraw(ctx, hwnd);
            0
        }
        0x00F2 => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| (w.control.check & 3) | if w.control.pressed { 4 } else { 0 })
            .unwrap_or(0),
        0x00F3 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.pressed = wparam != 0;
            }
            redraw(ctx, hwnd);
            0
        }
        0x00F4 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.style = (w.style & !0xFFFF) | (wparam & 0xFFFF);
            }
            if lparam != 0 {
                redraw(ctx, hwnd);
            }
            0
        }
        0x00F5 => {
            button_click(ctx, memory, hwnd);
            0
        }
        WM_SETTEXT => {
            let result = def_window_proc(ctx, memory, hwnd, msg, wparam, lparam);
            redraw(ctx, hwnd);
            result
        }
        WM_SETFONT => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.font = wparam;
            }
            if lparam != 0 {
                redraw(ctx, hwnd);
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
        _ => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

// ── Static ───────────────────────────────────────────────────────────────

fn static_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    match msg {
        WM_PAINT => {
            let ps = user::scratch(ctx, 80);
            let hdc = user::begin_paint(ctx, memory, hwnd, ps);
            let Some(window) = ctx.shared.borrow().gui.window(hwnd).cloned() else {
                return 0;
            };
            let (cw, ch) = client_size(&window);
            let rect = Rect::new(0, 0, cw, ch);
            let kind = window.style & 0x1F;
            select_font(ctx, hwnd, hdc);
            match kind {
                0..=2 | 0xB | 0xC => {
                    let (_, _, fill) = control_colors(ctx, memory, hwnd, WM_CTLCOLORSTATIC, hdc);
                    gdi::with_dc(ctx, memory, hdc, |target, dctx| {
                        let device = dctx.rect(0, 0, cw, ch);
                        target.fill_rect(
                            device,
                            &fill,
                            gfx::R2_COPYPEN,
                            dctx.brush_origin(),
                            dctx.mono,
                        );
                    });
                    if window.disabled {
                        if let Some(dc) = ctx.shared.borrow_mut().gdi.dcs.get_mut(&hdc) {
                            dc.attrs.text_color = gfx::rgb_to_colorref(sys_color(17));
                        }
                    }
                    let align = match kind {
                        1 => 0x1,
                        2 => 0x2,
                        _ => 0,
                    };
                    let wrap = if kind == 0xB || kind == 0xC {
                        0x20
                    } else {
                        0x10
                    };
                    let prefix = if window.style & 0x80 != 0 { 0x800 } else { 0 };
                    control_text(
                        ctx,
                        memory,
                        hdc,
                        rect,
                        window.title.as_bytes(),
                        align | wrap | prefix | 0x40,
                    );
                }
                4..=9 => {
                    let color = match kind {
                        4 | 7 => sys_color(6),
                        5 | 8 => sys_color(16),
                        _ => sys_color(20),
                    };
                    with_client(ctx, hwnd, |p| {
                        if kind <= 6 {
                            p.fill(rect, color);
                        } else {
                            p.hline(0, cw, 0, color);
                            p.hline(0, cw, ch - 1, color);
                            p.vline(0, 0, ch, color);
                            p.vline(cw - 1, 0, ch, color);
                        }
                    });
                }
                0x10..=0x12 => {
                    with_client(ctx, hwnd, |p| {
                        if kind != 0x11 {
                            p.hline(0, cw, 0, sys_color(16));
                            p.hline(0, cw, 1, sys_color(20));
                        }
                        if kind != 0x10 {
                            p.vline(0, 0, ch, sys_color(16));
                            p.vline(1, 0, ch, sys_color(20));
                        }
                        if kind == 0x12 {
                            p.sunken(rect);
                        }
                    });
                }
                0xD => {
                    send_draw_item(ctx, memory, window.parent, [5, window.id, 0, 1, 0, hwnd, hdc], (cw, ch));
                }
                _ => {}
            }
            user::end_paint(ctx, hwnd, hdc);
            user::free_scratch(ctx, ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_NCHITTEST => {
            if window_style(ctx, hwnd) & 0x100 != 0 {
                1
            } else {
                user::HTTRANSPARENT as u32
            }
        }
        WM_SETTEXT => {
            let result = def_window_proc(ctx, memory, hwnd, msg, wparam, lparam);
            redraw(ctx, hwnd);
            result
        }
        WM_ENABLE => {
            redraw(ctx, hwnd);
            0
        }
        0x0170 | 0x0171 => 0, // STM_SETICON / STM_GETICON
        WM_GETDLGCODE => 0x100,
        _ => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

// ── Edit ─────────────────────────────────────────────────────────────────

fn edit_lines(text: &[u8]) -> Vec<(usize, usize)> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in text.iter().enumerate() {
        if *byte == b'\n' {
            let end = if index > start && text[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            lines.push((start, end));
            start = index + 1;
        }
    }
    lines.push((start, text.len()));
    lines
}

fn edit_paint<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, hdc: u32) {
    let Some(window) = ctx.shared.borrow().gui.window(hwnd).cloned() else {
        return;
    };
    let (cw, ch) = client_size(&window);
    let multiline = window.style & 0x4 != 0;
    let readonly = window.style & 0x800 != 0;
    let message = if readonly || window.disabled {
        WM_CTLCOLORSTATIC
    } else {
        WM_CTLCOLOREDIT
    };
    select_font(ctx, hwnd, hdc);
    let (text_color, bk_color, fill) = control_colors(ctx, memory, hwnd, message, hdc);
    let focused = ctx.shared.borrow().gui.focus_hwnd == hwnd;
    let font = {
        let shared = ctx.shared.borrow();
        let attrs = shared
            .gdi
            .dcs
            .get(&hdc)
            .map(|dc| dc.attrs.clone())
            .unwrap_or_default();
        shared.gdi.font(attrs.font)
    };
    let face = font.face();
    let password = window.style & 0x20 != 0;
    let display: Vec<u8> = if password {
        vec![b'*'; window.text.len()]
    } else {
        window.text.clone()
    };
    let (selection_start, selection_end) = {
        let (a, b) = (
            window.control.anchor.min(display.len()),
            window.control.caret.min(display.len()),
        );
        (a.min(b), a.max(b))
    };
    let margin = 1;
    let scroll_line = window.control.scroll_line.max(0) as usize;
    let caret = window.control.caret.min(display.len());
    let mut caret_pos = (margin, margin);
    let mut h_offset = 0;
    if !multiline {
        let caret_x = face.text_width(&display[..caret]);
        if caret_x > cw - 4 {
            h_offset = caret_x - (cw - 4);
        }
    }
    gdi::with_dc(ctx, memory, hdc, |target, dctx| {
        let device = dctx.rect(0, 0, cw, ch);
        target.fill_rect(
            device,
            &fill,
            gfx::R2_COPYPEN,
            dctx.brush_origin(),
            dctx.mono,
        );
        let text_rgb = if window.disabled {
            sys_color(17)
        } else {
            gfx::colorref_to_rgb(text_color)
        };
        let _ = bk_color;
        let lines = if multiline {
            edit_lines(&display)
        } else {
            vec![(0, display.len())]
        };
        let line_height = face.height;
        let total_height = if multiline {
            line_height * lines.len() as i32
        } else {
            line_height
        };
        let base_y = if multiline {
            margin
        } else {
            ((ch - total_height) / 2).max(0)
        };
        for (row, (start, end)) in lines.iter().enumerate().skip(scroll_line) {
            let y = base_y + (row - scroll_line) as i32 * line_height;
            if y > ch {
                break;
            }
            let mut x = margin - h_offset;
            for index in *start..*end {
                let byte = display[index];
                let advance = if byte == b'\t' {
                    face.advance(b' ') * 8
                } else {
                    face.advance(byte)
                };
                let selected = index >= selection_start && index < selection_end && focused;
                let (sx, sy) = dctx.pt(x, y);
                if selected {
                    target.fill_rect(
                        Rect::new(sx, sy, sx + advance, sy + line_height),
                        &gfx::Fill::Solid(sys_color(13)),
                        gfx::R2_COPYPEN,
                        (0, 0),
                        dctx.mono,
                    );
                }
                if byte != b'\t' {
                    gfx::draw_text(
                        target,
                        face,
                        sx,
                        sy,
                        &[byte],
                        if selected { sys_color(14) } else { text_rgb },
                        false,
                    );
                }
                x += advance;
            }
            if caret >= *start && caret <= *end {
                let width: i32 = display[*start..caret]
                    .iter()
                    .map(|b| {
                        if *b == b'\t' {
                            face.advance(b' ') * 8
                        } else {
                            face.advance(*b)
                        }
                    })
                    .sum();
                caret_pos = (margin - h_offset + width, y);
            }
        }
        if focused && !readonly {
            let (sx, sy) = dctx.pt(caret_pos.0, caret_pos.1);
            for dy in 0..line_height {
                target.plot(sx, sy + dy, 0, gfx::R2_COPYPEN);
            }
        }
    });
}

fn edit_set_text(ctx: &ApiContext, hwnd: u32, text: Vec<u8>) {
    let mut shared = ctx.shared.borrow_mut();
    if let Some(w) = shared.gui.window_mut(hwnd) {
        w.control.caret = 0;
        w.control.anchor = 0;
        w.control.scroll_line = 0;
        w.text = text;
    }
}

fn edit_replace<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    insert: &[u8],
    notify: bool,
) {
    let changed = {
        let mut shared = ctx.shared.borrow_mut();
        let Some(w) = shared.gui.window_mut(hwnd) else {
            return;
        };
        let len = w.text.len();
        let (a, b) = (w.control.anchor.min(len), w.control.caret.min(len));
        let (start, end) = (a.min(b), a.max(b));
        let limit = if w.control.limit == 0 {
            30000
        } else {
            w.control.limit as usize
        };
        let mut insert = insert.to_vec();
        if w.style & 0x8 != 0 {
            insert.make_ascii_uppercase();
        }
        if w.style & 0x10 != 0 {
            insert.make_ascii_lowercase();
        }
        if w.style & 0x2000 != 0 {
            insert.retain(|b| b.is_ascii_digit());
        }
        let room = limit.saturating_sub(len - (end - start));
        insert.truncate(room);
        if start == end && insert.is_empty() {
            false
        } else {
            w.text.splice(start..end, insert.iter().copied());
            w.control.caret = start + insert.len();
            w.control.anchor = w.control.caret;
            w.control.modified = true;
            true
        }
    };
    edit_scroll_to_caret(ctx, hwnd);
    redraw(ctx, hwnd);
    if changed && notify {
        notify_parent(ctx, memory, hwnd, EN_UPDATE);
        notify_parent(ctx, memory, hwnd, EN_CHANGE);
    }
}

fn edit_scroll_to_caret(ctx: &ApiContext, hwnd: u32) {
    let mut shared = ctx.shared.borrow_mut();
    let Some(w) = shared.gui.window_mut(hwnd) else {
        return;
    };
    if w.style & 0x4 == 0 {
        return;
    }
    let face = gfx::pick_face(13, false);
    let (_, ch) = client_size(w);
    let visible = (ch / face.height).max(1);
    let caret = w.control.caret.min(w.text.len());
    let line = w.text[..caret].iter().filter(|b| **b == b'\n').count() as i32;
    if line < w.control.scroll_line {
        w.control.scroll_line = line;
    } else if line >= w.control.scroll_line + visible {
        w.control.scroll_line = line - visible + 1;
    }
}

fn edit_hit(ctx: &ApiContext, hwnd: u32, x: i32, y: i32) -> usize {
    let shared = ctx.shared.borrow();
    let Some(w) = shared.gui.window(hwnd) else {
        return 0;
    };
    let face = gfx::pick_face(13, false);
    let multiline = w.style & 0x4 != 0;
    let lines = if multiline {
        edit_lines(&w.text)
    } else {
        vec![(0, w.text.len())]
    };
    let row = if multiline {
        ((y - 1) / face.height + w.control.scroll_line).clamp(0, lines.len() as i32 - 1) as usize
    } else {
        0
    };
    let (start, end) = lines[row];
    let mut pen = 1;
    for index in start..end {
        let advance = face.advance(w.text[index]);
        if x < pen + advance / 2 {
            return index;
        }
        pen += advance;
    }
    end
}

fn edit_move(ctx: &ApiContext, hwnd: u32, vk: u32, shift: bool, control: bool) {
    let mut shared = ctx.shared.borrow_mut();
    let Some(w) = shared.gui.window_mut(hwnd) else {
        return;
    };
    let text = w.text.clone();
    let caret = w.control.caret.min(text.len());
    let lines = edit_lines(&text);
    let row = lines
        .iter()
        .position(|(s, e)| caret >= *s && caret <= *e)
        .unwrap_or(0);
    let new = match vk {
        0x25 => {
            if control {
                let mut i = caret;
                while i > 0 && text[i - 1] == b' ' {
                    i -= 1;
                }
                while i > 0 && text[i - 1] != b' ' && text[i - 1] != b'\n' {
                    i -= 1;
                }
                i
            } else if caret > 0
                && text[caret - 1] == b'\n'
                && caret >= 2
                && text[caret - 2] == b'\r'
            {
                caret - 2
            } else {
                caret.saturating_sub(1)
            }
        }
        0x27 => {
            if control {
                let mut i = caret;
                while i < text.len() && text[i] != b' ' && text[i] != b'\n' {
                    i += 1;
                }
                while i < text.len() && text[i] == b' ' {
                    i += 1;
                }
                i
            } else if caret < text.len() && text[caret] == b'\r' {
                (caret + 2).min(text.len())
            } else {
                (caret + 1).min(text.len())
            }
        }
        0x24 => {
            if control {
                0
            } else {
                lines[row].0
            }
        }
        0x23 => {
            if control {
                text.len()
            } else {
                lines[row].1
            }
        }
        0x26 | 0x28 => {
            let target = if vk == 0x26 {
                row.checked_sub(1)
            } else {
                (row + 1 < lines.len()).then_some(row + 1)
            };
            match target {
                Some(target) => {
                    let column = caret - lines[row].0;
                    (lines[target].0 + column).min(lines[target].1)
                }
                None => caret,
            }
        }
        _ => caret,
    };
    w.control.caret = new;
    if !shift {
        w.control.anchor = new;
    }
    drop(shared);
    edit_scroll_to_caret(ctx, hwnd);
    redraw(ctx, hwnd);
}

fn edit_handle<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) -> u32 {
    let text = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.text.clone())
        .unwrap_or_default();
    let existing = ctx.shared.borrow().gui.window(hwnd).and_then(|w| {
        w.props
            .iter()
            .find(|(k, _)| k == "otw_edithandle")
            .map(|(_, v)| *v)
    });
    let needed = text.len() as u32 + 1;
    let handle = match existing {
        Some(handle) if ctx.shared.borrow().heap.size_of(handle).unwrap_or(0) >= needed => handle,
        other => {
            if let Some(old) = other {
                ctx.shared.borrow_mut().heap.free(old);
            }
            let handle = ctx
                .shared
                .borrow_mut()
                .heap
                .alloc(needed.max(256) * 2)
                .unwrap_or(0);
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.props.retain(|(k, _)| k != "otw_edithandle");
                w.props.push(("otw_edithandle".into(), handle));
            }
            handle
        }
    };
    let _ = write_cstr(memory, handle, &text);
    handle
}

fn edit_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    let style = window_style(ctx, hwnd);
    let multiline = style & 0x4 != 0;
    let readonly = style & 0x800 != 0;
    match msg {
        WM_PAINT => {
            let ps = user::scratch(ctx, 80);
            let hdc = user::begin_paint(ctx, memory, hwnd, ps);
            edit_paint(ctx, memory, hwnd, hdc);
            user::end_paint(ctx, hwnd, hdc);
            user::free_scratch(ctx, ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_SETTEXT => {
            let text = read_cstr(memory, lparam);
            edit_set_text(ctx, hwnd, text);
            redraw(ctx, hwnd);
            notify_parent(ctx, memory, hwnd, EN_CHANGE);
            1
        }
        WM_GETTEXT => {
            let text = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| w.text.clone())
                .unwrap_or_default();
            if wparam == 0 {
                return 0;
            }
            let mut text = text;
            text.truncate(wparam as usize - 1);
            let _ = write_cstr(memory, lparam, &text);
            text.len() as u32
        }
        WM_GETTEXTLENGTH => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.text.len() as u32)
            .unwrap_or(0),
        WM_SETFOCUS => {
            redraw(ctx, hwnd);
            if !multiline {
                if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                    w.control.anchor = 0;
                    w.control.caret = w.text.len();
                }
            }
            notify_parent(ctx, memory, hwnd, EN_SETFOCUS);
            0
        }
        WM_KILLFOCUS => {
            redraw(ctx, hwnd);
            notify_parent(ctx, memory, hwnd, EN_KILLFOCUS);
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            set_focus(ctx, memory, hwnd);
            let index = edit_hit(ctx, hwnd, lo(lparam), hi(lparam));
            let shift = ctx.shared.borrow().gui.keys[0x10] & 0x80 != 0;
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.caret = index;
                if !shift {
                    w.control.anchor = index;
                }
            }
            ctx.shared.borrow_mut().gui.capture = hwnd;
            redraw(ctx, hwnd);
            0
        }
        WM_MOUSEMOVE => {
            if ctx.shared.borrow().gui.capture == hwnd && wparam & 1 != 0 {
                let index = edit_hit(ctx, hwnd, lo(lparam), hi(lparam));
                if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                    w.control.caret = index;
                }
                redraw(ctx, hwnd);
            }
            0
        }
        WM_LBUTTONUP => {
            if ctx.shared.borrow().gui.capture == hwnd {
                ctx.shared.borrow_mut().gui.capture = 0;
            }
            0
        }
        WM_CHAR => {
            if readonly && wparam != 0x03 && wparam != 0x01 {
                return 0;
            }
            let ch = wparam as u8;
            match ch {
                0x08 => {
                    let empty_selection = ctx
                        .shared
                        .borrow()
                        .gui
                        .window(hwnd)
                        .map(|w| w.control.caret == w.control.anchor)
                        .unwrap_or(true);
                    if empty_selection {
                        if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                            let caret = w.control.caret.min(w.text.len());
                            let back = if caret >= 2 && &w.text[caret - 2..caret] == b"\r\n" {
                                2
                            } else {
                                1
                            };
                            w.control.anchor = caret.saturating_sub(back);
                        }
                    }
                    edit_replace(ctx, memory, hwnd, b"", true);
                }
                0x0D => {
                    if multiline
                        && (style & 0x1000 != 0
                            || ctx
                                .shared
                                .borrow()
                                .gui
                                .window(hwnd)
                                .map(|w| w.parent)
                                .map(|p| !is_dialog(ctx, p))
                                .unwrap_or(true))
                    {
                        edit_replace(ctx, memory, hwnd, b"\r\n", true);
                    }
                }
                0x09 => {
                    if multiline {
                        edit_replace(ctx, memory, hwnd, b"\t", true);
                    }
                }
                0x03 => {
                    edit_proc(ctx, memory, hwnd, 0x0301, 0, 0);
                }
                0x16 => {
                    edit_proc(ctx, memory, hwnd, 0x0302, 0, 0);
                }
                0x18 => {
                    edit_proc(ctx, memory, hwnd, 0x0300, 0, 0);
                }
                0x01 => {
                    if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                        w.control.anchor = 0;
                        w.control.caret = w.text.len();
                    }
                    redraw(ctx, hwnd);
                }
                c if c >= 0x20 => edit_replace(ctx, memory, hwnd, &[c], true),
                _ => {}
            }
            0
        }
        WM_KEYDOWN => {
            let (shift, control) = {
                let shared = ctx.shared.borrow();
                (
                    shared.gui.keys[0x10] & 0x80 != 0,
                    shared.gui.keys[0x11] & 0x80 != 0,
                )
            };
            match wparam {
                0x25..=0x28 | 0x24 | 0x23 => edit_move(ctx, hwnd, wparam, shift, control),
                0x2E => {
                    if !readonly {
                        let empty = ctx
                            .shared
                            .borrow()
                            .gui
                            .window(hwnd)
                            .map(|w| w.control.caret == w.control.anchor)
                            .unwrap_or(true);
                        if empty {
                            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                                let caret = w.control.caret.min(w.text.len());
                                let forward = if w.text[caret..].starts_with(b"\r\n") {
                                    2
                                } else {
                                    1
                                };
                                w.control.anchor = (caret + forward).min(w.text.len());
                            }
                        }
                        edit_replace(ctx, memory, hwnd, b"", true);
                    }
                }
                _ => {}
            }
            0
        }
        WM_GETDLGCODE => {
            let mut code = 0x1 | 0x2 | 0x80 | 0x8000; // WANTARROWS | WANTTAB? | HASSETSEL | WANTCHARS
            code &= !0x2;
            if multiline {
                code |= 0x4; // DLGC_WANTALLKEYS
            }
            code
        }
        0x00B0 => {
            // EM_GETSEL
            let (a, b) = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| (w.control.anchor, w.control.caret))
                .unwrap_or((0, 0));
            let (start, end) = (a.min(b) as u32, a.max(b) as u32);
            if wparam != 0 {
                let _ = memory.write_u32(wparam, start);
            }
            if lparam != 0 {
                let _ = memory.write_u32(lparam, end);
            }
            make_long(start as i32, end as i32)
        }
        0x00B1 => {
            // EM_SETSEL
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                let len = w.text.len();
                let start = if wparam as i32 == -1 {
                    w.control.caret
                } else {
                    (wparam as usize).min(len)
                };
                let end = if lparam as i32 == -1 {
                    len
                } else {
                    (lparam as usize).min(len)
                };
                w.control.anchor = start;
                w.control.caret = end;
            }
            redraw(ctx, hwnd);
            0
        }
        0x00B7 => {
            edit_scroll_to_caret(ctx, hwnd);
            redraw(ctx, hwnd);
            1
        }
        0x00B8 => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.control.modified as u32)
            .unwrap_or(0),
        0x00B9 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.modified = wparam != 0;
            }
            0
        }
        0x00BA => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| edit_lines(&w.text).len() as u32)
            .unwrap_or(1),
        0x00BB => {
            let shared = ctx.shared.borrow();
            let Some(w) = shared.gui.window(hwnd) else {
                return 0;
            };
            let lines = edit_lines(&w.text);
            let line = if wparam as i32 == -1 {
                let caret = w.control.caret.min(w.text.len());
                lines
                    .iter()
                    .position(|(s, e)| caret >= *s && caret <= *e)
                    .unwrap_or(0)
            } else {
                wparam as usize
            };
            lines.get(line).map(|(s, _)| *s as u32).unwrap_or(u32::MAX)
        }
        0x00BC => {
            let text = read_cstr(memory, wparam);
            edit_set_text(ctx, hwnd, text);
            redraw(ctx, hwnd);
            0
        }
        0x00BD => edit_handle(ctx, memory, hwnd),
        0x00C1 => {
            let shared = ctx.shared.borrow();
            let Some(w) = shared.gui.window(hwnd) else {
                return 0;
            };
            let index = if wparam as i32 == -1 {
                w.control.caret
            } else {
                wparam as usize
            };
            edit_lines(&w.text)
                .iter()
                .find(|(s, e)| index >= *s && index <= *e)
                .map(|(s, e)| (e - s) as u32)
                .unwrap_or(0)
        }
        0x00C2 => {
            let text = read_cstr(memory, lparam);
            edit_replace(ctx, memory, hwnd, &text, true);
            0
        }
        0x00C4 => {
            let (start, end) = {
                let shared = ctx.shared.borrow();
                let Some(w) = shared.gui.window(hwnd) else {
                    return 0;
                };
                edit_lines(&w.text)
                    .get(wparam as usize)
                    .copied()
                    .unwrap_or((0, 0))
            };
            let max = memory
                .read_bytes(lparam, 2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .unwrap_or(0) as usize;
            let text = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| w.text[start..end].to_vec())
                .unwrap_or_default();
            let length = text.len().min(max);
            let _ = memory.write_bytes(lparam, &text[..length]);
            length as u32
        }
        0x00C5 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.limit = wparam;
            }
            0
        }
        0x00C6 | 0x00C7 | 0x00CD | 0x0304 => 0,
        0x00C8 => 1,
        0x00C9 => {
            let shared = ctx.shared.borrow();
            let Some(w) = shared.gui.window(hwnd) else {
                return 0;
            };
            let index = if wparam as i32 == -1 {
                w.control.caret
            } else {
                wparam as usize
            };
            w.text[..index.min(w.text.len())]
                .iter()
                .filter(|b| **b == b'\n')
                .count() as u32
        }
        0x00CB | 0x00CC | 0x00D3 => 1,
        0x00CE => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.control.scroll_line as u32)
            .unwrap_or(0),
        0x00CF => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                if wparam != 0 {
                    w.style |= 0x800;
                } else {
                    w.style &= !0x800;
                }
            }
            redraw(ctx, hwnd);
            1
        }
        0x00D5 => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| {
                if w.control.limit == 0 {
                    30000
                } else {
                    w.control.limit
                }
            })
            .unwrap_or(30000),
        0x00B6 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.scroll_line = (w.control.scroll_line + lparam as i32).max(0);
            }
            redraw(ctx, hwnd);
            1
        }
        0x0300 | 0x0301 => {
            // WM_CUT / WM_COPY (never from password fields).
            let selected = {
                let shared = ctx.shared.borrow();
                shared.gui.window(hwnd).map(|w| {
                    let len = w.text.len();
                    let (a, b) = (w.control.anchor.min(len), w.control.caret.min(len));
                    w.text[a.min(b)..a.max(b)].to_vec()
                })
            };
            if let Some(selected) = selected.filter(|s| !s.is_empty()) {
                if style & 0x20 == 0 {
                    crate::win32::clipboard::set_text(ctx, memory, hwnd, &selected);
                    if msg == 0x0300 && !readonly {
                        edit_replace(ctx, memory, hwnd, b"", true);
                    }
                }
            }
            0
        }
        0x0302 => {
            // WM_PASTE: single-line fields take only the first line.
            if readonly {
                return 0;
            }
            if let Some(mut text) = crate::win32::clipboard::get_text(ctx, memory) {
                if !multiline {
                    if let Some(end) = text.iter().position(|b| *b == b'\r' || *b == b'\n') {
                        text.truncate(end);
                    }
                }
                edit_replace(ctx, memory, hwnd, &text, true);
            }
            0
        }
        0x0303 => {
            edit_replace(ctx, memory, hwnd, b"", true);
            0
        }
        WM_SETFONT => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.font = wparam;
            }
            redraw(ctx, hwnd);
            0
        }
        WM_ENABLE => {
            redraw(ctx, hwnd);
            0
        }
        _ => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

fn is_dialog(ctx: &ApiContext, hwnd: u32) -> bool {
    ctx.shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.class == "#32770")
        .unwrap_or(false)
}

// ── List box ─────────────────────────────────────────────────────────────

const LB_ITEM_HEIGHT: i32 = 14;

fn listbox_paint<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, hdc: u32) {
    let Some(window) = ctx.shared.borrow().gui.window(hwnd).cloned() else {
        return;
    };
    let (cw, ch) = client_size(&window);
    select_font(ctx, hwnd, hdc);
    let (_, _, fill) = control_colors(ctx, memory, hwnd, WM_CTLCOLORLISTBOX, hdc);
    let focused = ctx.shared.borrow().gui.focus_hwnd == hwnd;
    let face = gfx::pick_face(13, false);
    gdi::with_dc(ctx, memory, hdc, |target, dctx| {
        let device = dctx.rect(0, 0, cw, ch);
        target.fill_rect(
            device,
            &fill,
            gfx::R2_COPYPEN,
            dctx.brush_origin(),
            dctx.mono,
        );
        for (index, (text, _)) in window
            .control
            .items
            .iter()
            .enumerate()
            .skip(window.control.top.max(0) as usize)
        {
            let y = (index as i32 - window.control.top) * LB_ITEM_HEIGHT;
            if y > ch {
                break;
            }
            let (sx, sy) = dctx.pt(0, y);
            let selected = index as i32 == window.control.selected;
            if selected {
                target.fill_rect(
                    Rect::new(sx, sy, sx + cw, sy + LB_ITEM_HEIGHT),
                    &gfx::Fill::Solid(sys_color(13)),
                    gfx::R2_COPYPEN,
                    (0, 0),
                    dctx.mono,
                );
            }
            gfx::draw_text(
                target,
                face,
                sx + 2,
                sy,
                text,
                if selected {
                    sys_color(14)
                } else {
                    sys_color(8)
                },
                false,
            );
            if selected && focused {
                let mut painter = Painter {
                    surface: target.surface,
                    clip: target.clip.clone(),
                    ox: 0,
                    oy: 0,
                };
                focus_rect(
                    &mut painter,
                    Rect::new(sx, sy, sx + cw, sy + LB_ITEM_HEIGHT),
                );
            }
        }
    });
}

fn find_item(items: &[(Vec<u8>, u32)], start: i32, text: &[u8], exact: bool) -> i32 {
    let text = text.to_ascii_lowercase();
    let n = items.len();
    for offset in 0..n {
        let index = ((start + 1).max(0) as usize + offset) % n;
        let item = items[index].0.to_ascii_lowercase();
        if (exact && item == text) || (!exact && item.starts_with(&text)) {
            return index as i32;
        }
    }
    -1
}

/// Shared list operations for LB_* and CB_* messages. `base` maps the
/// message to its list-box equivalent.
fn list_message<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    lb_msg: u32,
    wparam: u32,
    lparam: u32,
    has_strings: bool,
) -> Option<u32> {
    let err = u32::MAX;
    let read_item = |memory: &mut M| {
        if has_strings {
            read_cstr(memory, lparam)
        } else {
            Vec::new()
        }
    };
    Some(match lb_msg {
        0x0180 | 0x0181 => {
            let text = read_item(memory);
            let data = if has_strings { 0 } else { lparam };
            let sorted =
                window_style(ctx, hwnd) & 0x2 != 0 && lb_msg == 0x0180 && !is_combo(ctx, hwnd)
                    || (is_combo(ctx, hwnd)
                        && window_style(ctx, hwnd) & 0x100 != 0
                        && lb_msg == 0x0180);
            let mut shared = ctx.shared.borrow_mut();
            let w = shared.gui.window_mut(hwnd)?;
            let index = if lb_msg == 0x0181 && wparam as i32 >= 0 {
                (wparam as usize).min(w.control.items.len())
            } else if sorted {
                w.control
                    .items
                    .iter()
                    .position(|(t, _)| t.to_ascii_lowercase() > text.to_ascii_lowercase())
                    .unwrap_or(w.control.items.len())
            } else {
                w.control.items.len()
            };
            w.control.items.insert(index, (text, data));
            drop(shared);
            redraw(ctx, hwnd);
            index as u32
        }
        0x0182 => {
            let mut shared = ctx.shared.borrow_mut();
            let w = shared.gui.window_mut(hwnd)?;
            if (wparam as usize) < w.control.items.len() {
                w.control.items.remove(wparam as usize);
                if w.control.selected >= w.control.items.len() as i32 {
                    w.control.selected = -1;
                }
                let n = w.control.items.len() as u32;
                drop(shared);
                redraw(ctx, hwnd);
                n
            } else {
                err
            }
        }
        0x0184 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.items.clear();
                w.control.selected = -1;
                w.control.top = 0;
            }
            redraw(ctx, hwnd);
            0
        }
        0x0186 => {
            let mut shared = ctx.shared.borrow_mut();
            let w = shared.gui.window_mut(hwnd)?;
            let index = wparam as i32;
            if index >= w.control.items.len() as i32 {
                return Some(err);
            }
            w.control.selected = index;
            if !is_combo_window(w) {
                let (_, ch) = client_size(w);
                let visible = (ch / LB_ITEM_HEIGHT).max(1);
                if index >= 0 && index < w.control.top {
                    w.control.top = index;
                } else if index >= w.control.top + visible {
                    w.control.top = index - visible + 1;
                }
            }
            if is_combo_window(w) {
                w.text = w
                    .control
                    .items
                    .get(index.max(0) as usize)
                    .map(|(t, _)| t.clone())
                    .unwrap_or_default();
                if index < 0 {
                    w.text.clear();
                }
            }
            drop(shared);
            redraw(ctx, hwnd);
            if index < 0 {
                err
            } else {
                index as u32
            }
        }
        0x0185 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                if wparam != 0 {
                    w.control.selected = lparam as i32;
                }
            }
            redraw(ctx, hwnd);
            0
        }
        0x0187 => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| (w.control.selected == wparam as i32) as u32)?,
        0x0188 => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.control.selected as u32)?,
        0x0189 => {
            let text = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .and_then(|w| w.control.items.get(wparam as usize).cloned());
            match text {
                Some((text, data)) => {
                    if has_strings {
                        let _ = write_cstr(memory, lparam, &text);
                        text.len() as u32
                    } else {
                        let _ = memory.write_u32(lparam, data);
                        4
                    }
                }
                None => err,
            }
        }
        0x018A => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .and_then(|w| {
                w.control
                    .items
                    .get(wparam as usize)
                    .map(|(t, _)| t.len() as u32)
            })
            .unwrap_or(err),
        0x018B => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.control.items.len() as u32)?,
        0x018C | 0x018F | 0x01A2 => {
            let text = read_cstr(memory, lparam);
            let items = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| w.control.items.clone())?;
            let found = find_item(&items, wparam as i32, &text, lb_msg == 0x01A2);
            if lb_msg == 0x018C && found >= 0 {
                list_message(ctx, memory, hwnd, 0x0186, found as u32, 0, has_strings);
            }
            if found < 0 {
                err
            } else {
                found as u32
            }
        }
        0x018E => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.control.top as u32)?,
        0x0197 => {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.control.top = wparam as i32;
            }
            redraw(ctx, hwnd);
            0
        }
        0x0190 => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| (w.control.selected >= 0) as u32)?,
        0x0199 => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .and_then(|w| w.control.items.get(wparam as usize).map(|(_, d)| *d))
            .unwrap_or(err),
        0x019A => {
            let mut shared = ctx.shared.borrow_mut();
            let w = shared.gui.window_mut(hwnd)?;
            match w.control.items.get_mut(wparam as usize) {
                Some(item) => {
                    item.1 = lparam;
                    0
                }
                None => err,
            }
        }
        0x01A1 => LB_ITEM_HEIGHT as u32,
        0x01A0 => 0,
        0x018D => 0,
        _ => return None,
    })
}

fn is_combo(ctx: &ApiContext, hwnd: u32) -> bool {
    ctx.shared
        .borrow()
        .gui
        .window(hwnd)
        .map(is_combo_window)
        .unwrap_or(false)
}

fn is_combo_window(w: &crate::win32::GuestWindow) -> bool {
    w.class.eq_ignore_ascii_case("ComboBox")
}

fn listbox_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    let has_strings = window_style(ctx, hwnd) & 0x30 != 0x10 || window_style(ctx, hwnd) & 0x40 != 0;
    if let Some(result) = list_message(ctx, memory, hwnd, msg, wparam, lparam, has_strings) {
        return result;
    }
    match msg {
        WM_PAINT => {
            let ps = user::scratch(ctx, 80);
            let hdc = user::begin_paint(ctx, memory, hwnd, ps);
            listbox_paint(ctx, memory, hwnd, hdc);
            user::end_paint(ctx, hwnd, hdc);
            user::free_scratch(ctx, ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            set_focus(ctx, memory, hwnd);
            let index = {
                let shared = ctx.shared.borrow();
                let w = shared.gui.window(hwnd);
                w.map(|w| w.control.top + hi(lparam) / LB_ITEM_HEIGHT)
                    .unwrap_or(-1)
            };
            let count = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| w.control.items.len() as i32)
                .unwrap_or(0);
            if index >= 0 && index < count {
                list_message(ctx, memory, hwnd, 0x0186, index as u32, 0, true);
                if window_style(ctx, hwnd) & 0x1 != 0 {
                    notify_parent(ctx, memory, hwnd, LBN_SELCHANGE);
                    if msg == WM_LBUTTONDBLCLK {
                        notify_parent(ctx, memory, hwnd, LBN_DBLCLK);
                    }
                }
            }
            0
        }
        WM_KEYDOWN => {
            let (selected, count) = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| (w.control.selected, w.control.items.len() as i32))
                .unwrap_or((-1, 0));
            let next = match wparam {
                0x26 => (selected - 1).max(0),
                0x28 => (selected + 1).min(count - 1),
                0x24 => 0,
                0x23 => count - 1,
                _ => return 0,
            };
            if next != selected && next >= 0 {
                list_message(ctx, memory, hwnd, 0x0186, next as u32, 0, true);
                if window_style(ctx, hwnd) & 0x1 != 0 {
                    notify_parent(ctx, memory, hwnd, LBN_SELCHANGE);
                }
            }
            0
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            redraw(ctx, hwnd);
            0
        }
        WM_GETDLGCODE => 0x1 | 0x80,
        _ => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

// ── Combo box ────────────────────────────────────────────────────────────

fn combo_paint<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, hdc: u32) {
    let Some(window) = ctx.shared.borrow().gui.window(hwnd).cloned() else {
        return;
    };
    let (cw, ch) = client_size(&window);
    let focused = ctx.shared.borrow().gui.focus_hwnd == hwnd;
    let kind = window.style & 0x3;
    let height = ch.min(21);
    let face = gfx::pick_face(13, false);
    let text = window.text.clone();
    with_client(ctx, hwnd, |p| {
        let field = Rect::new(0, 0, cw, height);
        p.fill(
            field,
            if window.disabled {
                sys_color(15)
            } else {
                sys_color(5)
            },
        );
        p.sunken(field);
        if kind != 1 {
            let button = Rect::new(cw - 18, 2, cw - 2, height - 2);
            p.fill(button, sys_color(15));
            p.raised_button(button);
            let cx = button.left + 7;
            let cy = button.top + button.height() / 2 - 1;
            for i in 0..4 {
                p.hline(cx - 3 + i, cx + 4 - i, cy + i, 0);
            }
        }
        let inner = Rect::new(3, 3, cw - 20, height - 3);
        // The text stays out of the button.
        let narrowed = p.clip.intersect_rect(&Rect::new(0, 0, inner.right, height).offset(p.ox, p.oy));
        p.clip = narrowed;
        if kind == 3 && focused {
            p.fill(inner, sys_color(13));
            p.text(4, (height - face.height) / 2, &text, sys_color(14), false);
        } else {
            p.text(
                4,
                (height - face.height) / 2,
                &text,
                if window.disabled {
                    sys_color(17)
                } else {
                    sys_color(8)
                },
                false,
            );
        }
    });
    let _ = (memory, hdc);
}

fn combo_dropdown<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    notify_parent(ctx, memory, hwnd, CBN_DROPDOWN);
    let Some(window) = ctx.shared.borrow().gui.window(hwnd).cloned() else {
        return;
    };
    let items = window.control.items.clone();
    if items.is_empty() || !user::interactive(ctx, memory) {
        notify_parent(ctx, memory, hwnd, CBN_CLOSEUP);
        return;
    }
    let (sx, sy) = user::window_screen_origin(&ctx.shared.borrow().gui, hwnd);
    let visible = items.len().min(10) as i32;
    let rect = Rect::sized(sx, sy + 21, window.width, visible * LB_ITEM_HEIGHT + 2);
    let popup = {
        let mut shared = ctx.shared.borrow_mut();
        let popup = shared.gui.next_hwnd;
        shared.gui.next_hwnd += 2;
        shared.gui.windows.push(crate::win32::GuestWindow {
            hwnd: popup,
            class: "ComboLBox".into(),
            x: rect.left,
            y: rect.top,
            width: rect.width(),
            height: rect.height(),
            style: WS_POPUP,
            visible: true,
            owner: hwnd,
            ..Default::default()
        });
        user::ensure_surface(&mut shared, popup);
        popup
    };
    let mut hot = window.control.selected;
    let mut top = hot.max(0).min((items.len() as i32 - visible).max(0));
    let draw = |ctx: &ApiContext, hot: i32, top: i32| {
        let mut shared = ctx.shared.borrow_mut();
        with_painter(&mut shared, popup, false, |p| {
            let full = Rect::new(0, 0, rect.width(), rect.height());
            p.fill(full, sys_color(5));
            p.hline(0, full.right, 0, 0);
            p.hline(0, full.right, full.bottom - 1, 0);
            p.vline(0, 0, full.bottom, 0);
            p.vline(full.right - 1, 0, full.bottom, 0);
            for row in 0..visible {
                let index = top + row;
                let Some((text, _)) = items.get(index as usize) else {
                    break;
                };
                let y = 1 + row * LB_ITEM_HEIGHT;
                let selected = index == hot;
                if selected {
                    p.fill(
                        Rect::new(1, y, full.right - 1, y + LB_ITEM_HEIGHT),
                        sys_color(13),
                    );
                }
                p.text(
                    3,
                    y,
                    text,
                    if selected {
                        sys_color(14)
                    } else {
                        sys_color(8)
                    },
                    false,
                );
            }
        });
    };
    draw(ctx, hot, top);
    let mut chosen = None;
    let mut released_once = false;
    while let Some(event) = user::next_raw_input(ctx, memory) {
        match event {
            RawInput::Move { x, y } => {
                if rect.contains(x, y) {
                    let index = top + (y - rect.top - 1) / LB_ITEM_HEIGHT;
                    if index != hot && (index as usize) < items.len() {
                        hot = index;
                        draw(ctx, hot, top);
                    }
                }
            }
            RawInput::Button { x, y, down, .. } => {
                if rect.contains(x, y) {
                    if !down {
                        chosen = Some(hot);
                        break;
                    }
                } else if down {
                    break;
                } else if released_once {
                    // Click-release outside after the opening click.
                } else {
                    released_once = true;
                }
            }
            RawInput::Key { vk, down: true, .. } => match vk {
                0x1B => break,
                0x0D => {
                    chosen = Some(hot);
                    break;
                }
                0x26 | 0x28 => {
                    hot = (hot + if vk == 0x26 { -1 } else { 1 }).clamp(0, items.len() as i32 - 1);
                    if hot < top {
                        top = hot;
                    } else if hot >= top + visible {
                        top = hot - visible + 1;
                    }
                    draw(ctx, hot, top);
                }
                _ => {}
            },
            _ => {}
        }
    }
    {
        let mut shared = ctx.shared.borrow_mut();
        shared.gui.windows.retain(|w| w.hwnd != popup);
        shared.gdi.surfaces.remove(&popup);
        shared.gui.dirty = true;
    }
    notify_parent(ctx, memory, hwnd, CBN_CLOSEUP);
    if let Some(index) = chosen {
        if index >= 0 {
            list_message(ctx, memory, hwnd, 0x0186, index as u32, 0, true);
            notify_parent(ctx, memory, hwnd, CBN_SELENDOK);
            notify_parent(ctx, memory, hwnd, CBN_SELCHANGE);
        }
    }
    redraw(ctx, hwnd);
}

fn combo_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    let has_strings =
        window_style(ctx, hwnd) & 0x30 != 0x10 || window_style(ctx, hwnd) & 0x200 != 0;
    // Map CB_* to LB_* equivalents.
    let mapped = match msg {
        0x0143 => Some(0x0180),
        0x014A => Some(0x0181),
        0x0144 => Some(0x0182),
        0x014B => Some(0x0184),
        0x014E => Some(0x0186),
        0x0147 => Some(0x0188),
        0x0148 => Some(0x0189),
        0x0149 => Some(0x018A),
        0x0146 => Some(0x018B),
        0x014D => Some(0x018C),
        0x014C => Some(0x018F),
        0x0158 => Some(0x01A2),
        0x0150 => Some(0x0199),
        0x0151 => Some(0x019A),
        _ => None,
    };
    if let Some(lb) = mapped {
        let result =
            list_message(ctx, memory, hwnd, lb, wparam, lparam, has_strings).unwrap_or(u32::MAX);
        if lb == 0x0184 {
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.text.clear();
            }
        }
        return result;
    }
    match msg {
        WM_PAINT => {
            let ps = user::scratch(ctx, 80);
            let hdc = user::begin_paint(ctx, memory, hwnd, ps);
            combo_paint(ctx, memory, hwnd, hdc);
            user::end_paint(ctx, hwnd, hdc);
            user::free_scratch(ctx, ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            set_focus(ctx, memory, hwnd);
            if window_style(ctx, hwnd) & 0x3 != 1 {
                combo_dropdown(ctx, memory, hwnd);
            }
            0
        }
        WM_CHAR => {
            if window_style(ctx, hwnd) & 0x3 == 2 || window_style(ctx, hwnd) & 0x3 == 1 {
                let ch = wparam as u8;
                if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                    if ch == 0x08 {
                        w.text.pop();
                    } else if ch >= 0x20 {
                        w.text.push(ch);
                    }
                }
                redraw(ctx, hwnd);
                notify_parent(ctx, memory, hwnd, CBN_EDITCHANGE);
            }
            0
        }
        WM_KEYDOWN => {
            match wparam {
                0x73 => combo_dropdown(ctx, memory, hwnd),
                0x26 | 0x28 => {
                    let (selected, count) = ctx
                        .shared
                        .borrow()
                        .gui
                        .window(hwnd)
                        .map(|w| (w.control.selected, w.control.items.len() as i32))
                        .unwrap_or((-1, 0));
                    let next = (selected + if wparam == 0x26 { -1 } else { 1 }).clamp(0, count - 1);
                    if next != selected && count > 0 {
                        list_message(ctx, memory, hwnd, 0x0186, next as u32, 0, true);
                        notify_parent(ctx, memory, hwnd, CBN_SELCHANGE);
                    }
                }
                _ => {}
            }
            0
        }
        WM_SETTEXT => {
            let text = read_cstr(memory, lparam);
            if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                w.text = text;
            }
            redraw(ctx, hwnd);
            1
        }
        WM_GETTEXT => {
            let text = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| w.text.clone())
                .unwrap_or_default();
            if wparam == 0 {
                return 0;
            }
            let mut text = text;
            text.truncate(wparam as usize - 1);
            let _ = write_cstr(memory, lparam, &text);
            text.len() as u32
        }
        WM_GETTEXTLENGTH => ctx
            .shared
            .borrow()
            .gui
            .window(hwnd)
            .map(|w| w.text.len() as u32)
            .unwrap_or(0),
        0x0140 => 0,
        0x0141 | 0x0142 | 0x0153 | 0x0155 => 1,
        0x0154 => LB_ITEM_HEIGHT as u32,
        0x014F => {
            if wparam != 0 {
                combo_dropdown(ctx, memory, hwnd);
            }
            1
        }
        0x0157 => 0,
        0x0152 => {
            let rect = user::window_screen_rect(&ctx.shared.borrow().gui, hwnd);
            let _ = gdi::write_rect(memory, lparam, rect);
            1
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            redraw(ctx, hwnd);
            0
        }
        WM_GETDLGCODE => 0x1 | 0x80,
        _ => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

// ── Scroll bar control ───────────────────────────────────────────────────

fn scrollbar_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    let vertical = window_style(ctx, hwnd) & 0x1 != 0;
    match msg {
        WM_PAINT => {
            let ps = user::scratch(ctx, 80);
            let hdc = user::begin_paint(ctx, memory, hwnd, ps);
            let window = ctx.shared.borrow().gui.window(hwnd).cloned();
            if let Some(window) = window {
                let (cw, ch) = client_size(&window);
                let (min, max, pos, page) = window.scroll[0];
                with_client(ctx, hwnd, |p| {
                    user::draw_scroll_bar(p, Rect::new(0, 0, cw, ch), vertical, min, max, pos, page)
                });
            }
            user::end_paint(ctx, hwnd, hdc);
            user::free_scratch(ctx, ps);
            0
        }
        WM_ERASEBKGND => 1,
        0x00E0 | 0x00E1 | 0x00E2 | 0x00E3 | 0x00E6 => {
            let mut shared = ctx.shared.borrow_mut();
            let Some(w) = shared.gui.window_mut(hwnd) else {
                return 0;
            };
            let entry = &mut w.scroll[0];
            let result = match msg {
                0x00E0 => {
                    let previous = entry.2;
                    entry.2 = (wparam as i32).clamp(entry.0, entry.1.max(entry.0));
                    previous as u32
                }
                0x00E1 => entry.2 as u32,
                0x00E2 | 0x00E6 => {
                    entry.0 = wparam as i32;
                    entry.1 = lparam as i32;
                    entry.2 = entry.2.clamp(entry.0, entry.1.max(entry.0));
                    0
                }
                _ => {
                    let (min, max) = (entry.0, entry.1);
                    drop(shared);
                    let _ = memory.write_u32(wparam, min as u32);
                    let _ = memory.write_u32(lparam, max as u32);
                    return 0;
                }
            };
            drop(shared);
            redraw(ctx, hwnd);
            result
        }
        WM_LBUTTONDOWN => {
            let (parent, window) = {
                let shared = ctx.shared.borrow();
                let Some(w) = shared.gui.window(hwnd) else {
                    return 0;
                };
                (w.parent, w.clone())
            };
            let (cw, ch) = client_size(&window);
            let length = if vertical { ch } else { cw };
            let along = if vertical { hi(lparam) } else { lo(lparam) };
            let (min, max, pos, page) = window.scroll[0];
            let arrow = user::SCROLL_SIZE.min(length / 2);
            let code = if along < arrow {
                0
            } else if along >= length - arrow {
                1
            } else if let Some((start, _)) = user::scroll_thumb(length, min, max, pos, page) {
                if along < start {
                    2
                } else {
                    3
                }
            } else {
                return 0;
            };
            let message = if vertical {
                user::WM_VSCROLL
            } else {
                user::WM_HSCROLL
            };
            send(ctx, memory, parent, message, code, hwnd);
            send(ctx, memory, parent, message, 8, hwnd);
            0
        }
        _ => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

/// The rich edit control (RICHED32's `RICHEDIT` class) as a plain edit
/// control: text streamed in (plain or RTF) shows as plain text, and
/// formatting requests are accepted without effect.
fn rich_edit_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    const EM_EXGETSEL: u32 = 0x434;
    const EM_STREAMIN: u32 = 0x449;
    const SF_RTF: u32 = 2;
    match msg {
        EM_STREAMIN => {
            // EDITSTREAM { cookie, error, callback(cookie, buffer, size, &read) }
            let cookie = memory.read_u32(lparam).unwrap_or(0);
            let callback = memory.read_u32(lparam + 8).unwrap_or(0);
            let buffer = ctx.shared.borrow_mut().heap.alloc(4096 + 4).unwrap_or(0);
            let read_va = buffer + 4096;
            let mut data = Vec::new();
            for _ in 0..4096 {
                let _ = memory.write_u32(read_va, 0);
                let status = memory.call_guest(callback, &[cookie, buffer, 4096, read_va]).unwrap_or(1);
                let read = memory.read_u32(read_va).unwrap_or(0).min(4096);
                if status != 0 || read == 0 {
                    break;
                }
                data.extend(memory.read_bytes(buffer, read as usize).unwrap_or_default());
            }
            ctx.shared.borrow_mut().heap.free(buffer);
            let text = if wparam & SF_RTF != 0 || data.starts_with(b"{\\rtf") { rtf_text(&data) } else { data };
            let copy = ctx.shared.borrow_mut().heap.alloc(text.len() as u32 + 1).unwrap_or(0);
            let _ = memory.write_bytes(copy, &[text.as_slice(), &[0]].concat());
            edit_proc(ctx, memory, hwnd, WM_SETTEXT, 0, copy);
            ctx.shared.borrow_mut().heap.free(copy);
            text.len() as u32
        }
        EM_EXGETSEL => {
            let _ = memory.write_bytes(lparam, &[0; 8]);
            0
        }
        // Rich edit messages (formatting, events, options, targets).
        0x435..=0x45F => match msg {
            0x444 | 0x448 => 1, // EM_SETCHARFORMAT, EM_SETTARGETDEVICE
            _ => 0,
        },
        _ => edit_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

/// The text of an RTF document: control words dropped (`\par` and
/// `\line` as line breaks, `\tab`, `\'hh` characters), and the font,
/// colour and style tables and `{\* ...}` destinations skipped.
pub fn rtf_text(rtf: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut skip_depth: Option<usize> = None;
    let mut depth = 0usize;
    let mut i = 0;
    while i < rtf.len() {
        let byte = rtf[i];
        match byte {
            b'{' => {
                depth += 1;
                i += 1;
                if skip_depth.is_none() {
                    let rest = &rtf[i..];
                    let skipped = [b"\\*".as_slice(), b"\\fonttbl", b"\\colortbl", b"\\stylesheet", b"\\info", b"\\pict"];
                    if skipped.iter().any(|word| rest.starts_with(word)) {
                        skip_depth = Some(depth);
                    }
                }
            }
            b'}' => {
                if skip_depth == Some(depth) {
                    skip_depth = None;
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'\\' => {
                i += 1;
                let Some(&next) = rtf.get(i) else { break };
                if !next.is_ascii_alphabetic() {
                    i += 1;
                    if skip_depth.is_some() {
                        continue;
                    }
                    match next {
                        b'\'' => {
                            let hex = rtf.get(i..i + 2).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
                            if let Some(value) = hex {
                                out.push(value);
                            }
                            i += 2;
                        }
                        b'\\' | b'{' | b'}' => out.push(next),
                        b'~' => out.push(b' '),
                        _ => {}
                    }
                    continue;
                }
                let start = i;
                while rtf.get(i).is_some_and(u8::is_ascii_alphabetic) {
                    i += 1;
                }
                let word = &rtf[start..i];
                if rtf.get(i) == Some(&b'-') {
                    i += 1;
                }
                while rtf.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                if rtf.get(i) == Some(&b' ') {
                    i += 1;
                }
                if skip_depth.is_some() {
                    continue;
                }
                match word {
                    b"par" | b"line" => out.extend_from_slice(b"\r\n"),
                    b"tab" => out.push(b'\t'),
                    _ => {}
                }
            }
            b'\r' | b'\n' => i += 1,
            _ => {
                if skip_depth.is_none() {
                    out.push(byte);
                }
                i += 1;
            }
        }
    }
    out
}

/// Entry point for the Rust-backed control window procedures.
pub fn control_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    match name {
        "OTW_ButtonProc" => button_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_StaticProc" => static_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_EditProc" => edit_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_RichEditProc" => rich_edit_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_ListBoxProc" => listbox_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_ComboBoxProc" => combo_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_ScrollBarProc" => scrollbar_proc(ctx, memory, hwnd, msg, wparam, lparam),
        "OTW_ColorPanelProc" => {
            crate::win32::commdlg::color_panel_proc(ctx, memory, hwnd, msg, wparam, lparam)
        }
        _ => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

// ── Dialogs ──────────────────────────────────────────────────────────────

const DWL_MSGRESULT: usize = 0;

pub fn set_msg_result(ctx: &ApiContext, hwnd: u32, value: u32) {
    if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
        if w.extra.len() >= 4 {
            w.extra[DWL_MSGRESULT..DWL_MSGRESULT + 4].copy_from_slice(&value.to_le_bytes());
        }
    }
}

pub fn msg_result(ctx: &ApiContext, hwnd: u32) -> u32 {
    ctx.shared
        .borrow()
        .gui
        .window(hwnd)
        .filter(|w| w.extra.len() >= 4)
        .map(|w| u32::from_le_bytes([w.extra[0], w.extra[1], w.extra[2], w.extra[3]]))
        .unwrap_or(0)
}

fn tab_controls(ctx: &ApiContext, dialog: u32) -> Vec<u32> {
    let shared = ctx.shared.borrow();
    user::children(&shared.gui, dialog)
        .into_iter()
        .rev()
        .filter(|h| {
            shared
                .gui
                .window(*h)
                .map(|w| w.style & WS_TABSTOP != 0 && w.visible && !w.disabled)
                .unwrap_or(false)
        })
        .collect()
}

pub fn def_dlg_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    let dlg_proc = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.dlg_proc)
        .unwrap_or(0);
    if dlg_proc != 0 {
        set_msg_result(ctx, hwnd, 0);
        let handled =
            crate::win32::unicode::call_proc(ctx, memory, dlg_proc, hwnd, msg, wparam, lparam)
                .unwrap_or(0);
        if !is_window(ctx, hwnd) {
            return 0;
        }
        if handled != 0 {
            return match msg {
                WM_CTLCOLOREDIT..=WM_CTLCOLORSTATIC
                | 0x0132
                | WM_INITDIALOG
                | 0x0039
                | 0x002E
                | 0x002F
                | 0x0037 => handled,
                _ => msg_result(ctx, hwnd),
            };
        }
    }
    let is_msgbox = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.props.iter().any(|(k, _)| k == "otw_msgbox"))
        .unwrap_or(false);
    match msg {
        WM_CLOSE => {
            let cancel = dialog_item(ctx, hwnd, 2);
            send(ctx, memory, hwnd, WM_COMMAND, make_long(2, 0), cancel);
            0
        }
        WM_COMMAND if crate::win32::commdlg::is_color_dialog(ctx, hwnd) => {
            crate::win32::commdlg::color_command(
                ctx,
                memory,
                hwnd,
                lo(wparam) as u32,
                hi(wparam) as u32,
            );
            0
        }
        WM_COMMAND if is_file_dialog(ctx, hwnd) => {
            file_dialog_command(ctx, memory, hwnd, lo(wparam) as u32, hi(wparam) as u32);
            0
        }
        WM_COMMAND if is_msgbox => {
            if hi(wparam) == 0 {
                end_dialog(ctx, hwnd, lo(wparam) as u32);
            }
            0
        }
        WM_SETFOCUS => {
            let first = tab_controls(ctx, hwnd).first().copied();
            if let Some(first) = first {
                set_focus(ctx, memory, first);
            }
            0
        }
        WM_ERASEBKGND => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
        0x0400 => make_long(1, 0x534B), // DM_GETDEFID
        0x0401 => 1,
        0x0028 => {
            // WM_NEXTDLGCTL
            let target = if lparam & 0xFFFF != 0 {
                wparam
            } else {
                let controls = tab_controls(ctx, hwnd);
                let focus = ctx.shared.borrow().gui.focus_hwnd;
                let position = controls.iter().position(|h| *h == focus).unwrap_or(0);
                if controls.is_empty() {
                    0
                } else if wparam != 0 {
                    controls[(position + controls.len() - 1) % controls.len()]
                } else {
                    controls[(position + 1) % controls.len()]
                }
            };
            if target != 0 {
                set_focus(ctx, memory, target);
            }
            0
        }
        WM_CTLCOLORDLG | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            def_window_proc(ctx, memory, hwnd, msg, wparam, lparam)
        }
        _ => def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

/// A dialog's control with the given id (0 if none).
pub fn dialog_item_of(ctx: &ApiContext, dialog: u32, id: u32) -> u32 {
    dialog_item(ctx, dialog, id)
}

/// A dialog's first control that takes the focus.
pub fn first_tab_stop(ctx: &ApiContext, dialog: u32) -> Option<u32> {
    tab_controls(ctx, dialog).first().copied()
}

/// Run a dialog's modal loop until EndDialog; returns its result.
pub fn run_modal_for<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, dialog: u32, owner: u32) -> u32 {
    run_modal(ctx, memory, dialog, owner)
}

/// IsDialogMessage for the MSG at `message_va`.
pub fn is_dialog_message_for<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, dialog: u32, message_va: u32) -> u32 {
    let message = user::read_message(memory, message_va);
    u32::from(is_dialog_message(ctx, memory, dialog, message))
}

fn dialog_item(ctx: &ApiContext, dialog: u32, id: u32) -> u32 {
    let shared = ctx.shared.borrow();
    shared
        .gui
        .windows
        .iter()
        .find(|w| w.parent == dialog && w.style & WS_CHILD != 0 && w.id == id)
        .map(|w| w.hwnd)
        .unwrap_or(0)
}

/// EndDialog for dialogs driven by Rust (common dialogs).
pub fn end_dialog_with(ctx: &ApiContext, dialog: u32, result: u32) {
    end_dialog(ctx, dialog, result);
}

fn end_dialog(ctx: &ApiContext, dialog: u32, result: u32) {
    if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(dialog) {
        w.dlg_result = Some(result);
    }
}

/// Dialog base units (horizontal, vertical) for the default dialog font.
fn base_units() -> (i32, i32) {
    let face = gfx::pick_face(13, false);
    let sample = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let average = (face.text_width(sample) + 26) / 52;
    (average.max(5), face.height)
}

fn read_sz_or_ord<M: GuestMemory + ?Sized>(
    memory: &mut M,
    cursor: &mut u32,
) -> (Option<u32>, Vec<u8>) {
    let first = memory
        .read_bytes(*cursor, 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0);
    if first == 0xFFFF {
        let ordinal = memory
            .read_bytes(*cursor + 2, 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .unwrap_or(0) as u32;
        *cursor += 4;
        return (Some(ordinal), Vec::new());
    }
    let mut text = Vec::new();
    for _ in 0..256 {
        let unit = memory
            .read_bytes(*cursor, 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .unwrap_or(0);
        *cursor += 2;
        if unit == 0 {
            break;
        }
        text.push(if unit <= 0xFF { unit as u8 } else { b'?' });
    }
    (None, text)
}

/// One control of a dialog template.
#[derive(Clone, Debug)]
pub struct TemplateItem {
    pub style: u32,
    pub ex_style: u32,
    pub x: i32,
    pub y: i32,
    pub cx: i32,
    pub cy: i32,
    pub id: u32,
    pub class: String,
    pub title: Vec<u8>,
    /// Creation data (the item's extra bytes), or 0.
    pub extra_va: u32,
}

/// A dialog template (`DLGTEMPLATE` or `DLGTEMPLATEEX`), in dialog units.
#[derive(Clone, Debug)]
pub struct DialogTemplate {
    pub style: u32,
    pub ex_style: u32,
    pub x: i32,
    pub y: i32,
    pub cx: i32,
    pub cy: i32,
    pub class: Option<String>,
    pub title: Vec<u8>,
    pub items: Vec<TemplateItem>,
}

/// Parse a dialog template in guest memory, either format.
pub fn parse_template<M: GuestMemory + ?Sized>(memory: &mut M, template: u32) -> Option<DialogTemplate> {
    let read16 = |memory: &mut M, va: u32| memory.read_bytes(va, 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
    let extended = read16(memory, template) == 1 && read16(memory, template + 2) == 0xFFFF;
    let (style, ex_style, header) = if extended {
        (memory.read_u32(template + 12)?, memory.read_u32(template + 8)?, template + 16)
    } else {
        (memory.read_u32(template)?, memory.read_u32(template + 4)?, template + 8)
    };
    let item_count = read16(memory, header) as u32;
    let x = read16(memory, header + 2) as i16 as i32;
    let y = read16(memory, header + 4) as i16 as i32;
    let cx = read16(memory, header + 6) as i16 as i32;
    let cy = read16(memory, header + 8) as i16 as i32;
    let mut cursor = header + 10;
    let _menu = read_sz_or_ord(memory, &mut cursor);
    let (class_ord, class_name) = read_sz_or_ord(memory, &mut cursor);
    let (_, title) = read_sz_or_ord(memory, &mut cursor);
    if style & 0x40 != 0 {
        // DS_SETFONT: point size (and weight, italic, charset), typeface.
        cursor += if extended { 6 } else { 2 };
        let _ = read_sz_or_ord(memory, &mut cursor);
    }
    let class = match (class_ord, class_name.is_empty()) {
        (Some(_), _) | (None, true) => None,
        (None, false) => Some(String::from_utf8_lossy(&class_name).into_owned()),
    };
    let mut items = Vec::new();
    for _ in 0..item_count.min(256) {
        // Items are DWORD aligned.
        cursor = (cursor + 3) & !3;
        let (item_style, item_ex, geometry, id, next) = if extended {
            // helpID, exStyle, style, x, y, cx, cy, DWORD id
            (memory.read_u32(cursor + 8)?, memory.read_u32(cursor + 4)?, cursor + 12, memory.read_u32(cursor + 20)?, cursor + 24)
        } else {
            (memory.read_u32(cursor)?, memory.read_u32(cursor + 4)?, cursor + 8, read16(memory, cursor + 16) as u32, cursor + 18)
        };
        let ix = read16(memory, geometry) as i16 as i32;
        let iy = read16(memory, geometry + 2) as i16 as i32;
        let icx = read16(memory, geometry + 4) as i16 as i32;
        let icy = read16(memory, geometry + 6) as i16 as i32;
        cursor = next;
        let (class_ord, class_text) = read_sz_or_ord(memory, &mut cursor);
        let (title_ord, title_text) = read_sz_or_ord(memory, &mut cursor);
        let extra = read16(memory, cursor) as u32;
        let extra_va = if extra > 0 { cursor + 2 } else { 0 };
        cursor += 2 + extra;
        let class = match class_ord {
            Some(0x80) => "Button".to_string(),
            Some(0x81) => "Edit".to_string(),
            Some(0x82) => "Static".to_string(),
            Some(0x83) => "ListBox".to_string(),
            Some(0x84) => "ScrollBar".to_string(),
            Some(0x85) => "ComboBox".to_string(),
            Some(other) => format!("#{other}"),
            None => String::from_utf8_lossy(&class_text).into_owned(),
        };
        // A resource ordinal title (an icon or bitmap of a static) is kept
        // as "#id" text, as Windows passes it to the control.
        let title = match title_ord {
            Some(ordinal) => format!("#{ordinal}").into_bytes(),
            None => title_text,
        };
        items.push(TemplateItem { style: item_style, ex_style: item_ex, x: ix, y: iy, cx: icx, cy: icy, id, class, title, extra_va });
    }
    Some(DialogTemplate { style, ex_style, x, y, cx, cy, class, title, items })
}

/// Size in pixels of dialog units.
pub fn dialog_units_to_pixels(x: i32, y: i32) -> (i32, i32) {
    let (bx, by) = base_units();
    ((x * bx + 2) / 4, (y * by + 4) / 8)
}

/// Create a dialog from a DLGTEMPLATE or DLGTEMPLATEEX in guest memory.
pub fn create_dialog<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    instance: u32,
    template: u32,
    owner: u32,
    dlg_proc: u32,
    param: u32,
) -> u32 {
    match parse_template(memory, template) {
        Some(template) => create_dialog_from(ctx, memory, instance, &template, owner, dlg_proc, param),
        None => 0,
    }
}

/// Create a dialog from a parsed template.
#[allow(clippy::too_many_arguments)]
pub fn create_dialog_from<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    instance: u32,
    template: &DialogTemplate,
    owner: u32,
    dlg_proc: u32,
    param: u32,
) -> u32 {
    let DialogTemplate { style, ex_style, x, y, cx, cy, .. } = *template;
    let title = template.title.clone();
    let (bx, by) = base_units();
    let to_px = |value: i32, base: i32, div: i32| (value * base + div / 2) / div;
    let (width, height) = (to_px(cx, bx, 4), to_px(cy, by, 8));
    let mut window_style = style & !WS_VISIBLE;
    if style & 0x80 != 0 {
        window_style |= user::WS_DLGFRAME;
    }
    let ex = ex_style
        | if style & 0x80 != 0 {
            user::WS_EX_DLGMODALFRAME
        } else {
            0
        };
    let (l, t, r, b) = user::nc_insets_for(window_style, ex, false);
    let (owner_x, owner_y) = if owner != 0 {
        user::client_screen_origin(&ctx.shared.borrow().gui, owner)
    } else {
        (0, 0)
    };
    let (sw, sh) = ctx.shared.borrow().gui.screen;
    let full_w = width + l + r;
    let full_h = height + t + b;
    let (mut px, mut py) = if window_style & WS_CHILD != 0 {
        (to_px(x, bx, 4), to_px(y, by, 8))
    } else {
        (owner_x + to_px(x, bx, 4), owner_y + to_px(y, by, 8))
    };
    if style & 0x800 != 0 || (window_style & WS_CHILD == 0 && x == 0 && y == 0) {
        // DS_CENTER (and the common 0,0 template) centre over the owner.
        let area = if owner != 0 {
            user::window_screen_rect(&ctx.shared.borrow().gui, owner)
        } else {
            Rect::new(0, 0, sw, sh)
        };
        px = area.left + (area.width() - full_w) / 2;
        py = area.top + (area.height() - full_h) / 2;
    }
    px = px.clamp(0, (sw - full_w).max(0));
    py = py.clamp(0, (sh - full_h).max(0));
    let class = template.class.clone().unwrap_or_else(|| "#32770".to_string());
    let font = ctx.shared.borrow_mut().gdi.insert(GdiObject::Font(LogFont {
        height: -11,
        width: 0,
        escapement: 0,
        weight: 400,
        italic: false,
        underline: false,
        strike: false,
        charset: 0,
        pitch: 0x22,
        face: b"MS Sans Serif".to_vec(),
    }));
    let hwnd = user::create_window(
        ctx,
        memory,
        CreateParams {
            ex_style: ex,
            class,
            title: title.clone(),
            style: window_style,
            x: px,
            y: py,
            width: full_w,
            height: full_h,
            parent: owner,
            instance,
            param,
            ..CreateParams::default()
        },
    );
    if hwnd == 0 {
        return 0;
    }
    {
        let mut shared = ctx.shared.borrow_mut();
        if let Some(w) = shared.gui.window_mut(hwnd) {
            w.dlg_proc = dlg_proc;
            w.dlg_lparam = param;
            if w.extra.len() < 12 {
                w.extra.resize(12, 0);
            }
            w.extra[4..8].copy_from_slice(&dlg_proc.to_le_bytes());
            w.font = font;
        }
    }
    for item in &template.items {
        let child = user::create_window(
            ctx,
            memory,
            CreateParams {
                ex_style: item.ex_style,
                class: item.class.clone(),
                title: item.title.clone(),
                style: item.style | WS_CHILD,
                x: to_px(item.x, bx, 4),
                y: to_px(item.y, by, 8),
                width: to_px(item.cx, bx, 4),
                height: to_px(item.cy, by, 8),
                parent: hwnd,
                menu: item.id,
                instance,
                param: item.extra_va,
                ..CreateParams::default()
            },
        );
        if child != 0 {
            send(ctx, memory, child, WM_SETFONT, font, 0);
        }
    }
    let first = tab_controls(ctx, hwnd).first().copied().unwrap_or(0);
    let focus_default = send(ctx, memory, hwnd, WM_INITDIALOG, first, param);
    if !is_window(ctx, hwnd) {
        return 0;
    }
    if style & WS_VISIBLE != 0 {
        show_window(ctx, memory, hwnd, 5);
    }
    if focus_default != 0 && first != 0 {
        set_focus(ctx, memory, first);
    }
    hwnd
}

/// Run a modal loop until EndDialog is called for `dialog`.
fn run_modal<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dialog: u32,
    owner: u32,
) -> u32 {
    let owner_top = if owner != 0 {
        ctx.shared
            .borrow()
            .gui
            .top_level_for(owner)
            .unwrap_or(owner)
    } else {
        0
    };
    let owner_was_enabled = owner_top != 0
        && ctx
            .shared
            .borrow()
            .gui
            .window(owner_top)
            .map(|w| !w.disabled)
            .unwrap_or(false);
    if owner_was_enabled {
        if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(owner_top) {
            w.disabled = true;
        }
    }
    let visible = ctx
        .shared
        .borrow()
        .gui
        .window(dialog)
        .map(|w| w.visible)
        .unwrap_or(false);
    if !visible
        && ctx
            .shared
            .borrow()
            .gui
            .window(dialog)
            .map(|w| w.dlg_result.is_none())
            .unwrap_or(false)
    {
        show_window(ctx, memory, dialog, 5);
    }
    let mut result = u32::MAX;
    loop {
        if let Some(value) = ctx
            .shared
            .borrow()
            .gui
            .window(dialog)
            .and_then(|w| w.dlg_result)
        {
            result = value;
            break;
        }
        if !is_window(ctx, dialog) || memory.stopping() {
            break;
        }
        if !user::interactive(ctx, memory) {
            // Headless: there is nobody to press a button.
            eprintln!("[user] modal dialog {dialog:#x} has no host input; cancelling");
            result = 2;
            break;
        }
        let Some(message) = user::fetch_message(ctx, memory, 0, 0, 0, true, true) else {
            continue;
        };
        if message.message == user::WM_QUIT {
            user::post(ctx, 0, user::WM_QUIT, message.wparam, 0);
            ctx.shared.borrow_mut().gui.quit = true;
            break;
        }
        if !is_dialog_message(ctx, memory, dialog, message) {
            translate_and_dispatch(ctx, memory, message);
        }
    }
    if owner_was_enabled {
        if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(owner_top) {
            w.disabled = false;
        }
    }
    user::destroy_window(ctx, memory, dialog);
    if owner_top != 0 && is_window(ctx, owner_top) {
        user::activate(ctx, memory, owner_top, 1);
    }
    result
}

fn translate_and_dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    message: GuestMessage,
) {
    if message.message == WM_KEYDOWN || message.message == user::WM_SYSKEYDOWN {
        let mut shared = ctx.shared.borrow_mut();
        if let Some(position) = shared
            .gui
            .pending_chars
            .iter()
            .position(|m| m.hwnd == message.hwnd)
        {
            let ch = shared.gui.pending_chars.remove(position).unwrap();
            shared.gui.posted.push_front(ch);
        }
    }
    user::dispatch_message(ctx, memory, message);
}

/// IsDialogMessage: keyboard navigation inside a dialog.
fn is_dialog_message<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dialog: u32,
    message: GuestMessage,
) -> bool {
    if message.hwnd != dialog && !is_descendant(&ctx.shared.borrow().gui, message.hwnd, dialog) {
        return false;
    }
    if message.message == WM_KEYDOWN {
        let code = send(ctx, memory, message.hwnd, WM_GETDLGCODE, message.wparam, 0);
        match message.wparam {
            0x09 if code & 0x4 == 0 && code & 0x2 == 0 => {
                let shift = ctx.shared.borrow().gui.keys[0x10] & 0x80 != 0;
                send(ctx, memory, dialog, 0x0028, shift as u32, 0);
                ctx.shared
                    .borrow_mut()
                    .gui
                    .pending_chars
                    .retain(|m| m.hwnd != message.hwnd);
                return true;
            }
            0x0D if code & 0x4 == 0 => {
                let focus = ctx.shared.borrow().gui.focus_hwnd;
                let focus_is_push = ctx
                    .shared
                    .borrow()
                    .gui
                    .window(focus)
                    .map(|w| w.class.eq_ignore_ascii_case("Button") && w.style & 0xF <= 1)
                    .unwrap_or(false);
                let id = if focus_is_push {
                    ctx.shared
                        .borrow()
                        .gui
                        .window(focus)
                        .map(|w| w.id)
                        .unwrap_or(1)
                } else {
                    lo(send(ctx, memory, dialog, 0x0400, 0, 0)) as u32
                };
                let control = dialog_item(ctx, dialog, id);
                ctx.shared
                    .borrow_mut()
                    .gui
                    .pending_chars
                    .retain(|m| m.hwnd != message.hwnd);
                send(
                    ctx,
                    memory,
                    dialog,
                    WM_COMMAND,
                    make_long(id as i32, 0),
                    control,
                );
                return true;
            }
            0x1B => {
                let control = dialog_item(ctx, dialog, 2);
                ctx.shared
                    .borrow_mut()
                    .gui
                    .pending_chars
                    .retain(|m| m.hwnd != message.hwnd);
                send(ctx, memory, dialog, WM_COMMAND, make_long(2, 0), control);
                return true;
            }
            0x25..=0x28 if code & 0x1 == 0 => {
                // Arrow keys move within a radio/button group.
                let focus = ctx.shared.borrow().gui.focus_hwnd;
                let group = radio_group(ctx, dialog, focus);
                if let Some(position) = group.iter().position(|h| *h == focus) {
                    let next = if matches!(message.wparam, 0x25 | 0x26) {
                        (position + group.len() - 1) % group.len()
                    } else {
                        (position + 1) % group.len()
                    };
                    set_focus(ctx, memory, group[next]);
                    if window_style(ctx, group[next]) & 0xF == 9 {
                        button_click(ctx, memory, group[next]);
                    }
                }
                return true;
            }
            _ => {}
        }
    }
    // Plain letters act as mnemonics unless the focused control wants
    // characters itself (edit boxes, list boxes).
    let plain_mnemonic = message.message == WM_CHAR
        && (message.hwnd == dialog
            || send(ctx, memory, message.hwnd, WM_GETDLGCODE, message.wparam, 0) & 0x80 == 0)
        && (message.wparam as u8).is_ascii_alphanumeric();
    if message.message == WM_SYSCHAR || plain_mnemonic {
        // Mnemonic: find a control whose text has &<char>.
        let ch = (message.wparam as u8).to_ascii_lowercase();
        let target = {
            let shared = ctx.shared.borrow();
            user::children(&shared.gui, dialog)
                .into_iter()
                .rev()
                .find(|h| {
                    shared
                        .gui
                        .window(*h)
                        .map(|w| {
                            let t = w.title.as_bytes();
                            t.windows(2)
                                .any(|p| p[0] == b'&' && p[1].to_ascii_lowercase() == ch)
                        })
                        .unwrap_or(false)
                })
        };
        if let Some(target) = target {
            let kind = window_style(ctx, target) & 0xF;
            let is_button = ctx
                .shared
                .borrow()
                .gui
                .window(target)
                .map(|w| w.class.eq_ignore_ascii_case("Button"))
                .unwrap_or(false);
            if is_button && kind != 7 {
                set_focus(ctx, memory, target);
                button_click(ctx, memory, target);
            } else {
                // A static label focuses the next control.
                let next = {
                    let shared = ctx.shared.borrow();
                    let siblings: Vec<u32> = user::children(&shared.gui, dialog)
                        .into_iter()
                        .rev()
                        .collect();
                    siblings
                        .iter()
                        .position(|h| *h == target)
                        .and_then(|p| siblings.get(p + 1).copied())
                };
                if let Some(next) = next {
                    set_focus(ctx, memory, next);
                }
            }
            return true;
        }
    }
    translate_and_dispatch(ctx, memory, message);
    true
}

/// Break lines longer than `max_width` at spaces (or anywhere, for one
/// overlong word).
fn wrap_text(face: &gfx::FontFace, text: &[u8], max_width: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for (index, line) in text.split(|b| *b == b'\n').enumerate() {
        if index > 0 {
            out.push(b'\n');
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let mut current: Vec<u8> = Vec::new();
        for word in line.split(|b| *b == b' ') {
            let candidate = if current.is_empty() { word.to_vec() } else { [current.as_slice(), b" ", word].concat() };
            if face.text_width(&candidate) <= max_width || current.is_empty() {
                current = candidate;
            } else {
                out.extend_from_slice(&current);
                out.push(b'\n');
                current = word.to_vec();
            }
        }
        out.extend_from_slice(&current);
    }
    out
}

/// Show a modal message box built from standard controls.
pub fn message_box<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    owner: u32,
    text: &[u8],
    caption: &[u8],
    kind: u32,
) -> u32 {
    eprintln!(
        "guest MessageBox: {} — {}",
        String::from_utf8_lossy(caption),
        String::from_utf8_lossy(text)
    );
    let buttons: &[(u32, &[u8])] = match kind & 0xF {
        1 => &[(1, b"OK"), (2, b"Cancel")],
        2 => &[(3, b"&Abort"), (4, b"&Retry"), (5, b"&Ignore")],
        3 => &[(6, b"&Yes"), (7, b"&No"), (2, b"Cancel")],
        4 => &[(6, b"&Yes"), (7, b"&No")],
        5 => &[(4, b"&Retry"), (2, b"Cancel")],
        _ => &[(1, b"OK")],
    };
    if !user::interactive(ctx, memory) {
        return buttons[0].0;
    }
    let face = gfx::pick_face(13, false);
    // Long lines wrap at word boundaries, as Windows wraps message text
    // to about five eighths of the screen.
    let max_width = (ctx.shared.borrow().gui.screen.0 * 5 / 8).max(200);
    let wrapped = wrap_text(&face, text, max_width);
    let text = wrapped.as_slice();
    let lines: Vec<&[u8]> = text.split(|b| *b == b'\n').collect();
    let text_w = lines.iter().map(|l| face.text_width(l)).max().unwrap_or(0);
    let text_h = face.height * lines.len() as i32;
    let icon = kind & 0xF0 != 0;
    let button_row = buttons.len() as i32 * 81 - 6;
    let client_w = (text_w + if icon { 60 } else { 24 } + 12)
        .max(button_row + 24)
        .max(140);
    let client_h = text_h.max(if icon { 32 } else { 0 }) + 24 + 23 + 16;
    let style = user::WS_POPUP | user::WS_CAPTION | user::WS_SYSMENU | user::WS_DLGFRAME;
    let (l, t, r, b) = user::nc_insets_for(style, user::WS_EX_DLGMODALFRAME, false);
    let (sw, sh) = ctx.shared.borrow().gui.screen;
    let w = client_w + l + r;
    let h = client_h + t + b;
    let owner_top = if owner != 0 {
        ctx.shared
            .borrow()
            .gui
            .top_level_for(owner)
            .unwrap_or(owner)
    } else {
        ctx.shared.borrow().gui.active_hwnd
    };
    let area = if owner_top != 0 {
        user::window_screen_rect(&ctx.shared.borrow().gui, owner_top)
    } else {
        Rect::new(0, 0, sw, sh)
    };
    let dialog = user::create_window(
        ctx,
        memory,
        CreateParams {
            ex_style: user::WS_EX_DLGMODALFRAME,
            class: "#32770".into(),
            title: if caption.is_empty() {
                b"Error".to_vec()
            } else {
                caption.to_vec()
            },
            style,
            x: (area.left + (area.width() - w) / 2).clamp(0, (sw - w).max(0)),
            y: (area.top + (area.height() - h) / 2).clamp(0, (sh - h).max(0)),
            width: w,
            height: h,
            parent: owner_top,
            ..CreateParams::default()
        },
    );
    if dialog == 0 {
        return buttons[0].0;
    }
    if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(dialog) {
        window.props.push(("otw_msgbox".into(), 1));
        if window.extra.len() < 12 {
            window.extra.resize(12, 0);
        }
    }
    let text_x = if icon { 60 } else { 18 };
    user::create_window(
        ctx,
        memory,
        CreateParams {
            class: "Static".into(),
            title: text.to_vec(),
            style: WS_CHILD | WS_VISIBLE | 0x80,
            x: text_x,
            y: 16,
            width: text_w + 8,
            height: text_h,
            parent: dialog,
            menu: 0xFFFF,
            ..CreateParams::default()
        },
    );
    let mut x = (client_w - button_row) / 2;
    let mut first = 0;
    for (index, (id, label)) in buttons.iter().enumerate() {
        let button = user::create_window(
            ctx,
            memory,
            CreateParams {
                class: "Button".into(),
                title: label.to_vec(),
                style: WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | if index == 0 { 1 | WS_GROUP } else { 0 },
                x,
                y: client_h - 23 - 12,
                width: 75,
                height: 23,
                parent: dialog,
                menu: *id,
                ..CreateParams::default()
            },
        );
        if index == 0 {
            first = button;
        }
        x += 81;
    }
    show_window(ctx, memory, dialog, 5);
    if first != 0 {
        set_focus(ctx, memory, first);
    }
    run_modal(ctx, memory, dialog, owner_top)
}

// ── Common file dialogs ──────────────────────────────────────────────────

const FILE_EDIT_ID: u32 = 1152;
const FILE_LIST_ID: u32 = 1120;

fn is_file_dialog(ctx: &ApiContext, hwnd: u32) -> bool {
    ctx.shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.props.iter().any(|(k, _)| k == "otw_filedlg"))
        .unwrap_or(false)
}

fn file_dialog_command<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dialog: u32,
    id: u32,
    code: u32,
) {
    match (id, code) {
        (1, _) | (2, _) => end_dialog(ctx, dialog, id),
        (FILE_LIST_ID, LBN_SELCHANGE) | (FILE_LIST_ID, LBN_DBLCLK) => {
            let list = dialog_item(ctx, dialog, FILE_LIST_ID);
            let edit = dialog_item(ctx, dialog, FILE_EDIT_ID);
            let name = ctx.shared.borrow().gui.window(list).and_then(|w| {
                w.control
                    .items
                    .get(w.control.selected.max(0) as usize)
                    .map(|(t, _)| t.clone())
            });
            if let Some(name) = name {
                edit_set_text(ctx, edit, name);
                redraw(ctx, edit);
            }
            if code == LBN_DBLCLK {
                end_dialog(ctx, dialog, 1);
            }
        }
        _ => {
            let _ = memory;
        }
    }
}

/// First wildcard pattern of an OPENFILENAME filter ("Name\0*.bmp;*.dib\0...").
fn filter_patterns<M: GuestMemory + ?Sized>(
    memory: &mut M,
    filter_va: u32,
    index: u32,
) -> Vec<Vec<u8>> {
    if filter_va == 0 {
        return vec![b"*.*".to_vec()];
    }
    let mut strings = Vec::new();
    let mut cursor = filter_va;
    for _ in 0..32 {
        let text = read_cstr(memory, cursor);
        if text.is_empty() {
            break;
        }
        cursor += text.len() as u32 + 1;
        strings.push(text);
    }
    let pair = (index.max(1) as usize - 1) * 2 + 1;
    let patterns = strings
        .get(pair)
        .or_else(|| strings.get(1))
        .cloned()
        .unwrap_or_else(|| b"*.*".to_vec());
    patterns.split(|b| *b == b';').map(|p| p.to_vec()).collect()
}

fn pattern_match(pattern: &[u8], name: &[u8]) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    if pattern == b"*.*" || pattern == b"*" {
        return true;
    }
    match pattern.strip_prefix(b"*") {
        Some(suffix) => name.ends_with(suffix),
        None => pattern == name,
    }
}

/// GetOpenFileNameA / GetSaveFileNameA over the host directory backing C:.
pub fn file_dialog<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    ofn: u32,
    save: bool,
) -> u32 {
    let field = |memory: &mut M, offset: u32| memory.read_u32(ofn + offset).unwrap_or(0);
    let owner = field(memory, 4);
    let filter = field(memory, 12);
    let filter_index = field(memory, 24);
    let file_va = field(memory, 28);
    let max_file = field(memory, 32);
    let title_va = field(memory, 36);
    let max_title = field(memory, 40);
    let caption_va = field(memory, 48);
    let def_ext_va = field(memory, 60);
    let initial = if file_va != 0 {
        read_cstr(memory, file_va)
    } else {
        Vec::new()
    };
    let initial_name = initial
        .rsplit(|b| *b == b'\\' || *b == b'/')
        .next()
        .unwrap_or(&[])
        .to_vec();
    let patterns = filter_patterns(memory, filter, filter_index);
    let mut names: Vec<Vec<u8>> = std::fs::read_dir(crate::win32::crt::drive_root())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().as_bytes().to_vec())
                .filter(|n| patterns.iter().any(|p| pattern_match(p, n)))
                .collect()
        })
        .unwrap_or_default();
    names.sort_by_key(|n| n.to_ascii_lowercase());
    let caption = if caption_va != 0 {
        read_cstr(memory, caption_va)
    } else if save {
        b"Save As".to_vec()
    } else {
        b"Open".to_vec()
    };
    if !user::interactive(ctx, memory) {
        return 0;
    }
    let style = user::WS_POPUP | user::WS_CAPTION | user::WS_SYSMENU | user::WS_DLGFRAME;
    let (cw, ch) = (330, 250);
    let (l, t, r, b) = user::nc_insets_for(style, user::WS_EX_DLGMODALFRAME, false);
    let owner_top = if owner != 0 {
        ctx.shared
            .borrow()
            .gui
            .top_level_for(owner)
            .unwrap_or(owner)
    } else {
        ctx.shared.borrow().gui.active_hwnd
    };
    let (sw, sh) = ctx.shared.borrow().gui.screen;
    let (w, h) = (cw + l + r, ch + t + b);
    let dialog = user::create_window(
        ctx,
        memory,
        CreateParams {
            ex_style: user::WS_EX_DLGMODALFRAME,
            class: "#32770".into(),
            title: caption,
            style,
            x: ((sw - w) / 2).max(0),
            y: ((sh - h) / 2).max(0),
            width: w,
            height: h,
            parent: owner_top,
            ..CreateParams::default()
        },
    );
    if dialog == 0 {
        return 0;
    }
    if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(dialog) {
        window.props.push(("otw_filedlg".into(), 1));
        window.extra.resize(12, 0);
    }
    let child = |ctx: &ApiContext,
                 memory: &mut M,
                 class: &str,
                 title: &[u8],
                 style: u32,
                 rect: (i32, i32, i32, i32),
                 id: u32| {
        user::create_window(
            ctx,
            memory,
            CreateParams {
                ex_style: if class == "Edit" || class == "ListBox" {
                    user::WS_EX_CLIENTEDGE
                } else {
                    0
                },
                class: class.into(),
                title: title.to_vec(),
                style: WS_CHILD | WS_VISIBLE | style,
                x: rect.0,
                y: rect.1,
                width: rect.2,
                height: rect.3,
                parent: dialog,
                menu: id,
                ..CreateParams::default()
            },
        )
    };
    child(
        ctx,
        memory,
        "Static",
        b"File &name:",
        0,
        (12, 12, 200, 16),
        0xFFFF,
    );
    let edit = child(
        ctx,
        memory,
        "Edit",
        &initial_name,
        WS_TABSTOP | 0x80,
        (12, 30, 216, 22),
        FILE_EDIT_ID,
    );
    child(
        ctx,
        memory,
        "Static",
        b"Files in C:\\",
        0,
        (12, 60, 200, 16),
        0xFFFF,
    );
    let list = child(
        ctx,
        memory,
        "ListBox",
        b"",
        WS_TABSTOP | 0x1 | user::WS_VSCROLL,
        (12, 78, 216, 160),
        FILE_LIST_ID,
    );
    child(
        ctx,
        memory,
        "Button",
        if save { b"&Save" } else { b"&Open" },
        WS_TABSTOP | 1,
        (242, 30, 75, 23),
        1,
    );
    child(
        ctx,
        memory,
        "Button",
        b"Cancel",
        WS_TABSTOP,
        (242, 60, 75, 23),
        2,
    );
    if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(list) {
        window.control.items = names.into_iter().map(|n| (n, 0)).collect();
        window.control.selected = -1;
    }
    show_window(ctx, memory, dialog, 5);
    set_focus(ctx, memory, edit);
    let result = run_modal_keep(ctx, memory, dialog, owner_top);
    let text = ctx
        .shared
        .borrow()
        .gui
        .window(edit)
        .map(|w| w.text.clone())
        .unwrap_or_default();
    user::destroy_window(ctx, memory, dialog);
    if owner_top != 0 && is_window(ctx, owner_top) {
        user::activate(ctx, memory, owner_top, 1);
    }
    let text: Vec<u8> = String::from_utf8_lossy(&text).trim().as_bytes().to_vec();
    if result != 1 || text.is_empty() {
        return 0;
    }
    let mut name = text;
    if def_ext_va != 0 && !name.contains(&b'.') {
        let ext = read_cstr(memory, def_ext_va);
        if !ext.is_empty() {
            name.push(b'.');
            name.extend_from_slice(&ext);
        }
    }
    let full = if name.len() >= 2 && name[1] == b':' {
        name.clone()
    } else {
        [b"C:\\".as_slice(), &name].concat()
    };
    if full.len() + 1 > max_file as usize {
        return 0;
    }
    let _ = write_cstr(memory, file_va, &full);
    let file_offset = full
        .iter()
        .rposition(|b| *b == b'\\')
        .map(|p| p + 1)
        .unwrap_or(0);
    let ext_offset = full
        .iter()
        .rposition(|b| *b == b'.')
        .filter(|p| *p > file_offset)
        .map(|p| p + 1)
        .unwrap_or(full.len());
    let _ = memory.write_bytes(ofn + 56, &(file_offset as u16).to_le_bytes());
    let _ = memory.write_bytes(ofn + 58, &(ext_offset as u16).to_le_bytes());
    if title_va != 0 && max_title > 0 {
        let mut title = full[file_offset..].to_vec();
        title.truncate(max_title as usize - 1);
        let _ = write_cstr(memory, title_va, &title);
    }
    1
}

/// Modal loop that leaves the dialog alive so results can be read.
pub fn run_modal_keep<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dialog: u32,
    owner_top: u32,
) -> u32 {
    let enabled = owner_top != 0
        && ctx
            .shared
            .borrow()
            .gui
            .window(owner_top)
            .map(|w| !w.disabled)
            .unwrap_or(false);
    if enabled {
        if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(owner_top) {
            w.disabled = true;
        }
    }
    let mut result = 2;
    loop {
        if let Some(value) = ctx
            .shared
            .borrow()
            .gui
            .window(dialog)
            .and_then(|w| w.dlg_result)
        {
            result = value;
            break;
        }
        if !is_window(ctx, dialog) || memory.stopping() || !user::interactive(ctx, memory) {
            break;
        }
        let Some(message) = user::fetch_message(ctx, memory, 0, 0, 0, true, true) else {
            continue;
        };
        if message.message == user::WM_QUIT {
            ctx.shared.borrow_mut().gui.quit = true;
            break;
        }
        if !is_dialog_message(ctx, memory, dialog, message) {
            translate_and_dispatch(ctx, memory, message);
        }
    }
    if enabled {
        if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(owner_top) {
            w.disabled = false;
        }
    }
    result
}

// ── printf ───────────────────────────────────────────────────────────────

/// Format a C printf string. Arguments come from `args` (stack words) or,
/// when `va_list` is set, from guest memory starting at that address.
pub fn format_printf<M: GuestMemory + ?Sized>(
    memory: &mut M,
    format: &[u8],
    args: &[u32],
    va_list: Option<u32>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut arg_index = 0usize;
    let mut va = va_list.unwrap_or(0);
    let mut next = |memory: &mut M| -> u32 {
        match va_list {
            Some(_) => {
                let value = memory.read_u32(va).unwrap_or(0);
                va += 4;
                value
            }
            None => {
                let value = args.get(arg_index).copied().unwrap_or(0);
                arg_index += 1;
                value
            }
        }
    };
    let mut i = 0;
    while i < format.len() {
        let c = format[i];
        if c != b'%' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        if i >= format.len() {
            break;
        }
        if format[i] == b'%' {
            out.push(b'%');
            i += 1;
            continue;
        }
        let mut left = false;
        let mut zero = false;
        let mut plus = false;
        let mut space = false;
        let mut alt = false;
        while i < format.len() && b"-0+ #".contains(&format[i]) {
            match format[i] {
                b'-' => left = true,
                b'0' => zero = true,
                b'+' => plus = true,
                b' ' => space = true,
                _ => alt = true,
            }
            i += 1;
        }
        let mut width = 0usize;
        if i < format.len() && format[i] == b'*' {
            width = next(memory) as i32 as usize;
            i += 1;
        }
        while i < format.len() && format[i].is_ascii_digit() {
            width = width * 10 + (format[i] - b'0') as usize;
            i += 1;
        }
        let mut precision: Option<usize> = None;
        if i < format.len() && format[i] == b'.' {
            i += 1;
            let mut p = 0usize;
            if i < format.len() && format[i] == b'*' {
                p = next(memory) as usize;
                i += 1;
            }
            while i < format.len() && format[i].is_ascii_digit() {
                p = p * 10 + (format[i] - b'0') as usize;
                i += 1;
            }
            precision = Some(p);
        }
        let mut long_double = false;
        // `l` and `w` make %s and %c wide; %S and %C are always wide.
        let mut wide = false;
        while i < format.len() && b"hlLIwq".contains(&format[i]) {
            match format[i] {
                b'L' => long_double = true,
                b'l' | b'w' => wide = true,
                _ => {}
            }
            i += 1;
        }
        let _ = long_double;
        if i >= format.len() {
            break;
        }
        let spec = format[i];
        i += 1;
        let mut body: Vec<u8> = match spec {
            b'd' | b'i' => {
                let v = next(memory) as i32;
                let mut s = v.unsigned_abs().to_string().into_bytes();
                if let Some(p) = precision {
                    while s.len() < p {
                        s.insert(0, b'0');
                    }
                }
                if v < 0 {
                    s.insert(0, b'-');
                } else if plus {
                    s.insert(0, b'+');
                } else if space {
                    s.insert(0, b' ');
                }
                s
            }
            b'u' => next(memory).to_string().into_bytes(),
            b'x' | b'X' | b'o' | b'p' => {
                let v = next(memory);
                let mut s = match spec {
                    b'x' => format!("{v:x}"),
                    b'X' => format!("{v:X}"),
                    b'o' => format!("{v:o}"),
                    _ => format!("{v:08X}"),
                }
                .into_bytes();
                if let Some(p) = precision {
                    while s.len() < p {
                        s.insert(0, b'0');
                    }
                }
                if alt && v != 0 && spec != b'o' {
                    s.splice(
                        0..0,
                        if spec == b'X' {
                            b"0X".iter().copied()
                        } else {
                            b"0x".iter().copied()
                        },
                    );
                }
                s
            }
            b'c' | b'C' => {
                let value = next(memory);
                vec![if (wide || spec == b'C') && value > 0xFF { b'?' } else { value as u8 }]
            }
            b's' | b'S' => {
                let ptr = next(memory);
                let mut s = if ptr == 0 {
                    b"(null)".to_vec()
                } else if wide || spec == b'S' {
                    crate::win32::unicode::wide_to_ansi(&crate::win32::unicode::read_wstr(memory, ptr))
                } else {
                    read_cstr(memory, ptr)
                };
                if let Some(p) = precision {
                    s.truncate(p);
                }
                s
            }
            b'e' | b'E' | b'f' | b'g' | b'G' => {
                let lo_word = next(memory) as u64;
                let hi_word = next(memory) as u64;
                let value = f64::from_bits(lo_word | (hi_word << 32));
                let p = precision.unwrap_or(6);
                let s = match spec {
                    b'f' => format!("{value:.p$}"),
                    b'e' => format!("{value:.p$e}"),
                    b'E' => format!("{value:.p$E}"),
                    _ => {
                        let s = format!("{value}");
                        s
                    }
                };
                let mut s = s.into_bytes();
                if plus && value >= 0.0 {
                    s.insert(0, b'+');
                }
                s
            }
            b'n' => {
                let ptr = next(memory);
                let _ = memory.write_u32(ptr, out.len() as u32);
                Vec::new()
            }
            other => vec![b'%', other],
        };
        if body.len() < width {
            let pad = width - body.len();
            if left {
                body.extend(std::iter::repeat_n(b' ', pad));
            } else if zero && !matches!(spec, b's' | b'c') {
                let sign = matches!(body.first(), Some(b'-') | Some(b'+') | Some(b' ')) as usize;
                body.splice(sign..sign, std::iter::repeat_n(b'0', pad));
            } else {
                body.splice(0..0, std::iter::repeat_n(b' ', pad));
            }
        }
        out.extend_from_slice(&body);
    }
    out
}

// ── API surface ──────────────────────────────────────────────────────────

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "DialogBoxParamA"
        | "CreateDialogParamA"
        | "DialogBoxIndirectParamA"
        | "CreateDialogIndirectParamA" => 5,
        "EndDialog" | "GetDlgItem" | "IsDlgButtonChecked" | "IsDialogMessageA"
        | "MapDialogRect" => 2,
        "SetDlgItemTextA" | "CheckDlgButton" | "GetNextDlgTabItem" | "GetNextDlgGroupItem" => 3,
        "GetDlgItemTextA" | "SetDlgItemInt" | "GetDlgItemInt" | "CheckRadioButton" => 4,
        "SendDlgItemMessageA" => 5,
        "GetDialogBaseUnits" => 0,
        _ => return None,
    })
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
    count: usize,
    _caller: u32,
) -> Option<ApiOutcome> {
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    match name {
        "DialogBoxParamA" | "CreateDialogParamA" => {
            let template_name = user::res_name(memory, args[1]);
            let Some((template, _)) = user::find_resource_any(
                ctx,
                memory,
                args[0],
                &user::ResName::Id(5),
                &template_name,
            ) else {
                eprintln!("[user] dialog template {template_name:?} not found");
                return ok(if name == "DialogBoxParamA" {
                    u32::MAX
                } else {
                    0
                });
            };
            let dialog = create_dialog(ctx, memory, args[0], template, args[2], args[3], args[4]);
            if name == "CreateDialogParamA" {
                return ok(dialog);
            }
            if dialog == 0 {
                return ok(u32::MAX);
            }
            ok(run_modal(ctx, memory, dialog, args[2]))
        }
        "DialogBoxIndirectParamA" | "CreateDialogIndirectParamA" => {
            let dialog = create_dialog(ctx, memory, args[0], args[1], args[2], args[3], args[4]);
            if name == "CreateDialogIndirectParamA" {
                return ok(dialog);
            }
            if dialog == 0 {
                return ok(u32::MAX);
            }
            ok(run_modal(ctx, memory, dialog, args[2]))
        }
        "EndDialog" => {
            end_dialog(ctx, args[0], args[1]);
            // Modeless dialogs simply hide.
            ok(1)
        }
        "GetDlgItem" => ok(dialog_item(ctx, args[0], args[1])),
        "SetDlgItemTextA" => {
            let item = dialog_item(ctx, args[0], args[1]);
            ok(send(ctx, memory, item, WM_SETTEXT, 0, args[2]).max((item != 0) as u32))
        }
        "GetDlgItemTextA" => {
            let item = dialog_item(ctx, args[0], args[1]);
            if item == 0 {
                let _ = write_cstr(memory, args[2], b"");
                return ok(0);
            }
            ok(send(ctx, memory, item, WM_GETTEXT, args[3], args[2]))
        }
        "SetDlgItemInt" => {
            let item = dialog_item(ctx, args[0], args[1]);
            let text = if args[3] != 0 {
                (args[2] as i32).to_string()
            } else {
                args[2].to_string()
            };
            let va = user::scratch(ctx, 16);
            let _ = write_cstr(memory, va, text.as_bytes());
            send(ctx, memory, item, WM_SETTEXT, 0, va);
            user::free_scratch(ctx, va);
            ok(1)
        }
        "GetDlgItemInt" => {
            let item = dialog_item(ctx, args[0], args[1]);
            let text = ctx
                .shared
                .borrow()
                .gui
                .window(item)
                .map(|w| {
                    if w.class.eq_ignore_ascii_case("Edit")
                        || w.class.eq_ignore_ascii_case("ComboBox")
                    {
                        w.text.clone()
                    } else {
                        w.title.as_bytes().to_vec()
                    }
                })
                .unwrap_or_default();
            let trimmed = String::from_utf8_lossy(&text).trim().to_string();
            let parsed = if args[3] != 0 {
                trimmed.parse::<i32>().ok().map(|v| v as u32)
            } else {
                trimmed.parse::<u32>().ok()
            };
            if args[2] != 0 {
                let _ = memory.write_u32(args[2], parsed.is_some() as u32);
            }
            ok(parsed.unwrap_or(0))
        }
        "CheckDlgButton" => {
            let item = dialog_item(ctx, args[0], args[1]);
            send(ctx, memory, item, 0x00F1, args[2], 0);
            ok(1)
        }
        "IsDlgButtonChecked" => {
            let item = dialog_item(ctx, args[0], args[1]);
            ok(send(ctx, memory, item, 0x00F0, 0, 0))
        }
        "CheckRadioButton" => {
            for id in args[1]..=args[2].min(args[1] + 256) {
                let item = dialog_item(ctx, args[0], id);
                if item != 0 {
                    send(ctx, memory, item, 0x00F1, (id == args[3]) as u32, 0);
                }
            }
            ok(1)
        }
        "SendDlgItemMessageA" => {
            let item = dialog_item(ctx, args[0], args[1]);
            ok(send(ctx, memory, item, args[2], args[3], args[4]))
        }
        "IsDialogMessageA" => {
            let message = user::read_message(memory, args[1]);
            ok(is_dialog_message(ctx, memory, args[0], message) as u32)
        }
        "MapDialogRect" => {
            let (bx, by) = base_units();
            let rect = gdi::read_rect(memory, args[1]).unwrap_or_default();
            let map = |v: i32, base: i32, div: i32| (v * base + div / 2) / div;
            let _ = gdi::write_rect(
                memory,
                args[1],
                Rect::new(
                    map(rect.left, bx, 4),
                    map(rect.top, by, 8),
                    map(rect.right, bx, 4),
                    map(rect.bottom, by, 8),
                ),
            );
            ok(1)
        }
        "GetDialogBaseUnits" => {
            let (bx, by) = base_units();
            ok(make_long(bx, by))
        }
        "GetNextDlgTabItem" | "GetNextDlgGroupItem" => {
            let list = if name == "GetNextDlgTabItem" {
                tab_controls(ctx, args[0])
            } else {
                radio_group(ctx, args[0], args[1])
            };
            if list.is_empty() {
                return ok(args[1]);
            }
            let position = list.iter().position(|h| *h == args[1]);
            let next = match position {
                Some(p) if args[2] != 0 => list[(p + list.len() - 1) % list.len()],
                Some(p) => list[(p + 1) % list.len()],
                None => list[0],
            };
            ok(next)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Flat(Vec<u8>);
    impl GuestMemory for Flat {
        fn read_bytes(&mut self, va: u32, len: usize) -> Option<Vec<u8>> {
            self.0
                .get(va as usize..va as usize + len)
                .map(|s| s.to_vec())
        }
        fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Option<()> {
            self.0
                .get_mut(va as usize..va as usize + bytes.len())?
                .copy_from_slice(bytes);
            Some(())
        }
        fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
            self.write_bytes(va, &value.to_le_bytes())
        }
    }

    #[test]
    fn printf_formats_common_specifiers() {
        let mut memory = Flat(vec![0; 64]);
        memory.write_bytes(16, b"Paint\0");
        let out = format_printf(
            &mut memory,
            b"%s %d%% %04x %-3c|",
            &[16, (-5i32) as u32, 0xAB, b'z' as u32],
            None,
        );
        assert_eq!(out, b"Paint -5% 00ab z  |");
    }
}

/// Send WM_DRAWITEM to `parent` with a DRAWITEMSTRUCT in the process's
/// layout: CtlType, CtlID, itemID, itemAction, itemState, hwndItem, hDC,
/// rcItem (0, 0, width, height), itemData 0.
fn send_draw_item<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, parent: u32, fields: [u32; 7], (width, height): (i32, i32)) {
    let dis = user::scratch(ctx, 80);
    let _ = memory.write_bytes(dis, &[0u8; 80]);
    let view = crate::win32::x64::View::new(memory, &crate::win32::x64::DRAWITEMSTRUCT, dis);
    for (index, value) in fields.into_iter().enumerate() {
        view.set(memory, index, value);
    }
    let _ = gdi::write_rect(memory, view.at(7), Rect::new(0, 0, width, height));
    send(ctx, memory, parent, 0x002B, fields[1], dis);
    user::free_scratch(ctx, dis);
}
