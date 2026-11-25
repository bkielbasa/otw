//! The component categories manager (`CLSID_StdComponentCategoriesMgr`),
//! which OLE32 serves (DCOM95's COMCAT.DLL forwards to it): `ICatRegister`
//! records categories and the categories classes implement or require in
//! the registry, `ICatInformation` reads them back. Its enumerators
//! (`IEnumGUID`, `IEnumCATEGORYINFO`) are one object type holding a list
//! of fixed-size items.
//!
//! The registry layout is Windows': `HKCR\Component Categories\{catid}`
//! with descriptions named by hexadecimal locale, and
//! `HKCR\CLSID\{clsid}\Implemented Categories\{catid}` (or `Required`).

use crate::win32::{registry, shell, unicode, ApiContext, ApiOutcome, GuestMemory};

const S_OK: u32 = 0;
const S_FALSE: u32 = 1;
const E_NOINTERFACE: u32 = 0x8000_4002;
const E_FAIL: u32 = 0x8000_4005;
const CAT_E_CATIDNOEXIST: u32 = 0x8004_0160;

/// {0002E005-0000-0000-C000-000000000046}
pub const CLSID_CATMGR: [u8; 16] = guid(0x0002_E005);
const IID_IUNKNOWN: [u8; 16] = guid(0);
const IID_ICATREGISTER: [u8; 16] = guid(0x0002_E012);
const IID_ICATINFORMATION: [u8; 16] = guid(0x0002_E013);
const IID_IENUMGUID: [u8; 16] = guid(0x0002_E000);
const IID_IENUMCATEGORYINFO: [u8; 16] = guid(0x0002_E011);

/// An OLE GUID `{xxxxxxxx-0000-0000-C000-000000000046}`.
const fn guid(first: u32) -> [u8; 16] {
    let b = first.to_le_bytes();
    [b[0], b[1], b[2], b[3], 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46]
}

/// CATEGORYINFO: catid, lcid, WCHAR szDescription[128].
const CATEGORYINFO_SIZE: usize = 16 + 4 + 256;

const REGISTER_METHODS: &[&str] = &[
    "CatReg_QueryInterface",
    "CatReg_AddRef",
    "CatReg_Release",
    "CatReg_RegisterCategories",
    "CatReg_UnRegisterCategories",
    "CatReg_RegisterClassImplCategories",
    "CatReg_UnRegisterClassImplCategories",
    "CatReg_RegisterClassReqCategories",
    "CatReg_UnRegisterClassReqCategories",
];

const INFORMATION_METHODS: &[&str] = &[
    "CatInfo_QueryInterface",
    "CatInfo_AddRef",
    "CatInfo_Release",
    "CatInfo_EnumCategories",
    "CatInfo_GetCategoryDesc",
    "CatInfo_EnumClassesOfCategories",
    "CatInfo_IsClassOfCategories",
    "CatInfo_EnumImplCategoriesOfClass",
    "CatInfo_EnumReqCategoriesOfClass",
];

const ENUM_METHODS: &[&str] = &[
    "CatEnum_QueryInterface",
    "CatEnum_AddRef",
    "CatEnum_Release",
    "CatEnum_Next",
    "CatEnum_Skip",
    "CatEnum_Reset",
    "CatEnum_Clone",
];

/// Every built-in method, for the OLE32 thunk table.
pub fn methods() -> impl Iterator<Item = &'static &'static str> {
    REGISTER_METHODS.iter().chain(INFORMATION_METHODS).chain(ENUM_METHODS)
}

/// A manager object: two interface pointers (`ICatRegister` at the object,
/// `ICatInformation` 4 bytes on), one reference count.
#[derive(Clone, Debug, Default)]
pub struct CatManager {
    refs: u32,
}

/// An enumerator over fixed-size items.
#[derive(Clone, Debug, Default)]
pub struct CatEnum {
    refs: u32,
    iid: [u8; 16],
    items: Vec<Vec<u8>>,
    position: usize,
}

#[derive(Default, Debug)]
pub struct CatState {
    managers: std::collections::HashMap<u32, CatManager>,
    enums: std::collections::HashMap<u32, CatEnum>,
}

pub fn arg_count(name: &str) -> Option<usize> {
    if !methods().any(|m| *m == name) {
        return None;
    }
    Some(match name.split_once('_')?.1 {
        "AddRef" | "Release" | "Reset" => 1,
        "Skip" | "Clone" => 2,
        "QueryInterface" | "RegisterCategories" | "UnRegisterCategories" | "EnumCategories"
        | "EnumImplCategoriesOfClass" | "EnumReqCategoriesOfClass" => 3,
        "Next" | "GetCategoryDesc" | "RegisterClassImplCategories" | "UnRegisterClassImplCategories"
        | "RegisterClassReqCategories" | "UnRegisterClassReqCategories" => 4,
        "EnumClassesOfCategories" | "IsClassOfCategories" => 6,
        _ => return None,
    })
}


/// A new manager, returned as the interface `iid` asks for.
pub fn create<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, iid: &[u8], out: u32) -> u32 {
    let _ = memory.write_ptr(out, 0);
    let ps = crate::win32::ptr_size(ctx);
    let interface = if iid == IID_IUNKNOWN || iid == IID_ICATREGISTER {
        0
    } else if iid == IID_ICATINFORMATION {
        ps
    } else {
        return E_NOINTERFACE;
    };
    let object = ctx.shared.borrow_mut().heap.alloc(16).unwrap_or(0);
    crate::win32::set_vtable(ctx, memory, object, REGISTER_METHODS);
    crate::win32::set_vtable(ctx, memory, object + ps, INFORMATION_METHODS);
    ctx.shared.borrow_mut().categories.managers.insert(object, CatManager { refs: 1 });
    let _ = memory.write_ptr(out, u64::from(object + interface));
    S_OK
}

fn new_enum<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, iid: [u8; 16], items: Vec<Vec<u8>>) -> u32 {
    let object = ctx.shared.borrow_mut().heap.alloc(8).unwrap_or(0);
    crate::win32::set_vtable(ctx, memory, object, ENUM_METHODS);
    ctx.shared.borrow_mut().categories.enums.insert(object, CatEnum { refs: 1, iid, items, position: 0 });
    object
}

fn read_guid<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> [u8; 16] {
    let mut guid = [0u8; 16];
    if let Some(bytes) = memory.read_bytes(va, 16) {
        guid.copy_from_slice(&bytes);
    }
    guid
}

fn guids<M: GuestMemory + ?Sized>(memory: &mut M, count: u32, va: u32) -> Vec<[u8; 16]> {
    (0..count.min(4096)).map(|i| read_guid(memory, va + i * 16)).collect()
}

fn class_key(clsid: &[u8], kind: &str) -> String {
    format!("HKEY_CLASSES_ROOT\\CLSID\\{}\\{kind} Categories", shell::guid_text(clsid))
}

fn category_key(catid: &[u8]) -> String {
    format!("HKEY_CLASSES_ROOT\\Component Categories\\{}", shell::guid_text(catid))
}

/// The categories under a key, as GUIDs.
fn listed(key: &str) -> Vec<[u8; 16]> {
    registry::subkeys(key).iter().filter_map(|name| shell::parse_guid(name)).collect()
}

/// A category's description for a locale (or the first one there is).
fn description(catid: &[u8], lcid: u32) -> Option<String> {
    let values = registry::values(&category_key(catid));
    let wanted = format!("{lcid:x}");
    values
        .iter()
        .find(|(name, _, _)| name.eq_ignore_ascii_case(&wanted))
        .or_else(|| values.iter().find(|(name, _, _)| !name.is_empty()))
        .map(|(_, _, data)| String::from_utf8_lossy(data).into_owned())
}

fn wide(text: &str) -> Vec<u8> {
    text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    let (group, method) = name.split_once('_')?;
    let this = args[0];
    let value = match group {
        "CatReg" | "CatInfo" => {
            let object = if group == "CatReg" { this } else { this.wrapping_sub(crate::win32::ptr_size(ctx)) };
            if !ctx.shared.borrow().categories.managers.contains_key(&object) {
                return Some(ApiOutcome::new(E_FAIL, count));
            }
            manager(ctx, memory, object, method, args)
        }
        _ => {
            if !ctx.shared.borrow().categories.enums.contains_key(&this) {
                return Some(ApiOutcome::new(E_FAIL, count));
            }
            enumerator(ctx, memory, this, method, args)
        }
    };
    Some(ApiOutcome::new(value, count))
}

fn manager<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, object: u32, method: &str, args: &[u32]) -> u32 {
    match method {
        "QueryInterface" => {
            let iid = read_guid(memory, args[1]);
            let pointer = if iid == IID_IUNKNOWN || iid == IID_ICATREGISTER {
                object
            } else if iid == IID_ICATINFORMATION {
                object + crate::win32::ptr_size(ctx)
            } else {
                let _ = memory.write_ptr(args[2], 0);
                return E_NOINTERFACE;
            };
            ctx.shared.borrow_mut().categories.managers.get_mut(&object).unwrap().refs += 1;
            let _ = memory.write_ptr(args[2], u64::from(pointer));
            S_OK
        }
        "AddRef" => {
            let mut shared = ctx.shared.borrow_mut();
            let manager = shared.categories.managers.get_mut(&object).unwrap();
            manager.refs += 1;
            manager.refs
        }
        "Release" => {
            let mut shared = ctx.shared.borrow_mut();
            let manager = shared.categories.managers.get_mut(&object).unwrap();
            manager.refs = manager.refs.saturating_sub(1);
            let refs = manager.refs;
            if refs == 0 {
                shared.categories.managers.remove(&object);
                shared.heap.free(object);
            }
            refs
        }
        "RegisterCategories" => {
            for index in 0..args[1].min(4096) {
                let info = args[2] + index * CATEGORYINFO_SIZE as u32;
                let catid = read_guid(memory, info);
                let lcid = memory.read_u32(info + 16).unwrap_or(0x409);
                let text = unicode::wide_to_ansi(&unicode::read_wstr(memory, info + 20));
                registry::set_string(&category_key(&catid), &format!("{lcid:x}"), &text);
            }
            S_OK
        }
        "UnRegisterCategories" => {
            for catid in guids(memory, args[1], args[2]) {
                registry::delete_key(&category_key(&catid));
            }
            S_OK
        }
        "RegisterClassImplCategories" | "RegisterClassReqCategories" | "UnRegisterClassImplCategories"
        | "UnRegisterClassReqCategories" => {
            let clsid = read_guid(memory, args[1]);
            let kind = if method.contains("Impl") { "Implemented" } else { "Required" };
            let key = class_key(&clsid, kind);
            for catid in guids(memory, args[2], args[3]) {
                let entry = format!("{key}\\{}", shell::guid_text(&catid));
                if method.starts_with("Un") {
                    registry::delete_key(&entry);
                } else {
                    registry::create_key(&entry);
                }
            }
            if method.starts_with("Un") && registry::subkeys(&key).is_empty() {
                registry::delete_key(&key);
            }
            S_OK
        }
        "EnumCategories" => {
            let lcid = args[1];
            let items = listed("HKEY_CLASSES_ROOT\\Component Categories")
                .into_iter()
                .map(|catid| {
                    let mut item = vec![0u8; CATEGORYINFO_SIZE];
                    item[..16].copy_from_slice(&catid);
                    item[16..20].copy_from_slice(&lcid.to_le_bytes());
                    let text = wide(&description(&catid, lcid).unwrap_or_default());
                    let length = text.len().min(254);
                    item[20..20 + length].copy_from_slice(&text[..length]);
                    item
                })
                .collect();
            let enumerator = new_enum(ctx, memory, IID_IENUMCATEGORYINFO, items);
            let _ = memory.write_ptr(args[2], u64::from(enumerator));
            S_OK
        }
        "GetCategoryDesc" => {
            let catid = read_guid(memory, args[1]);
            let _ = memory.write_u32(args[3], 0);
            match description(&catid, args[2]) {
                Some(text) => {
                    let text = wide(&text);
                    let copy = ctx.shared.borrow_mut().heap.alloc(text.len() as u32).unwrap_or(0);
                    let _ = memory.write_bytes(copy, &text);
                    let _ = memory.write_u32(args[3], copy);
                    S_OK
                }
                None => CAT_E_CATIDNOEXIST,
            }
        }
        "EnumClassesOfCategories" => {
            // (count implemented, implemented[], count required, required[], out)
            // A count of -1 means "any".
            let implemented = (args[1] != u32::MAX).then(|| guids(memory, args[1], args[2]));
            let required = (args[3] != u32::MAX).then(|| guids(memory, args[3], args[4]));
            let items = registry::subkeys("HKEY_CLASSES_ROOT\\CLSID")
                .iter()
                .filter_map(|name| shell::parse_guid(name))
                .filter(|clsid| matches_categories(clsid, implemented.as_deref(), required.as_deref()))
                .map(|clsid| clsid.to_vec())
                .collect();
            let enumerator = new_enum(ctx, memory, IID_IENUMGUID, items);
            let _ = memory.write_u32(args[5], enumerator);
            S_OK
        }
        "IsClassOfCategories" => {
            let clsid = read_guid(memory, args[1]);
            let implemented = (args[2] != u32::MAX).then(|| guids(memory, args[2], args[3]));
            let required = (args[4] != u32::MAX).then(|| guids(memory, args[4], args[5]));
            if matches_categories(&clsid, implemented.as_deref(), required.as_deref()) { S_OK } else { S_FALSE }
        }
        "EnumImplCategoriesOfClass" | "EnumReqCategoriesOfClass" => {
            let clsid = read_guid(memory, args[1]);
            let kind = if method.contains("Impl") { "Implemented" } else { "Required" };
            let items = listed(&class_key(&clsid, kind)).into_iter().map(|g| g.to_vec()).collect();
            let enumerator = new_enum(ctx, memory, IID_IENUMGUID, items);
            let _ = memory.write_ptr(args[2], u64::from(enumerator));
            S_OK
        }
        _ => E_FAIL,
    }
}

/// Whether a class implements one of `implemented` (when given) and
/// requires nothing outside `required` (when given).
fn matches_categories(clsid: &[u8; 16], implemented: Option<&[[u8; 16]]>, required: Option<&[[u8; 16]]>) -> bool {
    if let Some(implemented) = implemented {
        let has = listed(&class_key(clsid, "Implemented"));
        if !implemented.is_empty() && !implemented.iter().any(|c| has.contains(c)) {
            return false;
        }
        if implemented.is_empty() && has.is_empty() {
            return false;
        }
    }
    if let Some(required) = required {
        let needs = listed(&class_key(clsid, "Required"));
        if !needs.iter().all(|c| required.contains(c)) {
            return false;
        }
    }
    true
}

fn enumerator<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, this: u32, method: &str, args: &[u32]) -> u32 {
    match method {
        "QueryInterface" => {
            let iid = read_guid(memory, args[1]);
            let own = ctx.shared.borrow().categories.enums[&this].iid;
            if iid == IID_IUNKNOWN || iid == own {
                ctx.shared.borrow_mut().categories.enums.get_mut(&this).unwrap().refs += 1;
                let _ = memory.write_ptr(args[2], u64::from(this));
                S_OK
            } else {
                let _ = memory.write_ptr(args[2], 0);
                E_NOINTERFACE
            }
        }
        "AddRef" => {
            let mut shared = ctx.shared.borrow_mut();
            let e = shared.categories.enums.get_mut(&this).unwrap();
            e.refs += 1;
            e.refs
        }
        "Release" => {
            let mut shared = ctx.shared.borrow_mut();
            let e = shared.categories.enums.get_mut(&this).unwrap();
            e.refs = e.refs.saturating_sub(1);
            let refs = e.refs;
            if refs == 0 {
                shared.categories.enums.remove(&this);
                shared.heap.free(this);
            }
            refs
        }
        "Next" => {
            let wanted = args[1] as usize;
            let items: Vec<Vec<u8>> = {
                let mut shared = ctx.shared.borrow_mut();
                let e = shared.categories.enums.get_mut(&this).unwrap();
                let end = (e.position + wanted).min(e.items.len());
                let items = e.items[e.position..end].to_vec();
                e.position = end;
                items
            };
            let mut at = args[2];
            for item in &items {
                let _ = memory.write_bytes(at, item);
                at += item.len() as u32;
            }
            if args[3] != 0 {
                let _ = memory.write_u32(args[3], items.len() as u32);
            }
            if items.len() == wanted { S_OK } else { S_FALSE }
        }
        "Skip" => {
            let mut shared = ctx.shared.borrow_mut();
            let e = shared.categories.enums.get_mut(&this).unwrap();
            let end = e.position + args[1] as usize;
            e.position = end.min(e.items.len());
            if end <= e.items.len() { S_OK } else { S_FALSE }
        }
        "Reset" => {
            ctx.shared.borrow_mut().categories.enums.get_mut(&this).unwrap().position = 0;
            S_OK
        }
        "Clone" => {
            let copy = ctx.shared.borrow().categories.enums[&this].clone();
            let clone = new_enum(ctx, memory, copy.iid, copy.items);
            ctx.shared.borrow_mut().categories.enums.get_mut(&clone).unwrap().position = copy.position;
            let _ = memory.write_u32(args[1], clone);
            S_OK
        }
        _ => E_FAIL,
    }
}
