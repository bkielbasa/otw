//! Bind contexts (`CreateBindCtx`): the `IBindCtx` that moniker binding
//! carries, holding the bind options, the objects bound so far (released
//! together) and named object parameters (URL monikers keep their binding
//! callbacks there).

use crate::{
    kernel::pe::ImportName,
    win32::{unicode, ApiContext, ApiOutcome, GuestMemory},
};

const S_OK: u32 = 0;
const E_NOTIMPL: u32 = 0x8000_4001;
const E_NOINTERFACE: u32 = 0x8000_4002;
const E_FAIL: u32 = 0x8000_4005;
const E_INVALIDARG: u32 = 0x8007_0057;

/// IUnknown and IBindCtx {0000000E-0000-0000-C000-000000000046}.
const IID_IUNKNOWN: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
const IID_IBINDCTX: [u8; 16] = [0x0E, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];

pub const METHODS: &[&str] = &[
    "BindCtx_QueryInterface",
    "BindCtx_AddRef",
    "BindCtx_Release",
    "BindCtx_RegisterObjectBound",
    "BindCtx_RevokeObjectBound",
    "BindCtx_ReleaseBoundObjects",
    "BindCtx_SetBindOptions",
    "BindCtx_GetBindOptions",
    "BindCtx_GetRunningObjectTable",
    "BindCtx_RegisterObjectParam",
    "BindCtx_GetObjectParam",
    "BindCtx_EnumObjectParam",
    "BindCtx_RevokeObjectParam",
];

#[derive(Clone, Debug, Default)]
pub struct BindCtx {
    refs: u32,
    bound: Vec<u32>,
    params: Vec<(String, u32)>,
    /// BIND_OPTS2 as last set (cbStruct first).
    options: Vec<u8>,
}

pub fn arg_count(name: &str) -> Option<usize> {
    if !METHODS.contains(&name) {
        return None;
    }
    Some(match name.split_once('_')?.1 {
        "AddRef" | "Release" | "ReleaseBoundObjects" => 1,
        "RegisterObjectBound" | "RevokeObjectBound" | "SetBindOptions" | "GetBindOptions"
        | "GetRunningObjectTable" | "EnumObjectParam" | "RevokeObjectParam" => 2,
        "QueryInterface" | "RegisterObjectParam" | "GetObjectParam" => 3,
        _ => return None,
    })
}

/// Call method `index` of a COM object.
fn method<M: GuestMemory + ?Sized>(memory: &mut M, object: u32, index: u32, args: &[u32]) -> u32 {
    let function = memory.read_u32(object).and_then(|vtable| memory.read_u32(vtable + index * 4));
    let mut all = vec![object];
    all.extend_from_slice(args);
    function.and_then(|f| memory.call_guest(f, &all)).unwrap_or(E_FAIL)
}

/// A new bind context, as `IBindCtx`.
pub fn create<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) -> u32 {
    let object = ctx.shared.borrow_mut().heap.alloc(4 + METHODS.len() as u32 * 4).unwrap_or(0);
    let _ = memory.write_u32(object, object + 4);
    for (index, name) in METHODS.iter().enumerate() {
        let _ = memory.write_u32(object + 4 + index as u32 * 4, ctx.builtin_proc(name));
    }
    crate::win32::wide_vtable(ctx, memory, object, METHODS);
    // BIND_OPTS2: cbStruct, grfFlags, grfMode (STGM_READWRITE), deadline, ...
    let mut options = vec![0u8; 32];
    options[0..4].copy_from_slice(&32u32.to_le_bytes());
    options[8..12].copy_from_slice(&2u32.to_le_bytes());
    ctx.shared.borrow_mut().bind_contexts.insert(
        object,
        BindCtx {
            refs: 1,
            options,
            ..BindCtx::default()
        },
    );
    object
}

fn key<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> String {
    unicode::wide_to_ansi(&unicode::read_wstr(memory, va)).iter().map(|&b| b as char).collect::<String>().to_ascii_lowercase()
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    let this = args[0];
    if !ctx.shared.borrow().bind_contexts.contains_key(&this) {
        return Some(ApiOutcome::new(E_FAIL, count));
    }
    let value = match name.split_once('_')?.1 {
        "QueryInterface" => {
            let iid = memory.read_bytes(args[1], 16).unwrap_or_default();
            if iid == IID_IUNKNOWN || iid == IID_IBINDCTX {
                ctx.shared.borrow_mut().bind_contexts.get_mut(&this).unwrap().refs += 1;
                let _ = memory.write_u32(args[2], this);
                S_OK
            } else {
                let _ = memory.write_u32(args[2], 0);
                E_NOINTERFACE
            }
        }
        "AddRef" => {
            let mut shared = ctx.shared.borrow_mut();
            let bc = shared.bind_contexts.get_mut(&this).unwrap();
            bc.refs += 1;
            bc.refs
        }
        "Release" => {
            let refs = {
                let mut shared = ctx.shared.borrow_mut();
                let bc = shared.bind_contexts.get_mut(&this).unwrap();
                bc.refs = bc.refs.saturating_sub(1);
                bc.refs
            };
            if refs == 0 {
                let bc = ctx.shared.borrow_mut().bind_contexts.remove(&this).unwrap();
                for object in bc.bound.iter().chain(bc.params.iter().map(|(_, o)| o)) {
                    method(memory, *object, 2, &[]);
                }
                ctx.shared.borrow_mut().heap.free(this);
            }
            refs
        }
        "RegisterObjectBound" => {
            if args[1] == 0 {
                return Some(ApiOutcome::new(E_INVALIDARG, count));
            }
            method(memory, args[1], 1, &[]);
            ctx.shared.borrow_mut().bind_contexts.get_mut(&this).unwrap().bound.push(args[1]);
            S_OK
        }
        "RevokeObjectBound" => {
            let found = {
                let mut shared = ctx.shared.borrow_mut();
                let bc = shared.bind_contexts.get_mut(&this).unwrap();
                bc.bound.iter().position(|&o| o == args[1]).map(|i| bc.bound.remove(i))
            };
            match found {
                Some(object) => {
                    method(memory, object, 2, &[]);
                    S_OK
                }
                None => 0x8004_01E9, // MK_E_NOTBOUND
            }
        }
        "ReleaseBoundObjects" => {
            let bound = std::mem::take(&mut ctx.shared.borrow_mut().bind_contexts.get_mut(&this).unwrap().bound);
            for object in bound {
                method(memory, object, 2, &[]);
            }
            S_OK
        }
        "SetBindOptions" => {
            let size = memory.read_u32(args[1]).unwrap_or(0).min(64);
            let data = memory.read_bytes(args[1], size as usize).unwrap_or_default();
            let mut shared = ctx.shared.borrow_mut();
            let options = &mut shared.bind_contexts.get_mut(&this).unwrap().options;
            options[..data.len().min(32)].copy_from_slice(&data[..data.len().min(32)]);
            options[0..4].copy_from_slice(&32u32.to_le_bytes());
            S_OK
        }
        "GetBindOptions" => {
            let wanted = memory.read_u32(args[1]).unwrap_or(0).min(32) as usize;
            let options = ctx.shared.borrow().bind_contexts[&this].options.clone();
            if wanted > 4 {
                let _ = memory.write_bytes(args[1] + 4, &options[4..wanted]);
            }
            S_OK
        }
        "GetRunningObjectTable" => crate::win32::dispatch_with_memory(
            ctx,
            memory,
            "OLE32.dll",
            &ImportName::ByName("GetRunningObjectTable".into()),
            &[0, args[1]],
            0,
        )
        .map(|o| o.value)
        .unwrap_or(E_FAIL),
        "RegisterObjectParam" => {
            let name = key(memory, args[1]);
            if args[2] != 0 {
                method(memory, args[2], 1, &[]);
            }
            let old = {
                let mut shared = ctx.shared.borrow_mut();
                let params = &mut shared.bind_contexts.get_mut(&this).unwrap().params;
                let old = params.iter().position(|(k, _)| *k == name).map(|i| params.remove(i).1);
                params.push((name, args[2]));
                old
            };
            if let Some(old) = old.filter(|&o| o != 0) {
                method(memory, old, 2, &[]);
            }
            S_OK
        }
        "GetObjectParam" => {
            let name = key(memory, args[1]);
            let found = ctx.shared.borrow().bind_contexts[&this].params.iter().find(|(k, _)| *k == name).map(|(_, o)| *o);
            match found {
                Some(object) if object != 0 => {
                    method(memory, object, 1, &[]);
                    let _ = memory.write_u32(args[2], object);
                    S_OK
                }
                _ => {
                    let _ = memory.write_u32(args[2], 0);
                    E_FAIL
                }
            }
        }
        "RevokeObjectParam" => {
            let name = key(memory, args[1]);
            let old = {
                let mut shared = ctx.shared.borrow_mut();
                let params = &mut shared.bind_contexts.get_mut(&this).unwrap().params;
                params.iter().position(|(k, _)| *k == name).map(|i| params.remove(i).1)
            };
            match old {
                Some(object) => {
                    if object != 0 {
                        method(memory, object, 2, &[]);
                    }
                    S_OK
                }
                None => E_FAIL,
            }
        }
        "EnumObjectParam" => {
            let _ = memory.write_u32(args[1], 0);
            E_NOTIMPL
        }
        _ => E_NOTIMPL,
    };
    Some(ApiOutcome::new(value, count))
}
