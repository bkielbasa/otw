//! COMDLG32 color dialog (`ChooseColorA`), modelled on the Windows 95 one:
//! 48 basic colors, 16 custom colors and, after "Define Custom Colors >>",
//! a hue/saturation spectrum, a luminance bar and HSL/RGB edit fields.

use crate::{
    win32::controls,
    win32::gdi::sys_color,
    win32::gfx::{colorref_to_rgb, rgb_to_colorref, Rect, Rgb},
    win32::user::{self, hi, lo, make_long, CreateParams, Painter, WS_CHILD, WS_VISIBLE},
    win32::{write_cstr, ApiContext, GuestMemory},
};

const CC_RGBINIT: u32 = 0x1;
const CC_FULLOPEN: u32 = 0x2;
const CC_PREVENTFULLOPEN: u32 = 0x4;

const ID_OK: u32 = 1;
const ID_CANCEL: u32 = 2;
const ID_HELP: u32 = 1038;
const ID_ADD: u32 = 712;
const ID_DEFINE: u32 = 719;
const ID_HUE: u32 = 703;
const ID_SAT: u32 = 704;
const ID_LUM: u32 = 705;
const ID_RED: u32 = 706;
const ID_GREEN: u32 = 707;
const ID_BLUE: u32 = 708;

/// Panel control ids (class `OTW_ColorPanel`).
const PANEL_BASIC: u32 = 720;
const PANEL_CUSTOM: u32 = 721;
const PANEL_SPECTRUM: u32 = 722;
const PANEL_LUM: u32 = 723;
const PANEL_PREVIEW: u32 = 724;

const CELL_W: i32 = 24;
const CELL_H: i32 = 20;
const COMPACT_WIDTH: i32 = 208;
const FULL_WIDTH: i32 = 444;
const CLIENT_HEIGHT: i32 = 278;
const SPECTRUM: Rect = Rect::new(216, 8, 396, 160);
const LUM_BAR: Rect = Rect::new(404, 8, 426, 160);

/// The Windows 95 basic color set, row by row (8 columns).
pub const BASIC_COLORS: [Rgb; 48] = [
    0xFF8080, 0xFFFF80, 0x80FF80, 0x00FF80, 0x80FFFF, 0x0080FF, 0xFF80C0, 0xFF80FF, 0xFF0000,
    0xFFFF00, 0x80FF00, 0x00FF40, 0x00FFFF, 0x0080C0, 0x8080C0, 0xFF00FF, 0x804040, 0xFF8040,
    0x00FF00, 0x008080, 0x004080, 0x8080FF, 0x800040, 0xFF0080, 0x800000, 0xFF8000, 0x008000,
    0x008040, 0x0000FF, 0x0000A0, 0x800080, 0x8000FF, 0x400000, 0x804000, 0x004000, 0x004040,
    0x000080, 0x000040, 0x400040, 0x400080, 0x000000, 0x808000, 0x808040, 0x808080, 0x408080,
    0xC0C0C0, 0x400040, 0xFFFFFF,
];

/// State of the open color dialog, shared with its panel controls.
#[derive(Clone, Debug)]
pub struct ColorDialogState {
    pub dialog: u32,
    pub color: Rgb,
    pub hue: i32,
    pub sat: i32,
    pub lum: i32,
    pub basic_sel: i32,
    pub custom: [Rgb; 16],
    pub custom_sel: i32,
    pub full: bool,
    pub updating: bool,
    pub full_controls: Vec<u32>,
}

// ── HLS (Windows uses a 0..240 scale) ────────────────────────────────────

const HLSMAX: i32 = 240;
const RGBMAX: i32 = 255;

pub fn rgb_to_hls(rgb: Rgb) -> (i32, i32, i32) {
    let (r, g, b) = (
        ((rgb >> 16) & 0xFF) as i32,
        ((rgb >> 8) & 0xFF) as i32,
        (rgb & 0xFF) as i32,
    );
    let cmax = r.max(g).max(b);
    let cmin = r.min(g).min(b);
    let lum = ((cmax + cmin) * HLSMAX + RGBMAX) / (2 * RGBMAX);
    if cmax == cmin {
        return (HLSMAX * 2 / 3, lum, 0);
    }
    let delta = cmax - cmin;
    let sat = if lum <= HLSMAX / 2 {
        (delta * HLSMAX + (cmax + cmin) / 2) / (cmax + cmin)
    } else {
        (delta * HLSMAX + (2 * RGBMAX - cmax - cmin) / 2) / (2 * RGBMAX - cmax - cmin)
    };
    let part = |c: i32| ((cmax - c) * (HLSMAX / 6) + delta / 2) / delta;
    let (rd, gd, bd) = (part(r), part(g), part(b));
    let mut hue = if r == cmax {
        bd - gd
    } else if g == cmax {
        HLSMAX / 3 + rd - bd
    } else {
        2 * HLSMAX / 3 + gd - rd
    };
    if hue < 0 {
        hue += HLSMAX;
    }
    if hue >= HLSMAX {
        hue -= HLSMAX;
    }
    (hue, lum, sat.clamp(0, HLSMAX))
}

fn hue_to_rgb(n1: i32, n2: i32, hue: i32) -> i32 {
    let hue = hue.rem_euclid(HLSMAX);
    if hue < HLSMAX / 6 {
        n1 + ((n2 - n1) * hue + HLSMAX / 12) / (HLSMAX / 6)
    } else if hue < HLSMAX / 2 {
        n2
    } else if hue < HLSMAX * 2 / 3 {
        n1 + ((n2 - n1) * (HLSMAX * 2 / 3 - hue) + HLSMAX / 12) / (HLSMAX / 6)
    } else {
        n1
    }
}

pub fn hls_to_rgb(hue: i32, lum: i32, sat: i32) -> Rgb {
    let (lum, sat) = (lum.clamp(0, HLSMAX), sat.clamp(0, HLSMAX));
    let (r, g, b) = if sat == 0 {
        let v = lum * RGBMAX / HLSMAX;
        (v, v, v)
    } else {
        let m2 = if lum <= HLSMAX / 2 {
            (lum * (HLSMAX + sat) + HLSMAX / 2) / HLSMAX
        } else {
            lum + sat - (lum * sat + HLSMAX / 2) / HLSMAX
        };
        let m1 = 2 * lum - m2;
        let channel =
            |h: i32| ((hue_to_rgb(m1, m2, h) * RGBMAX + HLSMAX / 2) / HLSMAX).clamp(0, 255);
        (
            channel(hue + HLSMAX / 3),
            channel(hue),
            channel(hue - HLSMAX / 3),
        )
    };
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

// ── State helpers ────────────────────────────────────────────────────────

fn with_state<R>(ctx: &ApiContext, f: impl FnOnce(&mut ColorDialogState) -> R) -> Option<R> {
    ctx.shared.borrow_mut().color_dialog.as_mut().map(f)
}

fn panel(ctx: &ApiContext, id: u32) -> u32 {
    let dialog = with_state(ctx, |s| s.dialog).unwrap_or(0);
    let shared = ctx.shared.borrow();
    shared
        .gui
        .windows
        .iter()
        .find(|w| w.parent == dialog && w.id == id && w.style & WS_CHILD != 0)
        .map(|w| w.hwnd)
        .unwrap_or(0)
}

fn redraw_panels(ctx: &ApiContext, ids: &[u32]) {
    for id in ids {
        let hwnd = panel(ctx, *id);
        if hwnd != 0 {
            user::invalidate(&mut ctx.shared.borrow_mut().gui, hwnd, None, false, false);
        }
    }
}

/// Write the HLS/RGB edit fields from the current state.
fn sync_edits<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) {
    let Some(state) = with_state(ctx, |s| s.clone()) else {
        return;
    };
    with_state(ctx, |s| s.updating = true);
    let values = [
        (ID_HUE, state.hue),
        (ID_SAT, state.sat),
        (ID_LUM, state.lum),
        (ID_RED, ((state.color >> 16) & 0xFF) as i32),
        (ID_GREEN, ((state.color >> 8) & 0xFF) as i32),
        (ID_BLUE, (state.color & 0xFF) as i32),
    ];
    let scratch = user::scratch(ctx, 16);
    for (id, value) in values {
        let edit = panel(ctx, id);
        if edit != 0 {
            let _ = write_cstr(memory, scratch, value.to_string().as_bytes());
            user::send(ctx, memory, edit, user::WM_SETTEXT, 0, scratch);
        }
    }
    user::free_scratch(ctx, scratch);
    with_state(ctx, |s| s.updating = false);
}

/// Set the current color. `hls` keeps an explicit hue/sat/lum (so dragging
/// through grey keeps its hue); otherwise it is derived from the color.
fn set_color<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    color: Rgb,
    hls: Option<(i32, i32, i32)>,
) {
    with_state(ctx, |s| {
        s.color = color;
        let (h, l, sat) = hls.unwrap_or_else(|| rgb_to_hls(color));
        s.hue = h;
        s.lum = l;
        s.sat = sat;
    });
    sync_edits(ctx, memory);
    redraw_panels(
        ctx,
        &[
            PANEL_SPECTRUM,
            PANEL_LUM,
            PANEL_PREVIEW,
            PANEL_BASIC,
            PANEL_CUSTOM,
        ],
    );
}

fn edit_value(ctx: &ApiContext, id: u32, max: i32) -> Option<i32> {
    let edit = panel(ctx, id);
    let text = ctx
        .shared
        .borrow()
        .gui
        .window(edit)
        .map(|w| w.text.clone())?;
    let value: i32 = String::from_utf8_lossy(&text).trim().parse().ok()?;
    Some(value.clamp(0, max))
}

// ── Panel control ────────────────────────────────────────────────────────

fn draw_swatch(p: &mut Painter<'_>, cell: Rect, color: Rgb, selected: bool) {
    let swatch = cell.inflate(-3, -3);
    p.sunken(swatch.inflate(1, 1));
    p.fill(swatch.inflate(-1, -1), color);
    if selected {
        for inset in [0, 1] {
            let r = cell.inflate(-inset, -inset);
            p.hline(r.left, r.right, r.top, 0);
            p.hline(r.left, r.right, r.bottom - 1, 0);
            p.vline(r.left, r.top, r.bottom, 0);
            p.vline(r.right - 1, r.top, r.bottom, 0);
        }
    }
}

fn paint_panel(ctx: &ApiContext, hwnd: u32, id: u32) {
    let Some(state) = with_state(ctx, |s| s.clone()) else {
        return;
    };
    let (cw, ch) = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(user::client_size)
        .unwrap_or((0, 0));
    let mut shared = ctx.shared.borrow_mut();
    user::with_painter(&mut shared, hwnd, true, |p| {
        p.fill(Rect::new(0, 0, cw, ch), sys_color(15));
        match id {
            PANEL_BASIC => {
                for (index, color) in BASIC_COLORS.iter().enumerate() {
                    let (col, row) = ((index % 8) as i32, (index / 8) as i32);
                    let cell = Rect::sized(col * CELL_W, row * CELL_H, CELL_W, CELL_H);
                    draw_swatch(p, cell, *color, state.basic_sel == index as i32);
                }
            }
            PANEL_CUSTOM => {
                for (index, color) in state.custom.iter().enumerate() {
                    let (col, row) = ((index % 8) as i32, (index / 8) as i32);
                    let cell = Rect::sized(col * CELL_W, row * CELL_H, CELL_W, CELL_H);
                    draw_swatch(p, cell, *color, state.custom_sel == index as i32);
                }
            }
            PANEL_SPECTRUM => {
                let inner = Rect::new(2, 2, cw - 2, ch - 2);
                p.sunken(Rect::new(0, 0, cw, ch));
                let (w, h) = (inner.width().max(2), inner.height().max(2));
                for y in 0..h {
                    let sat = HLSMAX - y * HLSMAX / (h - 1);
                    for x in 0..w {
                        let hue = x * (HLSMAX - 1) / (w - 1);
                        p.pixel(
                            inner.left + x,
                            inner.top + y,
                            hls_to_rgb(hue, HLSMAX / 2, sat),
                        );
                    }
                }
                // Crosshair at the current hue/saturation.
                let cx = inner.left + state.hue * (w - 1) / (HLSMAX - 1);
                let cy = inner.top + (HLSMAX - state.sat) * (h - 1) / HLSMAX;
                for d in 3..9 {
                    p.fill(Rect::sized(cx - d, cy - 1, 1, 3), 0);
                    p.fill(Rect::sized(cx + d, cy - 1, 1, 3), 0);
                    p.fill(Rect::sized(cx - 1, cy - d, 3, 1), 0);
                    p.fill(Rect::sized(cx - 1, cy + d, 3, 1), 0);
                }
            }
            PANEL_LUM => {
                let bar = Rect::new(0, 0, 12, ch);
                p.sunken(bar);
                let inner = bar.inflate(-2, -2);
                let h = inner.height().max(2);
                for y in 0..h {
                    let lum = HLSMAX - y * HLSMAX / (h - 1);
                    p.fill(
                        Rect::sized(inner.left, inner.top + y, inner.width(), 1),
                        hls_to_rgb(state.hue, lum, state.sat),
                    );
                }
                let ay = inner.top + (HLSMAX - state.lum) * (h - 1) / HLSMAX;
                for i in 0..5 {
                    p.vline(14 + i, ay - (4 - i), ay + (4 - i) + 1, 0);
                }
            }
            PANEL_PREVIEW => {
                let frame = Rect::new(0, 0, cw, ch);
                p.sunken(frame);
                let inner = frame.inflate(-2, -2);
                let half = inner.left + inner.width() / 2;
                // A true-colour display renders every color solid.
                p.fill(
                    Rect::new(inner.left, inner.top, half, inner.bottom),
                    state.color,
                );
                p.fill(
                    Rect::new(half, inner.top, inner.right, inner.bottom),
                    state.color,
                );
            }
            _ => {}
        }
    });
}

fn panel_click<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    id: u32,
    x: i32,
    y: i32,
    drag: bool,
) {
    let (cw, ch) = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(user::client_size)
        .unwrap_or((0, 0));
    match id {
        PANEL_BASIC | PANEL_CUSTOM if !drag => {
            let rows = if id == PANEL_BASIC { 6 } else { 2 };
            let (col, row) = (x / CELL_W, y / CELL_H);
            if !(0..8).contains(&col) || !(0..rows).contains(&row) {
                return;
            }
            let index = row * 8 + col;
            let color = if id == PANEL_BASIC {
                with_state(ctx, |s| {
                    s.basic_sel = index;
                    BASIC_COLORS[index as usize]
                })
            } else {
                with_state(ctx, |s| {
                    s.custom_sel = index;
                    s.basic_sel = -1;
                    s.custom[index as usize]
                })
            };
            if let Some(color) = color {
                set_color(ctx, memory, color, None);
            }
        }
        PANEL_SPECTRUM => {
            let (w, h) = ((cw - 4).max(2), (ch - 4).max(2));
            let hue = ((x - 2).clamp(0, w - 1) * (HLSMAX - 1)) / (w - 1);
            let sat = HLSMAX - ((y - 2).clamp(0, h - 1) * HLSMAX) / (h - 1);
            let lum = with_state(ctx, |s| s.lum).unwrap_or(HLSMAX / 2);
            // Picking from the spectrum of a black or white color makes the
            // choice visible, as Windows does by moving to mid luminance.
            let lum = if lum == 0 || lum == HLSMAX {
                HLSMAX / 2
            } else {
                lum
            };
            with_state(ctx, |s| s.basic_sel = -1);
            set_color(
                ctx,
                memory,
                hls_to_rgb(hue, lum, sat),
                Some((hue, lum, sat)),
            );
        }
        PANEL_LUM => {
            let h = (ch - 4).max(2);
            let lum = HLSMAX - ((y - 2).clamp(0, h - 1) * HLSMAX) / (h - 1);
            let (hue, sat) = with_state(ctx, |s| (s.hue, s.sat)).unwrap_or((0, 0));
            with_state(ctx, |s| s.basic_sel = -1);
            set_color(
                ctx,
                memory,
                hls_to_rgb(hue, lum, sat),
                Some((hue, lum, sat)),
            );
        }
        _ => {}
    }
}

/// Window procedure of the color dialog's custom-drawn panels.
pub fn color_panel_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    let id = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.id)
        .unwrap_or(0);
    match msg {
        user::WM_PAINT => {
            let ps = user::scratch(ctx, 80);
            let hdc = user::begin_paint(ctx, memory, hwnd, ps);
            paint_panel(ctx, hwnd, id);
            user::end_paint(ctx, hwnd, hdc);
            user::free_scratch(ctx, ps);
            0
        }
        user::WM_ERASEBKGND => 1,
        user::WM_LBUTTONDOWN | user::WM_LBUTTONDBLCLK => {
            ctx.shared.borrow_mut().gui.capture = hwnd;
            panel_click(ctx, memory, hwnd, id, lo(lparam), hi(lparam), false);
            if msg == user::WM_LBUTTONDBLCLK && (id == PANEL_BASIC || id == PANEL_CUSTOM) {
                // Double-clicking a swatch accepts it, like pressing OK.
                let dialog = with_state(ctx, |s| s.dialog).unwrap_or(0);
                let ok = panel(ctx, ID_OK);
                user::post(
                    ctx,
                    dialog,
                    user::WM_COMMAND,
                    make_long(ID_OK as i32, 0),
                    ok,
                );
            }
            0
        }
        user::WM_MOUSEMOVE => {
            if wparam & 1 != 0 && ctx.shared.borrow().gui.capture == hwnd {
                panel_click(ctx, memory, hwnd, id, lo(lparam), hi(lparam), true);
            }
            0
        }
        user::WM_LBUTTONUP => {
            if ctx.shared.borrow().gui.capture == hwnd {
                ctx.shared.borrow_mut().gui.capture = 0;
            }
            0
        }
        _ => user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

// ── Dialog ───────────────────────────────────────────────────────────────

fn expand<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) {
    let Some((dialog, controls, already)) =
        with_state(ctx, |s| (s.dialog, s.full_controls.clone(), s.full))
    else {
        return;
    };
    if already {
        return;
    }
    with_state(ctx, |s| s.full = true);
    let (x, y, height, extra) = {
        let shared = ctx.shared.borrow();
        let w = shared.gui.window(dialog);
        w.map(|w| (w.x, w.y, w.height, FULL_WIDTH - COMPACT_WIDTH))
            .unwrap_or((0, 0, 0, 0))
    };
    let width = ctx
        .shared
        .borrow()
        .gui
        .window(dialog)
        .map(|w| w.width)
        .unwrap_or(0)
        + extra;
    let (sw, _) = ctx.shared.borrow().gui.screen;
    let x = x.min((sw - width).max(0));
    user::set_window_pos(ctx, memory, dialog, 0, x, y, width, height, 0x4 | 0x10);
    for control in controls {
        user::show_window(ctx, memory, control, 5);
    }
    let define = panel(ctx, ID_DEFINE);
    if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(define) {
        w.disabled = true;
    }
    user::invalidate(&mut ctx.shared.borrow_mut().gui, define, None, true, false);
}

/// Commands from the dialog's buttons and edit fields.
pub fn color_command<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dialog: u32,
    id: u32,
    code: u32,
) {
    match id {
        ID_OK | ID_CANCEL if code == 0 => controls::end_dialog_with(ctx, dialog, id),
        ID_DEFINE if code == 0 => expand(ctx, memory),
        ID_ADD if code == 0 => {
            with_state(ctx, |s| {
                let slot = s.custom_sel.clamp(0, 15) as usize;
                s.custom[slot] = s.color;
                s.custom_sel = ((slot + 1) % 16) as i32;
            });
            redraw_panels(ctx, &[PANEL_CUSTOM]);
        }
        ID_HUE..=ID_BLUE if code == 0x300 => {
            if with_state(ctx, |s| s.updating).unwrap_or(true) {
                return;
            }
            let state = with_state(ctx, |s| s.clone()).unwrap();
            let (color, hls) = if id <= ID_LUM {
                let hue = edit_value(ctx, ID_HUE, HLSMAX - 1).unwrap_or(state.hue);
                let sat = edit_value(ctx, ID_SAT, HLSMAX).unwrap_or(state.sat);
                let lum = edit_value(ctx, ID_LUM, HLSMAX).unwrap_or(state.lum);
                (hls_to_rgb(hue, lum, sat), Some((hue, lum, sat)))
            } else {
                let r = edit_value(ctx, ID_RED, 255).unwrap_or(((state.color >> 16) & 0xFF) as i32);
                let g =
                    edit_value(ctx, ID_GREEN, 255).unwrap_or(((state.color >> 8) & 0xFF) as i32);
                let b = edit_value(ctx, ID_BLUE, 255).unwrap_or((state.color & 0xFF) as i32);
                ((r as u32) << 16 | (g as u32) << 8 | b as u32, None)
            };
            with_state(ctx, |s| {
                s.color = color;
                let (h, l, sat) = hls.unwrap_or_else(|| rgb_to_hls(color));
                s.hue = h;
                s.lum = l;
                s.sat = sat;
                s.basic_sel = -1;
            });
            // Only the other group of fields is rewritten while typing.
            let field_group = if id <= ID_LUM {
                [ID_RED, ID_GREEN, ID_BLUE]
            } else {
                [ID_HUE, ID_SAT, ID_LUM]
            };
            let state = with_state(ctx, |s| s.clone()).unwrap();
            with_state(ctx, |s| s.updating = true);
            let scratch = user::scratch(ctx, 16);
            for field in field_group {
                let value = match field {
                    ID_HUE => state.hue,
                    ID_SAT => state.sat,
                    ID_LUM => state.lum,
                    ID_RED => ((state.color >> 16) & 0xFF) as i32,
                    ID_GREEN => ((state.color >> 8) & 0xFF) as i32,
                    _ => (state.color & 0xFF) as i32,
                };
                let edit = panel(ctx, field);
                let _ = write_cstr(memory, scratch, value.to_string().as_bytes());
                user::send(ctx, memory, edit, user::WM_SETTEXT, 0, scratch);
            }
            user::free_scratch(ctx, scratch);
            with_state(ctx, |s| s.updating = false);
            redraw_panels(
                ctx,
                &[PANEL_SPECTRUM, PANEL_LUM, PANEL_PREVIEW, PANEL_BASIC],
            );
        }
        _ => {}
    }
}

pub fn is_color_dialog(ctx: &ApiContext, hwnd: u32) -> bool {
    ctx.shared
        .borrow()
        .color_dialog
        .as_ref()
        .map(|s| s.dialog == hwnd)
        .unwrap_or(false)
}

/// `ChooseColorA(CHOOSECOLORA*)`.
pub fn choose_color<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, cc: u32) -> u32 {
    let field = |memory: &mut M, offset: u32| memory.read_u32(cc + offset).unwrap_or(0);
    let owner = field(memory, 4);
    let initial = field(memory, 12);
    let custom_va = field(memory, 16);
    let flags = field(memory, 20);
    if !user::interactive(ctx, memory) {
        return 0;
    }
    let mut custom = [0xFFFFFFu32; 16];
    if custom_va != 0 {
        for (index, slot) in custom.iter_mut().enumerate() {
            *slot = colorref_to_rgb(
                memory
                    .read_u32(custom_va + index as u32 * 4)
                    .unwrap_or(0xFFFFFF)
                    & 0xFF_FFFF,
            );
        }
    }
    let color = if flags & CC_RGBINIT != 0 {
        colorref_to_rgb(initial & 0xFF_FFFF)
    } else {
        0
    };
    let (hue, lum, sat) = rgb_to_hls(color);
    let basic_sel = BASIC_COLORS
        .iter()
        .position(|c| *c == color)
        .map(|p| p as i32)
        .unwrap_or(-1);

    let style = user::WS_POPUP | user::WS_CAPTION | user::WS_SYSMENU | user::WS_DLGFRAME;
    let (l, t, r, b) = user::nc_insets_for(style, user::WS_EX_DLGMODALFRAME, false);
    let full = flags & CC_FULLOPEN != 0 && flags & CC_PREVENTFULLOPEN == 0;
    let client_w = if full { FULL_WIDTH } else { COMPACT_WIDTH };
    let (w, h) = (client_w + l + r, CLIENT_HEIGHT + t + b);
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
        Rect::new(0, 0, 640, 480)
    };
    let (sw, sh) = ctx.shared.borrow().gui.screen;
    let dialog = user::create_window(
        ctx,
        memory,
        CreateParams {
            ex_style: user::WS_EX_DLGMODALFRAME,
            class: "#32770".into(),
            title: b"Color".to_vec(),
            style,
            x: (area.left + 30).clamp(0, (sw - FULL_WIDTH - l - r).max(0)),
            y: (area.top + 40).clamp(0, (sh - h).max(0)),
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
        window.extra.resize(12, 0);
    }
    ctx.shared.borrow_mut().color_dialog = Some(ColorDialogState {
        dialog,
        color,
        hue,
        sat,
        lum,
        basic_sel,
        custom,
        custom_sel: 0,
        full,
        updating: false,
        full_controls: Vec::new(),
    });

    let font = ctx
        .shared
        .borrow()
        .gui
        .window(dialog)
        .map(|w| w.font)
        .unwrap_or(0);
    let child = |ctx: &ApiContext,
                 memory: &mut M,
                 class: &str,
                 title: &[u8],
                 style: u32,
                 rect: (i32, i32, i32, i32),
                 id: u32,
                 right_part: bool| {
        let visible = !right_part || full;
        let hwnd = user::create_window(
            ctx,
            memory,
            CreateParams {
                ex_style: if class == "Edit" {
                    user::WS_EX_CLIENTEDGE
                } else {
                    0
                },
                class: class.into(),
                title: title.to_vec(),
                style: WS_CHILD | if visible { WS_VISIBLE } else { 0 } | style,
                x: rect.0,
                y: rect.1,
                width: rect.2,
                height: rect.3,
                parent: dialog,
                menu: id,
                ..CreateParams::default()
            },
        );
        if font != 0 {
            user::send(ctx, memory, hwnd, user::WM_SETFONT, font, 0);
        }
        if right_part {
            with_state(ctx, |s| s.full_controls.push(hwnd));
        }
        hwnd
    };
    const TAB: u32 = 0x0001_0000;
    const GROUP: u32 = 0x0002_0000;
    child(
        ctx,
        memory,
        "Static",
        b"&Basic colors:",
        0,
        (8, 8, 192, 14),
        0xFFFF,
        false,
    );
    child(
        ctx,
        memory,
        "OTW_ColorPanel",
        b"",
        TAB | GROUP,
        (8, 24, 8 * CELL_W, 6 * CELL_H),
        PANEL_BASIC,
        false,
    );
    child(
        ctx,
        memory,
        "Static",
        b"&Custom colors:",
        0,
        (8, 152, 192, 14),
        0xFFFF,
        false,
    );
    child(
        ctx,
        memory,
        "OTW_ColorPanel",
        b"",
        TAB | GROUP,
        (8, 168, 8 * CELL_W, 2 * CELL_H),
        PANEL_CUSTOM,
        false,
    );
    let define = child(
        ctx,
        memory,
        "Button",
        b"&Define Custom Colors >>",
        TAB | GROUP,
        (8, 216, 192, 23),
        ID_DEFINE,
        false,
    );
    let ok = child(
        ctx,
        memory,
        "Button",
        b"OK",
        TAB | GROUP | 1,
        (8, 247, 60, 23),
        ID_OK,
        false,
    );
    child(
        ctx,
        memory,
        "Button",
        b"Cancel",
        TAB,
        (74, 247, 60, 23),
        ID_CANCEL,
        false,
    );
    let help = child(
        ctx,
        memory,
        "Button",
        b"&Help",
        TAB,
        (140, 247, 60, 23),
        ID_HELP,
        false,
    );
    if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(help) {
        w.disabled = true;
    }
    if flags & CC_PREVENTFULLOPEN != 0 || full {
        if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(define) {
            w.disabled = true;
        }
    }
    child(
        ctx,
        memory,
        "OTW_ColorPanel",
        b"",
        0,
        (
            SPECTRUM.left,
            SPECTRUM.top,
            SPECTRUM.width(),
            SPECTRUM.height(),
        ),
        PANEL_SPECTRUM,
        true,
    );
    child(
        ctx,
        memory,
        "OTW_ColorPanel",
        b"",
        0,
        (LUM_BAR.left, LUM_BAR.top, LUM_BAR.width(), LUM_BAR.height()),
        PANEL_LUM,
        true,
    );
    child(
        ctx,
        memory,
        "OTW_ColorPanel",
        b"",
        0,
        (216, 168, 66, 42),
        PANEL_PREVIEW,
        true,
    );
    child(
        ctx,
        memory,
        "Static",
        b"Color|S&olid",
        0x1,
        (216, 212, 66, 14),
        0xFFFF,
        true,
    );
    let fields: [(&[u8], u32, i32, i32); 6] = [
        (b"Hu&e:", ID_HUE, 288, 168),
        (b"&Sat:", ID_SAT, 288, 192),
        (b"&Lum:", ID_LUM, 288, 216),
        (b"&Red:", ID_RED, 370, 168),
        (b"&Green:", ID_GREEN, 370, 192),
        (b"Bl&ue:", ID_BLUE, 370, 216),
    ];
    for (label, id, x, y) in fields {
        child(
            ctx,
            memory,
            "Static",
            label,
            0x2,
            (x - 10, y + 3, 42, 14),
            0xFFFF,
            true,
        );
        child(
            ctx,
            memory,
            "Edit",
            b"",
            TAB | 0x2000,
            (x + 34, y, 34, 20),
            id,
            true,
        );
    }
    child(
        ctx,
        memory,
        "Button",
        b"&Add to Custom Colors",
        TAB,
        (216, 247, 220, 23),
        ID_ADD,
        true,
    );
    sync_edits(ctx, memory);

    user::show_window(ctx, memory, dialog, 5);
    user::set_focus(ctx, memory, ok);
    let result = controls::run_modal_keep(ctx, memory, dialog, owner_top);
    let state = ctx.shared.borrow_mut().color_dialog.take();
    user::destroy_window(ctx, memory, dialog);
    if owner_top != 0 && user::is_window(ctx, owner_top) {
        user::activate(ctx, memory, owner_top, 1);
    }
    let Some(state) = state else {
        return 0;
    };
    if custom_va != 0 {
        for (index, color) in state.custom.iter().enumerate() {
            let _ = memory.write_u32(custom_va + index as u32 * 4, rgb_to_colorref(*color));
        }
    }
    if result != ID_OK {
        return 0;
    }
    let _ = memory.write_u32(cc + 12, rgb_to_colorref(state.color));
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hls_round_trips_basic_colors() {
        for color in BASIC_COLORS {
            let (h, l, s) = rgb_to_hls(color);
            let back = hls_to_rgb(h, l, s);
            for shift in [16, 8, 0] {
                let a = ((color >> shift) & 0xFF) as i32;
                let b = ((back >> shift) & 0xFF) as i32;
                assert!(
                    (a - b).abs() <= 3,
                    "{color:06x} -> ({h},{l},{s}) -> {back:06x}"
                );
            }
        }
        assert_eq!(rgb_to_hls(0xFF0000), (0, 120, 240));
        assert_eq!(hls_to_rgb(80, 120, 240), 0x00FF00);
    }
}
