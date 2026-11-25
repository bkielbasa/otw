//! SHELL32's ordinal-only exports that Internet Explorer's DLLs import: item
//! ID lists, special folders, shell allocation and the drag-and-drop helpers.
//!
//! The shell namespace is not modelled. Item ID lists are byte strings of
//! `SHITEMID`s (a 16-bit size, then data) ending with a zero size, and those
//! made here hold paths. There is no drag image, so the `DAD_*` functions
//! succeed without drawing.

use crate::{
    kernel::pe::ImportName,
    win32::{read_cstr, unicode, ApiContext, ApiOutcome, GuestMemory},
};

const S_OK: u32 = 0;
const E_FAIL: u32 = 0x8000_4005;
const E_NOTIMPL: u32 = 0x8000_4001;

/// The name behind a SHELL32 ordinal.
pub fn ordinal_name(dll: &str, ordinal: u16) -> Option<&'static str> {
    if !dll.eq_ignore_ascii_case("shell32.dll") {
        return None;
    }
    Some(match ordinal {
        2 => "SHChangeNotifyRegister",
        4 => "SHChangeNotifyDeregister",
        16 => "ILFindLastID",
        17 => "ILRemoveLastID",
        18 => "ILClone",
        19 => "ILCloneFirst",
        21 => "ILIsEqual",
        23 => "ILIsParent",
        24 => "ILFindChild",
        25 => "ILCombine",
        26 => "ILLoadFromStream",
        27 => "ILSaveToStream",
        28 => "SHILCreateFromPath",
        43 => "PathIsExe",
        58 => "ParseField",
        59 => "RestartDialog",
        62 => "PickIconDlg",
        63 => "GetFileNameFromBrowse",
        64 => "DriveType",
        66 => "IsNetDrive",
        67 => "Shell_MergeMenus",
        68 => "SHGetSetSettings",
        71 => "Shell_GetImageLists",
        72 => "Shell_GetCachedImageIndex",
        73 => "SHShellFolderView_Message",
        74 => "SHCreateStdEnumFmtEtc",
        75 => "PathYetAnotherMakeUniqueName",
        77 => "SHMapPIDLToSystemImageListIndex",
        83 => "CIDLData_CreateFromIDArray",
        85 => "OpenRegStream",
        86 => "SHRegisterDragDrop",
        87 => "SHRevokeDragDrop",
        88 => "SHDoDragDrop",
        89 => "SHCloneSpecialIDList",
        90 => "SHFindFiles",
        98 => "SHGetRealIDL",
        100 => "SHRestricted",
        102 => "SHCoCreateInstance",
        103 => "SignalFileOpen",
        119 => "IsLFNDrive",
        121 => "SHFlushClipboard",
        123 => "SHFreeUnusedLibraries",
        129 => "DAD_AutoScroll",
        131 => "DAD_DragEnterEx",
        132 => "DAD_DragLeave",
        134 => "DAD_DragMove",
        136 => "DAD_SetDragImage",
        137 => "DAD_ShowDragImage",
        147 => "SHCLSIDFromString",
        149 => "SHFind_InitMenuPopup",
        151 => "SHLoadOLE",
        152 => "ILGetSize",
        153 => "ILGetNext",
        154 => "ILAppendID",
        155 => "ILFree",
        157 => "ILCreateFromPath",
        162 => "SHSimpleIDListFromPath",
        165 => "SHCreateDirectory",
        171 => "PathCleanupSpec",
        174 => "SHCreateShellFolderViewEx",
        175 => "SHGetSpecialFolderPath",
        176 => "SHSetInstanceExplorer",
        189 => "ILCreateFromPathA",
        195 => "SHFree",
        196 => "SHAlloc",
        _ => return None,
    })
}

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "DAD_DragLeave" | "SHFlushClipboard" | "SHFreeUnusedLibraries" => 0,
        "PathIsExe" | "DriveType" | "IsNetDrive" | "SignalFileOpen" | "IsLFNDrive"
        | "SHSetInstanceExplorer" | "ILCreateFromPathA" | "SHRevokeDragDrop" => 1,
        "SHRegisterDragDrop" | "SHAppBarMessage" | "SHGetPathFromIDListA" => 2,
        "SHGetSetSettings" | "SHGetRealIDL" | "ILAppendID" | "SHILCreateFromPath" => 3,
        "ParseField" | "CIDLData_CreateFromIDArray" | "SHFind_InitMenuPopup" | "ShellAboutW"
        | "ExtractAssociatedIconExW" => 4,
        "SHGetDataFromIDListA" => 5,
        "SHChangeNotifyRegister" => 6,
        "SHChangeNotifyDeregister" | "ILFindLastID" | "ILRemoveLastID" | "ILClone"
        | "ILCloneFirst" | "SHRestricted" | "SHLoadOLE" | "ILGetSize" | "ILGetNext" | "ILFree"
        | "ILCreateFromPath" | "SHSimpleIDListFromPath" | "SHFree" | "SHAlloc"
        | "DAD_ShowDragImage" | "SHLoadInProc" => 1,
        "ILIsEqual" | "ILFindChild" | "ILCombine" | "ILLoadFromStream" | "ILSaveToStream"
        | "Shell_GetImageLists" | "SHFindFiles" | "SHCLSIDFromString" | "SHCreateDirectory"
        | "PathCleanupSpec" | "SHCreateShellFolderViewEx" | "DAD_SetDragImage"
        // DAD_DragMove(POINT) passes the point by value.
        | "DAD_DragMove" => 2,
        "ILIsParent" | "RestartDialog" | "Shell_GetCachedImageIndex"
        | "SHShellFolderView_Message" | "SHCreateStdEnumFmtEtc"
        | "SHMapPIDLToSystemImageListIndex" | "SHCloneSpecialIDList" | "DAD_AutoScroll"
        | "DAD_DragEnterEx" => 3,
        "PickIconDlg" | "PathYetAnotherMakeUniqueName" | "OpenRegStream"
        | "SHGetSpecialFolderPath" | "SHGetSpecialFolderPathA" => 4,
        "SHDoDragDrop" | "SHCoCreateInstance" => 5,
        "Shell_MergeMenus" => 6,
        "GetFileNameFromBrowse" => 7,
        _ => return None,
    })
}

fn u16_at<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> u16 {
    memory
        .read_bytes(va, 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0)
}

/// The items of an ID list, as (address, size).
fn items<M: GuestMemory + ?Sized>(memory: &mut M, pidl: u32) -> Vec<(u32, u32)> {
    let mut list = Vec::new();
    if pidl == 0 {
        return list;
    }
    let mut at = pidl;
    loop {
        let size = u16_at(memory, at) as u32;
        if size == 0 || list.len() > 256 {
            break;
        }
        list.push((at, size));
        at += size;
    }
    list
}

/// Bytes of an ID list without its terminator.
fn bytes_of<M: GuestMemory + ?Sized>(memory: &mut M, pidl: u32) -> Vec<u8> {
    let size: u32 = items(memory, pidl).iter().map(|(_, size)| size).sum();
    if size == 0 {
        Vec::new()
    } else {
        memory.read_bytes(pidl, size as usize).unwrap_or_default()
    }
}

/// A new ID list (terminated) holding `bytes`.
fn alloc_list<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, bytes: &[u8]) -> u32 {
    let block = ctx
        .shared
        .borrow_mut()
        .heap
        .alloc(bytes.len() as u32 + 2)
        .unwrap_or(0);
    if block != 0 {
        let mut data = bytes.to_vec();
        data.extend_from_slice(&[0, 0]);
        let _ = memory.write_bytes(block, &data);
    }
    block
}

/// Parse `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`.
pub fn parse_guid(text: &str) -> Option<[u8; 16]> {
    let text = text.trim().trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = text.split('-').collect();
    if parts.len() != 5 || parts[3].len() != 4 || parts[4].len() != 12 {
        return None;
    }
    let mut guid = [0u8; 16];
    guid[..4].copy_from_slice(&u32::from_str_radix(parts[0], 16).ok()?.to_le_bytes());
    guid[4..6].copy_from_slice(&u16::from_str_radix(parts[1], 16).ok()?.to_le_bytes());
    guid[6..8].copy_from_slice(&u16::from_str_radix(parts[2], 16).ok()?.to_le_bytes());
    let tail = format!("{}{}", parts[3], parts[4]);
    for (index, byte) in guid[8..].iter_mut().enumerate() {
        *byte = u8::from_str_radix(tail.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(guid)
}

/// Format a GUID as the registry writes it.
pub fn guid_text(guid: &[u8]) -> String {
    let d1 = u32::from_le_bytes([guid[0], guid[1], guid[2], guid[3]]);
    let d2 = u16::from_le_bytes([guid[4], guid[5]]);
    let d3 = u16::from_le_bytes([guid[6], guid[7]]);
    let tail: String = guid[8..16].iter().map(|b| format!("{b:02X}")).collect();
    format!("{{{d1:08X}-{d2:04X}-{d3:04X}-{}-{}}}", &tail[..4], &tail[4..])
}

/// Read a string that is ANSI on Windows 95 but may be UTF-16 (the `AW`
/// exports take either; UTF-16 shows as a second byte of zero).
fn read_aw<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> Vec<u8> {
    let probe = memory.read_bytes(va, 2).unwrap_or_default();
    if probe.len() == 2 && probe[0] != 0 && probe[1] == 0 {
        let mut out = Vec::new();
        let mut at = va;
        while let Some(unit) = memory.read_bytes(at, 2) {
            if unit == [0, 0] || out.len() > 4096 {
                break;
            }
            out.push(unit[0]);
            at += 2;
        }
        out
    } else {
        read_cstr(memory, va)
    }
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let value = match name {
        "SHAlloc" => ctx.shared.borrow_mut().heap.alloc(args[0]).unwrap_or(0),
        "SHFree" | "ILFree" => {
            if args[0] != 0 {
                ctx.shared.borrow_mut().heap.free(args[0]);
            }
            0
        }
        "ILGetSize" => {
            let size: u32 = items(memory, args[0]).iter().map(|(_, size)| size).sum();
            if args[0] == 0 {
                0
            } else {
                size + 2
            }
        }
        "ILGetNext" => match items(memory, args[0]).first() {
            Some((at, size)) => at + size,
            None => 0,
        },
        "ILFindLastID" => items(memory, args[0]).last().map(|(at, _)| *at).unwrap_or(args[0]),
        "ILRemoveLastID" => match items(memory, args[0]).last() {
            Some((at, _)) => {
                let _ = memory.write_bytes(*at, &[0, 0]);
                1
            }
            None => 0,
        },
        "ILClone" => {
            if args[0] == 0 {
                0
            } else {
                let bytes = bytes_of(memory, args[0]);
                alloc_list(ctx, memory, &bytes)
            }
        }
        "ILCloneFirst" => match items(memory, args[0]).first() {
            Some(&(at, size)) => {
                let bytes = memory.read_bytes(at, size as usize).unwrap_or_default();
                alloc_list(ctx, memory, &bytes)
            }
            None if args[0] != 0 => alloc_list(ctx, memory, &[]),
            None => 0,
        },
        "ILCombine" => {
            if args[0] == 0 && args[1] == 0 {
                0
            } else {
                let mut bytes = bytes_of(memory, args[0]);
                bytes.extend(bytes_of(memory, args[1]));
                alloc_list(ctx, memory, &bytes)
            }
        }
        "ILIsEqual" => u32::from(bytes_of(memory, args[0]) == bytes_of(memory, args[1])),
        "ILIsParent" => {
            // (parent, child, immediate)
            let parent = items(memory, args[0]);
            let child = items(memory, args[1]);
            let prefix = bytes_of(memory, args[0]);
            let whole = bytes_of(memory, args[1]);
            let is_prefix = child.len() > parent.len() && whole.starts_with(&prefix);
            u32::from(is_prefix && (args[2] == 0 || child.len() == parent.len() + 1))
        }
        "ILFindChild" => {
            let prefix = bytes_of(memory, args[0]);
            let whole = bytes_of(memory, args[1]);
            if whole.starts_with(&prefix) {
                args[1] + prefix.len() as u32
            } else {
                0
            }
        }
        "ILLoadFromStream" | "ILSaveToStream" => E_NOTIMPL,
        "ILCreateFromPath" | "SHSimpleIDListFromPath" | "ILCreateFromPathA" => {
            let path = String::from_utf8_lossy(&read_aw(memory, args[0])).into_owned();
            unicode::make_pidl(ctx, memory, &path)
        }
        "SHCloneSpecialIDList" => match unicode::special_folder(args[1]) {
            Some(path) => unicode::make_pidl(ctx, memory, path),
            None => 0,
        },
        "SHGetSpecialFolderPath" | "SHGetSpecialFolderPathA" => {
            // (owner, buffer, csidl, create): ANSI on Windows 95.
            match unicode::special_folder(args[2]) {
                Some(path) => {
                    if let Some(host) = crate::win32::crt::host_path_in(ctx, path.as_bytes()) {
                        let _ = std::fs::create_dir_all(host);
                    }
                    let _ = crate::win32::write_cstr(memory, args[1], path.as_bytes());
                    1
                }
                None => 0,
            }
        }
        "SHCreateDirectory" => {
            let path = read_aw(memory, args[1]);
            match crate::win32::crt::host_path_in(ctx, &path) {
                Some(host) if std::fs::create_dir_all(&host).is_ok() => 0,
                _ => 3, // ERROR_PATH_NOT_FOUND
            }
        }
        "SHCLSIDFromString" => {
            let text = String::from_utf8_lossy(&read_aw(memory, args[0])).into_owned();
            match parse_guid(&text) {
                Some(guid) => {
                    let _ = memory.write_bytes(args[1], &guid);
                    S_OK
                }
                None => 0x8004_01F3, // CO_E_CLASSSTRING
            }
        }
        "SHCoCreateInstance" => {
            // (clsid text, &clsid, outer, iid, out)
            let mut clsid = args[1];
            if clsid == 0 {
                let text = String::from_utf8_lossy(&read_aw(memory, args[0])).into_owned();
                let Some(guid) = parse_guid(&text) else {
                    let _ = memory.write_u32(args[4], 0);
                    return Some(ApiOutcome::new(0x8004_01F3, count));
                };
                clsid = ctx.shared.borrow_mut().heap.alloc(16).unwrap_or(0);
                let _ = memory.write_bytes(clsid, &guid);
            }
            crate::win32::dispatch_with_memory(
                ctx,
                memory,
                "OLE32.dll",
                &ImportName::ByName("CoCreateInstance".into()),
                &[clsid, args[2], 1, args[3], args[4]],
                0,
            )
            .map(|outcome| outcome.value)
            .unwrap_or(E_FAIL)
        }
        "SHRestricted" => 0,
        "SHILCreateFromPath" => {
            // (path, &pidl, &attributes)
            let path = String::from_utf8_lossy(&read_aw(memory, args[0])).into_owned();
            let pidl = unicode::make_pidl(ctx, memory, &path);
            let _ = memory.write_u32(args[1], pidl);
            S_OK
        }
        "SHGetPathFromIDListA" => match unicode::pidl_path(memory, args[0]) {
            Some(path) => {
                let _ = crate::win32::write_cstr(memory, args[1], &path);
                1
            }
            None => 0,
        },
        "SHGetRealIDL" => {
            // (folder, pidl, &real): the same list.
            let bytes = bytes_of(memory, args[1]);
            let copy = alloc_list(ctx, memory, &bytes);
            let _ = memory.write_u32(args[2], copy);
            S_OK
        }
        "ILAppendID" => {
            // (pidl, item, append at end): a new list; the old one is freed.
            let mut bytes = bytes_of(memory, args[0]);
            let size = u16_at(memory, args[1]) as usize;
            let item = memory.read_bytes(args[1], size).unwrap_or_default();
            if args[2] != 0 {
                bytes.extend(item);
            } else {
                bytes.splice(0..0, item);
            }
            if args[0] != 0 {
                ctx.shared.borrow_mut().heap.free(args[0]);
            }
            alloc_list(ctx, memory, &bytes)
        }
        "PathIsExe" => {
            let path = read_aw(memory, args[0]).to_ascii_lowercase();
            u32::from([".exe", ".com", ".bat", ".pif", ".cmd"].iter().any(|e| path.ends_with(e.as_bytes())))
        }
        "ParseField" => 0,
        // Drive 2 (C:) is the only drive: fixed, local, long names.
        "DriveType" => {
            if args[0] == 2 {
                3
            } else {
                1
            }
        }
        "IsNetDrive" => 0,
        "IsLFNDrive" => 1,
        "SHGetSetSettings" => {
            // (SHELLSTATE*, mask, set): default settings. The structure
            // grew over Windows versions; its first word (the flag bits)
            // is common to all.
            if args[2] == 0 && args[0] != 0 {
                let _ = memory.write_u32(args[0], 0);
            }
            0
        }
        "CIDLData_CreateFromIDArray" => {
            let _ = memory.write_u32(args[3], 0);
            E_NOTIMPL
        }
        "SHChangeNotifyRegister" => 1,
        "SHRegisterDragDrop" | "SHRevokeDragDrop" => S_OK,
        "SignalFileOpen" => 1,
        "SHFlushClipboard" | "SHFreeUnusedLibraries" | "SHSetInstanceExplorer"
        | "SHFind_InitMenuPopup" => 0,
        "SHAppBarMessage" => 0,
        "ShellAboutW" => 1,
        "ExtractAssociatedIconExW" => 0,
        "SHGetDataFromIDListA" => E_NOTIMPL,
        "SHLoadOLE" | "SHLoadInProc" => S_OK,
        "SHChangeNotifyDeregister" => 1,
        "Shell_GetImageLists" => {
            for out in [args[0], args[1]] {
                if out != 0 {
                    let _ = memory.write_ptr(out, 0);
                }
            }
            0
        }
        "Shell_GetCachedImageIndex" => 0,
        "SHMapPIDLToSystemImageListIndex" => {
            if args[2] != 0 {
                let _ = memory.write_u32(args[2], 0);
            }
            0
        }
        "SHShellFolderView_Message" | "Shell_MergeMenus" | "PathCleanupSpec" => 0,
        "SHCreateStdEnumFmtEtc" => {
            let _ = memory.write_u32(args[2], 0);
            E_NOTIMPL
        }
        "SHCreateShellFolderViewEx" => {
            let _ = memory.write_u32(args[1], 0);
            E_NOTIMPL
        }
        "OpenRegStream" => 0,
        "RestartDialog" => 7, // IDNO
        "PickIconDlg" | "GetFileNameFromBrowse" | "PathYetAnotherMakeUniqueName" | "SHFindFiles" => 0,
        "SHDoDragDrop" => 0x0004_0101, // DRAGDROP_S_CANCEL
        "DAD_AutoScroll" => 0,
        "DAD_DragEnterEx" | "DAD_DragLeave" | "DAD_DragMove" | "DAD_SetDragImage"
        | "DAD_ShowDragImage" => 1,
        _ => return None,
    };
    Some(ApiOutcome::new(value, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_parse_and_format() {
        let text = "{25336920-03F9-11CF-8FD0-00AA00686F13}";
        let guid = parse_guid(text).unwrap();
        assert_eq!(guid[..4], [0x20, 0x69, 0x33, 0x25]);
        assert_eq!(guid[8], 0x8F);
        assert_eq!(guid_text(&guid), text);
    }
}
