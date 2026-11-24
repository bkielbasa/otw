//! USER32 menus: menu objects, MENU resources, the menu bar and popup menu
//! tracking loop, and keyboard accelerators.

use crate::{
    win32::gdi::sys_color,
    win32::gfx::Rect,
    win32::user::{
        self, ensure_surface, frame_width, has_menu_bar, make_long, send, text_width, with_painter,
        Painter, ResName, CAPTION_HEIGHT, MENU_HEIGHT, WM_COMMAND, WS_CAPTION, WS_POPUP,
    },
    win32::{
        read_cstr, write_cstr, ApiContext, ApiOutcome, GuestMemory, GuestMenu, GuestWindow,
        MenuItem, RawInput,
    },
};

const MF_GRAYED: u32 = 0x1;
const MF_DISABLED: u32 = 0x2;
const MF_BITMAP: u32 = 0x4;
const MF_CHECKED: u32 = 0x8;
const MF_POPUP: u32 = 0x10;
const MF_OWNERDRAW: u32 = 0x100;
const MF_BYPOSITION: u32 = 0x400;
const MF_SEPARATOR: u32 = 0x800;
const MF_HILITE: u32 = 0x80;

const WM_INITMENU: u32 = 0x0116;
const WM_INITMENUPOPUP: u32 = 0x0117;
const WM_MENUSELECT: u32 = 0x011F;
const WM_ENTERMENULOOP: u32 = 0x0211;
const WM_EXITMENULOOP: u32 = 0x0212;

const ITEM_HEIGHT: i32 = 18;
const SEPARATOR_HEIGHT: i32 = 9;

fn new_menu(ctx: &ApiContext, popup: bool) -> u32 {
    let mut shared = ctx.shared.borrow_mut();
    let handle = shared.gui.next_menu;
    shared.gui.next_menu += 4;
    shared.gui.menus.insert(
        handle,
        GuestMenu {
            items: Vec::new(),
            popup,
        },
    );
    handle
}

/// Parse a standard MENUITEMTEMPLATE resource into menu objects.
pub fn load_menu_template<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    va: u32,
) -> u32 {
    let version = memory
        .read_bytes(va, 4)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0);
    let header = memory
        .read_bytes(va + 2, 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0) as u32;
    if version != 0 {
        // MENUEX templates are not used by Win95 applications of this era.
        return new_menu(ctx, false);
    }
    let mut cursor = va + 4 + header;
    let root = new_menu(ctx, false);
    parse_items(ctx, memory, &mut cursor, root, 0);
    root
}

fn read_wide<M: GuestMemory + ?Sized>(memory: &mut M, cursor: &mut u32) -> Vec<u8> {
    let mut text = Vec::new();
    for _ in 0..512 {
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
    text
}

fn parse_items<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    cursor: &mut u32,
    menu: u32,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    for _ in 0..256 {
        let flags = memory
            .read_bytes(*cursor, 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .unwrap_or(0x80) as u32;
        *cursor += 2;
        let mut item = MenuItem {
            flags: flags & !0x80,
            ..MenuItem::default()
        };
        if flags & MF_POPUP == 0 {
            item.id = memory
                .read_bytes(*cursor, 2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .unwrap_or(0) as u32;
            *cursor += 2;
        }
        item.text = read_wide(memory, cursor);
        if flags & MF_POPUP == 0 && item.id == 0 && item.text.is_empty() {
            item.flags |= MF_SEPARATOR;
        }
        if flags & MF_POPUP != 0 {
            let sub = new_menu(ctx, true);
            parse_items(ctx, memory, cursor, sub, depth + 1);
            item.submenu = sub;
        }
        ctx.shared
            .borrow_mut()
            .gui
            .menus
            .get_mut(&menu)
            .map(|m| m.items.push(item));
        if flags & 0x80 != 0 {
            break;
        }
    }
}

pub fn load_menu<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    instance: u32,
    name_va: u32,
) -> u32 {
    let name = user::res_name(memory, name_va);
    match user::find_resource_any(ctx, memory, instance, &ResName::Id(4), &name) {
        Some((va, _)) => load_menu_template(ctx, memory, va),
        None => 0,
    }
}

/// Locate an item by command id (searching submenus) or by position.
/// Returns (menu handle, index).
fn locate(ctx: &ApiContext, menu: u32, item: u32, flags: u32) -> Option<(u32, usize)> {
    let shared = ctx.shared.borrow();
    if flags & MF_BYPOSITION != 0 {
        let m = shared.gui.menus.get(&menu)?;
        return ((item as usize) < m.items.len()).then_some((menu, item as usize));
    }
    fn search(
        gui: &crate::win32::GuiState,
        menu: u32,
        id: u32,
        depth: usize,
    ) -> Option<(u32, usize)> {
        if depth > 8 {
            return None;
        }
        let m = gui.menus.get(&menu)?;
        for (index, item) in m.items.iter().enumerate() {
            if item.flags & MF_POPUP == 0 && item.id == id && item.flags & MF_SEPARATOR == 0 {
                return Some((menu, index));
            }
        }
        for item in &m.items {
            if item.submenu != 0 {
                if let Some(found) = search(gui, item.submenu, id, depth + 1) {
                    return Some(found);
                }
            }
        }
        // A popup item can also be addressed by its submenu handle.
        for (index, item) in m.items.iter().enumerate() {
            if item.submenu == id && id != 0 {
                return Some((menu, index));
            }
        }
        None
    }
    search(&shared.gui, menu, item, 0)
}

/// Show a window's window menu (the system menu) and carry out the chosen
/// command. Items are enabled for the window's state, as in Windows: Restore
/// only when minimized or maximized, Move and Size only when restored, and
/// so on. It opens below the caption icon, or at `at` (screen coordinates).
pub fn track_system_menu<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    at: Option<(i32, i32)>,
) {
    let menu = sys_menu(ctx, hwnd);
    let (x, y) = {
        let mut shared = ctx.shared.borrow_mut();
        let Some(window) = shared.gui.window(hwnd).cloned() else {
            return;
        };
        let sizable = window.style & user::WS_THICKFRAME != 0;
        let restored = !window.maximized && !window.minimized;
        let enabled = |id: u32| match id {
            0xF120 => !restored,
            0xF010 => restored,
            0xF000 => restored && sizable,
            0xF020 => window.style & user::WS_MINIMIZEBOX != 0 && !window.minimized,
            0xF030 => window.style & user::WS_MAXIMIZEBOX != 0 && !window.maximized,
            _ => true,
        };
        if let Some(menu) = shared.gui.menus.get_mut(&menu) {
            for item in &mut menu.items {
                if item.id != 0 {
                    item.flags = if enabled(item.id) {
                        item.flags & !MF_GRAYED
                    } else {
                        item.flags | MF_GRAYED
                    };
                }
            }
        }
        let frame = frame_width(window.style, window.ex_style);
        let (wx, wy) = user::window_screen_origin(&shared.gui, hwnd);
        at.unwrap_or((wx + frame, wy + frame + CAPTION_HEIGHT))
    };
    // TPM_RETURNCMD | TPM_NONOTIFY: the program sees only WM_SYSCOMMAND.
    let command = track_popup(ctx, memory, menu, 0x100 | 0x80, x, y, hwnd);
    if command != 0 {
        send(ctx, memory, hwnd, user::WM_SYSCOMMAND, command, 0);
    }
}

fn sys_menu(ctx: &ApiContext, hwnd: u32) -> u32 {
    let existing = {
        let shared = ctx.shared.borrow();
        shared.gui.window(hwnd).and_then(|w| {
            w.props
                .iter()
                .find(|(k, _)| k == "otw_sysmenu")
                .map(|(_, v)| *v)
        })
    };
    if let Some(menu) = existing {
        return menu;
    }
    let menu = new_menu(ctx, true);
    let items: [(&[u8], u32); 7] = [
        (b"&Restore", 0xF120),
        (b"&Move", 0xF010),
        (b"&Size", 0xF000),
        (b"Mi&nimize", 0xF020),
        (b"Ma&ximize", 0xF030),
        (b"", 0),
        (b"&Close\tAlt+F4", 0xF060),
    ];
    {
        let mut shared = ctx.shared.borrow_mut();
        let m = shared.gui.menus.get_mut(&menu).unwrap();
        for (text, id) in items {
            m.items.push(MenuItem {
                flags: if id == 0 { MF_SEPARATOR } else { 0 },
                id,
                text: text.to_vec(),
                submenu: 0,
            });
        }
        if let Some(window) = shared.gui.window_mut(hwnd) {
            window.props.push(("otw_sysmenu".into(), menu));
        }
    }
    menu
}

// ── Layout and rendering ─────────────────────────────────────────────────

/// Screen rectangles of the menu bar items of `hwnd`.
pub fn bar_rects(gui: &crate::win32::GuiState, hwnd: u32) -> Vec<Rect> {
    let Some(window) = gui.window(hwnd) else {
        return Vec::new();
    };
    if !has_menu_bar(window) {
        return Vec::new();
    }
    let Some(menu) = gui.menus.get(&window.menu) else {
        return Vec::new();
    };
    let (wx, wy) = user::window_screen_origin(gui, hwnd);
    let frame = frame_width(window.style, window.ex_style);
    let mut y = frame;
    if window.style & WS_CAPTION == WS_CAPTION {
        y += CAPTION_HEIGHT;
    }
    let mut x = frame;
    menu.items
        .iter()
        .map(|item| {
            let label = item.text.split(|b| *b == b'\t').next().unwrap_or(&[]);
            let width = text_width(label) + 12;
            let rect = Rect::new(wx + x, wy + y + 1, wx + x + width, wy + y + MENU_HEIGHT - 1);
            x += width;
            rect
        })
        .collect()
}

struct PopupLayout {
    width: i32,
    height: i32,
    rows: Vec<Rect>,
    accel_x: i32,
}

fn popup_layout(menu: &GuestMenu) -> PopupLayout {
    let mut label_width = 0;
    let mut accel_width = 0;
    for item in &menu.items {
        let mut parts = item.text.splitn(2, |b| *b == b'\t');
        let label = parts.next().unwrap_or(&[]);
        label_width = label_width.max(text_width(label));
        if let Some(accel) = parts.next() {
            accel_width = accel_width.max(text_width(accel));
        }
    }
    let gap = if accel_width > 0 { 24 } else { 0 };
    let width = 3 + 20 + label_width + gap + accel_width + 20 + 3;
    let mut y = 3;
    let mut rows = Vec::with_capacity(menu.items.len());
    for item in &menu.items {
        let h = if item.flags & MF_SEPARATOR != 0 {
            SEPARATOR_HEIGHT
        } else {
            ITEM_HEIGHT
        };
        rows.push(Rect::new(3, y, width - 3, y + h));
        y += h;
    }
    PopupLayout {
        width: width.max(80),
        height: y + 3,
        rows,
        accel_x: 3 + 20 + label_width + gap,
    }
}

fn draw_popup(p: &mut Painter<'_>, menu: &GuestMenu, layout: &PopupLayout, hot: i32) {
    let full = Rect::new(0, 0, layout.width, layout.height);
    p.fill(full, sys_color(4));
    p.raised(full);
    for (index, (item, row)) in menu.items.iter().zip(&layout.rows).enumerate() {
        if item.flags & MF_SEPARATOR != 0 {
            let y = row.top + row.height() / 2 - 1;
            p.hline(row.left + 1, row.right - 1, y, sys_color(16));
            p.hline(row.left + 1, row.right - 1, y + 1, sys_color(20));
            continue;
        }
        let disabled = item.flags & (MF_GRAYED | MF_DISABLED) != 0;
        let highlighted = index as i32 == hot;
        let text_color = if highlighted {
            sys_color(14)
        } else {
            sys_color(7)
        };
        if highlighted {
            p.fill(*row, sys_color(13));
        }
        let mut parts = item.text.splitn(2, |b| *b == b'\t');
        let label = parts.next().unwrap_or(&[]).to_vec();
        let accel = parts.next().map(|a| a.to_vec());
        let ty = row.top + 2;
        if item.flags & MF_CHECKED != 0 {
            let cx = row.left + 6;
            let cy = row.top + 5;
            let color = if disabled { sys_color(16) } else { text_color };
            for i in 0..3 {
                p.vline(cx + i, cy + 2 + i, cy + 5 + i, color);
            }
            for i in 0..4 {
                p.vline(cx + 3 + i, cy + 3 - i, cy + 6 - i, color);
            }
        }
        if disabled && !highlighted {
            p.disabled_text(row.left + 17, ty, &label);
            if let Some(accel) = &accel {
                p.disabled_text(layout.accel_x - 3, ty, accel);
            }
        } else {
            let color = if disabled { sys_color(16) } else { text_color };
            p.mnemonic_text(row.left + 17, ty, &label, color);
            if let Some(accel) = &accel {
                p.mnemonic_text(layout.accel_x - 3, ty, accel, color);
            }
        }
        if item.flags & MF_POPUP != 0 {
            let ax = row.right - 10;
            let ay = row.top + row.height() / 2;
            for i in 0..4 {
                p.vline(ax + i, ay - 3 + i, ay + 4 - i, text_color);
            }
        }
    }
}

struct OpenPopup {
    menu: u32,
    hwnd: u32,
    origin: (i32, i32),
    layout: PopupLayout,
    hot: i32,
}

struct Tracker {
    owner: u32,
    bar_hwnd: u32,
    bar_menu: u32,
    bar_hot: i32,
    stack: Vec<OpenPopup>,
    notify: bool,
}

fn redraw_popup(ctx: &ApiContext, popup: &OpenPopup) {
    let menu = ctx
        .shared
        .borrow()
        .gui
        .menus
        .get(&popup.menu)
        .cloned()
        .unwrap_or_default();
    let mut shared = ctx.shared.borrow_mut();
    with_painter(&mut shared, popup.hwnd, false, |p| {
        draw_popup(p, &menu, &popup.layout, popup.hot)
    });
}

fn set_bar_hot(ctx: &ApiContext, tracker: &mut Tracker, index: i32) {
    tracker.bar_hot = index;
    if tracker.bar_hwnd == 0 {
        return;
    }
    let mut shared = ctx.shared.borrow_mut();
    shared.gui.menu_bar_hot = if index >= 0 {
        Some((tracker.bar_hwnd, index))
    } else {
        None
    };
    user::paint_nc(&mut shared, tracker.bar_hwnd);
}

fn menu_select<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    tracker: &Tracker,
    menu: u32,
    index: i32,
) {
    if !tracker.notify {
        return;
    }
    let item = ctx
        .shared
        .borrow()
        .gui
        .menus
        .get(&menu)
        .and_then(|m| m.items.get(index.max(0) as usize).cloned());
    match item {
        Some(item) if index >= 0 => {
            let id = if item.flags & MF_POPUP != 0 {
                index as u32
            } else {
                item.id
            };
            let flags = (item.flags & 0xFFFF) | MF_HILITE;
            send(
                ctx,
                memory,
                tracker.owner,
                WM_MENUSELECT,
                make_long(id as i32, flags as i32),
                menu,
            );
        }
        _ => {
            send(
                ctx,
                memory,
                tracker.owner,
                WM_MENUSELECT,
                make_long(0, 0),
                menu,
            );
        }
    }
}

fn open_popup<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    tracker: &mut Tracker,
    menu: u32,
    x: i32,
    y: i32,
    index: i32,
    avoid: Option<Rect>,
) {
    if tracker.notify {
        send(
            ctx,
            memory,
            tracker.owner,
            WM_INITMENUPOPUP,
            menu,
            make_long(index, 0),
        );
    }
    let menu_record = ctx
        .shared
        .borrow()
        .gui
        .menus
        .get(&menu)
        .cloned()
        .unwrap_or_default();
    let layout = popup_layout(&menu_record);
    let (sw, sh) = ctx.shared.borrow().gui.screen;
    let mut px = x;
    let mut py = y;
    if px + layout.width > sw {
        px = match avoid {
            Some(rect) if rect.left - layout.width >= 0 && tracker.stack.len() >= 1 => {
                rect.left - layout.width + 3
            }
            _ => (sw - layout.width).max(0),
        };
    }
    if py + layout.height > sh {
        py = match avoid {
            Some(rect) if rect.top - layout.height >= 0 && tracker.stack.is_empty() => {
                rect.top - layout.height
            }
            _ => (sh - layout.height).max(0),
        };
    }
    let hwnd = {
        let mut shared = ctx.shared.borrow_mut();
        let hwnd = shared.gui.next_hwnd;
        shared.gui.next_hwnd += 2;
        shared.gui.windows.push(GuestWindow {
            hwnd,
            class: "#32768".into(),
            x: px,
            y: py,
            width: layout.width,
            height: layout.height,
            style: WS_POPUP,
            ex_style: 0x8,
            owner: tracker.owner,
            visible: true,
            wndproc: ctx.builtin_proc("OTW_MenuProc"),
            ..GuestWindow::default()
        });
        ensure_surface(&mut shared, hwnd);
        hwnd
    };
    let popup = OpenPopup {
        menu,
        hwnd,
        origin: (px, py),
        layout,
        hot: -1,
    };
    redraw_popup(ctx, &popup);
    tracker.stack.push(popup);
}

fn close_to(ctx: &ApiContext, tracker: &mut Tracker, depth: usize) {
    while tracker.stack.len() > depth {
        let popup = tracker.stack.pop().unwrap();
        let mut shared = ctx.shared.borrow_mut();
        shared.gui.windows.retain(|w| w.hwnd != popup.hwnd);
        shared.gdi.surfaces.remove(&popup.hwnd);
        shared.gui.dirty = true;
    }
}

fn open_bar_item<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    tracker: &mut Tracker,
    index: i32,
) {
    close_to(ctx, tracker, 0);
    set_bar_hot(ctx, tracker, index);
    let rects = bar_rects(&ctx.shared.borrow().gui, tracker.bar_hwnd);
    let item = ctx
        .shared
        .borrow()
        .gui
        .menus
        .get(&tracker.bar_menu)
        .and_then(|m| m.items.get(index as usize).cloned());
    menu_select(ctx, memory, tracker, tracker.bar_menu, index);
    if let (Some(item), Some(rect)) = (item, rects.get(index as usize)) {
        if item.submenu != 0 && item.flags & (MF_GRAYED | MF_DISABLED) == 0 {
            open_popup(
                ctx,
                memory,
                tracker,
                item.submenu,
                rect.left,
                rect.bottom + 1,
                index,
                Some(*rect),
            );
        }
    }
}

/// Item index of the popup at depth `level` under a screen point.
fn popup_hit(tracker: &Tracker, level: usize, x: i32, y: i32) -> Option<i32> {
    let popup = tracker.stack.get(level)?;
    let (lx, ly) = (x - popup.origin.0, y - popup.origin.1);
    if !Rect::new(0, 0, popup.layout.width, popup.layout.height).contains(lx, ly) {
        return None;
    }
    Some(
        popup
            .layout
            .rows
            .iter()
            .position(|r| r.contains(lx, ly))
            .map(|p| p as i32)
            .unwrap_or(-1),
    )
}

fn set_popup_hot<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    tracker: &mut Tracker,
    level: usize,
    index: i32,
) {
    if tracker.stack.get(level).map(|p| p.hot) == Some(index) {
        return;
    }
    close_to(ctx, tracker, level + 1);
    let (menu, row) = {
        let popup = &mut tracker.stack[level];
        popup.hot = index;
        (
            popup.menu,
            popup.layout.rows.get(index.max(0) as usize).copied(),
        )
    };
    redraw_popup(ctx, &tracker.stack[level]);
    menu_select(ctx, memory, tracker, menu, index);
    let item = ctx
        .shared
        .borrow()
        .gui
        .menus
        .get(&menu)
        .and_then(|m| m.items.get(index.max(0) as usize).cloned());
    if let (Some(item), Some(row)) = (item, row) {
        if index >= 0 && item.submenu != 0 && item.flags & (MF_GRAYED | MF_DISABLED) == 0 {
            let origin = tracker.stack[level].origin;
            let width = tracker.stack[level].layout.width;
            let avoid = Rect::new(
                origin.0,
                origin.1 + row.top,
                origin.0 + width,
                origin.1 + row.bottom,
            );
            open_popup(
                ctx,
                memory,
                tracker,
                item.submenu,
                origin.0 + width - 3,
                origin.1 + row.top - 3,
                index,
                Some(avoid),
            );
        }
    }
}

fn selectable(ctx: &ApiContext, menu: u32, index: i32) -> Option<MenuItem> {
    let item = ctx
        .shared
        .borrow()
        .gui
        .menus
        .get(&menu)?
        .items
        .get(index.max(0) as usize)
        .cloned()?;
    (index >= 0 && item.flags & (MF_SEPARATOR | MF_GRAYED | MF_DISABLED) == 0).then_some(item)
}

fn step_hot(ctx: &ApiContext, menu: u32, current: i32, delta: i32) -> i32 {
    let items = ctx
        .shared
        .borrow()
        .gui
        .menus
        .get(&menu)
        .map(|m| m.items.clone())
        .unwrap_or_default();
    if items.is_empty() {
        return -1;
    }
    let n = items.len() as i32;
    let mut index = current;
    for _ in 0..n {
        index = if index < 0 && delta < 0 {
            n - 1
        } else {
            (index + delta).rem_euclid(n)
        };
        if items[index as usize].flags & MF_SEPARATOR == 0 {
            return index;
        }
    }
    current
}

fn mnemonic_index(ctx: &ApiContext, menu: u32, ch: u8) -> Option<i32> {
    let items = ctx
        .shared
        .borrow()
        .gui
        .menus
        .get(&menu)
        .map(|m| m.items.clone())
        .unwrap_or_default();
    let ch = ch.to_ascii_lowercase();
    items
        .iter()
        .position(|item| {
            let text = &item.text;
            text.windows(2)
                .any(|w| w[0] == b'&' && w[1].to_ascii_lowercase() == ch)
        })
        .map(|p| p as i32)
}

/// Run the modal menu loop. Returns the chosen command id, or 0.
fn run_loop<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    tracker: &mut Tracker,
    mut button_down: bool,
) -> u32 {
    let mut result = 0u32;
    if !user::interactive(ctx, memory) {
        close_to(ctx, tracker, 0);
        return 0;
    }
    loop {
        let Some(event) = user::next_raw_input(ctx, memory) else {
            break;
        };
        if std::env::var_os("OTW_TRACE_MSG").is_some() {
            eprintln!("[menu] event {event:?}");
        }
        match event {
            RawInput::Move { x, y } => {
                let mut handled = false;
                for level in (0..tracker.stack.len()).rev() {
                    if let Some(index) = popup_hit(tracker, level, x, y) {
                        if index >= 0 || level + 1 == tracker.stack.len() {
                            set_popup_hot(ctx, memory, tracker, level, index);
                        }
                        handled = true;
                        break;
                    }
                }
                if !handled && tracker.bar_hwnd != 0 && (button_down || !tracker.stack.is_empty()) {
                    let rects = bar_rects(&ctx.shared.borrow().gui, tracker.bar_hwnd);
                    if let Some(index) = rects.iter().position(|r| r.contains(x, y)) {
                        if index as i32 != tracker.bar_hot {
                            open_bar_item(ctx, memory, tracker, index as i32);
                        }
                    }
                }
            }
            RawInput::Button {
                x, y, down: true, ..
            } => {
                button_down = true;
                let in_popup =
                    (0..tracker.stack.len()).any(|level| popup_hit(tracker, level, x, y).is_some());
                if in_popup {
                    continue;
                }
                if tracker.bar_hwnd != 0 {
                    let rects = bar_rects(&ctx.shared.borrow().gui, tracker.bar_hwnd);
                    if let Some(index) = rects.iter().position(|r| r.contains(x, y)) {
                        if index as i32 == tracker.bar_hot && !tracker.stack.is_empty() {
                            break;
                        }
                        open_bar_item(ctx, memory, tracker, index as i32);
                        continue;
                    }
                }
                break;
            }
            RawInput::Button {
                x, y, down: false, ..
            } => {
                button_down = false;
                let mut chosen = None;
                for level in (0..tracker.stack.len()).rev() {
                    if let Some(index) = popup_hit(tracker, level, x, y) {
                        let menu = tracker.stack[level].menu;
                        if let Some(item) = selectable(ctx, menu, index) {
                            if item.flags & MF_POPUP == 0 {
                                chosen = Some(item.id);
                            }
                        }
                        break;
                    }
                }
                if let Some(id) = chosen {
                    result = id;
                    break;
                }
            }
            RawInput::Key { vk, down: true, ch } => {
                let depth = tracker.stack.len();
                match vk {
                    0x1B => {
                        if depth > 1 || (depth == 1 && tracker.bar_hwnd == 0) {
                            close_to(ctx, tracker, depth - 1);
                            if tracker.stack.is_empty() {
                                break;
                            }
                        } else if depth == 1 {
                            close_to(ctx, tracker, 0);
                        } else {
                            break;
                        }
                    }
                    0x12 | 0x79 => break,
                    0x26 | 0x28 => {
                        if depth == 0 {
                            if tracker.bar_hot >= 0 {
                                let index = tracker.bar_hot;
                                open_bar_item(ctx, memory, tracker, index);
                                if !tracker.stack.is_empty() {
                                    let menu = tracker.stack[0].menu;
                                    let hot = step_hot(ctx, menu, -1, 1);
                                    set_popup_hot(ctx, memory, tracker, 0, hot);
                                }
                            }
                        } else {
                            let level = depth - 1;
                            let menu = tracker.stack[level].menu;
                            let hot = step_hot(
                                ctx,
                                menu,
                                tracker.stack[level].hot,
                                if vk == 0x26 { -1 } else { 1 },
                            );
                            set_popup_hot(ctx, memory, tracker, level, hot);
                        }
                    }
                    0x25 | 0x27 => {
                        let level = depth.saturating_sub(1);
                        let submenu_open = vk == 0x27 && depth > 0 && {
                            let popup = &tracker.stack[level];
                            selectable(ctx, popup.menu, popup.hot)
                                .map(|i| i.flags & MF_POPUP != 0)
                                .unwrap_or(false)
                        };
                        if submenu_open {
                            let menu = tracker.stack[depth - 1 + 0].menu;
                            let _ = menu;
                            if tracker.stack.len() > depth {
                                let sub = tracker.stack[depth].menu;
                                let hot = step_hot(ctx, sub, -1, 1);
                                set_popup_hot(ctx, memory, tracker, depth, hot);
                            }
                        } else if vk == 0x25 && depth > 1 {
                            close_to(ctx, tracker, depth - 1);
                        } else if tracker.bar_hwnd != 0 {
                            let count = ctx
                                .shared
                                .borrow()
                                .gui
                                .menus
                                .get(&tracker.bar_menu)
                                .map(|m| m.items.len() as i32)
                                .unwrap_or(0);
                            if count > 0 {
                                let next = (tracker.bar_hot + if vk == 0x25 { -1 } else { 1 })
                                    .rem_euclid(count);
                                let reopen = depth > 0;
                                if reopen {
                                    open_bar_item(ctx, memory, tracker, next);
                                    if !tracker.stack.is_empty() {
                                        let menu = tracker.stack[0].menu;
                                        let hot = step_hot(ctx, menu, -1, 1);
                                        set_popup_hot(ctx, memory, tracker, 0, hot);
                                    }
                                } else {
                                    set_bar_hot(ctx, tracker, next);
                                }
                            }
                        }
                    }
                    0x0D => {
                        if depth == 0 {
                            if tracker.bar_hot >= 0 {
                                let index = tracker.bar_hot;
                                open_bar_item(ctx, memory, tracker, index);
                            }
                        } else {
                            let popup = &tracker.stack[depth - 1];
                            if let Some(item) = selectable(ctx, popup.menu, popup.hot) {
                                if item.flags & MF_POPUP == 0 {
                                    result = item.id;
                                    break;
                                }
                            }
                        }
                    }
                    _ => {
                        if let Some(ch) = ch.and_then(|c| u8::try_from(c).ok()) {
                            if depth == 0 {
                                if let Some(index) = mnemonic_index(ctx, tracker.bar_menu, ch) {
                                    open_bar_item(ctx, memory, tracker, index);
                                    if !tracker.stack.is_empty() {
                                        let menu = tracker.stack[0].menu;
                                        let hot = step_hot(ctx, menu, -1, 1);
                                        set_popup_hot(ctx, memory, tracker, 0, hot);
                                    }
                                }
                            } else {
                                let menu = tracker.stack[depth - 1].menu;
                                if let Some(index) = mnemonic_index(ctx, menu, ch) {
                                    set_popup_hot(ctx, memory, tracker, depth - 1, index);
                                    if let Some(item) = selectable(ctx, menu, index) {
                                        if item.flags & MF_POPUP == 0 {
                                            result = item.id;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    close_to(ctx, tracker, 0);
    set_bar_hot(ctx, tracker, -1);
    if tracker.notify {
        send(ctx, memory, tracker.owner, WM_MENUSELECT, 0xFFFF_0000, 0);
        send(ctx, memory, tracker.owner, WM_EXITMENULOOP, 0, 0);
    }
    result
}

/// DefWindowProc handling of a click on the menu bar.
pub fn track_menu_bar<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    x: i32,
    y: i32,
) {
    let menu = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|w| w.menu)
        .unwrap_or(0);
    if menu == 0 {
        return;
    }
    let rects = bar_rects(&ctx.shared.borrow().gui, hwnd);
    let Some(index) = rects.iter().position(|r| r.contains(x, y)) else {
        return;
    };
    send(ctx, memory, hwnd, WM_ENTERMENULOOP, 0, 0);
    send(ctx, memory, hwnd, WM_INITMENU, menu, 0);
    let mut tracker = Tracker {
        owner: hwnd,
        bar_hwnd: hwnd,
        bar_menu: menu,
        bar_hot: -1,
        stack: Vec::new(),
        notify: true,
    };
    open_bar_item(ctx, memory, &mut tracker, index as i32);
    let command = run_loop(ctx, memory, &mut tracker, true);
    if command != 0 {
        let command_message = if command >= 0xF000 {
            user::WM_SYSCOMMAND
        } else {
            WM_COMMAND
        };
        user::post(ctx, hwnd, command_message, command, 0);
    }
}

/// Keyboard activation of the menu bar (Alt, F10 or Alt+letter).
pub fn key_menu<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, ch: u32) {
    let menu = ctx
        .shared
        .borrow()
        .gui
        .window(hwnd)
        .filter(|w| has_menu_bar(w))
        .map(|w| w.menu)
        .unwrap_or(0);
    if menu == 0 {
        return;
    }
    let start = if ch != 0 {
        match mnemonic_index(ctx, menu, ch as u8) {
            Some(index) => Some(index),
            None => return,
        }
    } else {
        None
    };
    send(ctx, memory, hwnd, WM_ENTERMENULOOP, 0, 0);
    send(ctx, memory, hwnd, WM_INITMENU, menu, 0);
    let mut tracker = Tracker {
        owner: hwnd,
        bar_hwnd: hwnd,
        bar_menu: menu,
        bar_hot: -1,
        stack: Vec::new(),
        notify: true,
    };
    match start {
        Some(index) => {
            open_bar_item(ctx, memory, &mut tracker, index);
            if !tracker.stack.is_empty() {
                let menu = tracker.stack[0].menu;
                let hot = step_hot(ctx, menu, -1, 1);
                set_popup_hot(ctx, memory, &mut tracker, 0, hot);
            }
        }
        None => set_bar_hot(ctx, &mut tracker, 0),
    }
    let command = run_loop(ctx, memory, &mut tracker, false);
    if command != 0 {
        let command_message = if command >= 0xF000 {
            user::WM_SYSCOMMAND
        } else {
            WM_COMMAND
        };
        user::post(ctx, hwnd, command_message, command, 0);
    }
}

fn track_popup<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    menu: u32,
    flags: u32,
    x: i32,
    y: i32,
    owner: u32,
) -> u32 {
    let notify = flags & 0x80 == 0;
    let mut tracker = Tracker {
        owner,
        bar_hwnd: 0,
        bar_menu: 0,
        bar_hot: -1,
        stack: Vec::new(),
        notify,
    };
    if notify {
        send(ctx, memory, owner, WM_ENTERMENULOOP, 1, 0);
        send(ctx, memory, owner, WM_INITMENU, menu, 0);
    }
    let layout = {
        let shared = ctx.shared.borrow();
        popup_layout(&shared.gui.menus.get(&menu).cloned().unwrap_or_default())
    };
    let px = match flags & 0xC {
        4 => x - layout.width / 2,
        8 => x - layout.width,
        _ => x,
    };
    open_popup(ctx, memory, &mut tracker, menu, px, y, 0, None);
    let right_drag = ctx.shared.borrow().gui.keys[0x02] & 0x80 != 0;
    let command = run_loop(ctx, memory, &mut tracker, right_drag);
    if flags & 0x100 != 0 {
        return command;
    }
    if command != 0 {
        user::post(ctx, owner, WM_COMMAND, command, 0);
    }
    (command != 0) as u32
}

// ── Accelerators ─────────────────────────────────────────────────────────

fn load_accelerators<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    instance: u32,
    name_va: u32,
) -> u32 {
    let name = user::res_name(memory, name_va);
    let Some((va, size)) = user::find_resource_any(ctx, memory, instance, &ResName::Id(9), &name)
    else {
        return 0;
    };
    let bytes = memory.read_bytes(va, size as usize).unwrap_or_default();
    let mut entries = Vec::new();
    for chunk in bytes.chunks_exact(8) {
        let flags = u16::from_le_bytes([chunk[0], chunk[1]]) as u32;
        let key = u16::from_le_bytes([chunk[2], chunk[3]]) as u32;
        let cmd = u16::from_le_bytes([chunk[4], chunk[5]]) as u32;
        entries.push((flags & 0x7F, key, cmd));
        if flags & 0x80 != 0 {
            break;
        }
    }
    let mut shared = ctx.shared.borrow_mut();
    let handle = shared.alloc_handle() | 0x0060_0000;
    shared.gui.accelerators.insert(handle, entries);
    handle
}

fn translate_accelerator<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hwnd: u32,
    table: u32,
    msg_va: u32,
) -> u32 {
    let message = user::read_message(memory, msg_va);
    let entries = ctx
        .shared
        .borrow()
        .gui
        .accelerators
        .get(&table)
        .cloned()
        .unwrap_or_default();
    let (shift, control, alt) = {
        let shared = ctx.shared.borrow();
        (
            shared.gui.keys[0x10] & 0x80 != 0,
            shared.gui.keys[0x11] & 0x80 != 0,
            shared.gui.keys[0x12] & 0x80 != 0,
        )
    };
    for (flags, key, cmd) in entries {
        let virt = flags & 0x1 != 0;
        let matched = match message.message {
            user::WM_KEYDOWN | user::WM_SYSKEYDOWN if virt => {
                key == message.wparam
                    && (flags & 0x4 != 0) == shift
                    && (flags & 0x8 != 0) == control
                    && (flags & 0x10 != 0) == alt
            }
            user::WM_CHAR | user::WM_SYSCHAR if !virt => {
                key == message.wparam
                    && (flags & 0x10 != 0) == (message.message == user::WM_SYSCHAR)
            }
            _ => false,
        };
        if matched {
            let menu = ctx
                .shared
                .borrow()
                .gui
                .window(hwnd)
                .map(|w| w.menu)
                .unwrap_or(0);
            if menu != 0 {
                send(ctx, memory, hwnd, WM_INITMENU, menu, 0);
            }
            send(ctx, memory, hwnd, WM_COMMAND, make_long(cmd as i32, 1), 0);
            return 1;
        }
    }
    0
}

// ── API surface ──────────────────────────────────────────────────────────

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "CreateMenu" | "CreatePopupMenu" | "GetMenuCheckMarkDimensions" => 0,
        "DestroyMenu" | "GetMenu" | "GetMenuItemCount" | "DrawMenuBar" | "IsMenu"
        | "LoadMenuIndirectA" => 1,
        "SetMenu"
        | "GetSubMenu"
        | "GetMenuItemID"
        | "GetSystemMenu"
        | "LoadMenuA"
        | "LoadAcceleratorsA"
        | "DestroyAcceleratorTable" => {
            if name == "DestroyAcceleratorTable" {
                1
            } else {
                2
            }
        }
        "CheckMenuItem"
        | "EnableMenuItem"
        | "GetMenuState"
        | "RemoveMenu"
        | "DeleteMenu"
        | "TranslateAcceleratorA"
        | "CopyAcceleratorTableA"
        | "HiliteMenuItem" => {
            if name == "HiliteMenuItem" {
                4
            } else {
                3
            }
        }
        "AppendMenuA" => 4,
        "InsertMenuA" | "ModifyMenuA" | "GetMenuStringA" | "SetMenuItemBitmaps"
        | "CheckMenuRadioItem" => 5,
        "TrackPopupMenu" => 7,
        "TrackPopupMenuEx" => 6,
        _ => return None,
    })
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
    count: usize,
) -> Option<ApiOutcome> {
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    match name {
        "CreateMenu" => ok(new_menu(ctx, false)),
        "CreatePopupMenu" => ok(new_menu(ctx, true)),
        "LoadMenuA" => ok(load_menu(ctx, memory, args[0], args[1])),
        "LoadMenuIndirectA" => ok(load_menu_template(ctx, memory, args[0])),
        "DestroyMenu" => {
            let mut shared = ctx.shared.borrow_mut();
            fn remove(gui: &mut crate::win32::GuiState, menu: u32, depth: usize) {
                if depth > 8 {
                    return;
                }
                if let Some(record) = gui.menus.remove(&menu) {
                    for item in record.items {
                        if item.submenu != 0 {
                            remove(gui, item.submenu, depth + 1);
                        }
                    }
                }
            }
            let existed = shared.gui.menus.contains_key(&args[0]);
            remove(&mut shared.gui, args[0], 0);
            ok(existed as u32)
        }
        "IsMenu" => ok(ctx.shared.borrow().gui.menus.contains_key(&args[0]) as u32),
        "GetMenu" => ok(ctx
            .shared
            .borrow()
            .gui
            .window(args[0])
            .map(|w| w.menu)
            .unwrap_or(0)),
        "SetMenu" => {
            {
                let mut shared = ctx.shared.borrow_mut();
                let Some(window) = shared.gui.window_mut(args[0]) else {
                    return ok(0);
                };
                window.menu = args[1];
            }
            // The client area changed: same as SWP_FRAMECHANGED.
            user::set_window_pos(
                ctx,
                memory,
                args[0],
                0,
                0,
                0,
                0,
                0,
                0x1 | 0x2 | 0x4 | 0x10 | 0x20,
            );
            ok(1)
        }
        "DrawMenuBar" => {
            let mut shared = ctx.shared.borrow_mut();
            if let Some(window) = shared.gui.window_mut(args[0]) {
                window.nc_dirty = true;
            }
            shared.gui.dirty = true;
            ok(1)
        }
        "GetSystemMenu" => {
            if args[1] != 0 {
                return ok(0);
            }
            ok(sys_menu(ctx, args[0]))
        }
        "GetSubMenu" => {
            let shared = ctx.shared.borrow();
            ok(shared
                .gui
                .menus
                .get(&args[0])
                .and_then(|m| m.items.get(args[1] as usize))
                .map(|i| i.submenu)
                .unwrap_or(0))
        }
        "GetMenuItemCount" => ok(ctx
            .shared
            .borrow()
            .gui
            .menus
            .get(&args[0])
            .map(|m| m.items.len() as u32)
            .unwrap_or(u32::MAX)),
        "GetMenuItemID" => {
            let shared = ctx.shared.borrow();
            ok(shared
                .gui
                .menus
                .get(&args[0])
                .and_then(|m| m.items.get(args[1] as usize))
                .map(|i| {
                    if i.flags & MF_POPUP != 0 {
                        u32::MAX
                    } else {
                        i.id
                    }
                })
                .unwrap_or(u32::MAX))
        }
        "GetMenuState" => {
            let Some((menu, index)) = locate(ctx, args[0], args[1], args[2]) else {
                return ok(u32::MAX);
            };
            let shared = ctx.shared.borrow();
            let item = &shared.gui.menus[&menu].items[index];
            if item.flags & MF_POPUP != 0 {
                let count = shared
                    .gui
                    .menus
                    .get(&item.submenu)
                    .map(|m| m.items.len() as u32)
                    .unwrap_or(0);
                ok((count << 8) | (item.flags & 0xFF))
            } else {
                ok(item.flags & 0xFFFF)
            }
        }
        "GetMenuStringA" => {
            let Some((menu, index)) = locate(ctx, args[0], args[1], args[4]) else {
                return ok(0);
            };
            let text = ctx.shared.borrow().gui.menus[&menu].items[index]
                .text
                .clone();
            if args[2] == 0 {
                return ok(text.len() as u32);
            }
            let mut text = text;
            text.truncate(args[3].saturating_sub(1) as usize);
            let _ = write_cstr(memory, args[2], &text);
            ok(text.len() as u32)
        }
        "CheckMenuItem" | "EnableMenuItem" => {
            let Some((menu, index)) = locate(ctx, args[0], args[1], args[2]) else {
                return ok(u32::MAX);
            };
            let mut shared = ctx.shared.borrow_mut();
            let item = &mut shared.gui.menus.get_mut(&menu).unwrap().items[index];
            let previous;
            if name == "CheckMenuItem" {
                previous = item.flags & MF_CHECKED;
                item.flags = (item.flags & !MF_CHECKED) | (args[2] & MF_CHECKED);
            } else {
                previous = item.flags & (MF_GRAYED | MF_DISABLED);
                item.flags = (item.flags & !(MF_GRAYED | MF_DISABLED))
                    | (args[2] & (MF_GRAYED | MF_DISABLED));
            }
            // A changed menu bar item needs its frame redrawn.
            let owner = shared
                .gui
                .windows
                .iter()
                .find(|w| w.menu == menu)
                .map(|w| w.hwnd);
            if let Some(owner) = owner {
                if let Some(w) = shared.gui.window_mut(owner) {
                    w.nc_dirty = true;
                }
            }
            ok(previous)
        }
        "CheckMenuRadioItem" => {
            let (first, last, check, flags) = (args[1], args[2], args[3], args[4]);
            let mut shared = ctx.shared.borrow_mut();
            let Some(menu) = shared.gui.menus.get_mut(&args[0]) else {
                return ok(0);
            };
            for (index, item) in menu.items.iter_mut().enumerate() {
                let key = if flags & MF_BYPOSITION != 0 {
                    index as u32
                } else {
                    item.id
                };
                if key >= first && key <= last {
                    if key == check {
                        item.flags |= MF_CHECKED;
                    } else {
                        item.flags &= !MF_CHECKED;
                    }
                }
            }
            ok(1)
        }
        "HiliteMenuItem" => ok(1),
        "AppendMenuA" | "InsertMenuA" | "ModifyMenuA" => {
            let (menu, position, flags, id, data) = if name == "AppendMenuA" {
                (args[0], u32::MAX, args[1] | MF_BYPOSITION, args[2], args[3])
            } else {
                (args[0], args[1], args[2], args[3], args[4])
            };
            let text = if flags & (MF_BITMAP | MF_OWNERDRAW | MF_SEPARATOR) == 0 && data != 0 {
                read_cstr(memory, data)
            } else {
                Vec::new()
            };
            let item = MenuItem {
                flags: flags & !(MF_BYPOSITION) & 0xFFFF,
                id: if flags & MF_POPUP != 0 { 0 } else { id },
                text,
                submenu: if flags & MF_POPUP != 0 { id } else { 0 },
            };
            if name == "ModifyMenuA" {
                let Some((target, index)) = locate(ctx, menu, position, flags) else {
                    return ok(0);
                };
                ctx.shared
                    .borrow_mut()
                    .gui
                    .menus
                    .get_mut(&target)
                    .unwrap()
                    .items[index] = item;
                return ok(1);
            }
            let location = if position == u32::MAX {
                None
            } else {
                locate(ctx, menu, position, flags)
            };
            let mut shared = ctx.shared.borrow_mut();
            let (target, index) = match location {
                Some(found) => found,
                None => (
                    menu,
                    shared
                        .gui
                        .menus
                        .get(&menu)
                        .map(|m| m.items.len())
                        .unwrap_or(0),
                ),
            };
            let Some(record) = shared.gui.menus.get_mut(&target) else {
                return ok(0);
            };
            record.items.insert(index.min(record.items.len()), item);
            ok(1)
        }
        "RemoveMenu" | "DeleteMenu" => {
            let Some((menu, index)) = locate(ctx, args[0], args[1], args[2]) else {
                return ok(0);
            };
            let mut shared = ctx.shared.borrow_mut();
            let item = shared.gui.menus.get_mut(&menu).unwrap().items.remove(index);
            if name == "DeleteMenu" && item.submenu != 0 {
                shared.gui.menus.remove(&item.submenu);
            }
            ok(1)
        }
        "SetMenuItemBitmaps" => ok(1),
        "GetMenuCheckMarkDimensions" => ok(make_long(13, 13)),
        "TrackPopupMenu" | "TrackPopupMenuEx" => {
            let (menu, flags, x, y, owner) = if name == "TrackPopupMenu" {
                (args[0], args[1], args[2] as i32, args[3] as i32, args[5])
            } else {
                (args[0], args[1], args[2] as i32, args[3] as i32, args[4])
            };
            ok(track_popup(ctx, memory, menu, flags, x, y, owner))
        }
        "LoadAcceleratorsA" => ok(load_accelerators(ctx, memory, args[0], args[1])),
        "DestroyAcceleratorTable" => {
            ctx.shared.borrow_mut().gui.accelerators.remove(&args[0]);
            ok(1)
        }
        "TranslateAcceleratorA" => ok(translate_accelerator(
            ctx, memory, args[0], args[1], args[2],
        )),
        "CopyAcceleratorTableA" => {
            let entries = ctx
                .shared
                .borrow()
                .gui
                .accelerators
                .get(&args[0])
                .cloned()
                .unwrap_or_default();
            if args[1] == 0 {
                return ok(entries.len() as u32);
            }
            let mut bytes = Vec::new();
            for (index, (flags, key, cmd)) in entries.iter().take(args[2] as usize).enumerate() {
                let last = index + 1 == entries.len().min(args[2] as usize);
                bytes.push(*flags as u8 | if last { 0x80 } else { 0 });
                bytes.push(0);
                bytes.extend_from_slice(&(*key as u16).to_le_bytes());
                bytes.extend_from_slice(&(*cmd as u16).to_le_bytes());
                bytes.extend_from_slice(&[0, 0]);
            }
            let _ = memory.write_bytes(args[1], &bytes);
            ok(entries.len().min(args[2] as usize) as u32)
        }
        _ => None,
    }
}
