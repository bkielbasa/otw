//! USER32, GDI32, SHELL32 and ADVAPI32 functions of later Windows that
//! programs built for them import: Unicode menu and dialog functions,
//! keyboard-layout queries, frame controls, metafile stubs, file
//! operations and file information.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::{
    kernel::pe::ImportName,
    win32::{
        crt,
        gdi::{self, sys_color},
        gfx::{self, Rect, Rgb},
        read_cstr, unicode, ApiContext, ApiOutcome, GuestMemory,
    },
};

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "GetKeyboardLayout" => 1,
        "GetShortPathNameW" | "GetShortPathNameA" => 3,
        "EnumResourceNamesW" | "EnumResourceNamesA" => 4,
        // cdecl: the caller pops; the count bounds the arguments read.
        "wsprintfW" => 8,
        "wvsprintfW" => 3,
        "ShowCursor" | "GetWindowContextHelpId" | "TrackMouseEvent" | "CloseWindow" | "GetEnhMetaFileW"
        | "GetMetaFileW" | "DeleteEnhMetaFile" | "SHGetMalloc" | "SHGetDesktopFolder" | "ShellExecuteExW"
        | "ShellExecuteExA" | "SHFileOperationW" | "SHFileOperationA" | "GetDCBrushColor" => 1,
        "UnregisterHotKey" | "SetWindowContextHelpId" | "GetComboBoxInfo" | "CreateAcceleratorTableW"
        | "CreateAcceleratorTableA" | "CharLowerBuffW" | "CharUpperBuffW" | "GetTextMetricsW" | "ResetDCW"
        | "StartDocW" | "MapVirtualKeyA" | "MapVirtualKeyW" => 2,
        "MapVirtualKeyExW" | "MapVirtualKeyExA" | "GetKeyNameTextW" | "GetKeyNameTextA" | "AttachThreadInput"
        | "GetMetaFileBitsEx" | "GetEnhMetaFileHeader" | "GetEnhMetaFilePaletteEntries" | "PlayEnhMetaFile"
        | "SetClassLongW" | "SetClassLongA" | "ExtractIconW" | "ExtractIconA" | "IsTextUnicode" => 3,
        "RegisterHotKey" | "SendNotifyMessageW" | "SendNotifyMessageA" | "GetMenuItemRect" | "GetMenuBarInfo"
        | "DrawFrameControl" | "AppendMenuW" | "GetSystemPaletteEntries" | "CreateEnhMetaFileW"
        | "SetWinMetaFileBits" | "GetMenuItemInfoW" | "GetMenuItemInfoA" | "DragQueryFileW" | "DragQueryFileA"
        | "InsertMenuItemA" | "InsertMenuItemW" | "SetMenuItemInfoA" => 4,
        "GetOpenFileNameW" | "GetSaveFileNameW" | "ChooseFontW" | "PrintDlgW" => 1,
        "InsertMenuW" | "ModifyMenuW" | "DialogBoxParamW" | "CreateDialogParamW" | "SHGetDataFromIDListW"
        | "SHGetFileInfoW" | "SHGetFileInfoA" => 5,
        _ => return None,
    })
}

fn call<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, dll: &str, name: &str, args: &[u32]) -> u32 {
    crate::win32::dispatch_with_memory(ctx, memory, dll, &ImportName::ByName(name.into()), args, 0)
        .map(|o| o.value)
        .unwrap_or(0)
}

fn wide_text<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> Vec<u8> {
    unicode::wide_to_ansi(&unicode::read_wstr(memory, va))
}

/// A temporary guest copy of an ANSI string (freed by the caller).
fn temp_string(ctx: &ApiContext, memory: &mut (impl GuestMemory + ?Sized), text: &[u8]) -> u32 {
    let va = ctx.shared.borrow_mut().heap.alloc(text.len() as u32 + 1).unwrap_or(0);
    let _ = memory.write_bytes(va, &[text, b"\0"].concat());
    va
}

fn free(ctx: &ApiContext, va: u32) {
    if va != 0 {
        ctx.shared.borrow_mut().heap.free(va);
    }
}

/// Virtual keys with their scan codes and key names.
const KEYS: &[(u32, u32, &str)] = &[
    (0x08, 0x0E, "Backspace"),
    (0x09, 0x0F, "Tab"),
    (0x0D, 0x1C, "Enter"),
    (0x10, 0x2A, "Shift"),
    (0x11, 0x1D, "Ctrl"),
    (0x12, 0x38, "Alt"),
    (0x13, 0x45, "Pause"),
    (0x14, 0x3A, "Caps Lock"),
    (0x1B, 0x01, "Esc"),
    (0x20, 0x39, "Space"),
    (0x21, 0x49, "Page Up"),
    (0x22, 0x51, "Page Down"),
    (0x23, 0x4F, "End"),
    (0x24, 0x47, "Home"),
    (0x25, 0x4B, "Left"),
    (0x26, 0x48, "Up"),
    (0x27, 0x4D, "Right"),
    (0x28, 0x50, "Down"),
    (0x2C, 0x37, "Print Screen"),
    (0x2D, 0x52, "Insert"),
    (0x2E, 0x53, "Delete"),
    (0x6A, 0x37, "Num *"),
    (0x6B, 0x4E, "Num +"),
    (0x6D, 0x4A, "Num -"),
    (0x6F, 0x35, "Num /"),
    (0xBA, 0x27, ";"),
    (0xBB, 0x0D, "="),
    (0xBC, 0x33, ","),
    (0xBD, 0x0C, "-"),
    (0xBE, 0x34, "."),
    (0xBF, 0x35, "/"),
    (0xC0, 0x29, "`"),
    (0xDB, 0x1A, "["),
    (0xDC, 0x2B, "\\"),
    (0xDD, 0x1B, "]"),
    (0xDE, 0x28, "'"),
];
const LETTER_SCANS: &[u8; 26] = b"\x1e\x30\x2e\x20\x12\x21\x22\x23\x17\x24\x25\x26\x32\x31\x18\x19\x10\x13\x1f\x14\x16\x2f\x11\x2d\x15\x2c";
const DIGIT_SCANS: [u32; 10] = [0x0B, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A];

fn scan_of(vk: u32) -> u32 {
    match vk {
        0x30..=0x39 => DIGIT_SCANS[(vk - 0x30) as usize],
        0x41..=0x5A => LETTER_SCANS[(vk - 0x41) as usize] as u32,
        0x70..=0x79 => 0x3B + (vk - 0x70),
        0x7A | 0x7B => 0x57 + (vk - 0x7A),
        0x60..=0x69 => [0x52, 0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49][(vk - 0x60) as usize],
        _ => KEYS.iter().find(|k| k.0 == vk).map(|k| k.1).unwrap_or(0),
    }
}

fn vk_of(scan: u32) -> u32 {
    if let Some(i) = DIGIT_SCANS.iter().position(|&s| s == scan) {
        return 0x30 + i as u32;
    }
    if let Some(i) = LETTER_SCANS.iter().position(|&s| s as u32 == scan) {
        return 0x41 + i as u32;
    }
    match scan {
        0x3B..=0x44 => 0x70 + scan - 0x3B,
        0x57 | 0x58 => 0x7A + scan - 0x57,
        _ => KEYS.iter().find(|k| k.1 == scan).map(|k| k.0).unwrap_or(0),
    }
}

fn key_name(scan: u32, extended: bool) -> String {
    let vk = vk_of(scan);
    match vk {
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x70..=0x7B => format!("F{}", vk - 0x6F),
        _ => {
            // Without the extended bit the arrows and the block above them
            // are the keypad's.
            let name = KEYS.iter().find(|k| k.1 == scan).map(|k| k.2).unwrap_or("");
            if !extended && scan == 0x53 {
                "Num Del".to_string()
            } else if !extended && matches!(scan, 0x47..=0x52) && scan != 0x4A && scan != 0x4E {
                format!("Num {}", [7, 8, 9, 0, 4, 5, 6, 0, 1, 2, 3, 0, 0].get((scan - 0x47) as usize).copied().unwrap_or(0))
            } else {
                name.to_string()
            }
        }
    }
}

/// MF_ flags of a menu item to MENUITEMINFO type and state.
fn item_type_state(flags: u32) -> (u32, u32) {
    let kind = flags & (0x800 | 0x200 | 0x100 | 0x4 | 0x20 | 0x40 | 0x4000);
    let state = flags & (0x3 | 0x8 | 0x80 | 0x1000);
    (kind, state)
}

fn draw_frame<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, hdc: u32, rect: Rect, kind: u32, state: u32) {
    let pushed = state & 0x200 != 0; // DFCS_PUSHED
    let checked = state & 0x400 != 0; // DFCS_CHECKED
    gdi::with_dc(ctx, memory, hdc, |target, dctx| {
        let device = dctx.rect(rect.left, rect.top, rect.right, rect.bottom);
        let mono = gfx::MonoColors { foreground: 0, background: 0xFFFFFF };
        let mut fill = |r: Rect, colour: Rgb| target.fill_rect(r, &gfx::Fill::Solid(colour), gfx::R2_COPYPEN, (0, 0), mono);
        let Rect { left, top, right, bottom } = device;
        let frame = |fill: &mut dyn FnMut(Rect, Rgb), tl: Rgb, br: Rgb, inset: i32| {
            fill(Rect::new(left + inset, top + inset, right - inset, top + inset + 1), tl);
            fill(Rect::new(left + inset, top + inset, left + inset + 1, bottom - inset), tl);
            fill(Rect::new(left + inset, bottom - inset - 1, right - inset, bottom - inset), br);
            fill(Rect::new(right - inset - 1, top + inset, right - inset, bottom - inset), br);
        };
        // DFC_BUTTON with DFCS_BUTTONCHECK (1) or radio (4): a white box.
        if kind == 4 && state & 0xF != 0 {
            fill(device, 0xFFFFFF);
            frame(&mut fill, sys_color(16), sys_color(20), 0);
            frame(&mut fill, sys_color(21), sys_color(15), 1);
            if checked {
                let (cx, cy) = ((left + right) / 2, (top + bottom) / 2);
                fill(Rect::new(cx - 3, cy - 1, cx + 3, cy + 2), 0);
            }
            return;
        }
        fill(device, sys_color(15));
        if pushed || checked {
            frame(&mut fill, sys_color(16), sys_color(16), 0);
        } else {
            frame(&mut fill, sys_color(20), sys_color(21), 0);
            frame(&mut fill, sys_color(22), sys_color(16), 1);
        }
        // DFC_SCROLL (3) and DFC_CAPTION (1) glyphs: a small arrow or cross.
        let (cx, cy) = ((left + right) / 2, (top + bottom) / 2);
        match (kind, state & 0xF) {
            (3, glyph @ 0..=3) => {
                for i in 0..4 {
                    let (x0, x1, y) = match glyph {
                        0 => (cx - i, cx + i + 1, cy - 2 + i),
                        1 => (cx - i, cx + i + 1, cy + 1 - i),
                        _ => (cx, cx + 1, cy),
                    };
                    if glyph <= 1 {
                        fill(Rect::new(x0, y, x1, y + 1), 0);
                    } else {
                        let x = if glyph == 2 { cx - 2 + i } else { cx + 1 - i };
                        fill(Rect::new(x, cy - i, x + 1, cy + i + 1), 0);
                    }
                }
            }
            (1, 0) => {
                for i in 0..7 {
                    fill(Rect::new(cx - 3 + i, cy - 3 + i, cx - 1 + i, cy - 2 + i), 0);
                    fill(Rect::new(cx + 2 - i, cy - 3 + i, cx + 4 - i, cy - 2 + i), 0);
                }
            }
            _ => {}
        }
    });
}

/// GetOpenFileNameW / GetSaveFileNameW over the ANSI dialog: OPENFILENAMEW's
/// strings are converted into an OPENFILENAMEA and the results back.
fn file_dialog_w<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, ofn: u32, save: bool) -> u32 {
    // OPENFILENAME in the process's layout; see `x64::OPENFILENAME` for the
    // field order (3 filter, 4 custom filter, 6 filter index, 7/8 file
    // buffer, 9/10 title buffer, 11 initial folder, 12 title, 13 flags,
    // 14/15 name and extension offsets, 16 default extension).
    let layout = crate::win32::x64::OPENFILENAME;
    let size = layout.size(memory.ptr_size() == 8);
    let fields = memory.read_bytes(ofn, size as usize).unwrap_or_else(|| vec![0; size as usize]);
    let a = ctx.shared.borrow_mut().heap.alloc(size).unwrap_or(0);
    let _ = memory.write_bytes(a, &fields);
    let original = crate::win32::x64::View::new(memory, &layout, ofn);
    let copy = crate::win32::x64::View::new(memory, &layout, a);
    let mut temps = Vec::new();
    // The filter: pairs of strings ending with an empty one.
    let filter_va = original.get32(memory, 3);
    if filter_va != 0 {
        let list = path_list(memory, filter_va, true);
        let mut block: Vec<u8> = list.iter().flat_map(|s| s.iter().copied().chain([0])).collect();
        block.extend_from_slice(&[0, 0]);
        let va = ctx.shared.borrow_mut().heap.alloc(block.len() as u32).unwrap_or(0);
        let _ = memory.write_bytes(va, &block);
        temps.push(va);
        copy.set(memory, 3, va);
    }
    copy.set(memory, 4, 0); // no custom filter
    for index in [11, 12, 16] {
        let va = original.get32(memory, index);
        if va >= 0x10000 {
            let text = wide_text(memory, va);
            let temp = temp_string(ctx, memory, &text);
            temps.push(temp);
            copy.set(memory, index, temp);
        }
    }
    // The file buffer is in and out; the title buffer only out.
    let (file_va, file_max) = (original.get32(memory, 7), original.get32(memory, 8));
    let file_temp = ctx.shared.borrow_mut().heap.alloc(file_max.max(1) + 1).unwrap_or(0);
    let initial = if file_va != 0 { wide_text(memory, file_va) } else { Vec::new() };
    let _ = memory.write_bytes(file_temp, &[&initial[..initial.len().min(file_max.saturating_sub(1) as usize)], b"\0"].concat());
    copy.set(memory, 7, file_temp);
    let (title_va, title_max) = (original.get32(memory, 9), original.get32(memory, 10));
    let title_temp = if title_va != 0 { ctx.shared.borrow_mut().heap.alloc(title_max.max(1) + 1).unwrap_or(0) } else { 0 };
    let _ = memory.write_bytes(title_temp, &[0]);
    copy.set(memory, 9, title_temp);
    // Hooks and templates expect Unicode messages; the plain dialog is used.
    let flags = original.get32(memory, 13) & !(0x20 | 0x40 | 0x80);
    copy.set(memory, 13, flags);
    copy.set(memory, 0, size);
    let result = call(ctx, memory, "COMDLG32.dll", if save { "GetSaveFileNameA" } else { "GetOpenFileNameA" }, &[a]);
    if result != 0 {
        let chosen = read_cstr(memory, file_temp);
        if file_va != 0 && file_max > 0 {
            let n = chosen.len().min(file_max as usize - 1);
            unicode::write_wide(memory, file_va, &unicode::ansi_to_wide(&chosen[..n]), true);
        }
        if title_va != 0 && title_max > 0 {
            let title = read_cstr(memory, title_temp);
            let n = title.len().min(title_max as usize - 1);
            unicode::write_wide(memory, title_va, &unicode::ansi_to_wide(&title[..n]), true);
        }
        // Filter index and the name/extension offsets (characters).
        for index in [6, 14, 15] {
            let value = copy.get32(memory, index);
            original.set(memory, index, value);
        }
    }
    for va in temps.into_iter().chain([file_temp, title_temp, a]) {
        free(ctx, va);
    }
    result
}

/// The names of a module's resources of one type: ids, or strings.
fn resource_names<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, module: u32, kind: u32, wide: bool) -> Vec<Result<u32, Vec<u8>>> {
    let _ = ctx;
    let Some(root) = crate::win32::user::resource_directory(memory, module) else { return Vec::new() };
    let read_u16 = |memory: &mut M, va: u32| memory.read_bytes(va, 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u32).unwrap_or(0);
    let string_at = |memory: &mut M, va: u32| -> Vec<u8> {
        let length = read_u16(memory, va);
        let bytes = memory.read_bytes(va + 2, length as usize * 2).unwrap_or_default();
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        unicode::wide_to_ansi(&units)
    };
    let wanted: Result<u32, Vec<u8>> = if kind < 0x10000 {
        Ok(kind)
    } else if wide {
        Err(wide_text(memory, kind).to_ascii_uppercase())
    } else {
        Err(read_cstr(memory, kind).to_ascii_uppercase())
    };
    let entries = |memory: &mut M, directory: u32| -> Vec<(u32, u32)> {
        let count = read_u16(memory, directory + 12) + read_u16(memory, directory + 14);
        (0..count)
            .map(|i| (memory.read_u32(directory + 16 + i * 8).unwrap_or(0), memory.read_u32(directory + 20 + i * 8).unwrap_or(0)))
            .collect()
    };
    let types = entries(memory, root);
    let Some(&(_, names)) = types.iter().find(|(id, _)| match &wanted {
        Ok(wanted) => id & 0x8000_0000 == 0 && id == wanted,
        Err(wanted) => id & 0x8000_0000 != 0 && string_at(memory, root + (id & 0x7FFF_FFFF)).to_ascii_uppercase() == *wanted,
    }) else {
        return Vec::new();
    };
    if names & 0x8000_0000 == 0 {
        return Vec::new();
    }
    entries(memory, root + (names & 0x7FFF_FFFF))
        .into_iter()
        .map(|(id, _)| if id & 0x8000_0000 != 0 { Err(string_at(memory, root + (id & 0x7FFF_FFFF))) } else { Ok(id) })
        .collect()
}

/// The NUL-separated list of a SHFILEOPSTRUCT path field.
fn path_list<M: GuestMemory + ?Sized>(memory: &mut M, mut va: u32, wide: bool) -> Vec<Vec<u8>> {
    let mut list = Vec::new();
    if va == 0 {
        return list;
    }
    loop {
        let text = if wide { wide_text(memory, va) } else { read_cstr(memory, va) };
        if text.is_empty() {
            break;
        }
        va += (text.len() as u32 + 1) * if wide { 2 } else { 1 };
        list.push(text);
    }
    list
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// SHFileOperation's move, copy, delete and rename on drive C:.
fn file_operation(ctx: &ApiContext, function: u32, from: &[Vec<u8>], to: &[Vec<u8>]) -> u32 {
    for (index, source) in from.iter().enumerate() {
        let Some(source_host) = crt::host_path_in(ctx, source) else { return 2 };
        let result = match function {
            3 => {
                if source_host.is_dir() {
                    std::fs::remove_dir_all(&source_host)
                } else {
                    std::fs::remove_file(&source_host)
                }
            }
            1 | 2 | 4 => {
                let target = to.get(index).or(to.first()).cloned().unwrap_or_default();
                let Some(mut target_host) = crt::host_path_in(ctx, &target) else { return 2 };
                // Into an existing folder unless renaming one item.
                if target_host.is_dir() && function != 4 {
                    if let Some(name) = source_host.file_name() {
                        target_host = target_host.join(name);
                    }
                }
                match function {
                    2 => copy_tree(&source_host, &target_host),
                    _ => std::fs::rename(&source_host, &target_host),
                }
            }
            _ => return 0x78, // DE_INVALIDFILES
        };
        if result.is_err() {
            return 0x7C; // DE_INVALIDFILES
        }
    }
    0
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    match name {
        // ── Menus ──
        "AppendMenuW" | "InsertMenuW" | "ModifyMenuW" => {
            // The item is a string unless the flags make it a bitmap,
            // owner-drawn item or separator.
            let (flags_index, item_index) = if name == "AppendMenuW" { (1, 3) } else { (2, 4) };
            let mut a_args = args[..count].to_vec();
            let string = args[flags_index] & (0x4 | 0x100 | 0x800) == 0 && args[item_index] >= 0x10000;
            let temp = if string {
                let text = wide_text(memory, args[item_index]);
                temp_string(ctx, memory, &text)
            } else {
                0
            };
            if string {
                a_args[item_index] = temp;
            }
            let result = call(ctx, memory, "USER32.dll", &name.replace('W', "A"), &a_args);
            free(ctx, temp);
            ok(result)
        }
        "InsertMenuItemA" | "InsertMenuItemW" => {
            // InsertMenuItem(menu, item, by position, MENUITEMINFO*): as
            // InsertMenu with the item's type, state, ID or submenu, text.
            let info = args[3];
            let mask = mii(memory, info, 1);
            let kind = if mask & 0x110 != 0 { mii(memory, info, 2) } else { 0 };
            let state = if mask & 1 != 0 { mii(memory, info, 3) } else { 0 };
            let id = if mask & 2 != 0 { mii(memory, info, 4) } else { 0 };
            let submenu = if mask & 4 != 0 { mii(memory, info, 5) } else { 0 };
            let data = mii(memory, info, 9);
            let mut flags = (kind | state) & !0x10 | if args[2] != 0 { 0x400 } else { 0 };
            if submenu != 0 {
                flags |= 0x10; // MF_POPUP
            }
            let textual = flags & 0x904 == 0 && mask & 0x50 != 0 && data >= 0x10000;
            let text = if textual {
                let text = if name.ends_with('W') { wide_text(memory, data) } else { read_cstr(memory, data) };
                temp_string(ctx, memory, &text)
            } else {
                0
            };
            if !textual && flags & 0x904 == 0 {
                flags |= 0x800; // nothing to show: a separator
            }
            let item_id = if submenu != 0 { submenu } else { id };
            let result = call(ctx, memory, "USER32.dll", "InsertMenuA", &[args[0], args[1], flags, item_id, text]);
            free(ctx, text);
            ok(result)
        }
        "SetMenuItemInfoA" => {
            let mask = mii(memory, args[3], 1);
            let state = mii(memory, args[3], 3);
            let id = mii(memory, args[3], 4);
            let submenu = mii(memory, args[3], 5);
            let text_va = mii(memory, args[3], 9);
            let text = (mask & 0x50 != 0 && text_va >= 0x10000).then(|| read_cstr(memory, text_va));
            let mut shared = ctx.shared.borrow_mut();
            let item = shared.gui.menus.get_mut(&args[0]).and_then(|menu| {
                if args[2] != 0 {
                    menu.items.get_mut(args[1] as usize)
                } else {
                    menu.items.iter_mut().find(|item| item.id == args[1])
                }
            });
            let Some(item) = item else { return ok(0) };
            if let Some(text) = text {
                item.text = text;
            }
            if mask & 1 != 0 {
                item.flags = (item.flags & !0x108B) | (state & 0x108B);
            }
            if mask & 2 != 0 {
                item.id = id;
            }
            if mask & 4 != 0 {
                item.submenu = submenu;
                if submenu != 0 {
                    item.flags |= 0x10;
                }
            }
            shared.gui.dirty = true;
            ok(1)
        }
        "GetMenuItemInfoW" | "GetMenuItemInfoA" => {
            // GetMenuItemInfo(menu, item, by position, MENUITEMINFO*)
            let item = {
                let shared = ctx.shared.borrow();
                shared.gui.menus.get(&args[0]).and_then(|menu| {
                    if args[2] != 0 {
                        menu.items.get(args[1] as usize).cloned()
                    } else {
                        menu.items.iter().find(|item| item.id == args[1]).cloned()
                    }
                })
            };
            let Some(item) = item else { return ok(0) };
            let info = args[3];
            let mask = mii(memory, info, 1);
            let (kind, state) = item_type_state(item.flags);
            if mask & (0x10 | 0x100) != 0 {
                set_mii(memory, info, 2, kind);
            }
            if mask & 0x1 != 0 {
                set_mii(memory, info, 3, state);
            }
            if mask & 0x2 != 0 {
                set_mii(memory, info, 4, item.id);
            }
            if mask & 0x4 != 0 {
                set_mii(memory, info, 5, item.submenu);
            }
            if mask & 0x8 != 0 {
                set_mii(memory, info, 6, 0);
                set_mii(memory, info, 7, 0);
            }
            if mask & 0x20 != 0 {
                set_mii(memory, info, 8, 0);
            }
            if mask & (0x10 | 0x40) != 0 && kind & 0x904 == 0 {
                let (va, capacity) = (mii(memory, info, 9), mii(memory, info, 10));
                if va != 0 && capacity > 0 {
                    let n = item.text.len().min(capacity as usize - 1);
                    if name.ends_with('W') {
                        unicode::write_wide(memory, va, &unicode::ansi_to_wide(&item.text[..n]), true);
                    } else {
                        let _ = memory.write_bytes(va, &[&item.text[..n], b"\0"].concat());
                    }
                }
                set_mii(memory, info, 10, item.text.len() as u32);
            }
            ok(1)
        }
        "GetMenuItemRect" | "GetMenuBarInfo" => ok(0),
        "wsprintfW" | "wvsprintfW" => {
            // %s and %c are wide here and %S/%C narrow: swap them for the
            // ANSI formatter.
            let format: Vec<u8> = wide_text(memory, args[1]);
            let mut swapped = Vec::with_capacity(format.len());
            let mut in_spec = false;
            for &b in &format {
                if in_spec {
                    let mapped = match b {
                        b's' => b'S',
                        b'S' => b's',
                        b'c' => b'C',
                        b'C' => b'c',
                        _ => b,
                    };
                    swapped.push(mapped);
                    if b.is_ascii_alphabetic() && !b"hlLIwq".contains(&b) || b == b'%' {
                        in_spec = false;
                    }
                    continue;
                }
                swapped.push(b);
                in_spec = b == b'%';
            }
            // wvsprintfW takes its arguments as a va_list.
            let values: Vec<u32> = if name == "wvsprintfW" {
                (0..16).map(|i| memory.read_u32(args[2] + i * 4).unwrap_or(0)).collect()
            } else {
                args[2..].to_vec()
            };
            let text = crate::win32::controls::format_printf(memory, &swapped, &values, None);
            unicode::write_wide(memory, args[0], &unicode::ansi_to_wide(&text), true);
            Some(ApiOutcome::new(text.len() as u32, if name == "wvsprintfW" { 3 } else { 0 }))
        }
        "GetOpenFileNameW" | "GetSaveFileNameW" => ok(file_dialog_w(ctx, memory, args[0], name == "GetSaveFileNameW")),
        // Font and print dialogs are not available: as if cancelled.
        "ChooseFontW" | "PrintDlgW" => ok(0),
        // ── Dialogs: the dialog procedure speaks Unicode ──
        "DialogBoxParamW" | "CreateDialogParamW" => {
            unicode::mark_unicode(ctx, args[3]);
            let template = if args[1] >= 0x10000 {
                let text = wide_text(memory, args[1]);
                temp_string(ctx, memory, &text)
            } else {
                args[1]
            };
            let result = call(ctx, memory, "USER32.dll", &name.replace('W', "A"), &[args[0], template, args[2], args[3], args[4]]);
            if template != args[1] {
                free(ctx, template);
            }
            ok(result)
        }
        // ── Window classes ──
        "SetClassLongW" | "SetClassLongA" => {
            let mut shared = ctx.shared.borrow_mut();
            let shared = &mut *shared;
            let Some(class_name) = shared.gui.window(args[0]).map(|w| w.class.clone()) else { return ok(0) };
            let Some(class) = shared.gui.classes.iter_mut().find(|c| c.name.eq_ignore_ascii_case(&class_name)) else {
                return ok(0);
            };
            let slot = match args[1] as i32 {
                -8 => &mut class.menu_name,
                -10 => &mut class.background,
                -12 => &mut class.cursor,
                -14 | -34 => &mut class.icon,
                -24 => &mut class.wndproc,
                -26 => &mut class.style,
                _ => return ok(0),
            };
            let old = std::mem::replace(slot, args[2]);
            shared.gui.dirty = true;
            ok(old)
        }
        // ── Accelerators: ACCEL is fVirt (byte), key, cmd (words) ──
        "CreateAcceleratorTableW" | "CreateAcceleratorTableA" => {
            let bytes = memory.read_bytes(args[0], args[1] as usize * 6).unwrap_or_default();
            let entries: Vec<(u32, u32, u32)> = bytes
                .chunks_exact(6)
                .map(|c| ((c[0] & 0x7F) as u32, u16::from_le_bytes([c[2], c[3]]) as u32, u16::from_le_bytes([c[4], c[5]]) as u32))
                .collect();
            let mut shared = ctx.shared.borrow_mut();
            let handle = shared.alloc_handle() | 0x0060_0000;
            shared.gui.accelerators.insert(handle, entries);
            ok(handle)
        }
        // ── Keyboard ──
        "GetKeyboardLayout" => ok(0x0409_0409),
        // Long names are the short names here.
        "GetShortPathNameW" | "GetShortPathNameA" => {
            let wide = name.ends_with('W');
            let path = if wide { wide_text(memory, args[0]) } else { read_cstr(memory, args[0]) };
            if (args[2] as usize) <= path.len() {
                return ok(path.len() as u32 + 1);
            }
            if wide {
                unicode::write_wide(memory, args[1], &unicode::ansi_to_wide(&path), true);
            } else {
                let _ = memory.write_bytes(args[1], &[&path[..], b"\0"].concat());
            }
            ok(path.len() as u32)
        }
        "EnumResourceNamesW" | "EnumResourceNamesA" => {
            // EnumResourceNames(module, type, callback, lparam)
            let wide = name.ends_with('W');
            let module = if args[0] == 0 { ctx.image_base } else { args[0] };
            let names = resource_names(ctx, memory, module, args[1], wide);
            let mut temps = Vec::new();
            for entry in names {
                let name_arg = match entry {
                    Ok(id) => id,
                    Err(text) => {
                        let va = if wide {
                            let va = ctx.shared.borrow_mut().heap.alloc(text.len() as u32 * 2 + 2).unwrap_or(0);
                            unicode::write_wide(memory, va, &unicode::ansi_to_wide(&text), true);
                            va
                        } else {
                            temp_string(ctx, memory, &text)
                        };
                        temps.push(va);
                        va
                    }
                };
                let more = memory.call_guest(args[2], &[module, args[1], name_arg, args[3]]).unwrap_or(0);
                if more == 0 {
                    break;
                }
            }
            for va in temps {
                free(ctx, va);
            }
            ok(1)
        }
        "MapVirtualKeyA" | "MapVirtualKeyW" | "MapVirtualKeyExW" | "MapVirtualKeyExA" => ok(match args[1] {
            0 => scan_of(args[0]),
            1 | 3 => vk_of(args[0]),
            2 => match args[0] {
                0x30..=0x39 | 0x41..=0x5A | 0x20 => args[0],
                0x0D => 0x0D,
                0x08 => 0x08,
                0x09 => 0x09,
                0x1B => 0x1B,
                0xBA..=0xDE => KEYS.iter().find(|k| k.0 == args[0]).and_then(|k| k.2.bytes().next()).unwrap_or(0) as u32,
                _ => 0,
            },
            _ => 0,
        }),
        "GetKeyNameTextW" | "GetKeyNameTextA" => {
            let text = key_name((args[0] >> 16) & 0xFF, args[0] & 0x0100_0000 != 0);
            let n = text.len().min(args[2].saturating_sub(1) as usize);
            if name.ends_with('W') {
                let units: Vec<u16> = text[..n].encode_utf16().collect();
                unicode::write_wide(memory, args[1], &units, true);
            } else {
                let _ = memory.write_bytes(args[1], &[&text.as_bytes()[..n], b"\0"].concat());
            }
            ok(n as u32)
        }
        "CharLowerBuffW" | "CharUpperBuffW" => {
            let units: Vec<u16> = (0..args[1])
                .map(|i| memory.read_bytes(args[0] + i * 2, 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0))
                .map(|u| match char::from_u32(u as u32) {
                    Some(c) if name == "CharLowerBuffW" => c.to_lowercase().next().map(|c| c as u16).unwrap_or(u),
                    Some(c) => c.to_uppercase().next().map(|c| c as u16).unwrap_or(u),
                    None => u,
                })
                .collect();
            unicode::write_wide(memory, args[0], &units, false);
            ok(args[1])
        }
        "RegisterHotKey" | "UnregisterHotKey" | "AttachThreadInput" | "SetWindowContextHelpId" | "TrackMouseEvent" => ok(1),
        "GetWindowContextHelpId" => ok(0),
        "ShowCursor" => {
            let mut shared = ctx.shared.borrow_mut();
            shared.cursor_count += if args[0] != 0 { 1 } else { -1 };
            ok(shared.cursor_count as u32)
        }
        "SendNotifyMessageW" | "SendNotifyMessageA" => {
            let target = if name.ends_with('W') { "SendMessageW" } else { "SendMessageA" };
            call(ctx, memory, "USER32.dll", target, &args[..4]);
            ok(1)
        }
        "CloseWindow" => ok(u32::from(call(ctx, memory, "USER32.dll", "ShowWindow", &[args[0], 6]) != u32::MAX)),
        "GetComboBoxInfo" => {
            // COMBOBOXINFO: the item and button rectangles and the parts.
            let size = {
                let shared = ctx.shared.borrow();
                shared.gui.window(args[0]).map(|w| (w.width, w.height))
            };
            let Some((w, h)) = size else { return ok(0) };
            let item = Rect::new(3, 3, w - 20, h - 3);
            let button = Rect::new(w - 19, 2, w - 2, h - 2);
            let _ = gdi::write_rect(memory, args[1] + 4, item);
            let _ = gdi::write_rect(memory, args[1] + 20, button);
            let _ = memory.write_u32(args[1] + 36, 0);
            let _ = memory.write_u32(args[1] + 40, args[0]);
            let _ = memory.write_u32(args[1] + 44, args[0]);
            let _ = memory.write_u32(args[1] + 48, 0);
            ok(1)
        }
        "DrawFrameControl" => {
            let Some(rect) = gdi::read_rect(memory, args[1]) else { return ok(0) };
            draw_frame(ctx, memory, args[0], rect, args[2], args[3]);
            ok(1)
        }
        // ── GDI ──
        "GetDCBrushColor" => ok(0xFF_FFFF),
        "ResetDCW" => ok(args[0]),
        "StartDocW" => ok(0), // printing is not available
        "GetSystemPaletteEntries" => {
            // GetSystemPaletteEntries(dc, start, count, entries)
            let palette = gdi::default_palette();
            if args[3] == 0 {
                return ok(palette.len() as u32);
            }
            let mut written = 0;
            for (i, colour) in palette.iter().skip(args[1] as usize).take(args[2] as usize).enumerate() {
                let entry = [(colour >> 16) as u8, (colour >> 8) as u8, *colour as u8, 0];
                let _ = memory.write_bytes(args[3] + i as u32 * 4, &entry);
                written += 1;
            }
            ok(written)
        }
        "GetTextMetricsW" => {
            // TEXTMETRICA (56 bytes) to TEXTMETRICW (60): the four
            // character fields become WCHARs.
            let temp = ctx.shared.borrow_mut().heap.alloc(56).unwrap_or(0);
            let result = call(ctx, memory, "GDI32.dll", "GetTextMetricsA", &[args[0], temp]);
            let a = memory.read_bytes(temp, 56).unwrap_or_else(|| vec![0; 56]);
            free(ctx, temp);
            let mut w = a[..44].to_vec();
            for &c in &a[44..48] {
                w.extend_from_slice(&(c as u16).to_le_bytes());
            }
            w.extend_from_slice(&a[48..53]);
            w.resize(60, 0);
            let _ = memory.write_bytes(args[1], &w);
            ok(result)
        }
        // Metafiles are not supported: creating and reading them fails.
        "CreateEnhMetaFileW" | "GetEnhMetaFileW" | "GetMetaFileW" | "GetMetaFileBitsEx" | "SetWinMetaFileBits"
        | "GetEnhMetaFileHeader" | "GetEnhMetaFilePaletteEntries" | "PlayEnhMetaFile" => ok(0),
        "DeleteEnhMetaFile" => ok(1),
        // ── SHELL32 ──
        "SHGetMalloc" => ok(call(ctx, memory, "OLE32.dll", "CoGetMalloc", &[1, args[0]])),
        "SHGetDesktopFolder" => {
            let desktop = crate::win32::shellfolder::desktop(ctx, memory);
            let _ = memory.write_ptr(args[0], u64::from(desktop));
            ok(0)
        }
        "SHGetDataFromIDListW" => ok(0x8000_4001), // E_NOTIMPL
        "ShellExecuteExW" | "ShellExecuteExA" => {
            // SHELLEXECUTEINFO (process layout): verb, file, parameters; hInstApp.
            let wide = name.ends_with('W');
            let read = |memory: &mut M, va: u32| if va == 0 { Vec::new() } else if wide { wide_text(memory, va) } else { read_cstr(memory, va) };
            let info = crate::win32::x64::View::new(memory, &crate::win32::x64::SHELLEXECUTEINFO, args[0]);
            let (verb_va, file_va, params_va) = (info.get32(memory, 3), info.get32(memory, 4), info.get32(memory, 5));
            let (verb, file, params) = (read(memory, verb_va), read(memory, file_va), read(memory, params_va));
            let directory_va = info.get32(memory, 6);
            let directory = read(memory, directory_va);
            // Programs start (opening one, a downloaded installer say);
            // other files have no program to open them yet.
            let opens = verb.is_empty() || verb.eq_ignore_ascii_case(b"open") || verb.eq_ignore_ascii_case(b"runas");
            if opens && crate::win32::sync::is_program(&file) {
                let Some(process) = crate::win32::sync::shell_start(ctx, &file, &params, &directory) else {
                    info.set(memory, 8, 2); // SE_ERR_FNF
                    ctx.shared.borrow_mut().last_error = 2;
                    return ok(0);
                };
                info.set(memory, 8, 42);
                // SEE_MASK_NOCLOSEPROCESS: the caller gets the process.
                let mask = info.get32(memory, 1);
                info.set(memory, 14, if mask & 0x40 != 0 { process } else { 0 });
                return ok(1);
            }
            eprintln!(
                "[shell] ShellExecuteEx {} {} {} (only programs are started)",
                String::from_utf8_lossy(&verb),
                String::from_utf8_lossy(&file),
                String::from_utf8_lossy(&params)
            );
            info.set(memory, 8, 42);
            ok(1)
        }
        "SHFileOperationW" | "SHFileOperationA" => {
            // SHFILEOPSTRUCT (process layout): hwnd, wFunc, pFrom, pTo,
            // fFlags, fAnyOperationsAborted.
            let wide = name.ends_with('W');
            let op = crate::win32::x64::View::new(memory, &crate::win32::x64::SHFILEOPSTRUCT, args[0]);
            let function = op.get32(memory, 1);
            let from_va = op.get32(memory, 2);
            let to_va = op.get32(memory, 3);
            let from = path_list(memory, from_va, wide);
            let to = path_list(memory, to_va, wide);
            let result = file_operation(ctx, function, &from, &to);
            op.set(memory, 5, 0);
            ok(result)
        }
        "SHGetFileInfoW" | "SHGetFileInfoA" => {
            // SHGetFileInfo(path, attributes, SHFILEINFO*, size, flags)
            let wide = name.ends_with('W');
            let flags = args[4];
            if flags & 0x4000 != 0 {
                // SHGFI_SYSICONIMAGELIST: the system image list.
                let small = flags & 0x1 != 0;
                let list = ctx.shared.borrow().system_image_lists[usize::from(small)];
                let list = if list != 0 {
                    list
                } else {
                    let size = if small { 16 } else { 32 };
                    let handle = call(ctx, memory, "COMCTL32.dll", "ImageList_Create", &[size, size, 0x21, 4, 4]);
                    ctx.shared.borrow_mut().system_image_lists[usize::from(small)] = handle;
                    handle
                };
                if args[2] != 0 {
                    let _ = memory.write_u32(args[2] + 4, 0);
                }
                return ok(list);
            }
            let path = if flags & 0x8 != 0 && args[0] < 0x10000 {
                Vec::new()
            } else if wide {
                wide_text(memory, args[0])
            } else {
                read_cstr(memory, args[0])
            };
            if args[2] == 0 {
                return ok(1);
            }
            let display = path.rsplit(|&b| b == b'\\').next().unwrap_or(&path).to_vec();
            let is_dir = crt::host_path_in(ctx, &path).is_some_and(|p| p.is_dir());
            let kind: &[u8] = if is_dir { b"File Folder" } else { b"File" };
            let (name_size, type_size) = (260usize, 80usize);
            let char_size = if wide { 2 } else { 1 };
            let mut info = vec![0u8; 12 + (name_size + type_size) * char_size];
            let attributes: u32 = if is_dir { 0x2000_0000 } else { 0 };
            info[8..12].copy_from_slice(&attributes.to_le_bytes());
            let _ = memory.write_bytes(args[2], &info);
            let name_va = args[2] + 12;
            let type_va = name_va + (name_size * char_size) as u32;
            for (va, text) in [(name_va, &display[..display.len().min(259)]), (type_va, kind)] {
                if wide {
                    unicode::write_wide(memory, va, &unicode::ansi_to_wide(text), true);
                } else {
                    let _ = memory.write_bytes(va, &[text, b"\0"].concat());
                }
            }
            ok(1)
        }
        "DragQueryFileW" | "DragQueryFileA" => ok(0),
        "ExtractIconW" | "ExtractIconA" => ok(0),
        // ── ADVAPI32 ──
        "IsTextUnicode" => {
            // A byte-order mark, or mostly zero high bytes.
            let bytes = memory.read_bytes(args[0], (args[1] as usize).min(4096)).unwrap_or_default();
            let unicode = bytes.starts_with(&[0xFF, 0xFE])
                || (bytes.len() >= 2 && {
                    let pairs = bytes.len() / 2;
                    let zero_high = bytes.chunks_exact(2).filter(|c| c[1] == 0 && c[0] != 0).count();
                    zero_high * 4 >= pairs * 3
                });
            if args[2] != 0 {
                let _ = memory.write_u32(args[2], if unicode { 1 } else { 0 });
            }
            ok(u32::from(unicode))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    #[test]
    fn keys_map_between_codes_and_names() {
        assert_eq!(scan_of(0x41), 0x1E);
        assert_eq!(vk_of(0x1E), 0x41);
        assert_eq!(key_name(0x1E, false), "A");
        assert_eq!(key_name(0x3B, false), "F1");
        assert_eq!(key_name(0x53, true), "Delete");
        assert_eq!(key_name(0x53, false), "Num Del");
        assert_eq!(key_name(0x52, false), "Num 0");
        for &(vk, scan, _) in KEYS {
            if vk_of(scan) == vk {
                assert_eq!(scan_of(vk), scan);
            }
        }
    }
}

/// MENUITEMINFO field `index` in the process's layout: cbSize, fMask,
/// fType, fState, wID, hSubMenu, hbmpChecked, hbmpUnchecked, dwItemData,
/// dwTypeData, cch, hbmpItem.
pub(crate) fn mii<M: GuestMemory + ?Sized>(memory: &mut M, info: u32, index: usize) -> u32 {
    crate::win32::x64::View::new(memory, &crate::win32::x64::MENUITEMINFO, info).get32(memory, index)
}

fn set_mii<M: GuestMemory + ?Sized>(memory: &mut M, info: u32, index: usize, value: u32) {
    crate::win32::x64::View::new(memory, &crate::win32::x64::MENUITEMINFO, info).set(memory, index, value);
}
