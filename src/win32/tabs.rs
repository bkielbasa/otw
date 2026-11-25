//! The tab control (`SysTabControl32`) outside property sheets: a row of
//! tabs along the top, each with a label, an image index and an
//! application value. Owner-drawn tab controls (`TCS_OWNERDRAWFIXED`) have
//! their parent draw each tab through WM_DRAWITEM; the others are drawn
//! here. Selecting a tab with the mouse sends TCN_SELCHANGING and
//! TCN_SELCHANGE. TCITEM is read in the process's layout.

use crate::win32::{
    gdi::{self, sys_color},
    gfx::Rect,
    unicode, user,
    x64::{Layout, View, F},
    ApiContext, GuestMemory,
};

/// TCITEM: mask, dwState, dwStateMask, pszText, cchTextMax, iImage, lParam.
const TCITEM: Layout = Layout::new(&[F::D, F::D, F::D, F::P, F::D, F::D, F::P]);

const TCIF_TEXT: u32 = 0x1;
const TCIF_IMAGE: u32 = 0x2;
const TCIF_PARAM: u32 = 0x8;
const TCIF_STATE: u32 = 0x10;

const TCS_FIXEDWIDTH: u32 = 0x0400;
const TCS_OWNERDRAWFIXED: u32 = 0x2000;

const TCN_SELCHANGE: u32 = -551i32 as u32;
const TCN_SELCHANGING: u32 = -552i32 as u32;
const WM_NOTIFY: u32 = 0x004E;
const WM_LBUTTONDOWN: u32 = 0x0201;

/// Space either side of a label, and the default tab height.
const PADDING: i32 = 6;
const TAB_HEIGHT: i32 = 20;

#[derive(Clone, Debug, Default)]
struct Tab {
    text: Vec<u8>,
    image: i32,
    lparam: u64,
    state: u32,
}

#[derive(Clone, Debug)]
pub struct Tabs {
    tabs: Vec<Tab>,
    current: i32,
    focus: i32,
    image_list: u32,
    /// TCM_SETITEMSIZE: width (fixed-width tabs) and height.
    item_size: (i32, i32),
    min_width: i32,
    extended: u32,
}

impl Default for Tabs {
    fn default() -> Self {
        Self { tabs: Vec::new(), current: -1, focus: -1, image_list: 0, item_size: (0, 0), min_width: 0, extended: 0 }
    }
}

fn with_tabs<R>(ctx: &ApiContext, hwnd: u32, f: impl FnOnce(&mut Tabs) -> R) -> R {
    let mut shared = ctx.shared.borrow_mut();
    f(shared.gui.tab_controls.entry(hwnd).or_default())
}

fn style(ctx: &ApiContext, hwnd: u32) -> u32 {
    ctx.shared.borrow().gui.window(hwnd).map(|w| w.style).unwrap_or(0)
}

fn tab_height(tabs: &Tabs) -> i32 {
    if tabs.item_size.1 > 0 { tabs.item_size.1 } else { TAB_HEIGHT }
}

/// Each tab's rectangle, left to right along the top.
fn rects(tabs: &Tabs, style: u32) -> Vec<Rect> {
    let height = tab_height(tabs);
    let mut x = 2;
    tabs.tabs
        .iter()
        .map(|tab| {
            let width = if style & TCS_FIXEDWIDTH != 0 && tabs.item_size.0 > 0 {
                tabs.item_size.0
            } else {
                let image = if tabs.image_list != 0 && tab.image >= 0 { 20 } else { 0 };
                (user::text_width(&tab.text) + 2 * PADDING + image).max(tabs.min_width.max(0))
            };
            let rect = Rect::new(x, 2, x + width, 2 + height);
            x += width;
            rect
        })
        .collect()
}

fn read_text<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, wide: bool) -> Vec<u8> {
    if va == 0 || va == u32::MAX {
        return Vec::new();
    }
    if wide {
        unicode::wide_to_ansi(&unicode::read_wstr(memory, va))
    } else {
        crate::win32::read_cstr(memory, va)
    }
}

fn apply<M: GuestMemory + ?Sized>(memory: &mut M, tab: &mut Tab, info: u32, wide: bool) {
    let v = View::new(memory, &TCITEM, info);
    let mask = v.get32(memory, 0);
    if mask & TCIF_TEXT != 0 {
        let text = v.get32(memory, 3);
        tab.text = read_text(memory, text, wide);
    }
    if mask & TCIF_IMAGE != 0 {
        tab.image = v.get32(memory, 5) as i32;
    }
    if mask & TCIF_PARAM != 0 {
        tab.lparam = v.get(memory, 6);
    }
    if mask & TCIF_STATE != 0 {
        let state_mask = v.get32(memory, 2);
        tab.state = (tab.state & !state_mask) | (v.get32(memory, 1) & state_mask);
    }
}

fn fill<M: GuestMemory + ?Sized>(memory: &mut M, tab: &Tab, info: u32, wide: bool) {
    let v = View::new(memory, &TCITEM, info);
    let mask = v.get32(memory, 0);
    if mask & TCIF_TEXT != 0 {
        let (buffer, capacity) = (v.get32(memory, 3), v.get32(memory, 4) as usize);
        if buffer != 0 && capacity > 0 {
            if wide {
                let mut units = unicode::ansi_to_wide(&tab.text);
                units.truncate(capacity - 1);
                unicode::write_wide(memory, buffer, &units, true);
            } else {
                let mut bytes = tab.text.clone();
                bytes.truncate(capacity - 1);
                bytes.push(0);
                let _ = memory.write_bytes(buffer, &bytes);
            }
        }
    }
    if mask & TCIF_IMAGE != 0 {
        v.set(memory, 5, tab.image as u32);
    }
    if mask & TCIF_PARAM != 0 {
        v.set_wide(memory, 6, tab.lparam);
    }
    if mask & TCIF_STATE != 0 {
        let state_mask = v.get32(memory, 2);
        v.set(memory, 1, tab.state & state_mask);
    }
}

fn invalidate(ctx: &ApiContext, hwnd: u32) {
    let mut shared = ctx.shared.borrow_mut();
    user::invalidate(&mut shared.gui, hwnd, None, true, false);
    shared.gui.dirty = true;
}

/// Send a bare NMHDR notification to the parent.
fn notify<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, code: u32) -> u32 {
    let (parent, id) = match ctx.shared.borrow().gui.window(hwnd) {
        Some(window) => (window.parent, window.id),
        None => return 0,
    };
    if parent == 0 {
        return 0;
    }
    let block = user::scratch(ctx, 32);
    let view = View::new(memory, &crate::win32::x64::NMHDR, block);
    view.set(memory, 0, hwnd);
    view.set(memory, 1, id);
    view.set(memory, 2, code);
    let result = user::send(ctx, memory, parent, WM_NOTIFY, id, block);
    user::free_scratch(ctx, block);
    result
}

fn paint<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let ps = user::scratch(ctx, 80);
    let hdc = user::begin_paint(ctx, memory, hwnd, ps);
    let tabs = with_tabs(ctx, hwnd, |t| t.clone());
    let style = style(ctx, hwnd);
    let (width, height, parent, id) = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| {
            let (cw, ch) = user::client_size(w);
            (cw, ch, w.parent, w.id)
        })
        .unwrap_or_default();
    let rects = rects(&tabs, style);
    let strip = tab_height(&tabs) + 2;
    {
        let mut shared = ctx.shared.borrow_mut();
        user::with_painter(&mut shared, hwnd, true, |p| {
            p.fill(Rect::new(0, 0, width, height), sys_color(15));
            // The body's raised edge, open under the selected tab.
            p.raised_button(Rect::new(0, strip, width, height.max(strip + 2)));
            for (index, (tab, rect)) in tabs.tabs.iter().zip(&rects).enumerate() {
                let selected = index as i32 == tabs.current;
                let r = if selected { Rect::new(rect.left - 2, rect.top - 2, rect.right + 2, rect.bottom + 1) } else { *rect };
                p.fill(r, sys_color(15));
                p.hline(r.left + 2, r.right - 2, r.top, sys_color(20));
                p.vline(r.left, r.top + 2, r.bottom, sys_color(20));
                p.vline(r.right - 1, r.top + 2, r.bottom, sys_color(16));
                if style & TCS_OWNERDRAWFIXED == 0 {
                    let image = if tabs.image_list != 0 && tab.image >= 0 { 20 } else { 0 };
                    p.text(r.left + PADDING + image, r.top + 3, &tab.text, sys_color(18), false);
                }
            }
        });
    }
    if style & TCS_OWNERDRAWFIXED != 0 && parent != 0 {
        for (index, (tab, rect)) in tabs.tabs.iter().zip(&rects).enumerate() {
            let selected = index as i32 == tabs.current;
            let dis = user::scratch(ctx, 80);
            let _ = memory.write_bytes(dis, &[0u8; 80]);
            let view = View::new(memory, &crate::win32::x64::DRAWITEMSTRUCT, dis);
            // ODT_TAB, ODA_DRAWENTIRE, ODS_SELECTED.
            for (field, value) in [(0, 101), (1, id), (2, index as u32), (3, 1), (4, u32::from(selected)), (5, hwnd), (6, hdc)] {
                view.set(memory, field, value);
            }
            let r = if selected { Rect::new(rect.left, rect.top - 2, rect.right, rect.bottom) } else { *rect };
            let _ = gdi::write_rect(memory, view.at(7), Rect::new(r.left + 1, r.top + 1, r.right - 1, r.bottom));
            view.set_wide(memory, 8, tab.lparam);
            user::send(ctx, memory, parent, 0x002B, id, dis);
            user::free_scratch(ctx, dis);
        }
    }
    user::end_paint(ctx, hwnd, hdc);
    user::free_scratch(ctx, ps);
}

/// Select a tab as a click does: TCN_SELCHANGING (which may refuse), then
/// TCN_SELCHANGE.
fn click<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, index: i32) {
    if index == with_tabs(ctx, hwnd, |t| t.current) {
        return;
    }
    if notify(ctx, memory, hwnd, TCN_SELCHANGING) != 0 {
        return;
    }
    with_tabs(ctx, hwnd, |t| {
        t.current = index;
        t.focus = index;
    });
    invalidate(ctx, hwnd);
    notify(ctx, memory, hwnd, TCN_SELCHANGE);
}

fn hit(ctx: &ApiContext, hwnd: u32, x: i32, y: i32) -> i32 {
    let tabs = with_tabs(ctx, hwnd, |t| t.clone());
    rects(&tabs, style(ctx, hwnd))
        .iter()
        .position(|r| r.contains(x, y))
        .map(|i| i as i32)
        .unwrap_or(-1)
}

pub fn tab_proc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, msg: u32, wparam: u32, lparam: u32) -> u32 {
    let wide = matches!(msg, 0x133C..=0x133E);
    match msg {
        // TCM_GETIMAGELIST, TCM_SETIMAGELIST
        0x1302 => with_tabs(ctx, hwnd, |t| t.image_list),
        0x1303 => {
            let old = with_tabs(ctx, hwnd, |t| std::mem::replace(&mut t.image_list, lparam));
            invalidate(ctx, hwnd);
            old
        }
        // TCM_GETITEMCOUNT
        0x1304 => with_tabs(ctx, hwnd, |t| t.tabs.len() as u32),
        // TCM_GETITEMA/W
        0x1305 | 0x133C => match with_tabs(ctx, hwnd, |t| t.tabs.get(wparam as usize).cloned()) {
            Some(tab) => {
                fill(memory, &tab, lparam, wide);
                1
            }
            None => 0,
        },
        // TCM_SETITEMA/W
        0x1306 | 0x133D => {
            let Some(mut tab) = with_tabs(ctx, hwnd, |t| t.tabs.get(wparam as usize).cloned()) else { return 0 };
            apply(memory, &mut tab, lparam, wide);
            with_tabs(ctx, hwnd, |t| t.tabs[wparam as usize] = tab);
            invalidate(ctx, hwnd);
            1
        }
        // TCM_INSERTITEMA/W: the new index. The first tab is selected.
        0x1307 | 0x133E => {
            let mut tab = Tab { image: -1, ..Tab::default() };
            apply(memory, &mut tab, lparam, wide);
            let at = with_tabs(ctx, hwnd, |t| {
                let at = (wparam as usize).min(t.tabs.len());
                t.tabs.insert(at, tab);
                if t.current < 0 {
                    t.current = 0;
                } else if t.current >= at as i32 && t.tabs.len() > 1 {
                    t.current += 1;
                }
                at as u32
            });
            invalidate(ctx, hwnd);
            at
        }
        // TCM_DELETEITEM
        0x1308 => {
            let removed = with_tabs(ctx, hwnd, |t| {
                let index = wparam as usize;
                if index >= t.tabs.len() {
                    return false;
                }
                t.tabs.remove(index);
                let index = index as i32;
                if t.current == index {
                    t.current = -1;
                } else if t.current > index {
                    t.current -= 1;
                }
                true
            });
            invalidate(ctx, hwnd);
            u32::from(removed)
        }
        // TCM_DELETEALLITEMS
        0x1309 => {
            with_tabs(ctx, hwnd, |t| {
                t.tabs.clear();
                t.current = -1;
            });
            invalidate(ctx, hwnd);
            1
        }
        // TCM_GETITEMRECT
        0x130A => {
            let tabs = with_tabs(ctx, hwnd, |t| t.clone());
            match rects(&tabs, style(ctx, hwnd)).get(wparam as usize) {
                Some(rect) => {
                    let _ = gdi::write_rect(memory, lparam, *rect);
                    1
                }
                None => 0,
            }
        }
        // TCM_GETCURSEL, TCM_SETCURSEL (no notifications)
        0x130B => with_tabs(ctx, hwnd, |t| t.current as u32),
        0x130C => {
            let old = with_tabs(ctx, hwnd, |t| {
                let old = t.current;
                if (wparam as usize) < t.tabs.len() {
                    t.current = wparam as i32;
                    t.focus = wparam as i32;
                }
                old
            });
            invalidate(ctx, hwnd);
            old as u32
        }
        // TCM_HITTEST (TCHITTESTINFO: point, flags)
        0x130D => {
            let x = memory.read_u32(lparam).unwrap_or(0) as i32;
            let y = memory.read_u32(lparam + 4).unwrap_or(0) as i32;
            let index = hit(ctx, hwnd, x, y);
            let _ = memory.write_u32(lparam + 8, if index >= 0 { 6 } else { 1 });
            index as u32
        }
        // TCM_SETITEMEXTRA
        0x130E => 1,
        // TCM_ADJUSTRECT (larger, RECT*): window rect <-> display area.
        0x1328 => {
            let strip = with_tabs(ctx, hwnd, |t| tab_height(t)) + 2;
            if let Some(r) = gdi::read_rect(memory, lparam) {
                let adjusted = if wparam != 0 {
                    Rect::new(r.left - 2, r.top - strip - 2, r.right + 2, r.bottom + 2)
                } else {
                    Rect::new(r.left + 2, r.top + strip + 2, r.right - 2, r.bottom - 2)
                };
                let _ = gdi::write_rect(memory, lparam, adjusted);
            }
            0
        }
        // TCM_SETITEMSIZE: the old size.
        0x1329 => {
            let (cx, cy) = ((lparam & 0xFFFF) as i32, (lparam >> 16) as i32);
            let old = with_tabs(ctx, hwnd, |t| {
                let old = t.item_size;
                t.item_size = (cx, cy);
                old
            });
            invalidate(ctx, hwnd);
            (old.0 as u32 & 0xFFFF) | ((old.1 as u32) << 16)
        }
        // TCM_GETROWCOUNT
        0x132C => 1,
        // TCM_GETCURFOCUS, TCM_SETCURFOCUS
        0x132F => with_tabs(ctx, hwnd, |t| t.focus as u32),
        0x1330 => {
            with_tabs(ctx, hwnd, |t| t.focus = wparam as i32);
            0
        }
        // TCM_SETMINTABWIDTH: the old minimum.
        0x1331 => with_tabs(ctx, hwnd, |t| std::mem::replace(&mut t.min_width, lparam as i32) as u32),
        // TCM_SETEXTENDEDSTYLE, TCM_GETEXTENDEDSTYLE
        0x1334 => with_tabs(ctx, hwnd, |t| {
            let old = t.extended;
            let mask = if wparam == 0 { u32::MAX } else { wparam };
            t.extended = (t.extended & !mask) | (lparam & mask);
            old
        }),
        0x1335 => with_tabs(ctx, hwnd, |t| t.extended),
        0x1300..=0x13FF => 0,
        user::WM_PAINT => {
            paint(ctx, memory, hwnd);
            0
        }
        user::WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN => {
            let (x, y) = ((lparam & 0xFFFF) as i16 as i32, (lparam >> 16) as i16 as i32);
            let index = hit(ctx, hwnd, x, y);
            if index >= 0 {
                click(ctx, memory, hwnd, index);
            }
            0
        }
        user::WM_SIZE => {
            invalidate(ctx, hwnd);
            0
        }
        user::WM_DESTROY => {
            ctx.shared.borrow_mut().gui.tab_controls.remove(&hwnd);
            0
        }
        _ => user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}
