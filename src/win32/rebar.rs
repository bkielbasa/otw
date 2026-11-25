//! The rebar control (`ReBarWindow32`): bands holding child windows (a
//! toolbar, an address box) laid out in one row along the top of the
//! parent. Bands are added and changed with `REBARBANDINFO`, read in the
//! process's layout.

use crate::win32::{
    gdi::sys_color,
    gfx::Rect,
    user,
    x64::{Layout, View, F},
    ApiContext, GuestMemory,
};

/// REBARBANDINFO: cbSize, fMask, fStyle, clrFore, clrBack, lpText, cch,
/// iImage, hwndChild, cxMinChild, cyMinChild, cx, hbmBack, wID, cyChild,
/// cyMaxChild, cyIntegral, cxIdeal, lParam, cxHeader.
const REBARBANDINFO: Layout = Layout::sized(&[
    F::D, F::D, F::D, F::D, F::D, F::P, F::D, F::D, F::P, F::D, F::D, F::D, F::P, F::D, F::D, F::D, F::D, F::D,
    F::P, F::D,
]);

const RBBIM_STYLE: u32 = 0x1;
const RBBIM_CHILD: u32 = 0x10;
const RBBIM_CHILDSIZE: u32 = 0x20;
const RBBIM_SIZE: u32 = 0x40;
const RBBIM_ID: u32 = 0x100;
const RBBIM_IDEALSIZE: u32 = 0x200;
const RBBIM_LPARAM: u32 = 0x400;
const RBBS_HIDDEN: u32 = 0x8;

/// Space around a band's child.
const BORDER: i32 = 2;

#[derive(Clone, Debug, Default)]
pub struct Band {
    style: u32,
    child: u32,
    min_cx: i32,
    min_cy: i32,
    cx: i32,
    ideal: i32,
    id: u32,
    lparam: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Rebar {
    bands: Vec<Band>,
}

fn with_rebar<R>(ctx: &ApiContext, hwnd: u32, f: impl FnOnce(&mut Rebar) -> R) -> R {
    let mut shared = ctx.shared.borrow_mut();
    f(shared.gui.rebars.entry(hwnd).or_default())
}

/// Read a REBARBANDINFO's fields its mask names into `band`.
fn apply<M: GuestMemory + ?Sized>(memory: &mut M, band: &mut Band, info: u32) {
    let v = View::new(memory, &REBARBANDINFO, info);
    let mask = v.get32(memory, 1);
    if mask & RBBIM_STYLE != 0 {
        band.style = v.get32(memory, 2);
    }
    if mask & RBBIM_CHILD != 0 {
        band.child = v.get32(memory, 8);
    }
    if mask & RBBIM_CHILDSIZE != 0 {
        band.min_cx = v.get32(memory, 9) as i32;
        band.min_cy = v.get32(memory, 10) as i32;
    }
    if mask & RBBIM_SIZE != 0 {
        band.cx = v.get32(memory, 11) as i32;
    }
    if mask & RBBIM_ID != 0 {
        band.id = v.get32(memory, 13);
    }
    if mask & RBBIM_IDEALSIZE != 0 {
        band.ideal = v.get32(memory, 17) as i32;
    }
    if mask & RBBIM_LPARAM != 0 {
        band.lparam = v.get32(memory, 18);
    }
}

/// Fill a REBARBANDINFO from `band`, as its mask asks.
fn fill<M: GuestMemory + ?Sized>(memory: &mut M, band: &Band, info: u32) {
    let v = View::new(memory, &REBARBANDINFO, info);
    let mask = v.get32(memory, 1);
    for (bit, index, value) in [
        (RBBIM_STYLE, 2, band.style),
        (RBBIM_CHILD, 8, band.child),
        (RBBIM_CHILDSIZE, 9, band.min_cx as u32),
        (RBBIM_CHILDSIZE, 10, band.min_cy as u32),
        (RBBIM_SIZE, 11, band.cx as u32),
        (RBBIM_ID, 13, band.id),
        (RBBIM_IDEALSIZE, 17, band.ideal as u32),
        (RBBIM_LPARAM, 18, band.lparam),
    ] {
        if mask & bit != 0 {
            v.set(memory, index, value);
        }
    }
}

fn visible(band: &Band) -> bool {
    band.style & RBBS_HIDDEN == 0
}

/// The bar's height: the tallest visible band.
fn height(rebar: &Rebar) -> i32 {
    rebar.bands.iter().filter(|b| visible(b)).map(|b| b.min_cy + 2 * BORDER).max().unwrap_or(0)
}

/// Lay the bands out left to right, the last taking the rest of the width;
/// size the bar to its parent's width and the bands' height.
fn layout<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let rebar = with_rebar(ctx, hwnd, |r| r.clone());
    let (parent, x, y) = {
        let shared = ctx.shared.borrow();
        match shared.gui.window(hwnd) {
            Some(w) => (w.parent, w.x, w.y),
            None => return,
        }
    };
    let width = ctx.shared.borrow().gui.window(parent).map(|w| user::client_size(w).0).unwrap_or(0);
    let bar_height = height(&rebar);
    user::set_window_pos(ctx, memory, hwnd, 0, x, y, width, bar_height, 0x4 | 0x10); // NOZORDER | NOACTIVATE
    let shown: Vec<&Band> = rebar.bands.iter().filter(|b| visible(b)).collect();
    let mut left = 0;
    for (index, band) in shown.iter().enumerate() {
        let wanted = band.cx.max(band.ideal).max(band.min_cx) + 2 * BORDER;
        let band_width = if index + 1 == shown.len() { (width - left).max(wanted.min(width - left)) } else { wanted };
        if band.child != 0 {
            user::set_window_pos(
                ctx,
                memory,
                band.child,
                0,
                left + BORDER,
                BORDER,
                (band_width - 2 * BORDER).max(0),
                band.min_cy,
                0x4 | 0x10,
            );
        }
        left += band_width;
    }
    let mut shared = ctx.shared.borrow_mut();
    user::invalidate(&mut shared.gui, hwnd, None, true, false);
    shared.gui.dirty = true;
}

pub fn rebar_proc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, msg: u32, wparam: u32, lparam: u32) -> u32 {
    match msg {
        // RB_INSERTBANDA/W (index, info)
        0x0401 | 0x040A => {
            let mut band = Band::default();
            apply(memory, &mut band, lparam);
            if band.child != 0 {
                if let Some(child) = ctx.shared.borrow_mut().gui.window_mut(band.child) {
                    child.parent = hwnd;
                }
            }
            with_rebar(ctx, hwnd, |r| {
                let at = if wparam as i32 == -1 { r.bands.len() } else { (wparam as usize).min(r.bands.len()) };
                r.bands.insert(at, band);
            });
            layout(ctx, memory, hwnd);
            1
        }
        // RB_DELETEBAND
        0x0402 => {
            let removed = with_rebar(ctx, hwnd, |r| ((wparam as usize) < r.bands.len()).then(|| r.bands.remove(wparam as usize)).is_some());
            layout(ctx, memory, hwnd);
            u32::from(removed)
        }
        // RB_SETBANDINFOA/W (index, info)
        0x0406 | 0x040B => {
            let band = with_rebar(ctx, hwnd, |r| r.bands.get(wparam as usize).cloned());
            let Some(mut band) = band else { return 0 };
            apply(memory, &mut band, lparam);
            with_rebar(ctx, hwnd, |r| r.bands[wparam as usize] = band);
            layout(ctx, memory, hwnd);
            1
        }
        // RB_GETBANDINFOW/A
        0x041C | 0x041D => match with_rebar(ctx, hwnd, |r| r.bands.get(wparam as usize).cloned()) {
            Some(band) => {
                fill(memory, &band, lparam);
                1
            }
            None => 0,
        },
        // RB_GETBANDCOUNT
        0x040C => with_rebar(ctx, hwnd, |r| r.bands.len() as u32),
        // RB_GETROWCOUNT
        0x040D => u32::from(with_rebar(ctx, hwnd, |r| r.bands.iter().any(visible))),
        // RB_GETROWHEIGHT, RB_GETBARHEIGHT
        0x040E | 0x041B => with_rebar(ctx, hwnd, |r| height(r) as u32),
        // RB_IDTOINDEX
        0x0410 => with_rebar(ctx, hwnd, |r| r.bands.iter().position(|b| b.id == wparam).map(|i| i as u32).unwrap_or(u32::MAX)),
        // RB_SHOWBAND (index, show)
        0x0423 => {
            let changed = with_rebar(ctx, hwnd, |r| match r.bands.get_mut(wparam as usize) {
                Some(band) => {
                    if lparam != 0 {
                        band.style &= !RBBS_HIDDEN;
                    } else {
                        band.style |= RBBS_HIDDEN;
                    }
                    Some(band.child)
                }
                None => None,
            });
            match changed {
                Some(child) => {
                    if child != 0 {
                        user::show_window(ctx, memory, child, if lparam != 0 { 5 } else { 0 });
                    }
                    layout(ctx, memory, hwnd);
                    1
                }
                None => 0,
            }
        }
        // RB_GETRECT (index, RECT*)
        0x0409 => {
            let (rebar, width) = (with_rebar(ctx, hwnd, |r| r.clone()), ctx.shared.borrow().gui.window(hwnd).map(|w| w.width).unwrap_or(0));
            let mut left = 0;
            for (index, band) in rebar.bands.iter().enumerate() {
                if !visible(band) {
                    continue;
                }
                let band_width = band.cx.max(band.ideal).max(band.min_cx) + 2 * BORDER;
                if index == wparam as usize {
                    let _ = crate::win32::gdi::write_rect(memory, lparam, Rect::new(left, 0, (left + band_width).min(width), height(&rebar)));
                    return 1;
                }
                left += band_width;
            }
            0
        }
        // RB_SETBARINFO, RB_SETBKCOLOR, RB_MAXIMIZEBAND, RB_MINIMIZEBAND, RB_SETPARENT, ...
        0x0404 | 0x0413 | 0x0414 | 0x041E | 0x041F | 0x0407 => 1,
        // RB_GETBANDBORDERS (index, RECT*): the space around the child.
        0x0422 => {
            let _ = crate::win32::gdi::write_rect(memory, lparam, Rect::new(BORDER, BORDER, BORDER, BORDER));
            1
        }
        0x0400..=0x04FF => 0,
        user::WM_SIZE => {
            layout(ctx, memory, hwnd);
            0
        }
        user::WM_DESTROY => {
            ctx.shared.borrow_mut().gui.rebars.remove(&hwnd);
            0
        }
        user::WM_ERASEBKGND => {
            let (width, height) = ctx.shared.borrow().gui.window(hwnd).map(|w| (w.width, w.height)).unwrap_or((0, 0));
            let mut shared = ctx.shared.borrow_mut();
            user::with_painter(&mut shared, hwnd, true, |p| p.fill(Rect::new(0, 0, width, height), sys_color(15)));
            1
        }
        // Notifications and commands from the bands' children go to the
        // rebar's parent.
        user::WM_COMMAND | 0x004E => {
            let parent = ctx.shared.borrow().gui.window(hwnd).map(|w| w.parent).unwrap_or(0);
            if parent != 0 { user::send(ctx, memory, parent, msg, wparam, lparam) } else { 0 }
        }
        _ => user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}
