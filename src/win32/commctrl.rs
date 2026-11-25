//! More of COMCTL32: image lists, the toolbar (`ToolbarWindow32`), the
//! status bar (`msctls_statusbar32`), window subclassing
//! (`SetWindowSubclass`), and the icon drawing USER32 functions image lists
//! build on.

use std::collections::HashMap;

use crate::{
    kernel::pe::ImportName,
    win32::{
        gdi::{self, sys_color},
        gfx::{self, Rect, Rgb, Surface},
        icon::IconImage,
        read_cstr, unicode, user, ApiContext, ApiOutcome, GuestMemory,
    },
};

const WM_COMMAND: u32 = 0x0111;
const WM_NOTIFY: u32 = 0x004E;

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "InitCommonControls" => 0,
        "ImageList_GetImageCount" | "ImageList_Destroy" => 1,
        "ImageList_SetImageCount" | "ImageList_Remove" | "ImageList_GetBkColor" | "ImageList_SetBkColor" => 2,
        "ImageList_Add" | "ImageList_AddMasked" | "ImageList_ReplaceIcon" | "ImageList_GetIconSize"
        | "RemoveWindowSubclass" | "ImageList_GetIcon" | "ImageList_SetIconSize" | "ImageList_GetImageInfo" => 3,
        "ImageList_Replace" | "SetWindowSubclass" | "DefSubclassProc" | "CreateStatusWindowW"
        | "CreateStatusWindowA" | "DrawIcon" | "GetWindowSubclass" => 4,
        "ImageList_Create" => 5,
        "ImageList_Draw" => 6,
        "DrawIconEx" => 9,
        "ImageList_DrawEx" => 10,
        "CreateToolbarEx" => 13,
        "GetIconInfo" => 2,
        "OTW_SubclassProc" | "OTW_ToolbarProc" | "OTW_StatusProc" => 4,
        _ => return None,
    })
}

/// COMCTL32 functions exported only by ordinal.
pub fn ordinal_name(dll: &str, ordinal: u16) -> Option<&'static str> {
    if !dll.eq_ignore_ascii_case("comctl32.dll") {
        return None;
    }
    Some(match ordinal {
        17 => "InitCommonControls",
        410 => "SetWindowSubclass",
        411 => "GetWindowSubclass",
        412 => "RemoveWindowSubclass",
        413 => "DefSubclassProc",
        _ => return None,
    })
}

// ── Image lists ─────────────────────────────────────────────────────────

/// An image list: equal-sized images with transparency (`None` pixels).
#[derive(Clone, Debug, Default)]
pub struct ImageList {
    pub width: i32,
    pub height: i32,
    pub images: Vec<IconImage>,
}

/// Image list handles share the GDI handle space but live here.
const IMAGE_LIST_BASE: u32 = 0x00A1_0000;

fn new_image_list(ctx: &ApiContext, width: i32, height: i32) -> u32 {
    let mut shared = ctx.shared.borrow_mut();
    let handle = IMAGE_LIST_BASE + shared.alloc_handle();
    shared.image_lists.insert(handle, ImageList { width, height, images: Vec::new() });
    handle
}

/// Cut a bitmap into images of the list's size; `transparent` is the colour
/// shown as see-through, or the mask bitmap decides.
fn split(list: &ImageList, colour: &Surface, mask: Option<&Surface>, transparent: Option<Rgb>) -> Vec<IconImage> {
    let (w, h) = (list.width.max(1), list.height.max(1));
    let count = (colour.width / w).max(if colour.width > 0 { 1 } else { 0 });
    (0..count)
        .map(|index| {
            let mut pixels = Vec::with_capacity((w * h) as usize);
            for y in 0..h {
                for x in 0..w {
                    let sx = index * w + x;
                    let value = colour.get(sx, y).unwrap_or(0);
                    let hidden = match (mask, transparent) {
                        (Some(mask), _) => mask.get(sx, y).is_some_and(|m| m & 0xFF_FFFF != 0),
                        (None, Some(key)) => value == key,
                        _ => false,
                    };
                    pixels.push((!hidden).then_some(value));
                }
            }
            IconImage { width: w, height: h, pixels }
        })
        .collect()
}

fn icon_image(ctx: &ApiContext, hicon: u32, width: i32) -> Option<IconImage> {
    let shared = ctx.shared.borrow();
    let icon = shared.gui.icons.get(&hicon)?;
    let image = if width <= 16 && icon.small.width > 0 { &icon.small } else { &icon.large };
    Some(if image.width == width || width <= 0 { image.clone() } else { crate::win32::icon::scale(image, width) })
}

/// Draw an image at (x, y) of a DC, scaled to `size` when given.
pub fn draw_image<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hdc: u32,
    x: i32,
    y: i32,
    image: &IconImage,
    size: Option<(i32, i32)>,
    disabled: bool,
) {
    let (w, h) = size.filter(|(w, h)| *w > 0 && *h > 0).unwrap_or((image.width, image.height));
    gdi::with_dc(ctx, memory, hdc, |target, dctx| {
        for dy in 0..h {
            for dx in 0..w {
                let sx = dx * image.width / w.max(1);
                let sy = dy * image.height / h.max(1);
                let Some(Some(colour)) = image.pixels.get((sy * image.width + sx) as usize) else {
                    continue;
                };
                let colour = if disabled { grey(*colour) } else { *colour };
                let (px, py) = dctx.pt(x + dx, y + dy);
                target.plot(px, py, colour, gfx::R2_COPYPEN);
            }
        }
    });
}

/// A disabled image's pixel: light grey, darker where the image is dark.
fn grey(colour: Rgb) -> Rgb {
    let (r, g, b) = ((colour >> 16) & 0xFF, (colour >> 8) & 0xFF, colour & 0xFF);
    if (r * 30 + g * 59 + b * 11) / 100 < 160 {
        sys_color(16)
    } else {
        sys_color(15)
    }
}

// ── Subclassing ─────────────────────────────────────────────────────────

/// A window's subclass chain over its original procedure.
#[derive(Clone, Debug, Default)]
pub struct Subclasses {
    original: u32,
    /// (procedure, id, reference data), the most recent last.
    chain: Vec<(u32, u32, u32)>,
    /// Chain positions of the calls in progress, innermost last.
    active: Vec<usize>,
}

fn call_level<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    level: Option<usize>,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> u32 {
    let (entry, original) = {
        let shared = ctx.shared.borrow();
        let Some(subclasses) = shared.gui.subclasses.get(&hwnd) else {
            return 0;
        };
        (level.and_then(|l| subclasses.chain.get(l).copied()), subclasses.original)
    };
    let Some((proc_va, id, data)) = entry else {
        return user::call_wndproc(ctx, memory, original, hwnd, msg, wparam, lparam);
    };
    if let Some(subclasses) = ctx.shared.borrow_mut().gui.subclasses.get_mut(&hwnd) {
        subclasses.active.push(level.unwrap_or(0));
    }
    // Subclass procedures of Unicode windows receive Unicode messages.
    let wide = unicode::is_unicode_proc(ctx, original);
    let result = if wide {
        unicode::convert_message(ctx, memory, true, hwnd, msg, wparam, lparam, |memory, wparam, lparam| {
            memory.call_guest(proc_va, &[hwnd, msg, wparam, lparam, id, data]).unwrap_or(0)
        })
    } else {
        memory.call_guest(proc_va, &[hwnd, msg, wparam, lparam, id, data]).unwrap_or(0)
    };
    if let Some(subclasses) = ctx.shared.borrow_mut().gui.subclasses.get_mut(&hwnd) {
        subclasses.active.pop();
    }
    result
}

/// The procedure of a subclassed window: the newest subclass first.
pub fn subclass_proc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, msg: u32, wparam: u32, lparam: u32) -> u32 {
    let top = ctx.shared.borrow().gui.subclasses.get(&hwnd).map(|s| s.chain.len().checked_sub(1));
    match top {
        Some(level) => call_level(ctx, memory, hwnd, level, msg, wparam, lparam),
        None => user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

fn def_subclass_proc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, msg: u32, wparam: u32, lparam: u32) -> u32 {
    let (current, original) = {
        let shared = ctx.shared.borrow();
        let Some(subclasses) = shared.gui.subclasses.get(&hwnd) else {
            return user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam);
        };
        (subclasses.active.last().copied(), subclasses.original)
    };
    let next = current.and_then(|c| c.checked_sub(1));
    // The caller speaks Unicode if the window does; the next level expects
    // the system's ANSI form.
    if unicode::is_unicode_proc(ctx, original) {
        unicode::convert_message(ctx, memory, false, hwnd, msg, wparam, lparam, |memory, wparam, lparam| {
            call_level(ctx, memory, hwnd, next, msg, wparam, lparam)
        })
    } else {
        call_level(ctx, memory, hwnd, next, msg, wparam, lparam)
    }
}

fn set_subclass(ctx: &ApiContext, hwnd: u32, proc_va: u32, id: u32, data: u32) -> u32 {
    let thunk = ctx.builtin_proc("OTW_SubclassProc");
    let mut shared = ctx.shared.borrow_mut();
    let Some(current) = shared.gui.window(hwnd).map(|w| w.wndproc) else {
        return 0;
    };
    let subclasses = shared.gui.subclasses.entry(hwnd).or_insert_with(|| Subclasses { original: current, ..Default::default() });
    match subclasses.chain.iter_mut().find(|(p, i, _)| *p == proc_va && *i == id) {
        Some(entry) => entry.2 = data,
        None => subclasses.chain.push((proc_va, id, data)),
    }
    if let Some(window) = shared.gui.window_mut(hwnd) {
        window.wndproc = thunk;
    }
    1
}

fn remove_subclass(ctx: &ApiContext, hwnd: u32, proc_va: u32, id: u32) -> u32 {
    let mut shared = ctx.shared.borrow_mut();
    let Some(subclasses) = shared.gui.subclasses.get_mut(&hwnd) else {
        return 0;
    };
    let before = subclasses.chain.len();
    subclasses.chain.retain(|(p, i, _)| !(*p == proc_va && *i == id));
    let removed = subclasses.chain.len() != before;
    if subclasses.chain.is_empty() && subclasses.active.is_empty() {
        let original = subclasses.original;
        shared.gui.subclasses.remove(&hwnd);
        if let Some(window) = shared.gui.window_mut(hwnd) {
            window.wndproc = original;
        }
    }
    u32::from(removed)
}

// ── Toolbar ─────────────────────────────────────────────────────────────

const TBSTATE_CHECKED: u8 = 0x01;
const TBSTATE_PRESSED: u8 = 0x02;
const TBSTATE_ENABLED: u8 = 0x04;
const TBSTATE_HIDDEN: u8 = 0x08;
const BTNS_SEP: u8 = 0x01;
const BTNS_CHECK: u8 = 0x02;
const BTNS_GROUP: u8 = 0x04;
const BTNS_DROPDOWN: u8 = 0x08;
const BTNS_WHOLEDROPDOWN: u8 = 0x80;
const TBSTYLE_EX_DRAWDDARROWS: u32 = 0x1;
const TBN_DROPDOWN: u32 = -710i32 as u32;
const CCS_NORESIZE: u32 = 0x4;
const CCS_NOPARENTALIGN: u32 = 0x8;
const CCS_NODIVIDER: u32 = 0x40;
const DROPDOWN_ARROW: i32 = 12;

#[derive(Clone, Debug, Default)]
struct Button {
    image: i32,
    command: u32,
    state: u8,
    style: u8,
    data: u32,
    text: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Toolbar {
    buttons: Vec<Button>,
    image_list: u32,
    hot_list: u32,
    disabled_list: u32,
    /// The list TB_ADDBITMAP fills when no image list was set.
    own_list: u32,
    button_size: (i32, i32),
    bitmap_size: (i32, i32),
    ex_style: u32,
    parent: u32,
    pressed: Option<usize>,
    strings: Vec<Vec<u8>>,
    indent: i32,
}

impl Default for Toolbar {
    fn default() -> Self {
        Self {
            buttons: Vec::new(),
            image_list: 0,
            hot_list: 0,
            disabled_list: 0,
            own_list: 0,
            button_size: (23, 22),
            bitmap_size: (16, 15),
            ex_style: 0,
            parent: 0,
            pressed: None,
            strings: Vec::new(),
            indent: 0,
        }
    }
}

fn with_toolbar<R>(ctx: &ApiContext, hwnd: u32, f: impl FnOnce(&mut Toolbar) -> R) -> R {
    let mut shared = ctx.shared.borrow_mut();
    f(shared.gui.toolbars.entry(hwnd).or_default())
}

fn read_button<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, wide: bool, strings: &[Vec<u8>]) -> Button {
    let bytes = memory.read_bytes(va, 20).unwrap_or_else(|| vec![0; 20]);
    let dword = |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    let string = dword(16);
    let text = if string >= 0x10000 {
        if wide {
            unicode::wide_to_ansi(&unicode::read_wstr(memory, string))
        } else {
            read_cstr(memory, string)
        }
    } else {
        strings.get(string as usize).cloned().unwrap_or_default()
    };
    Button { image: dword(0) as i32, command: dword(4), state: bytes[8], style: bytes[9], data: dword(12), text }
}

fn button_width(toolbar: &Toolbar, button: &Button) -> i32 {
    if button.style & BTNS_SEP != 0 {
        return if button.image > 0 { button.image } else { 8 };
    }
    let arrow = button.style & BTNS_DROPDOWN != 0 && toolbar.ex_style & TBSTYLE_EX_DRAWDDARROWS != 0;
    toolbar.button_size.0 + if arrow { DROPDOWN_ARROW } else { 0 }
}

/// Button rectangles in client coordinates (hidden buttons are empty),
/// wrapping onto more rows when the toolbar is too narrow.
fn layout(toolbar: &Toolbar, width: i32, top: i32) -> Vec<Rect> {
    let mut x = toolbar.indent;
    let mut y = top;
    let (_, h) = toolbar.button_size;
    toolbar
        .buttons
        .iter()
        .map(|button| {
            if button.state & TBSTATE_HIDDEN != 0 {
                return Rect::new(x, y, x, y);
            }
            let w = button_width(toolbar, button);
            if x + w > width && x > toolbar.indent && button.style & BTNS_SEP == 0 && width > 0 {
                x = toolbar.indent;
                y += h;
            }
            let rect = Rect::new(x, y, x + w, y + h);
            x += w;
            rect
        })
        .collect()
}

fn client(ctx: &ApiContext, hwnd: u32) -> (i32, i32) {
    let shared = ctx.shared.borrow();
    shared
        .gui
        .window(hwnd)
        .map(|window| {
            let (l, t, r, b) = user::nc_insets(window);
            (window.width - l - r, window.height - t - b)
        })
        .unwrap_or((0, 0))
}

fn style_of(ctx: &ApiContext, hwnd: u32) -> u32 {
    ctx.shared.borrow().gui.window(hwnd).map(|w| w.style).unwrap_or(0)
}

fn toolbar_top(ctx: &ApiContext, hwnd: u32) -> i32 {
    if style_of(ctx, hwnd) & CCS_NODIVIDER != 0 { 0 } else { 2 }
}

fn rects(ctx: &ApiContext, hwnd: u32) -> Vec<Rect> {
    let width = client(ctx, hwnd).0;
    let top = toolbar_top(ctx, hwnd);
    with_toolbar(ctx, hwnd, |toolbar| layout(toolbar, width, top))
}

fn invalidate(ctx: &ApiContext, hwnd: u32) {
    let mut shared = ctx.shared.borrow_mut();
    user::invalidate(&mut shared.gui, hwnd, None, true, false);
    shared.gui.dirty = true;
}

fn notify_parent(ctx: &ApiContext, hwnd: u32) -> (u32, u32) {
    let shared = ctx.shared.borrow();
    let window = shared.gui.window(hwnd);
    let parent = shared.gui.toolbars.get(&hwnd).map(|t| t.parent).filter(|p| *p != 0);
    (parent.or(window.map(|w| w.parent)).unwrap_or(0), window.map(|w| w.id).unwrap_or(0))
}

/// Toolbars size themselves to the top of their parent.
fn auto_size<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let style = style_of(ctx, hwnd);
    if style & (CCS_NORESIZE | CCS_NOPARENTALIGN) != 0 {
        invalidate(ctx, hwnd);
        return;
    }
    let parent = ctx.shared.borrow().gui.window(hwnd).map(|w| w.parent).unwrap_or(0);
    let width = client(ctx, parent).0;
    let rects = {
        let top = toolbar_top(ctx, hwnd);
        with_toolbar(ctx, hwnd, |toolbar| layout(toolbar, width, top))
    };
    let button_height = with_toolbar(ctx, hwnd, |t| t.button_size.1);
    let bottom = rects.iter().map(|r| r.bottom).max().unwrap_or(toolbar_top(ctx, hwnd) + button_height);
    let height = bottom + 2;
    user::set_window_pos(ctx, memory, hwnd, 0, 0, 0, width, height, 0x4 | 0x10);
    invalidate(ctx, hwnd);
}

fn index_of(ctx: &ApiContext, hwnd: u32, command: u32) -> Option<usize> {
    with_toolbar(ctx, hwnd, |toolbar| toolbar.buttons.iter().position(|b| b.command == command))
}

fn paint_toolbar<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let ps = user::scratch(ctx, 80);
    let hdc = user::begin_paint(ctx, memory, hwnd, ps);
    let toolbar = with_toolbar(ctx, hwnd, |t| t.clone());
    let rects = rects(ctx, hwnd);
    let (width, height) = client(ctx, hwnd);
    let divider = style_of(ctx, hwnd) & CCS_NODIVIDER == 0;
    {
        let mut shared = ctx.shared.borrow_mut();
        user::with_painter(&mut shared, hwnd, true, |p| {
            p.fill(Rect::new(0, 0, width, height), sys_color(15));
            if divider {
                p.hline(0, width, 0, sys_color(16));
                p.hline(0, width, 1, sys_color(20));
            }
            for (button, rect) in toolbar.buttons.iter().zip(&rects) {
                if rect.is_empty() {
                    continue;
                }
                if button.style & BTNS_SEP != 0 {
                    let x = rect.left + rect.width() / 2 - 1;
                    p.vline(x, rect.top + 1, rect.bottom - 1, sys_color(16));
                    p.vline(x + 1, rect.top + 1, rect.bottom - 1, sys_color(20));
                    continue;
                }
                let down = button.state & (TBSTATE_PRESSED | TBSTATE_CHECKED) != 0;
                if button.state & TBSTATE_CHECKED != 0 {
                    p.fill(rect.inflate(-1, -1), 0xE0E0E0);
                }
                if down {
                    p.edge(*rect, sys_color(16), sys_color(16), sys_color(20), sys_color(20));
                }
                if button.style & BTNS_DROPDOWN != 0 && toolbar.ex_style & TBSTYLE_EX_DRAWDDARROWS != 0 {
                    let ax = rect.right - DROPDOWN_ARROW + 3;
                    let ay = rect.top + rect.height() / 2 - 1;
                    for i in 0..3 {
                        p.hline(ax + i, ax + 5 - i, ay + i, if button.state & TBSTATE_ENABLED != 0 { 0 } else { sys_color(16) });
                    }
                }
            }
        });
    }
    // Images through the DC, centred in their buttons.
    for (button, rect) in toolbar.buttons.iter().zip(&rects) {
        if rect.is_empty() || button.style & BTNS_SEP != 0 || button.image < 0 {
            continue;
        }
        let enabled = button.state & TBSTATE_ENABLED != 0;
        let list = if !enabled && toolbar.disabled_list != 0 {
            toolbar.disabled_list
        } else if toolbar.image_list != 0 {
            toolbar.image_list
        } else {
            toolbar.own_list
        };
        let image = ctx.shared.borrow().image_lists.get(&list).and_then(|l| l.images.get(button.image as usize).cloned());
        if let Some(image) = image {
            let arrow = if button.style & BTNS_DROPDOWN != 0 && toolbar.ex_style & TBSTYLE_EX_DRAWDDARROWS != 0 { DROPDOWN_ARROW } else { 0 };
            let shift = i32::from(button.state & (TBSTATE_PRESSED | TBSTATE_CHECKED) != 0);
            let x = rect.left + (rect.width() - arrow - image.width) / 2 + shift;
            let y = rect.top + (rect.height() - image.height) / 2 + shift;
            draw_image(ctx, memory, hdc, x, y, &image, None, !enabled && list != toolbar.disabled_list);
        }
    }
    user::end_paint(ctx, hwnd, hdc);
    user::free_scratch(ctx, ps);
}

fn add_bitmap<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, count: u32, info: u32) -> u32 {
    let (instance, id) = (memory.read_u32(info).unwrap_or(0), memory.read_u32(info + 4).unwrap_or(0));
    let (bw, bh) = with_toolbar(ctx, hwnd, |t| t.bitmap_size);
    let list = with_toolbar(ctx, hwnd, |t| if t.image_list != 0 { t.image_list } else { t.own_list });
    let list = if list != 0 {
        list
    } else {
        let list = new_image_list(ctx, bw, bh);
        with_toolbar(ctx, hwnd, |t| t.own_list = list);
        list
    };
    let first = ctx.shared.borrow().image_lists.get(&list).map(|l| l.images.len()).unwrap_or(0) as u32;
    // HINST_COMMCTRL's standard bitmaps: blank buttons.
    let bitmap = if instance == u32::MAX {
        0
    } else if instance == 0 {
        id
    } else {
        crate::win32::dispatch_with_memory(ctx, memory, "USER32.dll", &ImportName::ByName("LoadBitmapA".into()), &[instance, id], 0)
            .map(|o| o.value)
            .unwrap_or(0)
    };
    let mut images = match gdi::bitmap_pixels(ctx, memory, bitmap) {
        Some(surface) => {
            // Bitmap button images use the top-left grey as see-through.
            let shared = ctx.shared.borrow();
            let template = shared.image_lists.get(&list).cloned().unwrap_or_default();
            split(&template, &surface, None, Some(0xC0C0C0))
        }
        None => Vec::new(),
    };
    images.resize(count as usize, IconImage { width: bw, height: bh, pixels: vec![None; (bw * bh) as usize] });
    if let Some(l) = ctx.shared.borrow_mut().image_lists.get_mut(&list) {
        l.images.extend(images);
    }
    first
}

pub fn toolbar_proc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, msg: u32, wparam: u32, lparam: u32) -> u32 {
    let wide_text = matches!(msg, 0x444 | 0x443 | 0x43F | 0x440 | 0x44D | 0x44B);
    match msg {
        0x0001 => {
            // WM_CREATE: the owner is the parent.
            let parent = ctx.shared.borrow().gui.window(hwnd).map(|w| w.parent).unwrap_or(0);
            with_toolbar(ctx, hwnd, |t| t.parent = parent);
            0
        }
        0x0002 => {
            ctx.shared.borrow_mut().gui.toolbars.remove(&hwnd);
            0
        }
        0x000F => {
            paint_toolbar(ctx, memory, hwnd);
            0
        }
        0x0014 => 1,
        // WM_SIZE from the parent: follow it.
        0x0005 => {
            if style_of(ctx, hwnd) & (CCS_NORESIZE | CCS_NOPARENTALIGN) == 0 {
                let parent = ctx.shared.borrow().gui.window(hwnd).map(|w| w.parent).unwrap_or(0);
                let width = client(ctx, parent).0;
                if client(ctx, hwnd).0 != width {
                    auto_size(ctx, memory, hwnd);
                }
            }
            invalidate(ctx, hwnd);
            0
        }
        // WM_LBUTTONDOWN / DBLCLK: press a button.
        0x0201 | 0x0203 => {
            let (x, y) = ((lparam & 0xFFFF) as i16 as i32, (lparam >> 16) as i16 as i32);
            let rects = rects(ctx, hwnd);
            let hit = rects.iter().position(|r| r.contains(x, y));
            let Some(index) = hit else { return 0 };
            let button = with_toolbar(ctx, hwnd, |t| t.buttons[index].clone());
            if button.style & BTNS_SEP != 0 || button.state & TBSTATE_ENABLED == 0 {
                return 0;
            }
            let arrow_hit = button.style & BTNS_WHOLEDROPDOWN != 0
                || (button.style & BTNS_DROPDOWN != 0
                    && (with_toolbar(ctx, hwnd, |t| t.ex_style & TBSTYLE_EX_DRAWDDARROWS == 0)
                        || x >= rects[index].right - DROPDOWN_ARROW));
            if arrow_hit {
                // TBN_DROPDOWN with an NMTOOLBAR: header, item, TBBUTTON, text, rect.
                let (parent, id) = notify_parent(ctx, hwnd);
                let block = user::scratch(ctx, 80);
                let r = rects[index];
                let mut bytes = Vec::new();
                for v in [hwnd, id, TBN_DROPDOWN, button.command, button.image as u32, button.command] {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                bytes.extend_from_slice(&[button.state, button.style, 0, 0]);
                for v in [button.data, 0, 0, 0] {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                for v in [r.left, r.top, r.right, r.bottom] {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                let _ = memory.write_bytes(block, &bytes);
                with_toolbar(ctx, hwnd, |t| t.buttons[index].state |= TBSTATE_PRESSED);
                invalidate(ctx, hwnd);
                user::send(ctx, memory, parent, WM_NOTIFY, id, block);
                with_toolbar(ctx, hwnd, |t| {
                    if let Some(b) = t.buttons.get_mut(index) {
                        b.state &= !TBSTATE_PRESSED;
                    }
                });
                invalidate(ctx, hwnd);
                user::free_scratch(ctx, block);
                return 0;
            }
            with_toolbar(ctx, hwnd, |t| {
                t.pressed = Some(index);
                t.buttons[index].state |= TBSTATE_PRESSED;
            });
            ctx.shared.borrow_mut().gui.capture = hwnd;
            invalidate(ctx, hwnd);
            0
        }
        // WM_LBUTTONUP: a click when released over the pressed button.
        0x0202 => {
            let (x, y) = ((lparam & 0xFFFF) as i16 as i32, (lparam >> 16) as i16 as i32);
            {
                let mut shared = ctx.shared.borrow_mut();
                if shared.gui.capture == hwnd {
                    shared.gui.capture = 0;
                }
            }
            let Some(index) = with_toolbar(ctx, hwnd, |t| t.pressed.take()) else { return 0 };
            let inside = rects(ctx, hwnd).get(index).is_some_and(|r| r.contains(x, y));
            let command = with_toolbar(ctx, hwnd, |t| {
                let group: Vec<usize> = group_of(t, index);
                let button = &mut t.buttons[index];
                button.state &= !TBSTATE_PRESSED;
                if !inside {
                    return None;
                }
                if button.style & BTNS_CHECK != 0 {
                    if button.style & BTNS_GROUP != 0 {
                        for other in group {
                            t.buttons[other].state &= !TBSTATE_CHECKED;
                        }
                        t.buttons[index].state |= TBSTATE_CHECKED;
                    } else {
                        button.state ^= TBSTATE_CHECKED;
                    }
                }
                Some(t.buttons[index].command)
            });
            invalidate(ctx, hwnd);
            if let Some(command) = command {
                let (parent, _) = notify_parent(ctx, hwnd);
                user::send(ctx, memory, parent, WM_COMMAND, command & 0xFFFF, hwnd);
            }
            0
        }
        // TB_ENABLEBUTTON, TB_CHECKBUTTON, TB_PRESSBUTTON, TB_HIDEBUTTON, TB_INDETERMINATE
        0x401..=0x405 => {
            let Some(index) = index_of(ctx, hwnd, wparam) else { return 0 };
            let bit = [TBSTATE_ENABLED, TBSTATE_CHECKED, TBSTATE_PRESSED, TBSTATE_HIDDEN, 0x10][(msg - 0x401) as usize];
            with_toolbar(ctx, hwnd, |t| {
                let state = &mut t.buttons[index].state;
                if lparam & 0xFFFF != 0 { *state |= bit } else { *state &= !bit }
            });
            if msg == 0x404 {
                auto_size_if_needed(ctx, memory, hwnd);
            }
            invalidate(ctx, hwnd);
            1
        }
        // TB_ISBUTTONENABLED .. TB_ISBUTTONHIGHLIGHTED
        0x409..=0x40E => {
            let Some(index) = index_of(ctx, hwnd, wparam) else { return 0 };
            let bit = [TBSTATE_ENABLED, TBSTATE_CHECKED, TBSTATE_PRESSED, TBSTATE_HIDDEN, 0x10, 0x80][(msg - 0x409) as usize];
            with_toolbar(ctx, hwnd, |t| u32::from(t.buttons[index].state & bit != 0))
        }
        // TB_SETSTATE, TB_GETSTATE
        0x411 => {
            let Some(index) = index_of(ctx, hwnd, wparam) else { return 0 };
            with_toolbar(ctx, hwnd, |t| t.buttons[index].state = lparam as u8);
            invalidate(ctx, hwnd);
            1
        }
        0x412 => index_of(ctx, hwnd, wparam).map(|i| with_toolbar(ctx, hwnd, |t| t.buttons[i].state as u32)).unwrap_or(u32::MAX),
        // TB_ADDBITMAP
        0x413 => add_bitmap(ctx, memory, hwnd, wparam, lparam),
        // TB_ADDBUTTONSA/W, TB_INSERTBUTTONA/W
        0x414 | 0x444 | 0x415 | 0x443 => {
            let strings = with_toolbar(ctx, hwnd, |t| t.strings.clone());
            let wide = msg == 0x444 || msg == 0x443;
            let size = with_toolbar(ctx, hwnd, |_| 20);
            let new: Vec<Button> = if msg == 0x414 || msg == 0x444 {
                (0..wparam.min(1024)).map(|i| read_button(memory, lparam + i * size, wide, &strings)).collect()
            } else {
                vec![read_button(memory, lparam, wide, &strings)]
            };
            with_toolbar(ctx, hwnd, |t| {
                let at = if msg == 0x414 || msg == 0x444 { t.buttons.len() } else { (wparam as usize).min(t.buttons.len()) };
                for (offset, button) in new.into_iter().enumerate() {
                    t.buttons.insert(at + offset, button);
                }
            });
            auto_size_if_needed(ctx, memory, hwnd);
            let _ = wide_text;
            1
        }
        // TB_DELETEBUTTON
        0x416 => {
            let removed = with_toolbar(ctx, hwnd, |t| (wparam as usize) < t.buttons.len() && { t.buttons.remove(wparam as usize); true });
            invalidate(ctx, hwnd);
            u32::from(removed)
        }
        // TB_GETBUTTON
        0x417 => {
            let Some(button) = with_toolbar(ctx, hwnd, |t| t.buttons.get(wparam as usize).cloned()) else { return 0 };
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&(button.image as u32).to_le_bytes());
            bytes.extend_from_slice(&button.command.to_le_bytes());
            bytes.extend_from_slice(&[button.state, button.style, 0, 0]);
            bytes.extend_from_slice(&button.data.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            let _ = memory.write_bytes(lparam, &bytes);
            1
        }
        // TB_BUTTONCOUNT, TB_COMMANDTOINDEX
        0x418 => with_toolbar(ctx, hwnd, |t| t.buttons.len() as u32),
        0x419 => index_of(ctx, hwnd, wparam).map(|i| i as u32).unwrap_or(u32::MAX),
        // TB_ADDSTRINGA/W: a resource string or a list of NUL-separated strings.
        0x41C | 0x44D => {
            let wide = msg == 0x44D;
            let mut added = Vec::new();
            if wparam == 0 && lparam >= 0x10000 {
                let mut va = lparam;
                loop {
                    let text = if wide { unicode::wide_to_ansi(&unicode::read_wstr(memory, va)) } else { read_cstr(memory, va) };
                    if text.is_empty() {
                        break;
                    }
                    va += (text.len() as u32 + 1) * if wide { 2 } else { 1 };
                    added.push(text);
                }
            }
            with_toolbar(ctx, hwnd, |t| {
                let first = t.strings.len() as u32;
                t.strings.extend(added);
                first
            })
        }
        // TB_GETITEMRECT(index), TB_GETRECT(command)
        0x41D | 0x433 => {
            let index = if msg == 0x41D { Some(wparam as usize) } else { index_of(ctx, hwnd, wparam) };
            match index.and_then(|i| rects(ctx, hwnd).get(i).copied()) {
                Some(rect) => {
                    let _ = gdi::write_rect(memory, lparam, rect);
                    1
                }
                None => 0,
            }
        }
        // TB_BUTTONSTRUCTSIZE
        0x41E => 0,
        // TB_SETBUTTONSIZE, TB_SETBITMAPSIZE
        0x41F | 0x420 => {
            let size = ((lparam & 0xFFFF) as i32, (lparam >> 16) as i32);
            with_toolbar(ctx, hwnd, |t| {
                if msg == 0x41F {
                    t.button_size = (size.0.max(1), size.1.max(1));
                } else {
                    t.bitmap_size = size;
                    if t.button_size.0 < size.0 + 7 || t.button_size.1 < size.1 + 6 {
                        t.button_size = (size.0 + 7, size.1 + 7);
                    }
                }
            });
            1
        }
        // TB_AUTOSIZE
        0x421 => {
            auto_size(ctx, memory, hwnd);
            0
        }
        // TB_GETTOOLTIPS, TB_SETTOOLTIPS
        0x423 => 0,
        0x424 => 0,
        // TB_SETPARENT
        0x425 => with_toolbar(ctx, hwnd, |t| std::mem::replace(&mut t.parent, wparam)),
        // TB_SETROWS, TB_GETROWS
        0x427 => 0,
        0x428 => {
            let rects = rects(ctx, hwnd);
            let tops: std::collections::BTreeSet<i32> = rects.iter().filter(|r| !r.is_empty()).map(|r| r.top).collect();
            tops.len().max(1) as u32
        }
        // TB_SETINDENT
        0x42F => {
            with_toolbar(ctx, hwnd, |t| t.indent = wparam as i32);
            1
        }
        // TB_SETIMAGELIST / GET, hot and disabled lists
        0x430 => with_toolbar(ctx, hwnd, |t| std::mem::replace(&mut t.image_list, lparam)),
        0x431 => with_toolbar(ctx, hwnd, |t| t.image_list),
        0x434 => with_toolbar(ctx, hwnd, |t| std::mem::replace(&mut t.hot_list, lparam)),
        0x435 => with_toolbar(ctx, hwnd, |t| t.hot_list),
        0x436 => with_toolbar(ctx, hwnd, |t| std::mem::replace(&mut t.disabled_list, lparam)),
        0x437 => with_toolbar(ctx, hwnd, |t| t.disabled_list),
        // TB_SETSTYLE, TB_GETSTYLE
        0x438 => {
            if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                window.style = (window.style & 0xFFFF_0000) | (lparam & 0xFFFF);
            }
            invalidate(ctx, hwnd);
            0
        }
        0x439 => style_of(ctx, hwnd) & 0xFFFF,
        // TB_GETBUTTONSIZE
        0x43A => with_toolbar(ctx, hwnd, |t| (t.button_size.0 as u32 & 0xFFFF) | ((t.button_size.1 as u32) << 16)),
        // TB_SETBUTTONWIDTH, TB_SETMAXTEXTROWS
        0x43B | 0x43C => 1,
        // TB_GETBUTTONINFOW/A, TB_SETBUTTONINFOW/A
        0x43F | 0x441 | 0x440 | 0x442 => {
            let wide = msg == 0x43F || msg == 0x440;
            let mask = memory.read_u32(lparam + 4).unwrap_or(0);
            let index = if mask & 0x8000_0000 != 0 { Some(wparam as usize) } else { index_of(ctx, hwnd, wparam) };
            let Some(index) = index.filter(|i| with_toolbar(ctx, hwnd, |t| *i < t.buttons.len())) else {
                return u32::MAX;
            };
            if msg == 0x43F || msg == 0x441 {
                let button = with_toolbar(ctx, hwnd, |t| t.buttons[index].clone());
                if mask & 0x20 != 0 {
                    let _ = memory.write_u32(lparam + 8, button.command);
                }
                if mask & 0x1 != 0 {
                    let _ = memory.write_u32(lparam + 12, button.image as u32);
                }
                if mask & 0x4 != 0 {
                    let _ = memory.write_bytes(lparam + 16, &[button.state]);
                }
                if mask & 0x8 != 0 {
                    let _ = memory.write_bytes(lparam + 17, &[button.style]);
                }
                if mask & 0x40 != 0 {
                    let width = with_toolbar(ctx, hwnd, |t| button_width(t, &button)) as u16;
                    let _ = memory.write_bytes(lparam + 18, &width.to_le_bytes());
                }
                if mask & 0x10 != 0 {
                    let _ = memory.write_u32(lparam + 20, button.data);
                }
                if mask & 0x2 != 0 {
                    let (va, capacity) = (memory.read_u32(lparam + 24).unwrap_or(0), memory.read_u32(lparam + 28).unwrap_or(0));
                    if va != 0 && capacity > 0 {
                        let count = button.text.len().min(capacity as usize - 1);
                        if wide {
                            unicode::write_wide(memory, va, &unicode::ansi_to_wide(&button.text[..count]), true);
                        } else {
                            let _ = memory.write_bytes(va, &[&button.text[..count], b"\0"].concat());
                        }
                    }
                }
                index as u32
            } else {
                let command = memory.read_u32(lparam + 8).unwrap_or(0);
                let image = memory.read_u32(lparam + 12).unwrap_or(0) as i32;
                let flags = memory.read_bytes(lparam + 16, 2).unwrap_or_else(|| vec![0, 0]);
                let data = memory.read_u32(lparam + 20).unwrap_or(0);
                let text_va = memory.read_u32(lparam + 24).unwrap_or(0);
                let text = (mask & 0x2 != 0 && text_va >= 0x10000).then(|| {
                    if wide { unicode::wide_to_ansi(&unicode::read_wstr(memory, text_va)) } else { read_cstr(memory, text_va) }
                });
                with_toolbar(ctx, hwnd, |t| {
                    let button = &mut t.buttons[index];
                    if mask & 0x20 != 0 {
                        button.command = command;
                    }
                    if mask & 0x1 != 0 {
                        button.image = image;
                    }
                    if mask & 0x4 != 0 {
                        button.state = flags[0];
                    }
                    if mask & 0x8 != 0 {
                        button.style = flags[1];
                    }
                    if mask & 0x10 != 0 {
                        button.data = data;
                    }
                    if let Some(text) = text {
                        button.text = text;
                    }
                });
                invalidate(ctx, hwnd);
                1
            }
        }
        // TB_GETHOTITEM, TB_SETHOTITEM
        0x447 => u32::MAX,
        0x448 => u32::MAX,
        // TB_GETMAXSIZE
        0x453 => {
            let rects = rects(ctx, hwnd);
            let right = rects.iter().map(|r| r.right).max().unwrap_or(0);
            let bottom = rects.iter().map(|r| r.bottom).max().unwrap_or(0);
            let _ = memory.write_u32(lparam, right as u32);
            let _ = memory.write_u32(lparam + 4, bottom as u32);
            1
        }
        // TB_SETEXTENDEDSTYLE, TB_GETEXTENDEDSTYLE
        0x454 => {
            let old = with_toolbar(ctx, hwnd, |t| std::mem::replace(&mut t.ex_style, lparam));
            invalidate(ctx, hwnd);
            old
        }
        0x455 => with_toolbar(ctx, hwnd, |t| t.ex_style),
        // TB_SETPADDING, TB_GETPADDING
        0x457 | 0x456 => 0x0006_0007,
        // CCM_SETVERSION, CCM_GETVERSION
        0x2007 | 0x2008 => 5,
        0x400..=0x4FF => 0,
        _ => user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

/// The checkable buttons in the same group as `index`.
fn group_of(toolbar: &Toolbar, index: usize) -> Vec<usize> {
    let in_group = |b: &Button| b.style & BTNS_GROUP != 0 && b.style & BTNS_CHECK != 0;
    let mut start = index;
    while start > 0 && in_group(&toolbar.buttons[start - 1]) {
        start -= 1;
    }
    let mut end = index;
    while end + 1 < toolbar.buttons.len() && in_group(&toolbar.buttons[end + 1]) {
        end += 1;
    }
    (start..=end).collect()
}

fn auto_size_if_needed<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let height = client(ctx, hwnd).1;
    if height == 0 {
        auto_size(ctx, memory, hwnd);
    } else {
        invalidate(ctx, hwnd);
    }
}

// ── Status bar ──────────────────────────────────────────────────────────

const SBARS_SIZEGRIP: u32 = 0x100;
const SBT_NOBORDERS: u32 = 0x100;
const SBT_OWNERDRAW: u32 = 0x1000;
const STATUS_HEIGHT: i32 = 20;

#[derive(Clone, Debug, Default)]
pub struct StatusBar {
    /// Right edges of the parts (-1: to the end).
    parts: Vec<i32>,
    texts: HashMap<u32, (Vec<u8>, u32)>,
    simple: bool,
    simple_text: Vec<u8>,
}

fn with_status<R>(ctx: &ApiContext, hwnd: u32, f: impl FnOnce(&mut StatusBar) -> R) -> R {
    let mut shared = ctx.shared.borrow_mut();
    f(shared.gui.status_bars.entry(hwnd).or_default())
}

fn status_rects(ctx: &ApiContext, hwnd: u32) -> Vec<Rect> {
    let (width, height) = client(ctx, hwnd);
    let grip = if style_of(ctx, hwnd) & SBARS_SIZEGRIP != 0 { 14 } else { 0 };
    with_status(ctx, hwnd, |status| {
        if status.simple || status.parts.is_empty() {
            return vec![Rect::new(0, 2, width - grip, height)];
        }
        let mut left = 0;
        status
            .parts
            .iter()
            .map(|&right| {
                let right = if right < 0 { width - grip } else { right.min(width) };
                let rect = Rect::new(left, 2, right, height);
                left = right + 2;
                rect
            })
            .collect()
    })
}

/// Keep the status bar along the bottom of its parent.
fn place_status<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let parent = ctx.shared.borrow().gui.window(hwnd).map(|w| w.parent).unwrap_or(0);
    let (width, height) = client(ctx, parent);
    if style_of(ctx, hwnd) & (CCS_NORESIZE | CCS_NOPARENTALIGN) == 0 {
        user::set_window_pos(ctx, memory, hwnd, 0, 0, height - STATUS_HEIGHT, width, STATUS_HEIGHT, 0x4 | 0x10);
    }
    invalidate(ctx, hwnd);
}

fn paint_status<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let ps = user::scratch(ctx, 80);
    let hdc = user::begin_paint(ctx, memory, hwnd, ps);
    let rects = status_rects(ctx, hwnd);
    let status = with_status(ctx, hwnd, |s| s.clone());
    let (width, height) = client(ctx, hwnd);
    let grip = style_of(ctx, hwnd) & SBARS_SIZEGRIP != 0;
    let mut shared = ctx.shared.borrow_mut();
    user::with_painter(&mut shared, hwnd, true, |p| {
        p.fill(Rect::new(0, 0, width, height), sys_color(15));
        for (index, rect) in rects.iter().enumerate() {
            let (text, flags) = if status.simple {
                (status.simple_text.clone(), 0)
            } else {
                status.texts.get(&(index as u32)).cloned().unwrap_or_default()
            };
            if flags & SBT_NOBORDERS == 0 {
                p.hline(rect.left, rect.right, rect.top, sys_color(16));
                p.vline(rect.left, rect.top, rect.bottom, sys_color(16));
                p.hline(rect.left, rect.right, rect.bottom - 1, sys_color(20));
                p.vline(rect.right - 1, rect.top, rect.bottom, sys_color(20));
            }
            if flags & SBT_OWNERDRAW != 0 {
                continue;
            }
            // A tab centres the text after it, two tabs right-align it.
            let tabs = text.iter().take_while(|&&b| b == b'\t').count();
            let shown: Vec<u8> = text[tabs..].iter().map(|&b| if b == b'\t' { b' ' } else { b }).collect();
            let text_width = user::text_width(&shown);
            let x = match tabs {
                0 => rect.left + 4,
                1 => rect.left + (rect.width() - text_width) / 2,
                _ => rect.right - 4 - text_width,
            };
            let saved = p.clip.clone();
            p.clip = p.clip.intersect_rect(&rect.inflate(-1, -1).offset(p.ox, p.oy));
            p.text(x, rect.top + 2, &shown, sys_color(18), false);
            p.clip = saved;
        }
        if grip {
            for i in 0..3 {
                let offset = 3 + i * 4;
                for d in 0..2 {
                    let colour = if d == 0 { sys_color(20) } else { sys_color(16) };
                    let o = offset + d;
                    for k in 0..o {
                        p.pixel(width - 2 - o + k, height - 2 - k, colour);
                    }
                }
            }
        }
    });
    drop(shared);
    user::end_paint(ctx, hwnd, hdc);
    user::free_scratch(ctx, ps);
}

pub fn status_proc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, msg: u32, wparam: u32, lparam: u32) -> u32 {
    let read_text = |memory: &mut M, va: u32, wide: bool| -> Vec<u8> {
        if va < 0x10000 {
            Vec::new()
        } else if wide {
            unicode::wide_to_ansi(&unicode::read_wstr(memory, va))
        } else {
            read_cstr(memory, va)
        }
    };
    match msg {
        0x0001 => {
            place_status(ctx, memory, hwnd);
            0
        }
        0x0002 => {
            ctx.shared.borrow_mut().gui.status_bars.remove(&hwnd);
            0
        }
        // WM_SIZE (the parent forwards its own): follow the parent.
        0x0005 => {
            place_status(ctx, memory, hwnd);
            0
        }
        0x000F => {
            paint_status(ctx, memory, hwnd);
            0
        }
        0x0014 => 1,
        // WM_SETTEXT sets part 0.
        0x000C => {
            let text = read_cstr(memory, lparam);
            with_status(ctx, hwnd, |s| s.texts.insert(0, (text, 0)));
            invalidate(ctx, hwnd);
            1
        }
        // SB_SETTEXTA/W
        0x401 | 0x40B => {
            let text = read_text(memory, lparam, msg == 0x40B);
            let part = wparam & 0xFF;
            with_status(ctx, hwnd, |s| {
                if part == 0xFF || s.simple {
                    s.simple_text = text;
                } else {
                    s.texts.insert(part, (text, wparam & 0xFF00 | wparam & 0x1000));
                }
            });
            invalidate(ctx, hwnd);
            1
        }
        // SB_GETTEXTA/W: length in the low word, flags in the high word.
        0x402 | 0x40D => {
            let (text, flags) = with_status(ctx, hwnd, |s| s.texts.get(&(wparam & 0xFF)).cloned().unwrap_or_default());
            if lparam != 0 {
                if msg == 0x40D {
                    unicode::write_wide(memory, lparam, &unicode::ansi_to_wide(&text), true);
                } else {
                    let _ = memory.write_bytes(lparam, &[&text[..], b"\0"].concat());
                }
            }
            text.len() as u32 | (flags << 16)
        }
        // SB_GETTEXTLENGTHA/W
        0x403 | 0x40C => {
            let (text, flags) = with_status(ctx, hwnd, |s| s.texts.get(&(wparam & 0xFF)).cloned().unwrap_or_default());
            text.len() as u32 | (flags << 16)
        }
        // SB_SETPARTS(count, edges)
        0x404 => {
            let count = wparam.min(256);
            let parts: Vec<i32> = (0..count).map(|i| memory.read_u32(lparam + i * 4).unwrap_or(0) as i32).collect();
            with_status(ctx, hwnd, |s| s.parts = parts);
            invalidate(ctx, hwnd);
            1
        }
        // SB_GETPARTS(count, edges)
        0x406 => {
            let parts = with_status(ctx, hwnd, |s| s.parts.clone());
            if lparam != 0 {
                for (i, edge) in parts.iter().take(wparam as usize).enumerate() {
                    let _ = memory.write_u32(lparam + i as u32 * 4, *edge as u32);
                }
            }
            parts.len().max(1) as u32
        }
        // SB_GETBORDERS
        0x407 => {
            for (i, v) in [0u32, 2, 2].iter().enumerate() {
                let _ = memory.write_u32(lparam + i as u32 * 4, *v);
            }
            1
        }
        // SB_SETMINHEIGHT
        0x408 => 1,
        // SB_SIMPLE
        0x409 => {
            with_status(ctx, hwnd, |s| s.simple = wparam != 0);
            invalidate(ctx, hwnd);
            1
        }
        // SB_GETRECT
        0x40A => match status_rects(ctx, hwnd).get(wparam as usize) {
            Some(rect) => {
                let _ = gdi::write_rect(memory, lparam, *rect);
                1
            }
            None => 0,
        },
        // SB_ISSIMPLE
        0x40E => with_status(ctx, hwnd, |s| u32::from(s.simple)),
        // SB_SETICON, SB_SETTIPTEXT, SB_GETICON...
        0x400..=0x4FF => 0,
        // CCM_SETBKCOLOR
        0x2001 => 0xFF00_0000,
        0x2007 | 0x2008 => 5,
        _ => user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

// ── Functions ───────────────────────────────────────────────────────────

/// CreateWindowExA through the USER32 dispatcher, with an ANSI class name.
fn create_window<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    class: &str,
    text: &[u8],
    style: u32,
    parent: u32,
    id: u32,
) -> u32 {
    let strings = user::scratch(ctx, (class.len() + text.len() + 2) as u32);
    let _ = memory.write_bytes(strings, &[class.as_bytes(), b"\0", text, b"\0"].concat());
    let text_va = strings + class.len() as u32 + 1;
    let instance = ctx.image_base;
    let args = [0, strings, text_va, style, 0, 0, 0, 0, parent, id, instance, 0];
    let hwnd = crate::win32::dispatch_with_memory(ctx, memory, "USER32.dll", &ImportName::ByName("CreateWindowExA".into()), &args, 0)
        .map(|o| o.value)
        .unwrap_or(0);
    user::free_scratch(ctx, strings);
    hwnd
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    match name {
        "InitCommonControls" => ok(0),
        "OTW_SubclassProc" => ok(subclass_proc(ctx, memory, args[0], args[1], args[2], args[3])),
        "OTW_ToolbarProc" => ok(toolbar_proc(ctx, memory, args[0], args[1], args[2], args[3])),
        "OTW_StatusProc" => ok(status_proc(ctx, memory, args[0], args[1], args[2], args[3])),
        "SetWindowSubclass" => ok(set_subclass(ctx, args[0], args[1], args[2], args[3])),
        "RemoveWindowSubclass" => ok(remove_subclass(ctx, args[0], args[1], args[2])),
        "GetWindowSubclass" => {
            let data = ctx
                .shared
                .borrow()
                .gui
                .subclasses
                .get(&args[0])
                .and_then(|s| s.chain.iter().find(|(p, i, _)| *p == args[1] && *i == args[2]).map(|e| e.2));
            if args[3] != 0 {
                let _ = memory.write_u32(args[3], data.unwrap_or(0));
            }
            ok(u32::from(data.is_some()))
        }
        "DefSubclassProc" => ok(def_subclass_proc(ctx, memory, args[0], args[1], args[2], args[3])),
        "ImageList_Create" => ok(new_image_list(ctx, args[0] as i32, args[1] as i32)),
        "ImageList_Destroy" => ok(u32::from(ctx.shared.borrow_mut().image_lists.remove(&args[0]).is_some())),
        "ImageList_GetImageCount" => ok(ctx.shared.borrow().image_lists.get(&args[0]).map(|l| l.images.len() as u32).unwrap_or(0)),
        "ImageList_SetImageCount" => {
            let mut shared = ctx.shared.borrow_mut();
            let Some(list) = shared.image_lists.get_mut(&args[0]) else { return ok(0) };
            let blank = IconImage { width: list.width, height: list.height, pixels: vec![None; (list.width * list.height).max(0) as usize] };
            list.images.resize(args[1] as usize, blank);
            ok(1)
        }
        "ImageList_Remove" => {
            let mut shared = ctx.shared.borrow_mut();
            let Some(list) = shared.image_lists.get_mut(&args[0]) else { return ok(0) };
            if args[1] as i32 == -1 {
                list.images.clear();
            } else if (args[1] as usize) < list.images.len() {
                list.images.remove(args[1] as usize);
            }
            ok(1)
        }
        "ImageList_GetIconSize" => {
            let size = ctx.shared.borrow().image_lists.get(&args[0]).map(|l| (l.width, l.height));
            let Some((w, h)) = size else { return ok(0) };
            let _ = memory.write_u32(args[1], w as u32);
            let _ = memory.write_u32(args[2], h as u32);
            ok(1)
        }
        "ImageList_SetIconSize" => {
            let mut shared = ctx.shared.borrow_mut();
            let Some(list) = shared.image_lists.get_mut(&args[0]) else { return ok(0) };
            list.width = args[1] as i32;
            list.height = args[2] as i32;
            list.images.clear();
            ok(1)
        }
        "ImageList_GetBkColor" => ok(u32::MAX),
        "ImageList_SetBkColor" => ok(u32::MAX),
        "ImageList_Add" | "ImageList_AddMasked" | "ImageList_Replace" => {
            // Add(list, colour, mask), AddMasked(list, colour, key colour),
            // Replace(list, index, colour, mask)
            let (handle, colour_bitmap, extra) = if name == "ImageList_Replace" { (args[0], args[2], args[3]) } else { (args[0], args[1], args[2]) };
            let Some(template) = ctx.shared.borrow().image_lists.get(&handle).cloned() else { return ok(u32::MAX) };
            let Some(colour) = gdi::bitmap_pixels(ctx, memory, colour_bitmap) else { return ok(u32::MAX) };
            let images = if name == "ImageList_AddMasked" {
                split(&template, &colour, None, Some(extra & 0xFF_FFFF))
            } else {
                let mask = if extra != 0 { gdi::bitmap_pixels(ctx, memory, extra) } else { None };
                split(&template, &colour, mask.as_ref(), None)
            };
            let mut shared = ctx.shared.borrow_mut();
            let list = shared.image_lists.get_mut(&handle).expect("checked above");
            if name == "ImageList_Replace" {
                let index = args[1] as usize;
                match (images.into_iter().next(), list.images.get_mut(index)) {
                    (Some(image), Some(slot)) => {
                        *slot = image;
                        ok(1)
                    }
                    _ => ok(0),
                }
            } else {
                let first = list.images.len() as u32;
                list.images.extend(images);
                ok(first)
            }
        }
        "ImageList_ReplaceIcon" => {
            // ReplaceIcon(list, index or -1 to add, icon)
            let Some(width) = ctx.shared.borrow().image_lists.get(&args[0]).map(|l| l.width) else { return ok(u32::MAX) };
            let Some(image) = icon_image(ctx, args[2], width) else { return ok(u32::MAX) };
            let mut shared = ctx.shared.borrow_mut();
            let list = shared.image_lists.get_mut(&args[0]).expect("checked above");
            let index = args[1] as i32;
            if index < 0 || index as usize >= list.images.len() {
                list.images.push(image);
                ok(list.images.len() as u32 - 1)
            } else {
                list.images[index as usize] = image;
                ok(index as u32)
            }
        }
        "ImageList_GetIcon" => {
            // An icon handle made from the image.
            let image = ctx.shared.borrow().image_lists.get(&args[0]).and_then(|l| l.images.get(args[1] as usize).cloned());
            let Some(image) = image else { return ok(0) };
            let mut shared = ctx.shared.borrow_mut();
            let handle = shared.alloc_handle() | 0x0060_0000;
            shared.gui.icons.insert(handle, crate::win32::icon::Icon { large: image.clone(), small: image });
            ok(handle)
        }
        "ImageList_GetImageInfo" => {
            // IMAGEINFO: no separate bitmaps; the rectangle is the image.
            let size = ctx.shared.borrow().image_lists.get(&args[0]).map(|l| (l.width, l.height));
            let Some((w, h)) = size else { return ok(0) };
            let _ = memory.write_bytes(args[2], &[0u8; 16]);
            let _ = gdi::write_rect(memory, args[2] + 16, Rect::new(0, 0, w, h));
            ok(1)
        }
        "ImageList_Draw" | "ImageList_DrawEx" => {
            // Draw(list, index, dc, x, y, style), DrawEx(list, index, dc, x, y, dx, dy, bk, fg, style)
            let image = ctx.shared.borrow().image_lists.get(&args[0]).and_then(|l| l.images.get(args[1] as usize).cloned());
            let Some(image) = image else { return ok(0) };
            let size = if name == "ImageList_DrawEx" { Some((args[5] as i32, args[6] as i32)) } else { None };
            draw_image(ctx, memory, args[2], args[3] as i32, args[4] as i32, &image, size, false);
            ok(1)
        }
        "DrawIcon" => {
            if let Some(image) = icon_image(ctx, args[3], 32) {
                draw_image(ctx, memory, args[0], args[1] as i32, args[2] as i32, &image, None, false);
            }
            ok(1)
        }
        "DrawIconEx" => {
            // DrawIconEx(dc, x, y, icon, cx, cy, step, background brush, flags)
            let (cx, cy) = (args[4] as i32, args[5] as i32);
            let want = if cx > 0 { cx } else if args[8] & 0x10 != 0 { 32 } else { 32 };
            if let Some(image) = icon_image(ctx, args[3], want) {
                draw_image(ctx, memory, args[0], args[1] as i32, args[2] as i32, &image, Some((cx, cy)), false);
            }
            ok(1)
        }
        "GetIconInfo" => {
            // ICONINFO: fIcon, hotspot, mask and colour bitmaps.
            let Some(image) = icon_image(ctx, args[0], 0) else { return ok(0) };
            let mut colour = Surface::new(image.width, image.height, 24);
            let mut mask = Surface::new(image.width, image.height, 1);
            for y in 0..image.height {
                for x in 0..image.width {
                    match image.pixels[(y * image.width + x) as usize] {
                        Some(value) => colour.put(x, y, value),
                        None => mask.put(x, y, 0xFF_FFFF),
                    }
                }
            }
            let mask = gdi::insert_bitmap(ctx, mask);
            let colour = gdi::insert_bitmap(ctx, colour);
            let info = [1, (image.width / 2) as u32, (image.height / 2) as u32, mask, colour];
            let bytes: Vec<u8> = info.iter().flat_map(|v| v.to_le_bytes()).collect();
            let _ = memory.write_bytes(args[1], &bytes);
            ok(1)
        }
        "CreateStatusWindowW" | "CreateStatusWindowA" => {
            // CreateStatusWindow(style, text, parent, id)
            let text = if args[1] < 0x10000 {
                Vec::new()
            } else if name.ends_with('W') {
                unicode::wide_to_ansi(&unicode::read_wstr(memory, args[1]))
            } else {
                read_cstr(memory, args[1])
            };
            let hwnd = create_window(ctx, memory, "msctls_statusbar32", &[], args[0], args[2], args[3]);
            if hwnd != 0 && !text.is_empty() {
                with_status(ctx, hwnd, |s| s.texts.insert(0, (text, 0)));
            }
            ok(hwnd)
        }
        "CreateToolbarEx" => {
            // CreateToolbarEx(parent, style, id, bitmaps, bitmap instance,
            // bitmap id, buttons, button count, button cx, cy, bitmap cx, cy, struct size)
            let hwnd = create_window(ctx, memory, "ToolbarWindow32", &[], args[1], args[0], args[2]);
            if hwnd == 0 {
                return ok(0);
            }
            if args[10] != 0 || args[11] != 0 {
                toolbar_proc(ctx, memory, hwnd, 0x420, 0, (args[10] & 0xFFFF) | (args[11] << 16));
            }
            if args[8] != 0 || args[9] != 0 {
                toolbar_proc(ctx, memory, hwnd, 0x41F, 0, (args[8] & 0xFFFF) | (args[9] << 16));
            }
            if args[3] != 0 {
                let block = user::scratch(ctx, 8);
                let _ = memory.write_u32(block, args[4]);
                let _ = memory.write_u32(block + 4, args[5]);
                toolbar_proc(ctx, memory, hwnd, 0x413, args[3], block);
                user::free_scratch(ctx, block);
            }
            if args[6] != 0 && args[7] != 0 {
                toolbar_proc(ctx, memory, hwnd, 0x414, args[7], args[6]);
            }
            auto_size(ctx, memory, hwnd);
            ok(hwnd)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmaps_split_into_images() {
        let list = ImageList { width: 2, height: 1, images: Vec::new() };
        let mut strip = Surface::new(4, 1, 24);
        strip.pixels = vec![0xC0C0C0, 0x112233, 0x445566, 0xC0C0C0];
        let images = split(&list, &strip, None, Some(0xC0C0C0));
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].pixels, vec![None, Some(0x112233)]);
        assert_eq!(images[1].pixels, vec![Some(0x445566), None]);
    }

    #[test]
    fn toolbar_buttons_wrap_and_separators_are_narrow() {
        let mut toolbar = Toolbar::default();
        for style in [0, BTNS_SEP, 0, 0] {
            toolbar.buttons.push(Button { style, state: TBSTATE_ENABLED, ..Default::default() });
        }
        let rects = layout(&toolbar, 60, 2);
        assert_eq!(rects[0], Rect::new(0, 2, 23, 24));
        assert_eq!(rects[1], Rect::new(23, 2, 31, 24));
        assert_eq!(rects[2], Rect::new(31, 2, 54, 24));
        // The fourth does not fit and starts a second row.
        assert_eq!(rects[3], Rect::new(0, 24, 23, 46));
    }
}
