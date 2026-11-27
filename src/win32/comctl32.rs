//! COMCTL32 property sheets and wizards (`PropertySheet`).
//!
//! The sheet is a dialog whose procedure is Rust (`OTW_PropSheetProc`): a
//! frame with the wizard buttons (Back, Next/Finish, Cancel) or OK, Cancel
//! and Apply, hosting the program's pages. Each page is a child dialog made
//! from the page's template when first shown, with the program's dialog
//! procedure. The sheet talks to pages with `WM_NOTIFY` (`PSN_*`; a page
//! answers through `DWL_MSGRESULT`) and pages talk to the sheet with
//! `PSM_*` messages, as in Windows.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::win32::{
    controls::{self, DialogTemplate, TemplateItem},
    unicode,
    user::{self, ResName},
    ApiContext, ApiOutcome, GuestMemory,
};

const PSH_PROPTITLE: u32 = 0x1;
const PSH_PROPSHEETPAGE: u32 = 0x8;
const PSH_WIZARD: u32 = 0x20;
const PSH_USEPSTARTPAGE: u32 = 0x40;
const PSH_HASHELP: u32 = 0x200;
const PSH_USECALLBACK: u32 = 0x100;
const PSH_MODELESS: u32 = 0x400;
const PSH_WIZARD97: u32 = 0x0100_0000;
const PSH_WIZARD_LITE: u32 = 0x0040_0000;
const PSH_AEROWIZARD: u32 = 0x4000;

const PSP_DLGINDIRECT: u32 = 0x1;
const PSP_USECALLBACK: u32 = 0x80;

const PSN_SETACTIVE: u32 = -200i32 as u32;
const PSN_KILLACTIVE: u32 = -201i32 as u32;
const PSN_APPLY: u32 = -202i32 as u32;
const PSN_RESET: u32 = -203i32 as u32;
const PSN_WIZBACK: u32 = -206i32 as u32;
const PSN_WIZNEXT: u32 = -207i32 as u32;
const PSN_WIZFINISH: u32 = -208i32 as u32;
const PSN_QUERYCANCEL: u32 = -209i32 as u32;

const ID_BACK: u32 = 0x3023;
const ID_NEXT: u32 = 0x3024;
const ID_FINISH: u32 = 0x3025;
const ID_APPLY: u32 = 0x3021;
const ID_LINE: u32 = 0x3026;
/// The sheet's tab control (hidden in wizards), which MFC looks up.
const ID_TABS: u32 = 0x3020;
const IDOK: u32 = 1;
const IDCANCEL: u32 = 2;
const IDHELP: u32 = 9;

const WM_INITDIALOG: u32 = 0x0110;
const WM_COMMAND: u32 = 0x0111;
const WM_NOTIFY: u32 = 0x004E;
const WM_DESTROY: u32 = 0x0002;

/// A page of a sheet.
#[derive(Clone, Debug)]
pub struct Page {
    /// The sheet's copy of the page's PROPSHEETPAGE (passed to the page).
    psp: u32,
    template: u32,
    dlg_proc: u32,
    /// Resource id of the page's template (for PSN_WIZNEXT jumps).
    resource_id: u32,
    hwnd: u32,
}

/// The state of an open property sheet.
#[derive(Clone, Debug)]
pub struct Sheet {
    wizard: bool,
    wide: bool,
    instance: u32,
    pages: Vec<Page>,
    current: usize,
    /// Where pages go, in dialog units.
    page_x: i32,
    page_y: i32,
}

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "PropertySheetW" | "PropertySheetA" | "PropertySheet" | "CreatePropertySheetPageW"
        | "CreatePropertySheetPageA" | "CreatePropertySheetPage" | "DestroyPropertySheetPage" => 1,
        "OTW_PropSheetProc" | "OTW_TabProc" | "OTW_ListViewProc" => 4,
        _ => return None,
    })
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let value = match name {
        "PropertySheetW" => property_sheet(ctx, memory, args[0], true),
        "PropertySheetA" | "PropertySheet" => property_sheet(ctx, memory, args[0], false),
        "CreatePropertySheetPageW" | "CreatePropertySheetPageA" | "CreatePropertySheetPage" => {
            // An HPROPSHEETPAGE is a copy of the PROPSHEETPAGE.
            let size = memory.read_u32(args[0]).unwrap_or(0).clamp(40, 160);
            let bytes = memory.read_bytes(args[0], size as usize).unwrap_or_default();
            let copy = ctx.shared.borrow_mut().heap.alloc(size).unwrap_or(0);
            let _ = memory.write_bytes(copy, &bytes);
            if name == "CreatePropertySheetPageW" {
                ctx.shared.borrow_mut().gui.unicode_pages.insert(copy);
            }
            copy
        }
        "DestroyPropertySheetPage" => {
            ctx.shared.borrow_mut().heap.free(args[0]);
            1
        }
        "OTW_PropSheetProc" => sheet_proc(ctx, memory, args[0], args[1], args[2], args[3]),
        "OTW_TabProc" => tab_proc(ctx, memory, args[0], args[1], args[2], args[3]),
        "OTW_ListViewProc" => listview_proc(ctx, memory, args[0], args[1], args[2], args[3]),
        _ => return None,
    };
    Some(ApiOutcome::new(value, count))
}

fn rd<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> u32 {
    memory.read_u32(va).unwrap_or(0)
}

/// Offset of field `index` of a PROPSHEETHEADER or PROPSHEETPAGE: two
/// 32-bit fields (size, flags), then pointer-sized ones, except the
/// header's page count (field 6), a 32-bit value.
fn sheet_field(ctx: &ApiContext, index: u32, header: bool) -> u32 {
    if !ctx.shared.borrow().x64 {
        return index * 4;
    }
    match index {
        0 | 1 => index * 4,
        6 if header => 40,
        _ => 8 * (index - 1),
    }
}

fn psp_field(ctx: &ApiContext, index: u32) -> u32 {
    sheet_field(ctx, index, false)
}

fn psh_field(ctx: &ApiContext, index: u32) -> u32 {
    sheet_field(ctx, index, true)
}

fn text<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, wide: bool) -> Vec<u8> {
    if va < 0x10000 {
        return Vec::new();
    }
    if wide {
        unicode::wide_to_ansi(&unicode::read_wstr(memory, va))
    } else {
        crate::win32::read_cstr(memory, va)
    }
}

/// Build a page from a PROPSHEETPAGE in guest memory.
fn load_page<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, psp: u32, wide: bool) -> Option<Page> {
    let size = rd(memory, psp).clamp(40, 160);
    let bytes = memory.read_bytes(psp, size as usize)?;
    let copy = ctx.shared.borrow_mut().heap.alloc(size)?;
    let _ = memory.write_bytes(copy, &bytes);
    let flags = rd(memory, copy + psp_field(ctx, 1));
    let instance = rd(memory, copy + psp_field(ctx, 2));
    let name = rd(memory, copy + psp_field(ctx, 3));
    let (template, resource_id) = if flags & PSP_DLGINDIRECT != 0 {
        (name, 0)
    } else {
        let resource = if name < 0x10000 {
            ResName::Id(name)
        } else {
            ResName::Name(String::from_utf8_lossy(&text(memory, name, wide)).to_ascii_uppercase())
        };
        let (va, _) = user::find_resource_any(ctx, memory, instance, &ResName::Id(5), &resource)?;
        (va, if name < 0x10000 { name } else { 0 })
    };
    let dlg_proc = rd(memory, copy + psp_field(ctx, 6));
    if wide {
        unicode::mark_unicode(ctx, dlg_proc);
    }
    Some(Page { psp: copy, template, dlg_proc, resource_id, hwnd: 0 })
}

/// A DLGTEMPLATE for the sheet frame (so the program's sheet callback can
/// see and adjust it), in guest memory.
fn frame_template<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, frame: &DialogTemplate) -> u32 {
    let mut bytes = Vec::new();
    let push16 = |bytes: &mut Vec<u8>, value: u16| bytes.extend_from_slice(&value.to_le_bytes());
    let push_text = |bytes: &mut Vec<u8>, text: &[u8]| {
        for &byte in text {
            bytes.extend_from_slice(&(byte as u16).to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
    };
    bytes.extend_from_slice(&frame.style.to_le_bytes());
    bytes.extend_from_slice(&frame.ex_style.to_le_bytes());
    push16(&mut bytes, frame.items.len() as u16);
    for value in [frame.x, frame.y, frame.cx, frame.cy] {
        push16(&mut bytes, value as i16 as u16);
    }
    push16(&mut bytes, 0); // menu
    push16(&mut bytes, 0); // class
    push_text(&mut bytes, &frame.title);
    push16(&mut bytes, 8);
    push_text(&mut bytes, b"MS Sans Serif");
    for item in &frame.items {
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        bytes.extend_from_slice(&item.style.to_le_bytes());
        bytes.extend_from_slice(&item.ex_style.to_le_bytes());
        for value in [item.x, item.y, item.cx, item.cy] {
            push16(&mut bytes, value as i16 as u16);
        }
        push16(&mut bytes, item.id as u16);
        if item.class == "Static" || item.class == "Button" {
            push16(&mut bytes, 0xFFFF);
            push16(&mut bytes, if item.class == "Static" { 0x82 } else { 0x80 });
        } else {
            push_text(&mut bytes, item.class.as_bytes());
        }
        push_text(&mut bytes, &item.title);
        push16(&mut bytes, 0);
    }
    let block = ctx.shared.borrow_mut().heap.alloc(bytes.len() as u32).unwrap_or(0);
    let _ = memory.write_bytes(block, &bytes);
    block
}

/// `PropertySheet(PROPSHEETHEADER*)`: run a modal sheet, returning 1 when
/// it was finished (or OK'd) and 0 when cancelled; a modeless sheet returns
/// its window.
fn property_sheet<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, header: u32, wide: bool) -> u32 {
    let flags = rd(memory, header + psh_field(ctx, 1));
    let parent = rd(memory, header + psh_field(ctx, 2));
    let instance = rd(memory, header + psh_field(ctx, 3));
    let caption_va = rd(memory, header + psh_field(ctx, 5));
    let mut caption = text(memory, caption_va, wide);
    if flags & PSH_PROPTITLE != 0 {
        caption = [b"Properties for ".as_slice(), &caption].concat();
    }
    let count = rd(memory, header + psh_field(ctx, 6)).min(64);
    let start = rd(memory, header + psh_field(ctx, 7));
    let pages_va = rd(memory, header + psh_field(ctx, 8));
    let callback = if flags & PSH_USECALLBACK != 0 { rd(memory, header + psh_field(ctx, 9)) } else { 0 };
    let wizard = flags & (PSH_WIZARD | PSH_WIZARD97 | PSH_WIZARD_LITE | PSH_AEROWIZARD) != 0;

    let mut pages = Vec::new();
    let mut cursor = pages_va;
    for _ in 0..count {
        let psp = if flags & PSH_PROPSHEETPAGE != 0 {
            let psp = cursor;
            cursor += rd(memory, psp).max(40);
            psp
        } else {
            let handle = rd(memory, cursor);
            cursor += if ctx.shared.borrow().x64 { 8 } else { 4 };
            handle
        };
        let page_wide = wide || ctx.shared.borrow().gui.unicode_pages.contains(&psp);
        match load_page(ctx, memory, psp, page_wide) {
            Some(page) => pages.push(page),
            None => eprintln!("[comctl32] property sheet page {psp:#x} has no template"),
        }
    }
    if pages.is_empty() {
        return u32::MAX;
    }
    let start = if flags & PSH_USEPSTARTPAGE != 0 { 0 } else { (start as usize).min(pages.len() - 1) };

    // The frame: pages at (7, 7), the buttons below them.
    let (max_cx, max_cy) = pages.iter().fold((212, 188), |(cx, cy), page| {
        match controls::parse_template(memory, page.template) {
            Some(template) => (cx.max(template.cx), cy.max(template.cy)),
            None => (cx, cy),
        }
    });
    let (page_x, page_y) = (7, 7);
    let width = max_cx + 14;
    let buttons_y = page_y + max_cy + 11;
    let height = buttons_y + 14 + 7;
    let button = |id: u32, x: i32, label: &[u8], visible: bool| TemplateItem {
        // WS_CHILD | WS_VISIBLE | WS_TABSTOP, BS_DEFPUSHBUTTON for Next/OK.
        style: {
            let mut style = 0x5001_0000;
            if id == ID_NEXT || id == IDOK {
                style |= 1;
            }
            if !visible {
                style &= !0x1000_0000;
            }
            style
        },
        ex_style: 0,
        x,
        y: buttons_y,
        cx: 50,
        cy: 14,
        id,
        class: "Button".into(),
        title: label.to_vec(),
        extra_va: 0,
    };
    let cancel_x = width - 7 - 50;
    let mut items = vec![TemplateItem {
        style: 0x4000_0000 | if wizard { 0 } else { 0x1000_0000 },
        ex_style: 0,
        x: page_x,
        y: page_y,
        cx: max_cx,
        cy: max_cy,
        id: ID_TABS,
        class: "SysTabControl32".into(),
        title: Vec::new(),
        extra_va: 0,
    }];
    if wizard {
        let next_x = cancel_x - 7 - 50;
        let back_x = next_x - 50;
        items.push(TemplateItem {
            style: 0x5000_0010, // SS_ETCHEDHORZ
            ex_style: 0,
            x: 7,
            y: buttons_y - 7,
            cx: width - 14,
            cy: 1,
            id: ID_LINE,
            class: "Static".into(),
            title: Vec::new(),
            extra_va: 0,
        });
        items.push(button(ID_BACK, back_x, b"< &Back", true));
        items.push(button(ID_NEXT, next_x, b"&Next >", true));
        items.push(button(ID_FINISH, next_x, b"Finish", false));
        items.push(button(IDCANCEL, cancel_x, b"Cancel", true));
    } else {
        let apply_x = cancel_x;
        let cancel_x = apply_x - 6 - 50;
        let ok_x = cancel_x - 6 - 50;
        items.push(button(IDOK, ok_x, b"OK", true));
        items.push(button(IDCANCEL, cancel_x, b"Cancel", true));
        items.push(button(ID_APPLY, apply_x, b"&Apply", true));
    }
    if flags & PSH_HASHELP != 0 {
        items.push(button(IDHELP, 7, b"&Help", true));
    }
    let mut frame = DialogTemplate {
        // WS_POPUP | WS_CAPTION | WS_SYSMENU | DS_MODALFRAME | DS_3DLOOK | DS_CENTER | DS_SETFONT
        style: 0x80C8_0000 | 0x80 | 0x4 | 0x800 | 0x40,
        ex_style: 0x0001_0000, // WS_EX_CONTROLPARENT
        x: 0,
        y: 0,
        cx: width,
        cy: height,
        class: None,
        title: caption,
        items,
    };
    if callback != 0 {
        // PSCB_PRECREATE: the program may adjust the frame's template.
        let template = frame_template(ctx, memory, &frame);
        memory.call_guest(callback, &[0, 2, template]);
        if let Some(adjusted) = controls::parse_template(memory, template) {
            frame.style = adjusted.style;
            frame.ex_style = adjusted.ex_style;
        }
        ctx.shared.borrow_mut().heap.free(template);
    }
    let modeless = flags & PSH_MODELESS != 0;
    frame.style &= !0x1000_0000; // shown once the first page is ready
    let proc_va = ctx.builtin_proc("OTW_PropSheetProc");
    let sheet = Sheet { wizard, wide, instance, pages, current: usize::MAX, page_x, page_y };
    // The sheet is registered before WM_INITDIALOG reaches its procedure.
    ctx.shared.borrow_mut().gui.property_sheets.insert(0, sheet);
    let hwnd = controls::create_dialog_from(ctx, memory, instance, &frame, parent, proc_va, 0);
    let sheet = ctx.shared.borrow_mut().gui.property_sheets.remove(&0);
    let Some(sheet) = sheet else {
        return u32::MAX;
    };
    if hwnd == 0 {
        return u32::MAX;
    }
    ctx.shared.borrow_mut().gui.property_sheets.insert(hwnd, sheet);
    if callback != 0 {
        // PSCB_INITIALIZED
        memory.call_guest(callback, &[hwnd, 1, 0]);
    }
    activate(ctx, memory, hwnd, start, true);
    user::show_window(ctx, memory, hwnd, 5);
    if modeless {
        return hwnd;
    }
    let result = controls::run_modal_for(ctx, memory, hwnd, parent);
    ctx.shared.borrow_mut().gui.property_sheets.remove(&hwnd);
    result
}

fn with_sheet<R>(ctx: &ApiContext, hwnd: u32, f: impl FnOnce(&mut Sheet) -> R) -> Option<R> {
    ctx.shared.borrow_mut().gui.property_sheets.get_mut(&hwnd).map(f)
}

/// Send a PSN_* notification to a page; returns its DWL_MSGRESULT.
fn notify<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, sheet: u32, page: u32, code: u32) -> u32 {
    if page == 0 {
        return 0;
    }
    // PSHNOTIFY (header, lParam) in the process's layout.
    let block = ctx.shared.borrow_mut().heap.alloc(32).unwrap_or(0);
    let view = crate::win32::x64::View::new(memory, &crate::win32::x64::PSHNOTIFY, block);
    for (index, value) in [sheet, 0, code, 0].into_iter().enumerate() {
        view.set(memory, index, value);
    }
    controls::set_msg_result(ctx, page, 0);
    user::send(ctx, memory, page, WM_NOTIFY, 0, block);
    ctx.shared.borrow_mut().heap.free(block);
    controls::msg_result(ctx, page)
}

/// Make page `index` current: create it if needed, hide the old one.
fn activate<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, sheet_hwnd: u32, index: usize, forward: bool) {
    let Some(sheet) = ctx.shared.borrow().gui.property_sheets.get(&sheet_hwnd).cloned() else {
        return;
    };
    let mut index = index;
    for _ in 0..sheet.pages.len() {
        let Some(page) = sheet.pages.get(index).cloned() else {
            return;
        };
        // The page is already the current one (the tab control reports it)
        // while it is created and while it decides whether to show.
        let previous_index = sheet.current;
        with_sheet(ctx, sheet_hwnd, |sheet| sheet.current = index);
        let hwnd = if page.hwnd != 0 { page.hwnd } else { create_page(ctx, memory, sheet_hwnd, &sheet, index) };
        if hwnd == 0 {
            with_sheet(ctx, sheet_hwnd, |sheet| sheet.current = previous_index);
            return;
        }
        // PSN_SETACTIVE: 0 accepts, -1 skips the page, another value is the
        // resource id of the page to show instead.
        let answer = notify(ctx, memory, sheet_hwnd, hwnd, PSN_SETACTIVE) as i32;
        if answer != 0 {
            with_sheet(ctx, sheet_hwnd, |sheet| sheet.current = previous_index);
        }
        if answer == -1 {
            let next = if forward { index + 1 } else { index.wrapping_sub(1) };
            if next >= sheet.pages.len() {
                return;
            }
            index = next;
            continue;
        }
        if answer > 0 {
            if let Some(target) = sheet.pages.iter().position(|p| p.resource_id == answer as u32) {
                if target != index {
                    index = target;
                    continue;
                }
            }
        }
        let previous = sheet.pages.get(sheet.current).map(|p| p.hwnd).filter(|&h| h != 0 && h != hwnd);
        if let Some(previous) = previous {
            user::show_window(ctx, memory, previous, 0);
        }
        with_sheet(ctx, sheet_hwnd, |sheet| sheet.current = index);
        user::show_window(ctx, memory, hwnd, 5);
        if let Some(first) = controls::first_tab_stop(ctx, hwnd) {
            user::set_focus(ctx, memory, first);
        }
        return;
    }
}

fn create_page<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, sheet_hwnd: u32, sheet: &Sheet, index: usize) -> u32 {
    let page = &sheet.pages[index];
    let flags = rd(memory, page.psp + psp_field(ctx, 1));
    if flags & PSP_USECALLBACK != 0 {
        // PSPCB_CREATE: MFC hooks the page's window creation here.
        let callback = rd(memory, page.psp + psp_field(ctx, 8));
        if callback != 0 && memory.call_guest(callback, &[0, 2, page.psp]) == Some(0) {
            return 0;
        }
    }
    let Some(mut template) = controls::parse_template(memory, page.template) else {
        return 0;
    };
    // A page is a hidden child control-dialog in the sheet's page area.
    // (WS_POPUP, WS_CAPTION, WS_SYSMENU, WS_THICKFRAME, DS_MODALFRAME, DS_CENTER off;
    // WS_CHILD, DS_CONTROL, DS_3DLOOK on.)
    template.style = (template.style & !(0x8000_0000 | 0x00C0_0000 | 0x0008_0000 | 0x0004_0000 | 0x80 | 0x800 | 0x1000_0000))
        | 0x4000_0000
        | 0x400
        | 0x4;
    template.ex_style |= 0x0001_0000; // WS_EX_CONTROLPARENT
    template.x = sheet.page_x;
    template.y = sheet.page_y;
    let instance = rd(memory, page.psp + psp_field(ctx, 2));
    let instance = if instance != 0 { instance } else { sheet.instance };
    let hwnd = controls::create_dialog_from(ctx, memory, instance, &template, sheet_hwnd, page.dlg_proc, page.psp);
    with_sheet(ctx, sheet_hwnd, |sheet| sheet.pages[index].hwnd = hwnd);
    hwnd
}

fn current_page(ctx: &ApiContext, sheet_hwnd: u32) -> (usize, u32) {
    ctx.shared
        .borrow()
        .gui
        .property_sheets
        .get(&sheet_hwnd)
        .and_then(|sheet| sheet.pages.get(sheet.current).map(|page| (sheet.current, page.hwnd)))
        .unwrap_or((0, 0))
}

fn end<M: GuestMemory + ?Sized>(ctx: &ApiContext, _memory: &mut M, sheet_hwnd: u32, result: u32) {
    controls::end_dialog_with(ctx, sheet_hwnd, result);
}

/// The buttons, by PSBTN_* index: Back, Next, Finish, OK, Apply, Cancel.
fn press<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, sheet_hwnd: u32, button: u32) {
    let (index, page) = current_page(ctx, sheet_hwnd);
    let Some(sheet) = ctx.shared.borrow().gui.property_sheets.get(&sheet_hwnd).cloned() else {
        return;
    };
    let target_of = |answer: u32, default: usize| -> Option<usize> {
        match answer as i32 {
            -1 => None,
            0 => Some(default),
            id => sheet.pages.iter().position(|p| p.resource_id == id as u32),
        }
    };
    match button {
        0 => {
            // Back
            if let Some(target) = target_of(notify(ctx, memory, sheet_hwnd, page, PSN_WIZBACK), index.wrapping_sub(1)) {
                if target < sheet.pages.len() {
                    activate(ctx, memory, sheet_hwnd, target, false);
                }
            }
        }
        1 => {
            // Next
            if let Some(target) = target_of(notify(ctx, memory, sheet_hwnd, page, PSN_WIZNEXT), index + 1) {
                if target < sheet.pages.len() {
                    activate(ctx, memory, sheet_hwnd, target, true);
                }
            }
        }
        2 => {
            // Finish
            if notify(ctx, memory, sheet_hwnd, page, PSN_WIZFINISH) == 0 {
                end(ctx, memory, sheet_hwnd, 1);
            }
        }
        3 | 4 => {
            // OK, Apply
            if notify(ctx, memory, sheet_hwnd, page, PSN_KILLACTIVE) != 0 {
                return;
            }
            for page in sheet.pages.iter().filter(|p| p.hwnd != 0) {
                if notify(ctx, memory, sheet_hwnd, page.hwnd, PSN_APPLY) != 0 {
                    return;
                }
            }
            if button == 3 {
                end(ctx, memory, sheet_hwnd, 1);
            }
        }
        _ => {
            // Cancel
            if notify(ctx, memory, sheet_hwnd, page, PSN_QUERYCANCEL) != 0 {
                return;
            }
            for page in sheet.pages.iter().filter(|p| p.hwnd != 0) {
                notify(ctx, memory, sheet_hwnd, page.hwnd, PSN_RESET);
            }
            end(ctx, memory, sheet_hwnd, 0);
        }
    }
}

fn set_button<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, sheet_hwnd: u32, id: u32, visible: bool, enabled: bool) {
    let item = controls::dialog_item_of(ctx, sheet_hwnd, id);
    if item != 0 {
        user::show_window(ctx, memory, item, if visible { 5 } else { 0 });
        if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(item) {
            window.disabled = !enabled;
        }
        user::invalidate(&mut ctx.shared.borrow_mut().gui, item, None, true, true);
        ctx.shared.borrow_mut().gui.dirty = true;
    }
}

/// The sheet frame's dialog procedure.
pub fn sheet_proc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, msg: u32, wparam: u32, lparam: u32) -> u32 {
    let answer = |value: u32| {
        controls::set_msg_result(ctx, hwnd, value);
        1
    };
    let wide = ctx.shared.borrow().gui.property_sheets.get(&hwnd).map(|s| s.wide).unwrap_or(false);
    let wizard = ctx.shared.borrow().gui.property_sheets.get(&hwnd).map(|s| s.wizard).unwrap_or(true);
    match msg {
        WM_INITDIALOG => 0,
        WM_COMMAND if wparam >> 16 == 0 => {
            match wparam & 0xFFFF {
                ID_BACK => press(ctx, memory, hwnd, 0),
                ID_NEXT => press(ctx, memory, hwnd, 1),
                ID_FINISH => press(ctx, memory, hwnd, 2),
                // Enter presses the default button: Next (or Finish) in a wizard.
                IDOK if wizard => {
                    let finish = controls::dialog_item_of(ctx, hwnd, ID_FINISH);
                    let finish_shown = ctx.shared.borrow().gui.window(finish).is_some_and(|w| w.visible && !w.disabled);
                    press(ctx, memory, hwnd, if finish_shown { 2 } else { 1 });
                }
                IDOK => press(ctx, memory, hwnd, 3),
                ID_APPLY => press(ctx, memory, hwnd, 4),
                IDCANCEL => press(ctx, memory, hwnd, 5),
                _ => return 0,
            }
            1
        }
        // PSM_SETCURSEL
        0x465 => {
            let index = if lparam != 0 {
                ctx.shared.borrow().gui.property_sheets.get(&hwnd).and_then(|s| s.pages.iter().position(|p| p.psp == lparam))
            } else {
                Some(wparam as usize)
            };
            if let Some(index) = index {
                activate(ctx, memory, hwnd, index, true);
            }
            answer(1)
        }
        // PSM_SETCURSELID
        0x472 => {
            let index = ctx.shared.borrow().gui.property_sheets.get(&hwnd).and_then(|s| s.pages.iter().position(|p| p.resource_id == lparam));
            if let Some(index) = index {
                activate(ctx, memory, hwnd, index, true);
            }
            answer(u32::from(index.is_some()))
        }
        // PSM_SETWIZBUTTONS: PSWIZB_BACK 1, NEXT 2, FINISH 4, DISABLEDFINISH 8
        0x470 => {
            let finish = lparam & 0xC != 0;
            set_button(ctx, memory, hwnd, ID_BACK, true, lparam & 1 != 0);
            set_button(ctx, memory, hwnd, ID_NEXT, !finish, lparam & 2 != 0);
            set_button(ctx, memory, hwnd, ID_FINISH, finish, lparam & 4 != 0);
            answer(0)
        }
        // PSM_PRESSBUTTON
        0x471 => {
            press(ctx, memory, hwnd, wparam);
            answer(0)
        }
        // PSM_SETFINISHTEXTA/W: Finish replaces Back and Next.
        0x473 | 0x479 => {
            let label = text(memory, lparam, msg == 0x479);
            let item = controls::dialog_item_of(ctx, hwnd, ID_FINISH);
            if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(item) {
                window.title = crate::win32::unicode::ansi_to_string(&label);
            }
            set_button(ctx, memory, hwnd, ID_BACK, false, false);
            set_button(ctx, memory, hwnd, ID_NEXT, false, false);
            set_button(ctx, memory, hwnd, ID_FINISH, true, true);
            answer(0)
        }
        // PSM_SETTITLEA/W
        0x46F | 0x478 => {
            let title = text(memory, lparam, msg == 0x478);
            if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(hwnd) {
                window.title = crate::win32::unicode::ansi_to_string(&title);
                window.nc_dirty = true;
            }
            answer(0)
        }
        // PSM_GETCURRENTPAGEHWND
        0x476 => answer(current_page(ctx, hwnd).1),
        // PSM_ISDIALOGMESSAGE
        0x475 => {
            let result = controls::is_dialog_message_for(ctx, memory, hwnd, lparam);
            answer(result)
        }
        // PSM_QUERYSIBLINGS: forwarded to the pages until one answers.
        0x46C => {
            let pages: Vec<u32> = ctx.shared.borrow().gui.property_sheets.get(&hwnd).map(|s| s.pages.iter().map(|p| p.hwnd).filter(|&h| h != 0).collect()).unwrap_or_default();
            for page in pages {
                let result = user::send(ctx, memory, page, 0x46C, wparam, lparam);
                if result != 0 {
                    return answer(result);
                }
            }
            answer(0)
        }
        // PSM_CANCELTOCLOSE
        0x46B => {
            let item = controls::dialog_item_of(ctx, hwnd, IDCANCEL);
            if let Some(window) = ctx.shared.borrow_mut().gui.window_mut(item) {
                window.title = "Close".into();
            }
            answer(0)
        }
        // PSM_INDEXTOHWND, PSM_HWNDTOINDEX, PSM_INDEXTOPAGE, PSM_PAGETOINDEX, PSM_INDEXTOID, PSM_IDTOINDEX
        0x481..=0x485 => {
            let shared = ctx.shared.borrow();
            let pages = shared.gui.property_sheets.get(&hwnd).map(|s| s.pages.clone()).unwrap_or_default();
            drop(shared);
            let value = match msg {
                0x481 => pages.get(wparam as usize).map(|p| p.hwnd).unwrap_or(0),
                0x482 => pages.iter().position(|p| p.hwnd == wparam).map(|i| i as u32).unwrap_or(u32::MAX),
                0x483 => pages.get(wparam as usize).map(|p| p.psp).unwrap_or(0),
                0x484 => pages.iter().position(|p| p.psp == lparam).map(|i| i as u32).unwrap_or(u32::MAX),
                _ => pages.get(wparam as usize).map(|p| p.resource_id).unwrap_or(0),
            };
            answer(value)
        }
        // PSM_GETTABCONTROL
        0x474 => answer(controls::dialog_item_of(ctx, hwnd, ID_TABS)),
        // PSM_CHANGED, PSM_UNCHANGED, PSM_SETHEADERTITLE, PSM_SETHEADERSUBTITLE, PSM_RECALCPAGESIZES
        0x468 | 0x46D | 0x47D..=0x480 | 0x488 => answer(0),
        WM_DESTROY => {
            let _ = wide;
            0
        }
        _ => 0,
    }
}

/// SysTabControl32. Property sheets keep one (hidden in wizards); tabs are
/// not drawn yet, so it answers TCM_* queries as an empty control.
pub fn tab_proc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, msg: u32, wparam: u32, lparam: u32) -> u32 {
    let parent = ctx.shared.borrow().gui.window(hwnd).map(|w| w.parent).unwrap_or(0);
    if !ctx.shared.borrow().gui.property_sheets.contains_key(&parent) {
        return crate::win32::tabs::tab_proc(ctx, memory, hwnd, msg, wparam, lparam);
    }
    match msg {
        // The sheet's page count and current page.
        0x1304 | 0x130B => {
            let parent = ctx.shared.borrow().gui.window(hwnd).map(|w| w.parent).unwrap_or(0);
            let shared = ctx.shared.borrow();
            match shared.gui.property_sheets.get(&parent) {
                Some(sheet) if msg == 0x1304 => sheet.pages.len() as u32,
                Some(sheet) if sheet.current < sheet.pages.len() => sheet.current as u32,
                _ => if msg == 0x1304 { 0 } else { u32::MAX },
            }
        }
        // TCM_SETCURSEL and the rest
        0x1300..=0x13FF => 0,
        _ => user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

// ── List view (SysListView32) ───────────────────────────────────────────

const ROW_HEIGHT: i32 = 16;
const HEADER_HEIGHT: i32 = 18;
const LVS_EX_CHECKBOXES: u32 = 0x4;
const LVS_NOCOLUMNHEADER: u32 = 0x4000;
const LVIS_FOCUSED: u32 = 0x1;
const LVIS_SELECTED: u32 = 0x2;
const LVIS_STATEIMAGEMASK: u32 = 0xF000;
const LVN_ITEMCHANGED: u32 = -101i32 as u32;
const NM_CLICK: u32 = -2i32 as u32;
const NM_DBLCLK: u32 = -3i32 as u32;
const NM_RETURN: u32 = -4i32 as u32;
/// An item opened: double-clicked, or Enter pressed on it.
const LVN_ITEMACTIVATE: u32 = -114i32 as u32;
/// A key went down in the list (NMLVKEYDOWN: the virtual key).
const LVN_KEYDOWN: u32 = -155i32 as u32;
const LVN_GETDISPINFOA: u32 = -150i32 as u32;
const LVN_GETDISPINFOW: u32 = -177i32 as u32;
/// The list asks its parent for every item (LVM_SETITEMCOUNT says how many).
const LVS_OWNERDATA: u32 = 0x1000;
/// `pszText` of an item whose text the parent gives when asked.
const LPSTR_TEXTCALLBACK: u32 = u32::MAX;

#[derive(Clone, Debug, Default)]
struct ListItem {
    /// Text of the item and its subitems.
    texts: Vec<Vec<u8>>,
    /// Which of them the parent gives when asked (LVN_GETDISPINFO): bit
    /// `n` for subitem `n`. Subitems never given a text are asked for too,
    /// as Windows does.
    callback: u64,
    /// Which subitems were given a text of their own.
    given: u64,
    state: u32,
    lparam: u32,
}

impl ListItem {
    /// Set subitem `sub`'s text, or make the parent give it (None).
    fn set_text(&mut self, sub: usize, value: Option<Vec<u8>>) {
        if self.texts.len() <= sub {
            self.texts.resize(sub + 1, Vec::new());
        }
        let bit = 1u64 << sub.min(63);
        match value {
            Some(value) => {
                self.texts[sub] = value;
                self.callback &= !bit;
                self.given |= bit;
            }
            None => {
                self.texts[sub].clear();
                self.callback |= bit;
                self.given &= !bit;
            }
        }
    }

    fn is_callback(&self, sub: usize) -> bool {
        let bit = 1u64 << sub.min(63);
        self.callback & bit != 0 || (sub > 0 && self.given & bit == 0)
    }
}

/// A report-view list view: columns, rows, checkboxes and one selection.
#[derive(Clone, Debug, Default)]
pub struct ListView {
    columns: Vec<(Vec<u8>, i32)>,
    items: Vec<ListItem>,
    ex_style: u32,
    top: usize,
}

fn with_list<R>(ctx: &ApiContext, hwnd: u32, f: impl FnOnce(&mut ListView) -> R) -> R {
    let mut shared = ctx.shared.borrow_mut();
    f(shared.gui.list_views.entry(hwnd).or_default())
}

/// Read an LVITEM's text (pszText at 20), in either character set.
fn item_text<M: GuestMemory + ?Sized>(memory: &mut M, item: u32, wide: bool) -> Option<Vec<u8>> {
    let va = lvi(memory, item, 5);
    (va >= 0x10000 && va != LPSTR_TEXTCALLBACK).then(|| text(memory, va, wide))
}

/// An LVITEM's text as the list keeps it: `None` for LPSTR_TEXTCALLBACK
/// (the parent gives it when asked), else the text (empty without one).
fn item_text_or_callback<M: GuestMemory + ?Sized>(memory: &mut M, item: u32, wide: bool) -> Option<Vec<u8>> {
    if lvi(memory, item, 5) == LPSTR_TEXTCALLBACK {
        None
    } else {
        Some(item_text(memory, item, wide).unwrap_or_default())
    }
}

/// Subitem `sub` of item `index`'s text: kept, or asked of the parent
/// (LVN_GETDISPINFO, in the parent's character set).
fn display_text<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, index: usize, sub: usize) -> Vec<u8> {
    let Some(item) = with_list(ctx, hwnd, |list| list.items.get(index).cloned()) else {
        return Vec::new();
    };
    if !item.is_callback(sub) {
        return item.texts.get(sub).cloned().unwrap_or_default();
    }
    let (parent, id) = match ctx.shared.borrow().gui.window(hwnd) {
        Some(window) if window.parent != 0 => (window.parent, window.id),
        _ => return Vec::new(),
    };
    let wide = crate::win32::controls::notifies_wide(ctx, parent);
    // NMLVDISPINFO: NMHDR, then an LVITEM; and a buffer for the text.
    const CAPACITY: u32 = 260;
    let x64 = memory.ptr_size() == 8;
    let header = crate::win32::x64::NMHDR.size(x64);
    let size = header + crate::win32::x64::LVITEM.size(x64);
    let block = ctx.shared.borrow_mut().heap.alloc(size + CAPACITY * 2).unwrap_or(0);
    if block == 0 {
        return Vec::new();
    }
    let _ = memory.write_bytes(block, &vec![0u8; (size + CAPACITY * 2) as usize]);
    let buffer = block + size;
    let nmhdr = crate::win32::x64::View::new(memory, &crate::win32::x64::NMHDR, block);
    for (field, value) in [hwnd, id, if wide { LVN_GETDISPINFOW } else { LVN_GETDISPINFOA }].into_iter().enumerate() {
        nmhdr.set(memory, field, value);
    }
    let lvitem = block + header;
    for (field, value) in [(0, 1), (1, index as u32), (2, sub as u32), (5, buffer), (6, CAPACITY), (8, item.lparam)] {
        set_lvi(memory, lvitem, field, value);
    }
    user::send(ctx, memory, parent, WM_NOTIFY, id, block);
    // The parent may point pszText at text of its own.
    let va = lvi(memory, lvitem, 5);
    let value = if va == LPSTR_TEXTCALLBACK { Vec::new() } else { text(memory, va, wide) };
    ctx.shared.borrow_mut().heap.free(block);
    value
}

/// LVITEM field `index` in the process's layout: mask, iItem, iSubItem,
/// state, stateMask, pszText, cchTextMax, iImage, lParam, ...
fn lvi<M: GuestMemory + ?Sized>(memory: &mut M, item: u32, index: usize) -> u32 {
    crate::win32::x64::View::new(memory, &crate::win32::x64::LVITEM, item).get32(memory, index)
}

/// LVCOLUMN field `index`: mask, fmt, cx, pszText, ...
fn lvc<M: GuestMemory + ?Sized>(memory: &mut M, column: u32, index: usize) -> u32 {
    crate::win32::x64::View::new(memory, &crate::win32::x64::LVCOLUMN, column).get32(memory, index)
}

fn set_lvi<M: GuestMemory + ?Sized>(memory: &mut M, item: u32, index: usize, value: u32) {
    crate::win32::x64::View::new(memory, &crate::win32::x64::LVITEM, item).set(memory, index, value);
}

/// Write `value` into a caller buffer of `capacity` characters.
fn put_text<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, capacity: u32, value: &[u8], wide: bool) -> u32 {
    if va == 0 || capacity == 0 {
        return 0;
    }
    let count = value.len().min(capacity as usize - 1);
    if wide {
        unicode::write_wide(memory, va, &unicode::ansi_to_wide(&value[..count]), true);
    } else {
        let mut bytes = value[..count].to_vec();
        bytes.push(0);
        let _ = memory.write_bytes(va, &bytes);
    }
    count as u32
}

/// Tell the parent about a list view event (WM_NOTIFY with NMLISTVIEW).
fn list_notify<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, code: u32, item: i32, new_state: u32, old_state: u32) {
    let (parent, id) = match ctx.shared.borrow().gui.window(hwnd) {
        Some(window) => (window.parent, window.id),
        None => return,
    };
    if parent == 0 {
        return;
    }
    let lparam = with_list(ctx, hwnd, |list| list.items.get(item.max(0) as usize).map(|i| i.lparam).unwrap_or(0));
    // NMLISTVIEW in the process's layout.
    let block = ctx.shared.borrow_mut().heap.alloc(64).unwrap_or(0);
    let view = crate::win32::x64::View::new(memory, &crate::win32::x64::NMLISTVIEW, block);
    for (index, value) in [hwnd, id, code, item as u32, 0, new_state, old_state, if code == LVN_ITEMCHANGED { 8 } else { 0 }, 0, 0, lparam]
        .into_iter()
        .enumerate()
    {
        view.set(memory, index, value);
    }
    user::send(ctx, memory, parent, WM_NOTIFY, id, block);
    ctx.shared.borrow_mut().heap.free(block);
}

/// Send LVN_KEYDOWN (NMLVKEYDOWN: NMHDR, WORD wVKey, UINT flags).
fn list_key_down<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, key: u32) {
    let (parent, id) = match ctx.shared.borrow().gui.window(hwnd) {
        Some(window) if window.parent != 0 => (window.parent, window.id),
        _ => return,
    };
    let x64 = memory.ptr_size() == 8;
    let header = crate::win32::x64::NMHDR.size(x64);
    let block = ctx.shared.borrow_mut().heap.alloc(header + 8).unwrap_or(0);
    if block == 0 {
        return;
    }
    let _ = memory.write_bytes(block, &vec![0u8; (header + 8) as usize]);
    let nmhdr = crate::win32::x64::View::new(memory, &crate::win32::x64::NMHDR, block);
    for (field, value) in [hwnd, id, LVN_KEYDOWN].into_iter().enumerate() {
        nmhdr.set(memory, field, value);
    }
    let _ = memory.write_bytes(block + header, &(key as u16).to_le_bytes());
    user::send(ctx, memory, parent, WM_NOTIFY, id, block);
    ctx.shared.borrow_mut().heap.free(block);
}

/// Change an item's state bits, notifying the parent.
fn set_state<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, index: usize, state: u32, mask: u32) {
    let change = with_list(ctx, hwnd, |list| {
        // Selecting one item clears the others (single selection).
        let mut changes = Vec::new();
        if mask & state & LVIS_SELECTED != 0 {
            for (i, item) in list.items.iter_mut().enumerate() {
                if i != index && item.state & (LVIS_SELECTED | LVIS_FOCUSED) != 0 {
                    let old = item.state;
                    item.state &= !(LVIS_SELECTED | LVIS_FOCUSED);
                    changes.push((i, item.state, old));
                }
            }
        }
        if let Some(item) = list.items.get_mut(index) {
            let old = item.state;
            item.state = (item.state & !mask) | (state & mask);
            if item.state != old {
                changes.push((index, item.state, old));
            }
        }
        changes
    });
    for (index, new, old) in change {
        list_notify(ctx, memory, hwnd, LVN_ITEMCHANGED, index as i32, new, old);
    }
    invalidate_control(ctx, hwnd);
}

fn invalidate_control(ctx: &ApiContext, hwnd: u32) {
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

fn header_height(ctx: &ApiContext, hwnd: u32) -> i32 {
    let style = ctx.shared.borrow().gui.window(hwnd).map(|w| w.style).unwrap_or(0);
    if style & LVS_NOCOLUMNHEADER != 0 { 0 } else { HEADER_HEIGHT }
}

fn visible_rows(ctx: &ApiContext, hwnd: u32) -> usize {
    let (_, height) = client_size(ctx, hwnd);
    ((height - header_height(ctx, hwnd)) / ROW_HEIGHT).max(1) as usize
}

fn ensure_visible(ctx: &ApiContext, hwnd: u32, index: usize) {
    let rows = visible_rows(ctx, hwnd);
    with_list(ctx, hwnd, |list| {
        if index < list.top {
            list.top = index;
        } else if index >= list.top + rows {
            list.top = index + 1 - rows;
        }
    });
}

fn paint_list<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32) {
    let ps = user::scratch(ctx, 80);
    let hdc = user::begin_paint(ctx, memory, hwnd, ps);
    let mut list = with_list(ctx, hwnd, |list| list.clone());
    let (width, height) = client_size(ctx, hwnd);
    let header = header_height(ctx, hwnd);
    let rows = visible_rows(ctx, hwnd);
    // The parent gives the texts it keeps itself (only the rows shown).
    let columns = list.columns.len().max(1);
    for index in list.top..(list.top + rows).min(list.items.len()) {
        {
            for sub in 0..columns {
                if list.items[index].is_callback(sub) {
                    let value = display_text(ctx, memory, hwnd, index, sub);
                    let item = &mut list.items[index];
                    if item.texts.len() <= sub {
                        item.texts.resize(sub + 1, Vec::new());
                    }
                    item.texts[sub] = value;
                }
            }
        }
    }
    let focused = ctx.shared.borrow().gui.focus_hwnd == hwnd;
    let checkboxes = list.ex_style & LVS_EX_CHECKBOXES != 0;
    let scrollbar = list.items.len() > rows;
    let list_width = if scrollbar { width - crate::win32::user::SCROLL_SIZE } else { width };
    let mut shared = ctx.shared.borrow_mut();
    user::with_painter(&mut shared, hwnd, true, |p| {
        use crate::win32::{gdi::sys_color, gfx::Rect};
        p.fill(Rect::new(0, 0, width, height), sys_color(5));
        // Column headers.
        let mut x = 0;
        if header > 0 {
            for (title, column_width) in &list.columns {
                let cell = Rect::new(x, 0, (x + column_width).min(list_width), header);
                p.fill(cell, sys_color(15));
                p.raised_button(cell);
                p.text(x + 5, 2, title, sys_color(18), false);
                x += column_width;
            }
            if x < list_width {
                let rest = Rect::new(x, 0, list_width, header);
                p.fill(rest, sys_color(15));
                p.raised_button(rest);
            }
        }
        for (row, item) in list.items.iter().enumerate().skip(list.top).take(rows) {
            let y = header + (row - list.top) as i32 * ROW_HEIGHT;
            let selected = item.state & LVIS_SELECTED != 0;
            let mut x = 2;
            if checkboxes {
                let check = Rect::sized(x, y + 1, 13, 13);
                p.fill(check, 0xFFFFFF);
                p.sunken(check);
                if (item.state & LVIS_STATEIMAGEMASK) >> 12 == 2 {
                    let (cx, cy) = (check.left + 3, check.top + 5);
                    for i in 0..3 {
                        p.vline(cx + i, cy + i, cy + 3 + i, 0x000000);
                    }
                    for i in 0..4 {
                        p.vline(cx + 3 + i, cy + 1 - i, cy + 4 - i, 0x000000);
                    }
                }
                x += 17;
            }
            let first_width = list.columns.first().map(|c| c.1).unwrap_or(list_width) - x;
            let label = item.texts.first().cloned().unwrap_or_default();
            let label_width = crate::win32::user::text_width(&label) + 4;
            let label_rect = Rect::new(x, y, x + label_width.min(first_width.max(0)), y + ROW_HEIGHT);
            if selected {
                p.fill(label_rect, if focused { sys_color(13) } else { sys_color(15) });
            }
            // Each column's text is clipped to the column.
            let clip_to = |p: &mut crate::win32::user::Painter<'_>, rect: Rect, paint: &dyn Fn(&mut crate::win32::user::Painter<'_>)| {
                let narrowed = p.clip.intersect_rect(&rect.offset(p.ox, p.oy));
                let saved = std::mem::replace(&mut p.clip, narrowed);
                paint(p);
                p.clip = saved;
            };
            let first_end = list.columns.first().map(|c| c.1).unwrap_or(list_width);
            let color = if selected && focused { sys_color(14) } else { sys_color(8) };
            clip_to(p, Rect::new(x, y, first_end - 2, y + ROW_HEIGHT), &|p| {
                p.text(x + 2, y + 1, &label, color, false);
            });
            let mut column_x = first_end;
            for (index, (_, column_width)) in list.columns.iter().enumerate().skip(1) {
                if let Some(sub) = item.texts.get(index) {
                    let start = column_x;
                    clip_to(p, Rect::new(start, y, start + column_width - 2, y + ROW_HEIGHT), &|p| {
                        p.text(start + 6, y + 1, sub, sys_color(8), false);
                    });
                }
                column_x += column_width;
            }
        }
        if scrollbar {
            let bar = Rect::new(list_width, 0, width, height);
            crate::win32::user::draw_scroll_bar(p, bar, true, 0, list.items.len() as i32 - 1, list.top as i32, rows as u32);
        }
    });
    drop(shared);
    user::end_paint(ctx, hwnd, hdc);
    user::free_scratch(ctx, ps);
}

/// The SysListView32 window procedure (report view).
pub fn listview_proc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hwnd: u32, msg: u32, wparam: u32, lparam: u32) -> u32 {
    let count = || with_list(ctx, hwnd, |list| list.items.len());
    match msg {
        // LVM_GETITEMCOUNT
        0x1004 => count() as u32,
        // LVM_INSERTITEMA/W
        0x1007 | 0x104D => {
            let wide = msg == 0x104D;
            let mask = lvi(memory, lparam, 0);
            let index = lvi(memory, lparam, 1) as usize;
            let label = if mask & 1 != 0 { item_text_or_callback(memory, lparam, wide) } else { Some(Vec::new()) };
            let state_mask = lvi(memory, lparam, 4);
            let state = if mask & 8 != 0 { lvi(memory, lparam, 3) & state_mask } else { 0 };
            let item_lparam = if mask & 4 != 0 { lvi(memory, lparam, 8) } else { 0 };
            let at = with_list(ctx, hwnd, |list| {
                let at = index.min(list.items.len());
                let mut state = state;
                if list.ex_style & LVS_EX_CHECKBOXES != 0 && state & LVIS_STATEIMAGEMASK == 0 {
                    state |= 1 << 12; // unchecked
                }
                let mut item = ListItem { state, lparam: item_lparam, ..ListItem::default() };
                item.set_text(0, label);
                list.items.insert(at, item);
                at
            });
            invalidate_control(ctx, hwnd);
            at as u32
        }
        // LVM_SETITEMA/W, LVM_GETITEMA/W
        0x1006 | 0x104C | 0x1005 | 0x104B => {
            let wide = msg == 0x104C || msg == 0x104B;
            let mask = lvi(memory, lparam, 0);
            let index = lvi(memory, lparam, 1) as usize;
            let sub = lvi(memory, lparam, 2) as usize;
            if index >= count() {
                return 0;
            }
            if msg == 0x1006 || msg == 0x104C {
                if mask & 1 != 0 {
                    let value = item_text_or_callback(memory, lparam, wide);
                    with_list(ctx, hwnd, |list| list.items[index].set_text(sub, value));
                }
                if mask & 4 != 0 {
                    let value = lvi(memory, lparam, 8);
                    with_list(ctx, hwnd, |list| list.items[index].lparam = value);
                }
                if mask & 8 != 0 {
                    let (state, state_mask) = (lvi(memory, lparam, 3), lvi(memory, lparam, 4));
                    set_state(ctx, memory, hwnd, index, state, state_mask);
                }
                invalidate_control(ctx, hwnd);
            } else {
                let item = with_list(ctx, hwnd, |list| list.items[index].clone());
                if mask & 1 != 0 {
                    let (va, capacity) = (lvi(memory, lparam, 5), lvi(memory, lparam, 6));
                    let value = display_text(ctx, memory, hwnd, index, sub);
                    put_text(memory, va, capacity, &value, wide);
                }
                if mask & 4 != 0 {
                    set_lvi(memory, lparam, 8, item.lparam);
                }
                if mask & 8 != 0 {
                    let state_mask = lvi(memory, lparam, 4);
                    set_lvi(memory, lparam, 3, item.state & state_mask);
                }
            }
            1
        }
        // LVM_DELETEITEM, LVM_DELETEALLITEMS
        0x1008 => {
            let removed = with_list(ctx, hwnd, |list| (wparam as usize) < list.items.len() && { list.items.remove(wparam as usize); true });
            invalidate_control(ctx, hwnd);
            u32::from(removed)
        }
        0x1009 => {
            with_list(ctx, hwnd, |list| {
                list.items.clear();
                list.top = 0;
            });
            invalidate_control(ctx, hwnd);
            1
        }
        // LVM_GETNEXTITEM(start, flags): LVNI_ALL 0, LVNI_FOCUSED 1, LVNI_SELECTED 2
        0x100C => with_list(ctx, hwnd, |list| {
            let start = if wparam as i32 == -1 { 0 } else { wparam as usize + 1 };
            let wanted = lparam & 3;
            list.items
                .iter()
                .enumerate()
                .skip(start)
                .find(|(_, item)| wanted == 0 || item.state & wanted != 0)
                .map(|(i, _)| i as u32)
                .unwrap_or(u32::MAX)
        }),
        // LVM_ENSUREVISIBLE
        0x1013 => {
            ensure_visible(ctx, hwnd, wparam as usize);
            invalidate_control(ctx, hwnd);
            1
        }
        // LVM_INSERTCOLUMNA/W
        0x101B | 0x1061 => {
            let wide = msg == 0x1061;
            let mask = lvc(memory, lparam, 0);
            let width = if mask & 2 != 0 { lvc(memory, lparam, 2) as i32 } else { 80 };
            let title_va = lvc(memory, lparam, 3);
            let title = if mask & 4 != 0 { text(memory, title_va, wide) } else { Vec::new() };
            let at = with_list(ctx, hwnd, |list| {
                let at = (wparam as usize).min(list.columns.len());
                list.columns.insert(at, (title, width));
                at
            });
            invalidate_control(ctx, hwnd);
            at as u32
        }
        // LVM_GETCOLUMNA/W: what the mask asks of column wparam.
        0x1019 | 0x105F => {
            let Some((title, width)) = with_list(ctx, hwnd, |list| list.columns.get(wparam as usize).cloned()) else {
                return 0;
            };
            let mask = lvc(memory, lparam, 0);
            let view = crate::win32::x64::View::new(memory, &crate::win32::x64::LVCOLUMN, lparam);
            if mask & 0x1 != 0 {
                view.set(memory, 1, 0); // LVCF_FMT: LVCFMT_LEFT
            }
            if mask & 0x2 != 0 {
                view.set(memory, 2, width as u32); // LVCF_WIDTH
            }
            if mask & 0x4 != 0 {
                let (va, capacity) = (lvc(memory, lparam, 3), lvc(memory, lparam, 4));
                put_text(memory, va, capacity, &title, msg == 0x105F); // LVCF_TEXT
            }
            if mask & 0x8 != 0 {
                view.set(memory, 5, wparam); // LVCF_SUBITEM
            }
            if mask & 0x20 != 0 {
                view.set(memory, 7, wparam); // LVCF_ORDER
            }
            1
        }
        // LVM_SETCOLUMNA/W: the title and width.
        0x101A | 0x1060 => {
            let mask = lvc(memory, lparam, 0);
            let title_va = lvc(memory, lparam, 3);
            let title = (mask & 0x4 != 0).then(|| text(memory, title_va, msg == 0x1060));
            let width = (mask & 0x2 != 0).then(|| lvc(memory, lparam, 2) as i32);
            let found = with_list(ctx, hwnd, |list| {
                let Some(column) = list.columns.get_mut(wparam as usize) else { return false };
                if let Some(title) = title {
                    column.0 = title;
                }
                if let Some(width) = width {
                    column.1 = width;
                }
                true
            });
            invalidate_control(ctx, hwnd);
            u32::from(found)
        }
        // LVM_GETCOLUMNORDERARRAY(count, int[]): columns stay in order.
        0x103B => {
            let columns = with_list(ctx, hwnd, |list| list.columns.len());
            for index in 0..(wparam as usize).min(columns) {
                let _ = memory.write_u32(lparam + 4 * index as u32, index as u32);
            }
            u32::from(wparam as usize <= columns)
        }
        0x103A => 1, // LVM_SETCOLUMNORDERARRAY
        // LVM_DELETECOLUMN
        0x101C => u32::from(with_list(ctx, hwnd, |list| (wparam as usize) < list.columns.len() && { list.columns.remove(wparam as usize); true })),
        // LVM_GETCOLUMNWIDTH, LVM_SETCOLUMNWIDTH (LVSCW_AUTOSIZE* fill the client)
        0x101D => with_list(ctx, hwnd, |list| list.columns.get(wparam as usize).map(|c| c.1 as u32).unwrap_or(0)),
        0x101E => {
            let full = client_size(ctx, hwnd).0;
            with_list(ctx, hwnd, |list| {
                let others: i32 = list.columns.iter().enumerate().filter(|(i, _)| *i != wparam as usize).map(|(_, c)| c.1).sum();
                if let Some(column) = list.columns.get_mut(wparam as usize) {
                    column.1 = if (lparam as i32) < 0 { (full - others).max(40) } else { lparam as i32 };
                }
            });
            invalidate_control(ctx, hwnd);
            1
        }
        // LVM_SETITEMSTATE (index -1: all), LVM_GETITEMSTATE
        0x102B => {
            let (state, mask) = (lvi(memory, lparam, 3), lvi(memory, lparam, 4));
            let indices: Vec<usize> = if wparam as i32 == -1 { (0..count()).collect() } else { vec![wparam as usize] };
            for index in indices {
                set_state(ctx, memory, hwnd, index, state, mask);
            }
            1
        }
        0x102C => with_list(ctx, hwnd, |list| list.items.get(wparam as usize).map(|i| i.state & lparam).unwrap_or(0)),
        // LVM_GETITEMTEXTA/W, LVM_SETITEMTEXTA/W
        0x102D | 0x1073 => {
            let sub = lvi(memory, lparam, 2) as usize;
            let value = display_text(ctx, memory, hwnd, wparam as usize, sub);
            let (va, capacity) = (lvi(memory, lparam, 5), lvi(memory, lparam, 6));
            put_text(memory, va, capacity, &value, msg == 0x1073)
        }
        0x102E | 0x1074 => {
            let sub = lvi(memory, lparam, 2) as usize;
            let value = item_text_or_callback(memory, lparam, msg == 0x1074);
            with_list(ctx, hwnd, |list| {
                if let Some(item) = list.items.get_mut(wparam as usize) {
                    item.set_text(sub, value);
                }
            });
            invalidate_control(ctx, hwnd);
            1
        }
        // LVM_SETEXTENDEDLISTVIEWSTYLE(mask, style), LVM_GETEXTENDEDLISTVIEWSTYLE
        0x1036 => {
            let old = with_list(ctx, hwnd, |list| {
                let old = list.ex_style;
                let mask = if wparam == 0 { u32::MAX } else { wparam };
                list.ex_style = (list.ex_style & !mask) | (lparam & mask);
                if list.ex_style & LVS_EX_CHECKBOXES != 0 {
                    for item in &mut list.items {
                        if item.state & LVIS_STATEIMAGEMASK == 0 {
                            item.state |= 1 << 12;
                        }
                    }
                }
                old
            });
            invalidate_control(ctx, hwnd);
            old
        }
        0x1037 => with_list(ctx, hwnd, |list| list.ex_style),
        // LVM_GETSELECTEDCOUNT, LVM_GETTOPINDEX, LVM_GETCOUNTPERPAGE
        0x1032 => with_list(ctx, hwnd, |list| list.items.iter().filter(|i| i.state & LVIS_SELECTED != 0).count() as u32),
        0x1027 => with_list(ctx, hwnd, |list| list.top as u32),
        0x1028 => visible_rows(ctx, hwnd) as u32,
        // LVM_SETITEMCOUNT: a list that asks its parent for its items
        // (LVS_OWNERDATA) has that many; others only make room.
        0x102F => {
            let style = ctx.shared.borrow().gui.window(hwnd).map(|w| w.style).unwrap_or(0);
            if style & LVS_OWNERDATA != 0 {
                with_list(ctx, hwnd, |list| {
                    let callback = ListItem { callback: u64::MAX, ..ListItem::default() };
                    list.items.resize(wparam as usize, callback);
                    list.top = list.top.min(list.items.len().saturating_sub(1));
                });
                invalidate_control(ctx, hwnd);
            }
            1
        }
        // LVM_SETBKCOLOR, LVM_SETTEXTCOLOR, LVM_SETTEXTBKCOLOR, LVM_SETIMAGELIST,
        // LVM_REDRAWITEMS, LVM_UPDATE, LVM_SORTITEMS: accepted.
        0x1001 | 0x1024 | 0x1026 | 0x1003 | 0x1015 | 0x102A | 0x1030 => {
            if msg == 0x1015 || msg == 0x102A {
                invalidate_control(ctx, hwnd);
            }
            1
        }
        // LVM_GETHEADER, LVM_GETIMAGELIST and the rest: none.
        0x1000..=0x10FF => 0,
        0x000F => {
            paint_list(ctx, memory, hwnd);
            0
        }
        0x0014 => 1, // WM_ERASEBKGND
        // WM_LBUTTONDOWN / WM_LBUTTONDBLCLK: select; the checkbox toggles.
        0x0201 | 0x0203 => {
            user::set_focus(ctx, memory, hwnd);
            let (x, y) = ((lparam & 0xFFFF) as i16 as i32, (lparam >> 16) as i16 as i32);
            let header = header_height(ctx, hwnd);
            let (width, _) = client_size(ctx, hwnd);
            let rows = visible_rows(ctx, hwnd);
            let (top, total) = with_list(ctx, hwnd, |list| (list.top, list.items.len()));
            if total > rows && x >= width - crate::win32::user::SCROLL_SIZE {
                // The scroll bar: arrows step, the track pages.
                let step: isize = if y < header + 16 { -1 } else if y > header + (rows as i32 * ROW_HEIGHT) - 16 { 1 } else if y < header + (rows as i32 * ROW_HEIGHT) / 2 { -(rows as isize) } else { rows as isize };
                with_list(ctx, hwnd, |list| list.top = (list.top as isize + step).clamp(0, (total - rows) as isize) as usize);
                invalidate_control(ctx, hwnd);
                return 0;
            }
            if y < header {
                return 0;
            }
            let index = top + ((y - header) / ROW_HEIGHT) as usize;
            if index >= total {
                return 0;
            }
            let checkboxes = with_list(ctx, hwnd, |list| list.ex_style & LVS_EX_CHECKBOXES != 0);
            if checkboxes && x < 18 {
                let state = with_list(ctx, hwnd, |list| list.items[index].state);
                let checked = (state & LVIS_STATEIMAGEMASK) >> 12 == 2;
                set_state(ctx, memory, hwnd, index, if checked { 1 << 12 } else { 2 << 12 }, LVIS_STATEIMAGEMASK);
            }
            set_state(ctx, memory, hwnd, index, LVIS_SELECTED | LVIS_FOCUSED, LVIS_SELECTED | LVIS_FOCUSED);
            list_notify(ctx, memory, hwnd, if msg == 0x0203 { NM_DBLCLK } else { NM_CLICK }, index as i32, 0, 0);
            if msg == 0x0203 {
                list_notify(ctx, memory, hwnd, LVN_ITEMACTIVATE, index as i32, 0, 0);
            }
            0
        }
        // WM_MOUSEWHEEL
        0x020A => {
            let delta = (wparam >> 16) as i16 as i32;
            let rows = visible_rows(ctx, hwnd);
            with_list(ctx, hwnd, |list| {
                let max = list.items.len().saturating_sub(rows) as i32;
                list.top = (list.top as i32 - delta / 40).clamp(0, max) as usize;
            });
            invalidate_control(ctx, hwnd);
            0
        }
        // WM_KEYDOWN: the parent hears of every key (LVN_KEYDOWN: 7-Zip
        // goes up a folder on Backspace); arrows move, Space toggles the
        // checkbox, Enter opens.
        0x0100 => {
            list_key_down(ctx, memory, hwnd, wparam);
            let total = count();
            if total == 0 {
                return 0;
            }
            let current = with_list(ctx, hwnd, |list| list.items.iter().position(|i| i.state & LVIS_SELECTED != 0));
            // Enter opens the selected item.
            if wparam == 0x0D {
                list_notify(ctx, memory, hwnd, NM_RETURN, current.map_or(-1, |c| c as i32), 0, 0);
                if let Some(index) = current {
                    list_notify(ctx, memory, hwnd, LVN_ITEMACTIVATE, index as i32, 0, 0);
                }
                return 0;
            }
            let next = match wparam {
                0x26 => current.map(|c| c.saturating_sub(1)).unwrap_or(0),
                0x28 => current.map(|c| (c + 1).min(total - 1)).unwrap_or(0),
                0x24 => 0,
                0x23 => total - 1,
                0x20 => {
                    if let Some(index) = current {
                        let state = with_list(ctx, hwnd, |list| list.items[index].state);
                        let checked = (state & LVIS_STATEIMAGEMASK) >> 12 == 2;
                        set_state(ctx, memory, hwnd, index, if checked { 1 << 12 } else { 2 << 12 }, LVIS_STATEIMAGEMASK);
                    }
                    return 0;
                }
                _ => return user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
            };
            set_state(ctx, memory, hwnd, next, LVIS_SELECTED | LVIS_FOCUSED, LVIS_SELECTED | LVIS_FOCUSED);
            ensure_visible(ctx, hwnd, next);
            0
        }
        // WM_GETDLGCODE: the arrow keys belong to the list.
        0x0087 => 0x1,
        0x0007 | 0x0008 => {
            invalidate_control(ctx, hwnd);
            user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam)
        }
        0x0002 => {
            ctx.shared.borrow_mut().gui.list_views.remove(&hwnd);
            0
        }
        _ => user::def_window_proc(ctx, memory, hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    #[derive(Default)]
    struct Sparse(std::collections::BTreeMap<u32, u8>);
    impl GuestMemory for Sparse {
        fn read_bytes(&mut self, va: u32, len: usize) -> Option<Vec<u8>> {
            Some((0..len as u32).map(|i| self.0.get(&(va + i)).copied().unwrap_or(0)).collect())
        }
        fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Option<()> {
            for (i, &b) in bytes.iter().enumerate() {
                self.0.insert(va + i as u32, b);
            }
            Some(())
        }
        fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
            self.write_bytes(va, &value.to_le_bytes())
        }
    }

    fn list(ctx: &ApiContext, style: u32) -> u32 {
        let hwnd = 0x200;
        ctx.shared.borrow_mut().gui.windows.push(crate::win32::GuestWindow {
            hwnd,
            class: "SysListView32".into(),
            style,
            width: 300,
            height: 200,
            ..Default::default()
        });
        hwnd
    }

    #[test]
    fn callback_texts_are_kept_apart() {
        let mut item = ListItem::default();
        item.set_text(0, None);
        item.set_text(2, Some(b"size".to_vec()));
        assert!(item.is_callback(0));
        assert!(!item.is_callback(2));
        assert_eq!(item.texts[2], b"size");
        item.set_text(0, Some(b"name".to_vec()));
        assert!(!item.is_callback(0));
        // A subitem never given a text is asked for, as on Windows.
        assert!(item.is_callback(1));
        assert!(!ListItem::default().is_callback(0));
    }

    #[test]
    fn items_inserted_with_text_callbacks() {
        let ctx = ApiContext::default();
        let mut memory = Sparse::default();
        let hwnd = list(&ctx, 0x1); // LVS_REPORT
        // LVM_INSERTITEMW: mask LVIF_TEXT, item 0, pszText LPSTR_TEXTCALLBACK.
        let item = 0x2_0000;
        for (offset, value) in [(0, 1), (4, 0), (20, LPSTR_TEXTCALLBACK)] {
            memory.write_u32(item + offset, value);
        }
        assert_eq!(listview_proc(&ctx, &mut memory, hwnd, 0x104D, 0, item), 0);
        let kept = with_list(&ctx, hwnd, |list| list.items[0].clone());
        assert!(kept.is_callback(0));
        // A plain text is kept and given back as it is.
        let text = 0x2_0100;
        memory.write_bytes(text, b"7-Zip\0");
        for (offset, value) in [(0, 1), (4, 1), (20, text)] {
            memory.write_u32(item + offset, value);
        }
        listview_proc(&ctx, &mut memory, hwnd, 0x1007, 0, item);
        assert_eq!(display_text(&ctx, &mut memory, hwnd, 1, 0), b"7-Zip");
    }

    #[test]
    fn owner_data_lists_have_the_count_they_are_given() {
        let ctx = ApiContext::default();
        let mut memory = Sparse::default();
        let hwnd = list(&ctx, 0x1 | LVS_OWNERDATA);
        listview_proc(&ctx, &mut memory, hwnd, 0x102F, 4, 0); // LVM_SETITEMCOUNT
        assert_eq!(listview_proc(&ctx, &mut memory, hwnd, 0x1004, 0, 0), 4); // LVM_GETITEMCOUNT
        assert!(with_list(&ctx, hwnd, |list| list.items.iter().all(|i| i.is_callback(0) && i.is_callback(3))));
        // A list that keeps its items only makes room.
        let ctx = ApiContext::default();
        let hwnd = list(&ctx, 0x1);
        listview_proc(&ctx, &mut memory, hwnd, 0x102F, 4, 0);
        assert_eq!(listview_proc(&ctx, &mut memory, hwnd, 0x1004, 0, 0), 0);
    }
}
