//! The COM runtime: creating objects from their class IDs.
//!
//! `CoCreateInstance` and `CoGetClassObject` find a class the way OLE32
//! does: first among the class objects the program registered with
//! `CoRegisterClassObject`, then in the registry, where
//! `HKEY_CLASSES_ROOT\CLSID\{clsid}\InprocServer32` names the DLL that
//! serves it. The DLL is loaded and its `DllGetClassObject` asked for the
//! class factory, whose `CreateInstance` makes the object.
//!
//! Every object lives in the program's single apartment: there are no
//! local (out-of-process) servers and nothing is marshalled.

use crate::{
    kernel::pe::ImportName,
    win32::{module, registry, shell, ApiContext, ApiOutcome, GuestMemory},
};

const S_OK: u32 = 0;
const S_FALSE: u32 = 1;
const E_NOINTERFACE: u32 = 0x8000_4002;
const REGDB_E_CLASSNOTREG: u32 = 0x8004_0154;
const CO_E_DLLNOTFOUND: u32 = 0x8004_01F8;
const CO_E_ERRORINDLL: u32 = 0x8004_01F9;
const CO_E_CLASSSTRING: u32 = 0x8004_01F3;

/// IID_IClassFactory {00000001-0000-0000-C000-000000000046}.
const IID_ICLASSFACTORY: [u8; 16] = [1, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];

/// A class object registered with `CoRegisterClassObject`.
#[derive(Clone, Debug)]
pub struct ClassObject {
    pub clsid: [u8; 16],
    pub object: u32,
    pub cookie: u32,
}

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "DllRegisterServer" => 0,
        "DllGetClassObject" => 3,
        "CoRevokeClassObject" | "CoGetState" | "CoSetState" | "PropVariantClear" | "OleRun" => 1,
        "CoTaskMemRealloc" | "PropVariantCopy" | "FreePropVariantArray" | "CreateBindCtx"
        | "StringFromCLSID" | "StringFromIID" => 2,
        "CLSIDFromProgID" | "ProgIDFromCLSID" | "OleGetAutoConvert" => 2,
        "CoCreateInstance" | "CoGetClassObject" | "CoRegisterClassObject" => 5,
        _ => return None,
    })
}

fn tracing() -> bool {
    std::env::var_os("OTW_TRACE_COM").is_some()
}

fn read_guid<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> [u8; 16] {
    let mut guid = [0u8; 16];
    if let Some(bytes) = memory.read_bytes(va, 16) {
        guid.copy_from_slice(&bytes);
    }
    guid
}

/// Call method `index` of a COM object's vtable.
fn method<M: GuestMemory + ?Sized>(memory: &mut M, object: u32, index: u32, args: &[u32]) -> Option<u32> {
    let vtable = memory.read_u32(object)?;
    let function = memory.read_u32(vtable + index * 4)?;
    let mut all = vec![object];
    all.extend_from_slice(args);
    memory.call_guest(function, &all)
}

/// A guest copy of a GUID (freed by the caller).
fn temp_guid<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, guid: &[u8; 16]) -> u32 {
    let va = ctx.shared.borrow_mut().heap.alloc(16).unwrap_or(0);
    let _ = memory.write_bytes(va, guid);
    va
}

fn free(ctx: &ApiContext, va: u32) {
    if va != 0 {
        ctx.shared.borrow_mut().heap.free(va);
    }
}

/// The DLL registered for a class.
pub fn server_of(clsid: &[u8; 16]) -> Option<String> {
    let key = format!("HKEY_CLASSES_ROOT\\CLSID\\{}\\InprocServer32", shell::guid_text(clsid));
    let (_, path) = registry::query(&key, "")?;
    let path = String::from_utf8_lossy(&path).trim().trim_matches('"').to_owned();
    (!path.is_empty()).then_some(path)
}

/// `CoGetClassObject` for an in-process class: `iid` and `out` are guest
/// addresses.
pub fn get_class_object<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    clsid: &[u8; 16],
    iid: u32,
    out: u32,
) -> u32 {
    let _ = memory.write_u32(out, 0);
    let registered = ctx
        .shared
        .borrow()
        .class_objects
        .iter()
        .find(|c| &c.clsid == clsid)
        .map(|c| c.object);
    if let Some(object) = registered {
        return method(memory, object, 0, &[iid, out]).unwrap_or(E_NOINTERFACE);
    }
    let Some(server) = server_of(clsid) else {
        if tracing() {
            eprintln!("[com] {} is not registered", shell::guid_text(clsid));
        }
        return REGDB_E_CLASSNOTREG;
    };
    // Classes served by the subsystem's own DLLs (OLE32's monikers, its
    // proxy factories) have no genuine server to ask.
    if module::subsystem_only(&server) {
        if tracing() {
            eprintln!("[com] {} is served by {server}, which the subsystem replaces", shell::guid_text(clsid));
        }
        return 0x8004_0111; // CLASS_E_CLASSNOTAVAILABLE
    }
    let module = match module::load_library(ctx, memory, &server, 0) {
        Ok(handle) => handle,
        Err(error) => {
            if tracing() {
                eprintln!("[com] {}: {server} did not load ({error})", shell::guid_text(clsid));
            }
            return CO_E_DLLNOTFOUND;
        }
    };
    let entry = module::proc_address(ctx, memory, module, &ImportName::ByName("DllGetClassObject".into()), 0);
    let Some(entry) = entry else {
        return CO_E_ERRORINDLL;
    };
    let clsid_va = temp_guid(ctx, memory, clsid);
    let result = memory.call_guest(entry, &[clsid_va, iid, out]).unwrap_or(CO_E_ERRORINDLL);
    free(ctx, clsid_va);
    if tracing() {
        eprintln!("[com] {} from {server}: {result:#010x}", shell::guid_text(clsid));
    }
    result
}

/// `CoCreateInstance(clsid, outer, context, iid, out)`.
pub fn create_instance<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    clsid: &[u8; 16],
    outer: u32,
    iid: u32,
    out: u32,
) -> u32 {
    let _ = memory.write_u32(out, 0);
    let factory_iid = temp_guid(ctx, memory, &IID_ICLASSFACTORY);
    let slot = ctx.shared.borrow_mut().heap.alloc(4).unwrap_or(0);
    let status = get_class_object(ctx, memory, clsid, factory_iid, slot);
    let factory = memory.read_u32(slot).unwrap_or(0);
    free(ctx, factory_iid);
    free(ctx, slot);
    if status != S_OK || factory == 0 {
        return status;
    }
    // IClassFactory::CreateInstance(outer, iid, out), then Release.
    let result = method(memory, factory, 3, &[outer, iid, out]).unwrap_or(E_NOINTERFACE);
    let _ = method(memory, factory, 2, &[]);
    if tracing() {
        let wanted = read_guid(memory, iid);
        eprintln!(
            "[com] CoCreateInstance {} as {}: {result:#010x}",
            shell::guid_text(clsid),
            shell::guid_text(&wanted)
        );
    }
    result
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    let value = match name {
        // OLE32's own registration (DCOM95's INF registers it): the system
        // classes served by DLLs next to it, such as the component
        // categories manager in COMCAT.DLL.
        "DllRegisterServer" => {
            let system = crate::win32::crt::host_path(b"C:\\WINDOWS\\SYSTEM\\COMCAT.DLL");
            if system.is_some_and(|p| p.is_file()) {
                let key = "HKEY_CLASSES_ROOT\\CLSID\\{0002E005-0000-0000-C000-000000000046}";
                crate::win32::registry::set_string(key, "", b"Component Categories Manager");
                let server = format!("{key}\\InprocServer32");
                crate::win32::registry::set_string(&server, "", b"C:\\WINDOWS\\SYSTEM\\COMCAT.DLL");
                crate::win32::registry::set_string(&server, "ThreadingModel", b"Both");
            }
            0
        }
        // No class converts to another (no `AutoConvertTo` keys).
        "OleGetAutoConvert" => {
            let clsid = read_guid(memory, args[0]);
            let _ = memory.write_bytes(args[1], &clsid);
            0x8004_0152 // REGDB_E_KEYMISSING
        }
        // DLLs forwarding to OLE32 (COMCAT.DLL): no class factories here;
        // CoCreateInstance makes the classes OLE32 serves directly.
        "DllGetClassObject" => {
            let _ = memory.write_u32(args[2], 0);
            0x8004_0111 // CLASS_E_CLASSNOTAVAILABLE
        }
        "CoCreateInstance" => {
            let clsid = read_guid(memory, args[0]);
            if clsid == crate::win32::catmgr::CLSID_CATMGR && args[1] == 0 {
                let iid = memory.read_bytes(args[3], 16).unwrap_or_default();
                crate::win32::catmgr::create(ctx, memory, &iid, args[4])
            } else if clsid == crate::win32::shelllink::CLSID_SHELL_LINK.as_slice() && args[1] == 0 {
                let iid = memory.read_bytes(args[3], 16).unwrap_or_default();
                crate::win32::shelllink::create(ctx, memory, &iid, args[4])
            } else {
                create_instance(ctx, memory, &clsid, args[1], args[3], args[4])
            }
        }
        "CoGetClassObject" => {
            // (clsid, context, server info, iid, out)
            let clsid = read_guid(memory, args[0]);
            get_class_object(ctx, memory, &clsid, args[3], args[4])
        }
        "CoRegisterClassObject" => {
            // (clsid, object, context, flags, &cookie)
            let clsid = read_guid(memory, args[0]);
            let _ = method(memory, args[1], 1, &[]);
            let cookie = ctx.shared.borrow_mut().alloc_handle();
            ctx.shared.borrow_mut().class_objects.push(ClassObject {
                clsid,
                object: args[1],
                cookie,
            });
            let _ = memory.write_u32(args[4], cookie);
            S_OK
        }
        "CoRevokeClassObject" => {
            let removed = {
                let mut shared = ctx.shared.borrow_mut();
                let index = shared.class_objects.iter().position(|c| c.cookie == args[0]);
                index.map(|index| shared.class_objects.remove(index))
            };
            if let Some(class) = removed {
                let _ = method(memory, class.object, 2, &[]);
            }
            S_OK
        }
        "OleRun" => {
            // IRunnableObject::Run(NULL) if the object has one; otherwise it
            // is already running.
            const IID_IRUNNABLEOBJECT: [u8; 16] = [0x26, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
            let iid = temp_guid(ctx, memory, &IID_IRUNNABLEOBJECT);
            let slot = ctx.shared.borrow_mut().heap.alloc(4).unwrap_or(0);
            let result = if method(memory, args[0], 0, &[iid, slot]) == Some(S_OK) {
                let runnable = memory.read_u32(slot).unwrap_or(0);
                let result = method(memory, runnable, 4, &[0]).unwrap_or(S_OK);
                let _ = method(memory, runnable, 2, &[]);
                result
            } else {
                S_OK
            };
            free(ctx, iid);
            free(ctx, slot);
            result
        }
        "CreateBindCtx" => {
            let object = crate::win32::bindctx::create(ctx, memory);
            let _ = memory.write_u32(args[1], object);
            S_OK
        }
        "StringFromCLSID" | "StringFromIID" => {
            // A CoTaskMemAlloc'ed "{...}" string.
            let guid = read_guid(memory, args[0]);
            let text = shell::guid_text(&guid);
            let wide: Vec<u8> = text.bytes().flat_map(|b| [b, 0]).chain([0, 0]).collect();
            let block = ctx.shared.borrow_mut().heap.alloc(wide.len() as u32).unwrap_or(0);
            let _ = memory.write_bytes(block, &wide);
            let _ = memory.write_u32(args[1], block);
            S_OK
        }
        "CoTaskMemRealloc" => {
            // (block, size): a new block holding the old contents.
            let old_size = ctx.shared.borrow().heap.size_of(args[0]).unwrap_or(0);
            if args[1] == 0 {
                free(ctx, args[0]);
                0
            } else if args[0] != 0 && old_size >= args[1] {
                args[0]
            } else {
                let block = ctx.shared.borrow_mut().heap.alloc(args[1]).unwrap_or(0);
                if block != 0 && args[0] != 0 {
                    if let Some(bytes) = memory.read_bytes(args[0], old_size.min(args[1]) as usize) {
                        let _ = memory.write_bytes(block, &bytes);
                    }
                    free(ctx, args[0]);
                }
                block
            }
        }
        // PROPVARIANTs are 16 bytes; only simple (non-pointer) values occur.
        "PropVariantClear" => {
            let _ = memory.write_bytes(args[0], &[0; 16]);
            S_OK
        }
        "PropVariantCopy" => {
            let value = memory.read_bytes(args[1], 16).unwrap_or_else(|| vec![0; 16]);
            let _ = memory.write_bytes(args[0], &value);
            S_OK
        }
        "FreePropVariantArray" => {
            let _ = memory.write_bytes(args[1], &vec![0; args[0].min(4096) as usize * 16]);
            S_OK
        }
        "CoSetState" => {
            // Each thread's state object, held with a reference.
            let thread = memory.current_thread();
            if args[0] != 0 {
                let _ = method(memory, args[0], 1, &[]);
            }
            let old = if args[0] == 0 {
                ctx.shared.borrow_mut().com_state.remove(&thread)
            } else {
                ctx.shared.borrow_mut().com_state.insert(thread, args[0])
            };
            if let Some(old) = old.filter(|&o| o != 0) {
                let _ = method(memory, old, 2, &[]);
            }
            S_OK
        }
        "CoGetState" => {
            let thread = memory.current_thread();
            let state = ctx.shared.borrow().com_state.get(&thread).copied().unwrap_or(0);
            if state != 0 {
                let _ = method(memory, state, 1, &[]);
            }
            let _ = memory.write_u32(args[0], state);
            if state == 0 {
                S_FALSE
            } else {
                S_OK
            }
        }
        "CLSIDFromProgID" => {
            // (wide ProgID, &clsid): HKCR\ProgID\CLSID.
            let progid = crate::win32::unicode::wide_to_ansi(&crate::win32::unicode::read_wstr(memory, args[0]));
            let key = format!("HKEY_CLASSES_ROOT\\{}\\CLSID", String::from_utf8_lossy(&progid));
            match registry::query(&key, "").and_then(|(_, text)| shell::parse_guid(&String::from_utf8_lossy(&text))) {
                Some(guid) => {
                    let _ = memory.write_bytes(args[1], &guid);
                    S_OK
                }
                None => CO_E_CLASSSTRING,
            }
        }
        "ProgIDFromCLSID" => {
            let clsid = read_guid(memory, args[0]);
            let key = format!("HKEY_CLASSES_ROOT\\CLSID\\{}\\ProgID", shell::guid_text(&clsid));
            match registry::query(&key, "") {
                Some((_, text)) => {
                    let wide: Vec<u8> = text.iter().flat_map(|&b| [b, 0]).chain([0, 0]).collect();
                    let block = ctx.shared.borrow_mut().heap.alloc(wide.len() as u32).unwrap_or(0);
                    let _ = memory.write_bytes(block, &wide);
                    let _ = memory.write_u32(args[1], block);
                    S_OK
                }
                None => REGDB_E_CLASSNOTREG,
            }
        }
        _ => return None,
    };
    Some(ApiOutcome::new(value, count))
}
