//! The shell link object (`CLSID_ShellLink`): installers create Start Menu
//! and desktop shortcuts with `CoCreateInstance`, fill in an `IShellLinkA`
//! or `IShellLinkW`, and save it through `IPersistFile`. The object writes
//! the same `.LNK` files the shell reads.
//!
//! One guest block holds three interface pointers, each a vtable of
//! Rust-backed methods; the link itself lives on the host, keyed by the
//! block address.

use crate::win32::{
    crt,
    lnk::{self, ShellLink},
    read_cstr, unicode, ApiContext, ApiOutcome, GuestMemory,
};

const S_OK: u32 = 0;
const S_FALSE: u32 = 1;
const E_NOINTERFACE: u32 = 0x8000_4002;
const E_FAIL: u32 = 0x8000_4005;
const E_OUTOFMEMORY: u32 = 0x8007_000E;
const STG_E_FILENOTFOUND: u32 = 0x8003_0002;

pub const CLSID_SHELL_LINK: [u8; 16] = guid(0x0002_1401);
const IID_IUNKNOWN: [u8; 16] = guid(0);
const IID_IPERSIST: [u8; 16] = guid(0x0000_010C);
const IID_IPERSIST_FILE: [u8; 16] = guid(0x0000_010B);
const IID_ISHELL_LINK_A: [u8; 16] = guid(0x0002_14EE);
const IID_ISHELL_LINK_W: [u8; 16] = guid(0x0002_14F9);

/// `{xxxxxxxx-0000-0000-C000-000000000046}`, the form of every OLE and
/// shell GUID used here.
const fn guid(data1: u32) -> [u8; 16] {
    let d = data1.to_le_bytes();
    [d[0], d[1], d[2], d[3], 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46]
}

/// Interface pointers within the object: IShellLinkW, IPersistFile,
/// IShellLinkA.
const INTERFACES: [(&str, u32); 3] = [("ShellLinkW_", 0), ("PersistFile_", 4), ("ShellLinkA_", 8)];
const OBJECT_SIZE: u32 = 12;

/// An interface's offset in the object: interface pointers are 8 bytes in
/// a 64-bit process.
fn scaled(ctx: &ApiContext, offset: u32) -> u32 {
    offset * crate::win32::ptr_size(ctx) / 4
}

/// The methods, in vtable order per interface.
pub const SHELL_LINK_METHODS: &[&str] = &[
    "ShellLinkW_QueryInterface",
    "ShellLinkW_AddRef",
    "ShellLinkW_Release",
    "ShellLinkW_GetPath",
    "ShellLinkW_GetIDList",
    "ShellLinkW_SetIDList",
    "ShellLinkW_GetDescription",
    "ShellLinkW_SetDescription",
    "ShellLinkW_GetWorkingDirectory",
    "ShellLinkW_SetWorkingDirectory",
    "ShellLinkW_GetArguments",
    "ShellLinkW_SetArguments",
    "ShellLinkW_GetHotkey",
    "ShellLinkW_SetHotkey",
    "ShellLinkW_GetShowCmd",
    "ShellLinkW_SetShowCmd",
    "ShellLinkW_GetIconLocation",
    "ShellLinkW_SetIconLocation",
    "ShellLinkW_SetRelativePath",
    "ShellLinkW_Resolve",
    "ShellLinkW_SetPath",
    "PersistFile_QueryInterface",
    "PersistFile_AddRef",
    "PersistFile_Release",
    "PersistFile_GetClassID",
    "PersistFile_IsDirty",
    "PersistFile_Load",
    "PersistFile_Save",
    "PersistFile_SaveCompleted",
    "PersistFile_GetCurFile",
    "ShellLinkA_QueryInterface",
    "ShellLinkA_AddRef",
    "ShellLinkA_Release",
    "ShellLinkA_GetPath",
    "ShellLinkA_GetIDList",
    "ShellLinkA_SetIDList",
    "ShellLinkA_GetDescription",
    "ShellLinkA_SetDescription",
    "ShellLinkA_GetWorkingDirectory",
    "ShellLinkA_SetWorkingDirectory",
    "ShellLinkA_GetArguments",
    "ShellLinkA_SetArguments",
    "ShellLinkA_GetHotkey",
    "ShellLinkA_SetHotkey",
    "ShellLinkA_GetShowCmd",
    "ShellLinkA_SetShowCmd",
    "ShellLinkA_GetIconLocation",
    "ShellLinkA_SetIconLocation",
    "ShellLinkA_SetRelativePath",
    "ShellLinkA_Resolve",
    "ShellLinkA_SetPath",
];

/// A link being edited.
#[derive(Clone, Debug, Default)]
pub struct LinkObject {
    link: ShellLink,
    refs: u32,
    dirty: bool,
}

pub fn arg_count(name: &str) -> Option<usize> {
    let (_, method) = name.split_once('_')?;
    if !SHELL_LINK_METHODS.contains(&name) {
        return None;
    }
    Some(match method {
        "AddRef" | "Release" | "IsDirty" => 1,
        "GetIDList" | "SetIDList" | "SetDescription" | "SetWorkingDirectory" | "SetArguments" | "GetHotkey"
        | "SetHotkey" | "GetShowCmd" | "SetShowCmd" | "SetPath" | "GetClassID" | "SaveCompleted" | "GetCurFile" => 2,
        "QueryInterface" | "GetDescription" | "GetWorkingDirectory" | "GetArguments" | "SetIconLocation"
        | "SetRelativePath" | "Resolve" | "Load" | "Save" => 3,
        "GetIconLocation" => 4,
        "GetPath" => 5,
        _ => return None,
    })
}

/// Create a shell link object and return the interface `iid` asks for.
pub fn create<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, iid: &[u8], out: u32) -> u32 {
    let vtables: u32 = SHELL_LINK_METHODS.len() as u32 * 4;
    let Some(object) = ctx.shared.borrow_mut().heap.alloc(2 * OBJECT_SIZE + vtables) else {
        return E_OUTOFMEMORY;
    };
    for (prefix, offset) in INTERFACES {
        let names: Vec<&str> = SHELL_LINK_METHODS.iter().copied().filter(|name| name.starts_with(prefix)).collect();
        crate::win32::set_vtable(ctx, memory, object + scaled(ctx, offset), &names);
    }
    ctx.shared.borrow_mut().shell_links.insert(object, LinkObject::default());
    let result = query_interface(ctx, memory, object, iid, out);
    if result != S_OK {
        ctx.shared.borrow_mut().shell_links.remove(&object);
        ctx.shared.borrow_mut().heap.free(object);
    }
    result
}

fn query_interface<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, object: u32, iid: &[u8], out: u32) -> u32 {
    let offset = match iid {
        _ if iid == IID_IUNKNOWN || iid == IID_ISHELL_LINK_W => 0,
        _ if iid == IID_IPERSIST_FILE || iid == IID_IPERSIST => 4,
        _ if iid == IID_ISHELL_LINK_A => 8,
        _ => {
            let _ = memory.write_ptr(out, 0);
            return E_NOINTERFACE;
        }
    };
    if let Some(link) = ctx.shared.borrow_mut().shell_links.get_mut(&object) {
        link.refs += 1;
    }
    let _ = memory.write_ptr(out, u64::from(object + scaled(ctx, offset)));
    S_OK
}

/// Read a string in the interface's character set.
fn string<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, wide: bool) -> String {
    if va == 0 {
        return String::new();
    }
    let bytes = if wide { unicode::wide_to_ansi(&unicode::read_wstr(memory, va)) } else { read_cstr(memory, va) };
    bytes.iter().map(|&b| b as char).collect()
}

/// Write `text` to a buffer of `capacity` characters, truncated, with a NUL.
fn put<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, capacity: u32, text: &str, wide: bool) {
    if va == 0 || capacity == 0 {
        return;
    }
    let bytes: Vec<u8> = text.chars().map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' }).collect();
    let count = bytes.len().min(capacity as usize - 1);
    if wide {
        unicode::write_wide(memory, va, &unicode::ansi_to_wide(&bytes[..count]), true);
    } else {
        let _ = memory.write_bytes(va, &[&bytes[..count], b"\0"].concat());
    }
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    let (prefix, method) = name.split_once('_')?;
    let offset = INTERFACES.iter().find(|(p, _)| p.trim_end_matches('_') == prefix)?.1;
    let object = args[0].wrapping_sub(scaled(ctx, offset));
    if !ctx.shared.borrow().shell_links.contains_key(&object) {
        return ok(E_FAIL);
    }
    let wide = prefix != "ShellLinkA";
    let get = |ctx: &ApiContext| ctx.shared.borrow().shell_links[&object].link.clone();
    let edit = |ctx: &ApiContext, change: &dyn Fn(&mut ShellLink)| {
        let mut shared = ctx.shared.borrow_mut();
        let entry = shared.shell_links.get_mut(&object).expect("checked above");
        change(&mut entry.link);
        entry.dirty = true;
    };
    match method {
        "QueryInterface" => {
            let iid = memory.read_bytes(args[1], 16).unwrap_or_default();
            ok(query_interface(ctx, memory, object, &iid, args[2]))
        }
        "AddRef" => {
            let mut shared = ctx.shared.borrow_mut();
            let entry = shared.shell_links.get_mut(&object).expect("checked above");
            entry.refs += 1;
            ok(entry.refs)
        }
        "Release" => {
            let mut shared = ctx.shared.borrow_mut();
            let entry = shared.shell_links.get_mut(&object).expect("checked above");
            entry.refs = entry.refs.saturating_sub(1);
            let refs = entry.refs;
            if refs == 0 {
                shared.shell_links.remove(&object);
                shared.heap.free(object);
            }
            ok(refs)
        }
        // IShellLink
        "GetPath" => {
            let link = get(ctx);
            put(memory, args[1], args[2], &link.target, wide);
            if args[3] != 0 {
                // WIN32_FIND_DATA: zeroed, which callers accept.
                let size = if wide { 592 } else { 320 };
                let _ = memory.write_bytes(args[3], &vec![0u8; size]);
            }
            ok(if link.target.is_empty() { S_FALSE } else { S_OK })
        }
        "GetIDList" => {
            let _ = memory.write_ptr(args[1], 0);
            ok(E_FAIL)
        }
        "SetIDList" | "SetHotkey" | "SetShowCmd" | "SetRelativePath" | "Resolve" => ok(S_OK),
        "GetHotkey" => {
            let _ = memory.write_bytes(args[1], &[0, 0]);
            ok(S_OK)
        }
        "GetShowCmd" => {
            let _ = memory.write_u32(args[1], 1); // SW_SHOWNORMAL
            ok(S_OK)
        }
        "GetDescription" | "GetWorkingDirectory" | "GetArguments" => {
            let link = get(ctx);
            let text = match method {
                "GetDescription" => link.description,
                "GetWorkingDirectory" => link.working_dir,
                _ => link.arguments,
            };
            put(memory, args[1], args[2], &text, wide);
            ok(S_OK)
        }
        "SetPath" | "SetDescription" | "SetWorkingDirectory" | "SetArguments" => {
            let text = string(memory, args[1], wide);
            edit(ctx, &|link| match method {
                "SetPath" => link.target = text.clone(),
                "SetDescription" => link.description = text.clone(),
                "SetWorkingDirectory" => link.working_dir = text.clone(),
                _ => link.arguments = text.clone(),
            });
            ok(S_OK)
        }
        "GetIconLocation" => {
            put(memory, args[1], args[2], "", wide);
            let _ = memory.write_u32(args[3], 0);
            ok(S_OK)
        }
        // The shell takes icons from the target, so the location is not kept.
        "SetIconLocation" => ok(S_OK),
        // IPersistFile
        "GetClassID" => {
            let _ = memory.write_bytes(args[1], &CLSID_SHELL_LINK);
            ok(S_OK)
        }
        "IsDirty" => ok(if ctx.shared.borrow().shell_links[&object].dirty { S_OK } else { S_FALSE }),
        "Load" => {
            let path = string(memory, args[1], true);
            let link = crt::host_path_in(ctx, path.as_bytes()).and_then(|p| std::fs::read(p).ok()).and_then(|b| lnk::read(&b));
            match link {
                Some(link) => {
                    let mut shared = ctx.shared.borrow_mut();
                    let entry = shared.shell_links.get_mut(&object).expect("checked above");
                    entry.link = link;
                    entry.dirty = false;
                    ok(S_OK)
                }
                None => ok(STG_E_FILENOTFOUND),
            }
        }
        "Save" => {
            let path = string(memory, args[1], true);
            let bytes = lnk::write(&get(ctx));
            let saved = crt::host_path_in(ctx, path.as_bytes()).is_some_and(|p| std::fs::write(p, bytes).is_ok());
            if saved {
                if let Some(entry) = ctx.shared.borrow_mut().shell_links.get_mut(&object) {
                    entry.dirty = false;
                }
            }
            ok(if saved { S_OK } else { E_FAIL })
        }
        "SaveCompleted" => ok(S_OK),
        "GetCurFile" => {
            let _ = memory.write_u32(args[1], 0);
            ok(S_FALSE)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_has_an_argument_count() {
        for name in SHELL_LINK_METHODS {
            assert!(arg_count(name).is_some(), "{name}");
        }
        assert_eq!(SHELL_LINK_METHODS.iter().filter(|n| n.starts_with("ShellLinkW_")).count(), 21);
        assert_eq!(SHELL_LINK_METHODS.iter().filter(|n| n.starts_with("PersistFile_")).count(), 9);
        assert_eq!(SHELL_LINK_METHODS.iter().filter(|n| n.starts_with("ShellLinkA_")).count(), 21);
    }
}
