//! The tree view control (`SysTreeView32`): a hierarchy of items with
//! normal and state images (installers draw check boxes as state images),
//! expanding and collapsing, single selection, hit testing and the parent
//! notifications programs rely on.

use std::collections::HashMap;

use crate::win32::{
    commctrl, gdi::sys_color, gfx::Rect, unicode, user, ApiContext, GuestMemory,
};

const TVI_ROOT: u32 = 0xFFFF_0000;
const TVI_FIRST: u32 = 0xFFFF_0001;
const TVI_LAST: u32 = 0xFFFF_0002;
const TVI_SORT: u32 = 0xFFFF_0003;

const TVIF_TEXT: u32 = 0x1;
const TVIF_IMAGE: u32 = 0x2;
const TVIF_PARAM: u32 = 0x4;
const TVIF_STATE: u32 = 0x8;
const TVIF_SELECTEDIMAGE: u32 = 0x20;
const TVIF_CHILDREN: u32 = 0x40;

const TVIS_SELECTED: u32 = 0x2;
const TVIS_EXPANDED: u32 = 0x20;

const TVS_HASBUTTONS: u32 = 0x1;
const TVS_LINESATROOT: u32 = 0x4;

const WM_NOTIFY: u32 = 0x4E;
const NM_CLICK: u32 = (-2i32) as u32;
const NM_DBLCLK: u32 = (-3i32) as u32;
const TVN_SELCHANGINGA: u32 = (-401i32) as u32;
const TVN_SELCHANGEDA: u32 = (-402i32) as u32;
const TVN_ITEMEXPANDINGA: u32 = (-405i32) as u32;
const TVN_ITEMEXPANDEDA: u32 = (-406i32) as u32;
const TVN_KEYDOWN: u32 = (-412i32) as u32;

const ROW_HEIGHT: i32 = 16;

#[derive(Clone, Debug, Default)]
pub struct TreeItem {
    parent: u32,
    children: Vec<u32>,
    text: Vec<u8>,
    image: i32,
    selected_image: i32,
    state: u32,
    lparam: u32,
    /// cChildren: whether it shows a button though it has no children yet.
    has_children: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TreeView {
    items: HashMap<u32, TreeItem>,
    roots: Vec<u32>,
    next: u32,
    selected: u32,
    /// The first visible row.
    top: usize,
    /// Normal and state image lists.
    normal_images: u32,
    state_images: u32,
    indent: i32,
}

fn rd<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> u32 {
    memory.read_u32(va).unwrap_or(0)
}

fn with_tree<R>(ctx: &ApiContext, hwnd: u32, f: impl FnOnce(&mut TreeView) -> R) -> R {
    let mut shared = ctx.shared.borrow_mut();
    let tree = shared.gui.tree_views.entry(hwnd).or_insert_with(|| TreeView { next: 1, indent: 19, ..TreeView::default() });
    f(tree)
}

fn style(ctx: &ApiContext, hwnd: u32) -> u32 {
    ctx.shared.borrow().gui.window(hwnd).map(|w| w.style).unwrap_or(0)
}

fn text_of<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, wide: bool) -> Vec<u8> {
    if wide {
        unicode::wide_to_ansi(&unicode::read_wstr(memory, va))
    } else {
        crate::win32::read_cstr(memory, va)
    }
}

/// Apply a TVITEM's fields (mask at 0, state 8, state mask 12, text 16,
/// image 24, selected image 28, children 32, lParam 36) to an item.
fn apply_item<M: GuestMemory + ?Sized>(memory: &mut M, item: &mut TreeItem, va: u32, wide: bool) {
    let mask = rd(memory, va);
    if mask & TVIF_TEXT != 0 {
        let text = rd(memory, va + 16);
        item.text = if text >= 0x10000 && text != u32::MAX { text_of(memory, text, wide) } else { Vec::new() };
    }
    if mask & TVIF_IMAGE != 0 {
        item.image = rd(memory, va + 24) as i32;
    }
    if mask & TVIF_SELECTEDIMAGE != 0 {
        item.selected_image = rd(memory, va + 28) as i32;
    }
    if mask & TVIF_STATE != 0 {
        let state_mask = rd(memory, va + 12);
        item.state = (item.state & !state_mask) | (rd(memory, va + 8) & state_mask);
    }
    if mask & TVIF_CHILDREN != 0 {
        item.has_children = rd(memory, va + 32) != 0;
    }
    if mask & TVIF_PARAM != 0 {
        item.lparam = rd(memory, va + 36);
    }
}

/// Fill a TVITEM from an item, as its mask asks.
fn write_item<M: GuestMemory + ?Sized>(memory: &mut M, handle: u32, item: &TreeItem, va: u32, wide: bool) {
    let mask = rd(memory, va);
    let _ = memory.write_u32(va + 4, handle);
    if mask & TVIF_STATE != 0 {
        let state_mask = rd(memory, va + 12);
        let _ = memory.write_u32(va + 8, item.state & state_mask);
    }
    if mask & TVIF_TEXT != 0 {
        let buffer = rd(memory, va + 16);
        let capacity = rd(memory, va + 20) as usize;
        if buffer != 0 && capacity > 0 {
            let count = item.text.len().min(capacity - 1);
            if wide {
                unicode::write_wide(memory, buffer, &unicode::ansi_to_wide(&item.text[..count]), true);
            } else {
                let _ = memory.write_bytes(buffer, &[&item.text[..count], b"\0"].concat());
            }
        }
    }
    if mask & TVIF_IMAGE != 0 {
        let _ = memory.write_u32(va + 24, item.image as u32);
    }
    if mask & TVIF_SELECTEDIMAGE != 0 {
        let _ = memory.write_u32(va + 28, item.selected_image as u32);
    }
    if mask & TVIF_CHILDREN != 0 {
        let _ = memory.write_u32(va + 32, u32::from(item.has_children || !item.children.is_empty()));
    }
    if mask & TVIF_PARAM != 0 {
        let _ = memory.write_u32(va + 36, item.lparam);
    }
}

/// The rows on screen: (item, depth), expanded children included.
fn rows(tree: &TreeView) -> Vec<(u32, i32)> {
    fn walk(tree: &TreeView, items: &[u32], depth: i32, out: &mut Vec<(u32, i32)>) {
        for &handle in items {
            out.push((handle, depth));
            if let Some(item) = tree.items.get(&handle) {
                if item.state & TVIS_EXPANDED != 0 {
                    walk(tree, &item.children, depth + 1, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(tree, &tree.roots, 0, &mut out);
    out
}

/// Where a row's parts start: (button, state image, image, text).
fn layout(tree: &TreeView, style: u32, depth: i32) -> (i32, i32, i32, i32) {
    let root_offset = if style & TVS_LINESATROOT != 0 { 1 } else { 0 };
    let base = (depth + root_offset) * tree.indent;
    let button = base - tree.indent;
    let mut x = base + 2;
    let state = x;
    if tree.state_images != 0 {
        x += 16;
    }
    let image = x;
    if tree.normal_images != 0 {
        x += 18;
    }
    (button, state, image, x + 2)
}

fn invalidate(ctx: &ApiContext, hwnd: u32) {
    let mut shared = ctx.shared.borrow_mut();
    user::invalidate(&mut shared.gui, hwnd, None, true, false);
    shared.gui.dirty = true;
}

fn client_size(ctx: &ApiContext, hwnd: u32) -> (i32, i32) {
    ctx.shared
        .borrow()
        .gui
        .window(hwnd)
        .map(|window| {
            let (l, t, r, b) = user::nc_insets(window);
            (window.width - l - r, window.height - t - b)
        })
        .unwrap_or((0, 0))
}

/// Send WM_NOTIFY with an NMTREEVIEW (header, action, old item, new item,
/// point) or a plain NMHDR.
fn notify<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, code: u32, action: u32, old: u32, new: u32) -> u32 {
    let (parent, id) = match ctx.shared.borrow().gui.window(hwnd) {
        Some(window) => (window.parent, window.id),
        None => return 0,
    };
    if parent == 0 {
        return 0;
    }
    let block = ctx.shared.borrow_mut().heap.alloc(104).unwrap_or(0);
    let _ = memory.write_bytes(block, &[0; 104]);
    for (offset, value) in [(0, hwnd), (4, id), (8, code), (12, action)] {
        let _ = memory.write_u32(block + offset, value);
    }
    for (offset, handle) in [(16u32, old), (56, new)] {
        if handle == 0 {
            continue;
        }
        let item = with_tree(ctx, hwnd, |tree| tree.items.get(&handle).cloned());
        if let Some(item) = item {
            let _ = memory.write_u32(block + offset, TVIF_HANDLE_ALL);
            write_item(memory, handle, &item, block + offset, false);
        }
    }
    let result = user::send(ctx, memory, parent, WM_NOTIFY, id, block);
    ctx.shared.borrow_mut().heap.free(block);
    result
}

/// The fields filled in NMTREEVIEW items.
const TVIF_HANDLE_ALL: u32 = TVIF_STATE | TVIF_PARAM | TVIF_IMAGE | TVIF_SELECTEDIMAGE | 0x10;

fn select<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, handle: u32, action: u32) {
    let old = with_tree(ctx, hwnd, |tree| tree.selected);
    if old == handle {
        return;
    }
    if notify(ctx, memory, hwnd, TVN_SELCHANGINGA, action, old, handle) != 0 {
        return;
    }
    with_tree(ctx, hwnd, |tree| {
        if let Some(item) = tree.items.get_mut(&old) {
            item.state &= !TVIS_SELECTED;
        }
        if let Some(item) = tree.items.get_mut(&handle) {
            item.state |= TVIS_SELECTED;
        }
        tree.selected = handle;
    });
    notify(ctx, memory, hwnd, TVN_SELCHANGEDA, action, old, handle);
    invalidate(ctx, hwnd);
}

/// TVE_COLLAPSE 1, TVE_EXPAND 2, TVE_TOGGLE 3.
fn expand<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, handle: u32, how: u32) -> u32 {
    let Some(expanded) = with_tree(ctx, hwnd, |tree| tree.items.get(&handle).map(|i| i.state & TVIS_EXPANDED != 0)) else {
        return 0;
    };
    let want = match how & 3 {
        1 => false,
        2 => true,
        3 => !expanded,
        _ => return 0,
    };
    if want == expanded {
        return 1;
    }
    let action = if want { 2 } else { 1 };
    if notify(ctx, memory, hwnd, TVN_ITEMEXPANDINGA, action, 0, handle) != 0 {
        return 0;
    }
    with_tree(ctx, hwnd, |tree| {
        if let Some(item) = tree.items.get_mut(&handle) {
            item.state ^= TVIS_EXPANDED;
        }
    });
    notify(ctx, memory, hwnd, TVN_ITEMEXPANDEDA, action, 0, handle);
    invalidate(ctx, hwnd);
    1
}

fn delete(tree: &mut TreeView, handle: u32) {
    let Some(item) = tree.items.remove(&handle) else { return };
    for child in item.children {
        delete(tree, child);
    }
    if let Some(parent) = tree.items.get_mut(&item.parent) {
        parent.children.retain(|&c| c != handle);
    }
    tree.roots.retain(|&c| c != handle);
    if tree.selected == handle {
        tree.selected = 0;
    }
}

/// Hit-test a client point: (item, TVHT_* flags).
fn hit_test(ctx: &ApiContext, hwnd: u32, x: i32, y: i32) -> (u32, u32) {
    let style = style(ctx, hwnd);
    let (width, height) = client_size(ctx, hwnd);
    if x < 0 || y < 0 || x >= width || y >= height {
        return (0, 0x1); // TVHT_NOWHERE
    }
    with_tree(ctx, hwnd, |tree| {
        let rows = rows(tree);
        let row = (y / ROW_HEIGHT) as usize + tree.top;
        let Some(&(handle, depth)) = rows.get(row) else {
            return (0, 0x1);
        };
        let (button, state, image, text) = layout(tree, style, depth);
        let label_end = text + user::text_width(&tree.items[&handle].text) + 4;
        let flags = if x < button.max(0) {
            0x8 // TVHT_ONITEMINDENT
        } else if x < button + tree.indent && style & TVS_HASBUTTONS != 0 && x < state {
            0x10 // TVHT_ONITEMBUTTON
        } else if tree.state_images != 0 && x >= state && x < state + 16 {
            0x40 // TVHT_ONITEMSTATEICON
        } else if tree.normal_images != 0 && x >= image && x < image + 16 {
            0x2 // TVHT_ONITEMICON
        } else if x >= text && x < label_end {
            0x4 // TVHT_ONITEMLABEL
        } else if x >= label_end {
            0x20 // TVHT_ONITEMRIGHT
        } else {
            0x8
        };
        (handle, flags)
    })
}

/// Show the vertical scroll bar while the rows overflow the window, and
/// keep its range, page and position in step with the first visible row.
fn sync_scroll<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let total = with_tree(ctx, hwnd, |tree| rows(tree).len());
    let fits = |ctx: &ApiContext| (client_size(ctx, hwnd).1 / ROW_HEIGHT).max(1) as usize;
    user::show_scroll_bar(ctx, memory, hwnd, 1, total > fits(ctx));
    let visible = fits(ctx);
    let top = with_tree(ctx, hwnd, |tree| {
        tree.top = tree.top.min(total.saturating_sub(visible));
        tree.top
    });
    if let Some(w) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
        w.scroll[1] = (0, total.saturating_sub(1) as i32, top as i32, visible as u32);
    }
}

/// Scroll so that `top` is the first visible row.
fn scroll_to<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, top: i64) {
    with_tree(ctx, hwnd, |tree| tree.top = top.max(0) as usize);
    sync_scroll(ctx, memory, hwnd);
    invalidate(ctx, hwnd);
}

fn paint<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    sync_scroll(ctx, memory, hwnd);
    let ps = user::scratch(ctx, 80);
    let hdc = user::begin_paint(ctx, memory, hwnd, ps);
    let tree = with_tree(ctx, hwnd, |tree| tree.clone());
    let style = style(ctx, hwnd);
    let (width, height) = client_size(ctx, hwnd);
    let focused = ctx.shared.borrow().gui.focus_hwnd == hwnd;
    let rows = rows(&tree);
    let visible = (height / ROW_HEIGHT).max(1) as usize;
    let mut images = Vec::new();
    {
        let mut shared = ctx.shared.borrow_mut();
        user::with_painter(&mut shared, hwnd, true, |p| {
            p.fill(Rect::new(0, 0, width, height), sys_color(5));
            for (row, &(handle, depth)) in rows.iter().enumerate().skip(tree.top).take(visible) {
                let item = &tree.items[&handle];
                let y = (row - tree.top) as i32 * ROW_HEIGHT;
                let (button, state, image, text) = layout(&tree, style, depth);
                let has_children = item.has_children || !item.children.is_empty();
                if style & TVS_HASBUTTONS != 0 && has_children && button >= 0 {
                    let boxed = Rect::sized(button + tree.indent / 2 - 4, y + 4, 9, 9);
                    p.fill(boxed, 0xFFFFFF);
                    p.hline(boxed.left, boxed.right - 1, boxed.top, 0x808080);
                    p.hline(boxed.left, boxed.right - 1, boxed.bottom - 1, 0x808080);
                    p.vline(boxed.left, boxed.top, boxed.bottom - 1, 0x808080);
                    p.vline(boxed.right - 1, boxed.top, boxed.bottom - 1, 0x808080);
                    p.hline(boxed.left + 2, boxed.right - 2, boxed.top + 4, 0x000000);
                    if item.state & TVIS_EXPANDED == 0 {
                        p.vline(boxed.left + 4, boxed.top + 2, boxed.bottom - 2, 0x000000);
                    }
                }
                let state_index = (item.state >> 12) & 0xF;
                if tree.state_images != 0 && state_index > 0 {
                    images.push((tree.state_images, state_index as i32 - 1, state, y));
                }
                if tree.normal_images != 0 {
                    let index = if item.state & TVIS_SELECTED != 0 { item.selected_image } else { item.image };
                    images.push((tree.normal_images, index, image, y));
                }
                let label = Rect::new(text, y, text + user::text_width(&item.text) + 4, y + ROW_HEIGHT);
                let selected = item.state & TVIS_SELECTED != 0;
                if selected {
                    p.fill(label, if focused { sys_color(13) } else { sys_color(15) });
                }
                let color = if selected && focused { sys_color(14) } else { sys_color(8) };
                p.text(text + 2, y + 1, &item.text, color, false);
            }
        });
    }
    for (list, index, x, y) in images {
        let image = ctx.shared.borrow().image_lists.get(&list).and_then(|l| l.images.get(index.max(0) as usize).cloned());
        if let Some(image) = image {
            commctrl::draw_image(ctx, memory, hdc, x, y, &image, None, false);
        }
    }
    user::end_paint(ctx, hwnd, hdc);
    user::free_scratch(ctx, ps);
}

pub fn treeview_proc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, msg: u32, wparam: u32, lparam: u32) -> u32 {
    match msg {
        // TVM_INSERTITEMA/W: TVINSERTSTRUCT { parent, insert after, TVITEM }
        0x1100 | 0x1132 => {
            let wide = msg == 0x1132;
            let parent = rd(memory, lparam);
            let after = rd(memory, lparam + 4);
            let mut item = TreeItem::default();
            apply_item(memory, &mut item, lparam + 8, wide);
            let parent = if parent == TVI_ROOT { 0 } else { parent };
            item.parent = parent;
            let handle = with_tree(ctx, hwnd, |tree| {
                if parent != 0 && !tree.items.contains_key(&parent) {
                    return 0;
                }
                let handle = tree.next;
                tree.next += 1;
                let text = item.text.clone();
                tree.items.insert(handle, item);
                let siblings = if parent == 0 { tree.roots.clone() } else { tree.items[&parent].children.clone() };
                let position = match after {
                    TVI_FIRST => 0,
                    TVI_LAST => siblings.len(),
                    TVI_SORT => siblings
                        .iter()
                        .position(|s| tree.items.get(s).is_some_and(|i| i.text.to_ascii_lowercase() > text.to_ascii_lowercase()))
                        .unwrap_or(siblings.len()),
                    other => siblings.iter().position(|&s| s == other).map(|i| i + 1).unwrap_or(siblings.len()),
                };
                let list = if parent == 0 { &mut tree.roots } else { &mut tree.items.get_mut(&parent).expect("checked").children };
                list.insert(position.min(list.len()), handle);
                handle
            });
            invalidate(ctx, hwnd);
            handle
        }
        // TVM_DELETEITEM
        0x1101 => {
            with_tree(ctx, hwnd, |tree| {
                if lparam == 0 || lparam == TVI_ROOT {
                    tree.items.clear();
                    tree.roots.clear();
                    tree.selected = 0;
                    tree.top = 0;
                } else {
                    delete(tree, lparam);
                }
            });
            invalidate(ctx, hwnd);
            1
        }
        // TVM_EXPAND
        0x1102 => expand(ctx, memory, hwnd, lparam, wparam),
        // TVM_GETITEMRECT (wParam: text only; lParam: in item, out RECT)
        0x1104 => {
            let handle = rd(memory, lparam);
            let style = style(ctx, hwnd);
            let (width, _) = client_size(ctx, hwnd);
            let found = with_tree(ctx, hwnd, |tree| {
                let rows = rows(tree);
                let row = rows.iter().position(|(h, _)| *h == handle)?;
                let depth = rows[row].1;
                let y = (row as i32 - tree.top as i32) * ROW_HEIGHT;
                let (_, _, _, text) = layout(tree, style, depth);
                let end = text + user::text_width(&tree.items[&handle].text) + 4;
                Some(if wparam != 0 { (text, y, end, y + ROW_HEIGHT) } else { (0, y, width, y + ROW_HEIGHT) })
            });
            match found {
                Some((l, t, r, b)) => {
                    for (offset, value) in [(0, l), (4, t), (8, r), (12, b)] {
                        let _ = memory.write_u32(lparam + offset, value as u32);
                    }
                    1
                }
                None => 0,
            }
        }
        // TVM_GETCOUNT
        0x1105 => with_tree(ctx, hwnd, |tree| tree.items.len() as u32),
        // TVM_GETINDENT, TVM_SETINDENT
        0x1106 => with_tree(ctx, hwnd, |tree| tree.indent as u32),
        0x1107 => {
            with_tree(ctx, hwnd, |tree| tree.indent = (wparam as i32).max(8));
            invalidate(ctx, hwnd);
            0
        }
        // TVM_GETIMAGELIST, TVM_SETIMAGELIST (TVSIL_NORMAL 0, TVSIL_STATE 2)
        0x1108 => with_tree(ctx, hwnd, |tree| if wparam == 2 { tree.state_images } else { tree.normal_images }),
        0x1109 => {
            let old = with_tree(ctx, hwnd, |tree| {
                let slot = if wparam == 2 { &mut tree.state_images } else { &mut tree.normal_images };
                std::mem::replace(slot, lparam)
            });
            invalidate(ctx, hwnd);
            old
        }
        // TVM_GETNEXTITEM
        0x110A => with_tree(ctx, hwnd, |tree| {
            let item = tree.items.get(&lparam);
            let siblings = |parent: u32| if parent == 0 { tree.roots.clone() } else { tree.items.get(&parent).map(|p| p.children.clone()).unwrap_or_default() };
            match wparam {
                0 => tree.roots.first().copied().unwrap_or(0),
                1 | 2 => {
                    let Some(item) = item else { return 0 };
                    let list = siblings(item.parent);
                    let index = list.iter().position(|&s| s == lparam).unwrap_or(0);
                    if wparam == 1 {
                        list.get(index + 1).copied().unwrap_or(0)
                    } else if index > 0 {
                        list[index - 1]
                    } else {
                        0
                    }
                }
                3 => item.map(|i| i.parent).unwrap_or(0),
                4 => {
                    if lparam == 0 || lparam == TVI_ROOT {
                        tree.roots.first().copied().unwrap_or(0)
                    } else {
                        item.and_then(|i| i.children.first().copied()).unwrap_or(0)
                    }
                }
                5 => rows(tree).get(tree.top).map(|r| r.0).unwrap_or(0),
                6 | 7 => {
                    let rows = rows(tree);
                    let index = rows.iter().position(|r| r.0 == lparam);
                    match (index, wparam) {
                        (Some(i), 6) => rows.get(i + 1).map(|r| r.0).unwrap_or(0),
                        (Some(i), _) if i > 0 => rows[i - 1].0,
                        _ => 0,
                    }
                }
                9 => tree.selected,
                _ => 0,
            }
        }),
        // TVM_SELECTITEM (TVGN_CARET 9; others: first visible / drop target)
        0x110B => {
            if wparam == 9 {
                select(ctx, memory, hwnd, lparam, 0);
            } else if wparam == 5 {
                with_tree(ctx, hwnd, |tree| {
                    if let Some(row) = rows(tree).iter().position(|r| r.0 == lparam) {
                        tree.top = row;
                    }
                });
                invalidate(ctx, hwnd);
            }
            1
        }
        // TVM_GETITEMA/W
        0x110C | 0x113E => {
            let handle = rd(memory, lparam + 4);
            match with_tree(ctx, hwnd, |tree| tree.items.get(&handle).cloned()) {
                Some(item) => {
                    write_item(memory, handle, &item, lparam, msg == 0x113E);
                    1
                }
                None => 0,
            }
        }
        // TVM_SETITEMA/W
        0x110D | 0x113F => {
            let handle = rd(memory, lparam + 4);
            let Some(mut item) = with_tree(ctx, hwnd, |tree| tree.items.get(&handle).cloned()) else {
                return 0;
            };
            apply_item(memory, &mut item, lparam, msg == 0x113F);
            with_tree(ctx, hwnd, |tree| tree.items.insert(handle, item));
            invalidate(ctx, hwnd);
            1
        }
        // TVM_GETVISIBLECOUNT
        0x1110 => (client_size(ctx, hwnd).1 / ROW_HEIGHT).max(1) as u32,
        // TVM_HITTEST: TVHITTESTINFO { POINT, flags, item }
        0x1111 => {
            let (x, y) = (rd(memory, lparam) as i32, rd(memory, lparam + 4) as i32);
            let (item, flags) = hit_test(ctx, hwnd, x, y);
            let _ = memory.write_u32(lparam + 8, flags);
            let _ = memory.write_u32(lparam + 12, item);
            item
        }
        // TVM_ENSUREVISIBLE
        0x1114 => {
            let visible = (client_size(ctx, hwnd).1 / ROW_HEIGHT).max(1) as usize;
            with_tree(ctx, hwnd, |tree| {
                // Expand the parents.
                let mut parent = tree.items.get(&lparam).map(|i| i.parent).unwrap_or(0);
                while parent != 0 {
                    let item = tree.items.get_mut(&parent).expect("parent");
                    item.state |= TVIS_EXPANDED;
                    parent = item.parent;
                }
                if let Some(row) = rows(tree).iter().position(|r| r.0 == lparam) {
                    if row < tree.top {
                        tree.top = row;
                    } else if row >= tree.top + visible {
                        tree.top = row + 1 - visible;
                    }
                }
            });
            invalidate(ctx, hwnd);
            1
        }
        // TVM_SORTCHILDREN
        0x1113 => {
            with_tree(ctx, hwnd, |tree| {
                let mut list = if lparam == 0 || lparam == TVI_ROOT { tree.roots.clone() } else { tree.items.get(&lparam).map(|i| i.children.clone()).unwrap_or_default() };
                list.sort_by_key(|h| tree.items.get(h).map(|i| i.text.to_ascii_lowercase()).unwrap_or_default());
                if lparam == 0 || lparam == TVI_ROOT {
                    tree.roots = list;
                } else if let Some(item) = tree.items.get_mut(&lparam) {
                    item.children = list;
                }
            });
            invalidate(ctx, hwnd);
            1
        }
        // TVM_GETITEMHEIGHT, TVM_SETITEMHEIGHT
        0x111C | 0x111B => ROW_HEIGHT as u32,
        // TVM_GETITEMSTATE (item, mask)
        0x1127 => with_tree(ctx, hwnd, |tree| tree.items.get(&wparam).map(|i| i.state & lparam).unwrap_or(0)),
        // TVM_SETBKCOLOR, TVM_SETTEXTCOLOR, TVM_SETTOOLTIPS, ...
        0x111D | 0x111E | 0x1118 | 0x1119 | 0x110F | 0x1116 => 0,
        user::WM_PAINT => {
            paint(ctx, memory, hwnd);
            0
        }
        user::WM_ERASEBKGND => 1,
        // WM_VSCROLL: lines, pages, the thumb.
        0x0115 => {
            let top = with_tree(ctx, hwnd, |tree| tree.top as i64);
            let page = (client_size(ctx, hwnd).1 / ROW_HEIGHT).max(1) as i64;
            let target = match wparam & 0xFFFF {
                0 => top - 1,
                1 => top + 1,
                2 => top - page,
                3 => top + page,
                4 | 5 => (wparam >> 16) as i64,
                6 => 0,
                7 => i64::MAX / 2,
                _ => top,
            };
            scroll_to(ctx, memory, hwnd, target);
            0
        }
        // WM_MOUSEWHEEL: three rows a notch.
        0x020A => {
            let delta = (wparam >> 16) as i16 as i64;
            let top = with_tree(ctx, hwnd, |tree| tree.top as i64);
            scroll_to(ctx, memory, hwnd, top - delta / 120 * 3);
            0
        }
        user::WM_LBUTTONDOWN | user::WM_LBUTTONDBLCLK => {
            let (x, y) = ((lparam & 0xFFFF) as i16 as i32, (lparam >> 16) as i16 as i32);
            user::set_focus(ctx, memory, hwnd);
            let (item, flags) = hit_test(ctx, hwnd, x, y);
            if item != 0 {
                if flags & 0x10 != 0 || msg == user::WM_LBUTTONDBLCLK {
                    expand(ctx, memory, hwnd, item, 3);
                }
                if flags & (0x2 | 0x4 | 0x40 | 0x20) != 0 {
                    select(ctx, memory, hwnd, item, 1); // TVC_BYMOUSE
                }
            }
            notify(ctx, memory, hwnd, if msg == user::WM_LBUTTONDBLCLK { NM_DBLCLK } else { NM_CLICK }, 0, 0, 0);
            0
        }
        user::WM_KEYDOWN => {
            // TVN_KEYDOWN { NMHDR, wVKey, flags }: the parent may act on it.
            let (parent, id) = ctx.shared.borrow().gui.window(hwnd).map(|w| (w.parent, w.id)).unwrap_or((0, 0));
            if parent != 0 {
                let block = ctx.shared.borrow_mut().heap.alloc(20).unwrap_or(0);
                for (offset, value) in [(0, hwnd), (4, id), (8, TVN_KEYDOWN), (12, wparam & 0xFFFF), (16, 0)] {
                    let _ = memory.write_u32(block + offset, value);
                }
                user::send(ctx, memory, parent, WM_NOTIFY, id, block);
                ctx.shared.borrow_mut().heap.free(block);
            }
            let (selected, rows) = with_tree(ctx, hwnd, |tree| (tree.selected, rows(tree)));
            let index = rows.iter().position(|r| r.0 == selected);
            let target = match (wparam, index) {
                (0x26, Some(i)) if i > 0 => Some(rows[i - 1].0),
                (0x28, Some(i)) => rows.get(i + 1).map(|r| r.0),
                (0x26 | 0x28, None) => rows.first().map(|r| r.0),
                (0x25, Some(_)) => {
                    expand(ctx, memory, hwnd, selected, 1);
                    None
                }
                (0x27, Some(_)) => {
                    expand(ctx, memory, hwnd, selected, 2);
                    None
                }
                _ => None,
            };
            if let Some(target) = target {
                select(ctx, memory, hwnd, target, 2); // TVC_BYKEYBOARD
                treeview_proc(ctx, memory, hwnd, 0x1114, 0, target);
            }
            0
        }
        user::WM_SETFOCUS | user::WM_KILLFOCUS => {
            invalidate(ctx, hwnd);
            0
        }
        user::WM_GETDLGCODE => 0x1 | 0x4, // DLGC_WANTARROWS | DLGC_WANTCHARS
        user::WM_DESTROY => {
            ctx.shared.borrow_mut().gui.tree_views.remove(&hwnd);
            0
        }
        _ => user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}
