//! The shell namespace: the desktop folder (`SHGetDesktopFolder`) and the
//! file-system folders below it, as `IShellFolder` objects, with
//! `IEnumIDList` to list a folder.
//!
//! Item ID lists (PIDLs) for files are one item per path component: a
//! drive item (`C:\`) then folder and file items, each its name. Items of
//! type 0x1F hold a class ID (a "regitem", such as Internet Explorer's
//! Internet folder); binding to one creates that class and initialises it
//! with its absolute ID list, so namespace extensions supply their own
//! items below it.

use crate::win32::{com, read_cstr, unicode, ApiContext, ApiOutcome, GuestMemory};

const S_OK: u32 = 0;
const S_FALSE: u32 = 1;
const E_NOTIMPL: u32 = 0x8000_4001;
const E_NOINTERFACE: u32 = 0x8000_4002;
const E_FAIL: u32 = 0x8000_4005;
const E_INVALIDARG: u32 = 0x8007_0057;
const FILE_NOT_FOUND: u32 = 0x8007_0002;

const IID_IUNKNOWN: [u8; 16] = guid(0);
const IID_ISHELLFOLDER: [u8; 16] = guid(0x0002_14E6);
const IID_IENUMIDLIST: [u8; 16] = guid(0x0002_14F2);
const IID_IPERSISTFOLDER: [u8; 16] = guid(0x0002_14EA);

const fn guid(data1: u32) -> [u8; 16] {
    let d = data1.to_le_bytes();
    [d[0], d[1], d[2], d[3], 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46]
}

/// Item types.
pub const ITEM_REGITEM: u8 = 0x1F;
pub const ITEM_DRIVE: u8 = 0x2F;
pub const ITEM_FOLDER: u8 = 0x31;
pub const ITEM_FILE: u8 = 0x32;

/// The shell's own virtual folders.
pub const CLSID_MY_COMPUTER: [u8; 16] =
    [0xE0, 0x4F, 0xD0, 0x20, 0xEA, 0x3A, 0x69, 0x10, 0xA2, 0xD8, 0x08, 0x00, 0x2B, 0x30, 0x30, 0x9D];
const CLSID_NETWORK: [u8; 16] =
    [0x60, 0x2C, 0x8D, 0x20, 0xEA, 0x3A, 0x69, 0x10, 0xA2, 0xD7, 0x08, 0x00, 0x2B, 0x30, 0x30, 0x9D];
const CLSID_RECYCLE_BIN: [u8; 16] =
    [0x40, 0xF0, 0x5F, 0x64, 0x81, 0x50, 0x1B, 0x10, 0x9F, 0x08, 0x00, 0xAA, 0x00, 0x2F, 0x95, 0x4E];

/// A class item: sort order, then the class ID.
pub fn regitem(clsid: &[u8; 16]) -> Vec<u8> {
    let mut data = vec![0x50];
    data.extend_from_slice(clsid);
    item(ITEM_REGITEM, &data)
}

/// The ID lists of the shell's virtual folders (`CSIDL_*`): the desktop is
/// the empty list.
pub fn special_items(csidl: u32) -> Option<Vec<u8>> {
    Some(match csidl & 0xFF {
        0x00 => Vec::new(),
        0x11 => regitem(&CLSID_MY_COMPUTER),
        0x12 | 0x13 => regitem(&CLSID_NETWORK),
        0x0A => regitem(&CLSID_RECYCLE_BIN),
        _ => return None,
    })
}

fn is_my_computer(data: &[u8]) -> bool {
    data.len() >= 17 && data[1..17] == CLSID_MY_COMPUTER
}

const SFGAO_STREAM: u32 = 0x0040_0000;
const SFGAO_BROWSABLE: u32 = 0x0800_0000;
const SFGAO_FILESYSANCESTOR: u32 = 0x1000_0000;
const SFGAO_FOLDER: u32 = 0x2000_0000;
const SFGAO_FILESYSTEM: u32 = 0x4000_0000;
const SFGAO_HASSUBFOLDER: u32 = 0x8000_0000;

pub const METHODS: &[&str] = &[
    "ShellFolder_QueryInterface",
    "ShellFolder_AddRef",
    "ShellFolder_Release",
    "ShellFolder_ParseDisplayName",
    "ShellFolder_EnumObjects",
    "ShellFolder_BindToObject",
    "ShellFolder_BindToStorage",
    "ShellFolder_CompareIDs",
    "ShellFolder_CreateViewObject",
    "ShellFolder_GetAttributesOf",
    "ShellFolder_GetUIObjectOf",
    "ShellFolder_GetDisplayNameOf",
    "ShellFolder_SetNameOf",
    "EnumIDList_QueryInterface",
    "EnumIDList_AddRef",
    "EnumIDList_Release",
    "EnumIDList_Next",
    "EnumIDList_Skip",
    "EnumIDList_Reset",
    "EnumIDList_Clone",
];

/// A folder object: the file-system path it shows (empty for the desktop).
#[derive(Clone, Debug, Default)]
pub struct Folder {
    path: String,
    refs: u32,
}

/// An enumeration of a folder's items.
#[derive(Clone, Debug, Default)]
pub struct Enumeration {
    items: Vec<Vec<u8>>,
    next: usize,
    refs: u32,
}

pub fn arg_count(name: &str) -> Option<usize> {
    if !METHODS.contains(&name) {
        return None;
    }
    let (_, method) = name.split_once('_')?;
    Some(match method {
        "AddRef" | "Release" | "Reset" => 1,
        "Skip" | "Clone" => 2,
        "QueryInterface" => 3,
        "EnumObjects" | "CreateViewObject" | "GetAttributesOf" | "CompareIDs" | "GetDisplayNameOf"
        | "Next" => 4,
        "BindToObject" | "BindToStorage" => 5,
        "SetNameOf" => 6,
        "ParseDisplayName" | "GetUIObjectOf" => 7,
        _ => return None,
    })
}

// ── Item ID lists ──────────────────────────────────────────────────────────

/// One item: size, type, data.
pub fn item(kind: u8, data: &[u8]) -> Vec<u8> {
    let size = (data.len() + 3) as u16;
    let mut item = size.to_le_bytes().to_vec();
    item.push(kind);
    item.extend_from_slice(data);
    item
}

/// The items of a path, relative to the folder `base` (empty: the desktop).
pub fn path_items(base: &str, path: &str) -> Option<Vec<u8>> {
    let path = path.replace('/', "\\");
    let mut list = Vec::new();
    let rest = if base.is_empty() {
        let bytes = path.as_bytes();
        if bytes.len() < 2 || bytes[1] != b':' {
            return None;
        }
        let drive = format!("{}:\\", (bytes[0] as char).to_ascii_uppercase());
        list.extend(item(ITEM_DRIVE, &[drive.as_bytes(), b"\0"].concat()));
        path[2..].to_owned()
    } else {
        path.clone()
    };
    let mut current = base.trim_end_matches('\\').to_owned();
    if base.is_empty() {
        current = path[..2].to_owned();
    }
    for part in rest.split('\\').filter(|p| !p.is_empty()) {
        current = format!("{current}\\{part}");
        let kind = if is_dir(&current) { ITEM_FOLDER } else { ITEM_FILE };
        list.extend(item(kind, &[part.as_bytes(), b"\0"].concat()));
    }
    Some(list)
}

fn is_dir(guest: &str) -> bool {
    crate::win32::crt::host_path(guest.as_bytes()).is_some_and(|host| host.is_dir())
}

/// Items of a guest ID list: (type, data).
pub fn read_items<M: GuestMemory + ?Sized>(memory: &mut M, pidl: u32) -> Vec<(u8, Vec<u8>)> {
    let mut items = Vec::new();
    let mut at = pidl;
    if pidl == 0 {
        return items;
    }
    while items.len() < 64 {
        let size = memory.read_bytes(at, 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0) as u32;
        if size < 3 {
            break;
        }
        let bytes = memory.read_bytes(at + 2, size as usize - 2).unwrap_or_default();
        items.push((bytes[0], bytes[1..].to_vec()));
        at += size;
    }
    items
}

fn name_of(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    data[..end].iter().map(|&b| b as char).collect()
}

/// The file-system path an ID list names below `base`, if it is one.
pub fn items_path(base: &str, items: &[(u8, Vec<u8>)]) -> Option<String> {
    let mut path = base.trim_end_matches('\\').to_owned();
    for (kind, data) in items {
        match *kind {
            // Drives are below My Computer.
            ITEM_REGITEM if is_my_computer(data) => {}
            ITEM_DRIVE => path = name_of(data).trim_end_matches('\\').to_owned(),
            ITEM_FOLDER | ITEM_FILE => path = format!("{path}\\{}", name_of(data)),
            _ => return None,
        }
    }
    if path.len() == 2 {
        path.push('\\');
    }
    (!path.is_empty()).then_some(path)
}

/// A new guest ID list holding `bytes` (and the terminator).
pub fn alloc_list<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, bytes: &[u8]) -> u32 {
    let block = ctx.shared.borrow_mut().heap.alloc(bytes.len() as u32 + 2).unwrap_or(0);
    let _ = memory.write_bytes(block, &[bytes, &[0, 0]].concat());
    block
}

// ── Objects ────────────────────────────────────────────────────────────────

/// A folder object for `path` (empty: the desktop), as `IShellFolder`.
pub fn folder<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, path: &str) -> u32 {
    let object = make_object(ctx, memory, "ShellFolder_");
    ctx.shared.borrow_mut().shell_folders.insert(
        object,
        Folder {
            path: path.to_owned(),
            refs: 1,
        },
    );
    object
}

fn make_object<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, prefix: &str) -> u32 {
    let methods: Vec<&&str> = METHODS.iter().filter(|m| m.starts_with(prefix)).collect();
    let object = ctx.shared.borrow_mut().heap.alloc(4 + methods.len() as u32 * 4).unwrap_or(0);
    let _ = memory.write_u32(object, object + 4);
    for (index, name) in methods.iter().enumerate() {
        let _ = memory.write_u32(object + 4 + index as u32 * 4, ctx.builtin_proc(name));
    }
    let names: Vec<&str> = methods.iter().map(|m| **m).collect();
    crate::win32::wide_vtable(ctx, memory, object, &names);
    object
}

/// The desktop folder (one per program).
pub fn desktop<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) -> u32 {
    let existing = ctx.shared.borrow().desktop_folder;
    if existing != 0 {
        if let Some(folder) = ctx.shared.borrow_mut().shell_folders.get_mut(&existing) {
            folder.refs += 1;
        }
        return existing;
    }
    let object = folder(ctx, memory, "");
    ctx.shared.borrow_mut().desktop_folder = object;
    object
}

fn read_guid<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> [u8; 16] {
    let mut guid = [0u8; 16];
    if let Some(bytes) = memory.read_bytes(va, 16) {
        guid.copy_from_slice(&bytes);
    }
    guid
}

/// Call method `index` of a COM object.
fn method<M: GuestMemory + ?Sized>(memory: &mut M, object: u32, index: u32, args: &[u32]) -> u32 {
    let function = memory.read_u32(object).and_then(|vtable| memory.read_u32(vtable + index * 4));
    let mut all = vec![object];
    all.extend_from_slice(args);
    function.and_then(|f| memory.call_guest(f, &all)).unwrap_or(E_FAIL)
}

/// Bind to the object an ID list (relative to `base`) names.
fn bind<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    base: &str,
    pidl: u32,
    iid: u32,
    out: u32,
) -> u32 {
    let _ = memory.write_u32(out, 0);
    let items = read_items(memory, pidl);
    let wanted = read_guid(memory, iid);
    if let Some((ITEM_REGITEM, data)) = items.first().filter(|(_, data)| !is_my_computer(data)) {
        // A namespace extension: create it, initialise it with its ID list,
        // and let it bind the rest.
        let mut clsid = [0u8; 16];
        if data.len() >= 17 {
            clsid.copy_from_slice(&data[1..17]);
        }
        let folder_iid = scratch_guid(ctx, memory, &IID_ISHELLFOLDER);
        let slot = ctx.shared.borrow_mut().heap.alloc(4).unwrap_or(0);
        let status = com::create_instance(ctx, memory, &clsid, 0, folder_iid, slot);
        let extension = memory.read_u32(slot).unwrap_or(0);
        if status != S_OK || extension == 0 {
            free(ctx, folder_iid);
            free(ctx, slot);
            return status;
        }
        let first = alloc_list(ctx, memory, &item(ITEM_REGITEM, data));
        let persist_iid = scratch_guid(ctx, memory, &IID_IPERSISTFOLDER);
        if method(memory, extension, 0, &[persist_iid, slot]) == S_OK {
            let persist = memory.read_u32(slot).unwrap_or(0);
            method(memory, persist, 3, &[first]); // IPersistFolder::Initialize
            method(memory, persist, 2, &[]);
        }
        let size = 2 + data.len() as u32 + 1;
        let result = if items.len() == 1 {
            method(memory, extension, 0, &[iid, out])
        } else {
            // IShellFolder::BindToObject(rest, bind context, iid, out)
            method(memory, extension, 5, &[pidl + size, 0, iid, out])
        };
        method(memory, extension, 2, &[]);
        free(ctx, folder_iid);
        free(ctx, persist_iid);
        free(ctx, slot);
        return result;
    }
    let Some(path) = items_path(base, &items) else {
        return E_INVALIDARG;
    };
    if wanted != IID_ISHELLFOLDER && wanted != IID_IUNKNOWN {
        return E_NOINTERFACE;
    }
    if !is_dir(&path) {
        return FILE_NOT_FOUND;
    }
    let object = folder(ctx, memory, &path);
    let _ = memory.write_u32(out, object);
    S_OK
}

fn scratch_guid<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, guid: &[u8; 16]) -> u32 {
    let va = ctx.shared.borrow_mut().heap.alloc(16).unwrap_or(0);
    let _ = memory.write_bytes(va, guid);
    va
}

fn free(ctx: &ApiContext, va: u32) {
    if va != 0 {
        ctx.shared.borrow_mut().heap.free(va);
    }
}

/// Attributes of the item an ID list names below `base`.
fn attributes(base: &str, items: &[(u8, Vec<u8>)]) -> u32 {
    match items.last() {
        Some((ITEM_REGITEM, _)) => SFGAO_FOLDER | SFGAO_BROWSABLE | SFGAO_HASSUBFOLDER,
        _ => match items_path(base, items) {
            Some(path) if is_dir(&path) => {
                SFGAO_FOLDER | SFGAO_FILESYSTEM | SFGAO_FILESYSANCESTOR | SFGAO_HASSUBFOLDER
            }
            Some(_) => SFGAO_FILESYSTEM | SFGAO_STREAM,
            None => 0,
        },
    }
}

/// Write a name into a STRRET (as STRRET_CSTR).
fn write_strret<M: GuestMemory + ?Sized>(memory: &mut M, strret: u32, text: &str) {
    let bytes: Vec<u8> = text.chars().map(|c| c as u32 as u8).take(259).collect();
    let _ = memory.write_u32(strret, 2);
    let _ = memory.write_bytes(strret + 4, &[bytes.as_slice(), &[0]].concat());
}

/// The items of a folder, sorted, as ID lists of one item.
fn list_folder(path: &str) -> Vec<Vec<u8>> {
    if path.is_empty() {
        return vec![item(ITEM_DRIVE, b"C:\\\0")];
    }
    let Some(host) = crate::win32::crt::host_path(path.as_bytes()) else {
        return Vec::new();
    };
    let mut entries: Vec<(bool, String)> = std::fs::read_dir(host)
        .map(|dir| {
            dir.flatten()
                .map(|entry| (entry.path().is_dir(), entry.file_name().to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.to_ascii_lowercase().cmp(&b.1.to_ascii_lowercase())));
    entries
        .into_iter()
        .map(|(dir, name)| item(if dir { ITEM_FOLDER } else { ITEM_FILE }, &[name.as_bytes(), b"\0"].concat()))
        .collect()
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let (interface, method_name) = name.split_once('_')?;
    let this = args[0];
    let value = if interface == "EnumIDList" {
        enumeration(ctx, memory, method_name, args)
    } else {
        let Some(base) = ctx.shared.borrow().shell_folders.get(&this).map(|f| f.path.clone()) else {
            return Some(ApiOutcome::new(E_FAIL, count));
        };
        folder_method(ctx, memory, &base, method_name, args)
    };
    Some(ApiOutcome::new(value, count))
}

fn folder_method<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, base: &str, method_name: &str, args: &[u32]) -> u32 {
    let this = args[0];
    match method_name {
        "QueryInterface" => {
            let iid = read_guid(memory, args[1]);
            if iid == IID_IUNKNOWN || iid == IID_ISHELLFOLDER {
                if let Some(folder) = ctx.shared.borrow_mut().shell_folders.get_mut(&this) {
                    folder.refs += 1;
                }
                let _ = memory.write_u32(args[2], this);
                S_OK
            } else {
                let _ = memory.write_u32(args[2], 0);
                E_NOINTERFACE
            }
        }
        "AddRef" => {
            let mut shared = ctx.shared.borrow_mut();
            let folder = shared.shell_folders.get_mut(&this).expect("checked");
            folder.refs += 1;
            folder.refs
        }
        "Release" => {
            let mut shared = ctx.shared.borrow_mut();
            let desktop = shared.desktop_folder == this;
            let folder = shared.shell_folders.get_mut(&this).expect("checked");
            folder.refs = folder.refs.saturating_sub(1);
            let refs = folder.refs;
            if refs == 0 && !desktop {
                shared.shell_folders.remove(&this);
                shared.heap.free(this);
            }
            refs
        }
        "ParseDisplayName" => {
            // (window, bind context, name, &eaten, &pidl, &attributes)
            let mut text: String = unicode::wide_to_ansi(&unicode::read_wstr(memory, args[3]))
                .iter()
                .map(|&b| b as char)
                .collect();
            if let Some(rest) = text.strip_prefix("file:") {
                text = rest.trim_start_matches('/').replace('/', "\\").replace('|', ":");
            }
            let _ = memory.write_u32(args[5], 0);
            let full = if base.is_empty() {
                text.clone()
            } else {
                format!("{}\\{text}", base.trim_end_matches('\\'))
            };
            let exists = crate::win32::crt::host_path(full.as_bytes()).is_some_and(|host| host.exists());
            let Some(list) = path_items(base, &text).filter(|_| exists) else {
                return if exists { E_INVALIDARG } else { FILE_NOT_FOUND };
            };
            let pidl = alloc_list(ctx, memory, &list);
            let _ = memory.write_u32(args[5], pidl);
            if args[4] != 0 {
                let eaten = unicode::read_wstr(memory, args[3]).len() as u32;
                let _ = memory.write_u32(args[4], eaten);
            }
            if args[6] != 0 {
                let wanted = memory.read_u32(args[6]).unwrap_or(0);
                let items = read_items(memory, pidl);
                let _ = memory.write_u32(args[6], wanted & attributes(base, &items));
            }
            S_OK
        }
        "EnumObjects" => {
            // (window, flags, &enumerator)
            const SHCONTF_FOLDERS: u32 = 0x20;
            const SHCONTF_NONFOLDERS: u32 = 0x40;
            let items: Vec<Vec<u8>> = list_folder(base)
                .into_iter()
                .filter(|item| {
                    let folder = item[2] == ITEM_FOLDER || item[2] == ITEM_DRIVE;
                    (folder && args[2] & SHCONTF_FOLDERS != 0) || (!folder && args[2] & SHCONTF_NONFOLDERS != 0)
                })
                .collect();
            let object = make_object(ctx, memory, "EnumIDList_");
            ctx.shared
                .borrow_mut()
                .shell_enumerations
                .insert(object, Enumeration { items, next: 0, refs: 1 });
            let _ = memory.write_u32(args[3], object);
            S_OK
        }
        "BindToObject" => bind(ctx, memory, base, args[1], args[3], args[4]),
        "BindToStorage" | "CreateViewObject" => {
            let out = if method_name == "BindToStorage" { args[4] } else { args[3] };
            let _ = memory.write_u32(out, 0);
            E_NOTIMPL
        }
        "CompareIDs" => {
            let left = read_items(memory, args[2]);
            let right = read_items(memory, args[3]);
            let key = |items: &[(u8, Vec<u8>)]| {
                items_path(base, items).map(|p| p.to_ascii_lowercase()).unwrap_or_else(|| format!("{items:?}"))
            };
            match key(&left).cmp(&key(&right)) {
                std::cmp::Ordering::Less => 0xFFFF,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }
        }
        "GetAttributesOf" => {
            // (count, pidls, &attributes in/out)
            let mut combined = u32::MAX;
            for index in 0..args[1].min(256) {
                let pidl = memory.read_u32(args[2] + index * 4).unwrap_or(0);
                let items = read_items(memory, pidl);
                combined &= attributes(base, &items);
            }
            let wanted = memory.read_u32(args[3]).unwrap_or(0);
            let _ = memory.write_u32(args[3], wanted & combined);
            S_OK
        }
        "GetUIObjectOf" => {
            let _ = memory.write_u32(args[6], 0);
            E_NOTIMPL
        }
        "GetDisplayNameOf" => {
            // (pidl, flags, STRRET*)
            const SHGDN_INFOLDER: u32 = 0x1;
            const SHGDN_FORPARSING: u32 = 0x8000;
            let items = read_items(memory, args[1]);
            if base.is_empty() {
                let name = match items.as_slice() {
                    [] => Some("Desktop"),
                    [(ITEM_REGITEM, data)] if is_my_computer(data) => Some("My Computer"),
                    _ => None,
                };
                if let Some(name) = name {
                    write_strret(memory, args[3], name);
                    return S_OK;
                }
            }
            let Some(path) = items_path(base, &items) else {
                return E_INVALIDARG;
            };
            let text = if args[2] & SHGDN_FORPARSING != 0 && args[2] & SHGDN_INFOLDER == 0 {
                path
            } else {
                let name = path.trim_end_matches('\\').rsplit('\\').next().unwrap_or("").to_owned();
                if name.len() == 2 && name.ends_with(':') {
                    format!("({name})")
                } else {
                    name
                }
            };
            write_strret(memory, args[3], &text);
            S_OK
        }
        "SetNameOf" => {
            if args[5] != 0 {
                let _ = memory.write_u32(args[5], 0);
            }
            E_NOTIMPL
        }
        _ => E_NOTIMPL,
    }
}

fn enumeration<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, method_name: &str, args: &[u32]) -> u32 {
    let this = args[0];
    if !ctx.shared.borrow().shell_enumerations.contains_key(&this) {
        return E_FAIL;
    }
    match method_name {
        "QueryInterface" => {
            let iid = read_guid(memory, args[1]);
            if iid == IID_IUNKNOWN || iid == IID_IENUMIDLIST {
                ctx.shared.borrow_mut().shell_enumerations.get_mut(&this).unwrap().refs += 1;
                let _ = memory.write_u32(args[2], this);
                S_OK
            } else {
                let _ = memory.write_u32(args[2], 0);
                E_NOINTERFACE
            }
        }
        "AddRef" => {
            let mut shared = ctx.shared.borrow_mut();
            let e = shared.shell_enumerations.get_mut(&this).unwrap();
            e.refs += 1;
            e.refs
        }
        "Release" => {
            let mut shared = ctx.shared.borrow_mut();
            let e = shared.shell_enumerations.get_mut(&this).unwrap();
            e.refs = e.refs.saturating_sub(1);
            let refs = e.refs;
            if refs == 0 {
                shared.shell_enumerations.remove(&this);
                shared.heap.free(this);
            }
            refs
        }
        "Next" => {
            // (count, pidls out, &fetched)
            let mut fetched = 0;
            while fetched < args[1] {
                let next = {
                    let mut shared = ctx.shared.borrow_mut();
                    let e = shared.shell_enumerations.get_mut(&this).unwrap();
                    let item = e.items.get(e.next).cloned();
                    if item.is_some() {
                        e.next += 1;
                    }
                    item
                };
                let Some(item) = next else { break };
                let pidl = alloc_list(ctx, memory, &item);
                let _ = memory.write_u32(args[2] + fetched * 4, pidl);
                fetched += 1;
            }
            if args[3] != 0 {
                let _ = memory.write_u32(args[3], fetched);
            }
            if fetched == args[1] {
                S_OK
            } else {
                S_FALSE
            }
        }
        "Skip" => {
            let mut shared = ctx.shared.borrow_mut();
            let e = shared.shell_enumerations.get_mut(&this).unwrap();
            e.next = (e.next + args[1] as usize).min(e.items.len());
            if e.next < e.items.len() {
                S_OK
            } else {
                S_FALSE
            }
        }
        "Reset" => {
            ctx.shared.borrow_mut().shell_enumerations.get_mut(&this).unwrap().next = 0;
            S_OK
        }
        "Clone" => {
            let copy = ctx.shared.borrow().shell_enumerations[&this].clone();
            let object = make_object(ctx, memory, "EnumIDList_");
            ctx.shared
                .borrow_mut()
                .shell_enumerations
                .insert(object, Enumeration { refs: 1, ..copy });
            let _ = memory.write_u32(args[1], object);
            S_OK
        }
        _ => E_NOTIMPL,
    }
}

/// The path an ID list (relative to the desktop) names, for
/// `SHGetPathFromIDList`.
pub fn pidl_path<M: GuestMemory + ?Sized>(memory: &mut M, pidl: u32) -> Option<Vec<u8>> {
    let items = read_items(memory, pidl);
    if pidl != 0 && items.is_empty() {
        // The desktop.
        return Some(b"C:\\WINDOWS\\Desktop".to_vec());
    }
    items_path("", &items).map(|p| p.chars().map(|c| c as u32 as u8).collect())
}

/// An ANSI string argument.
pub fn ansi_arg<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> String {
    read_cstr(memory, va).iter().map(|&b| b as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_items_round_trip() {
        let list = path_items("", "C:\\").unwrap();
        assert_eq!(list, item(ITEM_DRIVE, b"C:\\\0"));
        let mut bytes = list.clone();
        bytes.extend(item(ITEM_FILE, b"TEST.HTM\0"));
        let mut items = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let size = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
            items.push((bytes[at + 2], bytes[at + 3..at + size].to_vec()));
            at += size;
        }
        assert_eq!(items_path("", &items).as_deref(), Some("C:\\TEST.HTM"));
        assert_eq!(items_path("", &items[..1]).as_deref(), Some("C:\\"));
        assert_eq!(items_path("C:\\WINDOWS", &items[1..]).as_deref(), Some("C:\\WINDOWS\\TEST.HTM"));
    }
}
