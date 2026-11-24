//! In-memory OLE structured storage: `ILockBytes`, `IStorage` and `IStream`
//! objects backed by host buffers. MFC's clipboard and embedding code saves
//! documents into storages; the tree of storages and streams lives here and
//! the guest sees ordinary COM objects with Rust-backed vtables.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use crate::win32::{ApiContext, ApiOutcome, GuestMemory};

const S_OK: u32 = 0;
const S_FALSE: u32 = 1;
const E_NOTIMPL: u32 = 0x8000_4001;
const E_OUTOFMEMORY: u32 = 0x8007_000E;
const STG_E_FILENOTFOUND: u32 = 0x8003_0002;
const STG_E_FILEALREADYEXISTS: u32 = 0x8003_0050;
const STG_E_INVALIDFUNCTION: u32 = 0x8003_0001;

const STGM_CREATE: u32 = 0x1000;

pub type Buffer = Rc<RefCell<Vec<u8>>>;

#[derive(Debug, Default)]
pub struct Node {
    pub streams: HashMap<String, Buffer>,
    pub children: HashMap<String, Rc<RefCell<Node>>>,
    pub clsid: [u8; 16],
    pub state_bits: u32,
}

#[derive(Debug)]
pub struct StreamObject {
    pub data: Buffer,
    pub pos: u64,
    pub name: String,
}

/// Host state behind the guest's storage objects, keyed by object address.
#[derive(Debug, Default)]
pub struct StorageState {
    pub storages: HashMap<u32, (Rc<RefCell<Node>>, String)>,
    pub streams: HashMap<u32, StreamObject>,
    /// ILockBytes objects and the root storage created on them.
    pub lockbytes: HashMap<u32, (Buffer, Option<Rc<RefCell<Node>>>)>,
}

/// Interface prefixes in `BUILTIN_COM`, in vtable order.
pub const STORAGE_METHODS: &[&str] = &[
    "Lkb_QueryInterface",
    "Lkb_AddRef",
    "Lkb_Release",
    "Lkb_ReadAt",
    "Lkb_WriteAt",
    "Lkb_Flush",
    "Lkb_SetSize",
    "Lkb_LockRegion",
    "Lkb_UnlockRegion",
    "Lkb_Stat",
    "Stg_QueryInterface",
    "Stg_AddRef",
    "Stg_Release",
    "Stg_CreateStream",
    "Stg_OpenStream",
    "Stg_CreateStorage",
    "Stg_OpenStorage",
    "Stg_CopyTo",
    "Stg_MoveElementTo",
    "Stg_Commit",
    "Stg_Revert",
    "Stg_EnumElements",
    "Stg_DestroyElement",
    "Stg_RenameElement",
    "Stg_SetElementTimes",
    "Stg_SetClass",
    "Stg_SetStateBits",
    "Stg_Stat",
    "Stm_QueryInterface",
    "Stm_AddRef",
    "Stm_Release",
    "Stm_Read",
    "Stm_Write",
    "Stm_Seek",
    "Stm_SetSize",
    "Stm_CopyTo",
    "Stm_Commit",
    "Stm_Revert",
    "Stm_LockRegion",
    "Stm_UnlockRegion",
    "Stm_Stat",
    "Stm_Clone",
];

pub fn method_args(name: &str) -> Option<usize> {
    Some(match name {
        "Lkb_AddRef" | "Lkb_Release" | "Lkb_Flush" | "Stg_AddRef" | "Stg_Release"
        | "Stg_Revert" | "Stm_AddRef" | "Stm_Release" | "Stm_Revert" => 1,
        "Stg_Commit" | "Stg_DestroyElement" | "Stg_SetClass" | "Stm_Commit" | "Stm_Clone" => 2,
        "Lkb_QueryInterface" | "Lkb_SetSize" | "Lkb_Stat" | "Stg_QueryInterface"
        | "Stg_RenameElement" | "Stg_SetStateBits" | "Stg_Stat" | "Stm_QueryInterface"
        | "Stm_SetSize" | "Stm_Stat" => 3,
        "Stm_Read" | "Stm_Write" => 4,
        "Stg_CopyTo"
        | "Stg_MoveElementTo"
        | "Stg_EnumElements"
        | "Stg_SetElementTimes"
        | "Stm_Seek" => 5,
        "Lkb_ReadAt" | "Lkb_WriteAt" | "Lkb_LockRegion" | "Lkb_UnlockRegion"
        | "Stg_CreateStream" | "Stg_OpenStream" | "Stg_CreateStorage" | "Stm_CopyTo"
        | "Stm_LockRegion" | "Stm_UnlockRegion" => 6,
        "Stg_OpenStorage" => 7,
        _ => return None,
    })
}

fn read_wide<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> String {
    let mut out = String::new();
    if va == 0 {
        return out;
    }
    for index in 0..256u32 {
        let unit = memory
            .read_bytes(va + index * 2, 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .unwrap_or(0);
        if unit == 0 {
            break;
        }
        out.push(char::from_u32(unit as u32).unwrap_or('?'));
    }
    out
}

fn object<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, prefix: &str) -> u32 {
    let methods: Vec<&str> = STORAGE_METHODS
        .iter()
        .copied()
        .filter(|m| m.starts_with(prefix))
        .collect();
    let block = ctx
        .shared
        .borrow_mut()
        .heap
        .alloc(8 + 4 * methods.len() as u32)
        .unwrap_or(0);
    if block == 0 {
        return 0;
    }
    let _ = memory.write_u32(block, block + 8);
    for (index, name) in methods.iter().enumerate() {
        let _ = memory.write_u32(block + 8 + index as u32 * 4, ctx.builtin_proc(name));
    }
    block
}

fn new_storage<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    node: Rc<RefCell<Node>>,
    name: &str,
) -> u32 {
    let object = object(ctx, memory, "Stg_");
    if object != 0 {
        ctx.shared
            .borrow_mut()
            .storage
            .storages
            .insert(object, (node, name.to_owned()));
    }
    object
}

fn new_stream<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    data: Buffer,
    name: &str,
) -> u32 {
    let object = object(ctx, memory, "Stm_");
    if object != 0 {
        ctx.shared.borrow_mut().storage.streams.insert(
            object,
            StreamObject {
                data,
                pos: 0,
                name: name.to_owned(),
            },
        );
    }
    object
}

/// IStream over a host buffer, for CreateStreamOnHGlobal.
pub fn stream_on_bytes<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    bytes: Vec<u8>,
) -> u32 {
    new_stream(ctx, memory, Rc::new(RefCell::new(bytes)), "")
}

pub fn stream_bytes(ctx: &ApiContext, stream: u32) -> Option<Vec<u8>> {
    ctx.shared
        .borrow()
        .storage
        .streams
        .get(&stream)
        .map(|s| s.data.borrow().clone())
}

/// Write a STATSTG. Names are allocated with the task allocator.
fn write_stat<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    out: u32,
    name: &str,
    kind: u32,
    size: u64,
    clsid: [u8; 16],
    flag: u32,
) {
    let mut bytes = vec![0u8; 72];
    if flag & 1 == 0 {
        let wide: Vec<u8> = name
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let va = ctx
            .shared
            .borrow_mut()
            .heap
            .alloc(wide.len() as u32)
            .unwrap_or(0);
        if va != 0 {
            let _ = memory.write_bytes(va, &wide);
        }
        bytes[0..4].copy_from_slice(&va.to_le_bytes());
    }
    bytes[4..8].copy_from_slice(&kind.to_le_bytes());
    bytes[8..16].copy_from_slice(&size.to_le_bytes());
    bytes[48..64].copy_from_slice(&clsid);
    let _ = memory.write_bytes(out, &bytes);
}

fn deep_copy(from: &Node, to: &mut Node) {
    for (name, data) in &from.streams {
        to.streams
            .insert(name.clone(), Rc::new(RefCell::new(data.borrow().clone())));
    }
    for (name, child) in &from.children {
        let mut copy = Node::default();
        deep_copy(&child.borrow(), &mut copy);
        to.children
            .insert(name.clone(), Rc::new(RefCell::new(copy)));
    }
    to.clsid = from.clsid;
}

/// OLE32 functions that create storage objects.
pub fn api_args(name: &str) -> Option<usize> {
    Some(match name {
        "GetHGlobalFromStream"
        | "GetHGlobalFromILockBytes"
        | "StgIsStorageILockBytes"
        | "WriteClassStg"
        | "ReadClassStg"
        | "WriteClassStm"
        | "ReadClassStm" => {
            if name.starts_with("StgIs") {
                1
            } else {
                2
            }
        }
        "CreateStreamOnHGlobal"
        | "CreateILockBytesOnHGlobal"
        | "WriteFmtUserTypeStg"
        | "ReadFmtUserTypeStg" => 3,
        "StgCreateDocfileOnILockBytes" | "StgCreateDocfile" => 4,
        "StgOpenStorageOnILockBytes" => 6,
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
    let out = |memory: &mut M, va: u32, value: u32| {
        if va != 0 {
            let _ = memory.write_u32(va, value);
        }
    };
    match name {
        "CreateILockBytesOnHGlobal" => {
            let initial = if args[0] != 0 {
                let size = ctx.shared.borrow().heap.size_of(args[0]).unwrap_or(0);
                memory
                    .read_bytes(args[0], size as usize)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let object = object(ctx, memory, "Lkb_");
            if object == 0 {
                return ok(E_OUTOFMEMORY);
            }
            ctx.shared
                .borrow_mut()
                .storage
                .lockbytes
                .insert(object, (Rc::new(RefCell::new(initial)), None));
            out(memory, args[2], object);
            ok(S_OK)
        }
        "StgCreateDocfileOnILockBytes" | "StgOpenStorageOnILockBytes" => {
            let lockbytes = args[0];
            let existing = ctx
                .shared
                .borrow()
                .storage
                .lockbytes
                .get(&lockbytes)
                .and_then(|(_, root)| root.clone());
            let creating = name == "StgCreateDocfileOnILockBytes";
            let root = match (existing, creating) {
                (Some(root), false) => root,
                (None, false) => return ok(STG_E_FILENOTFOUND),
                _ => Rc::new(RefCell::new(Node::default())),
            };
            if let Some(entry) = ctx
                .shared
                .borrow_mut()
                .storage
                .lockbytes
                .get_mut(&lockbytes)
            {
                entry.1 = Some(root.clone());
            }
            let storage = new_storage(ctx, memory, root, "Root Entry");
            let out_index = if creating { 3 } else { 5 };
            out(memory, args[out_index], storage);
            ok(if storage == 0 { E_OUTOFMEMORY } else { S_OK })
        }
        "StgCreateDocfile" => {
            // Compound files on disk are not supported; a transient storage
            // still lets callers that only need scratch space proceed.
            if args[0] != 0 {
                out(memory, args[3], 0);
                return ok(E_NOTIMPL);
            }
            let storage = new_storage(
                ctx,
                memory,
                Rc::new(RefCell::new(Node::default())),
                "Root Entry",
            );
            out(memory, args[3], storage);
            ok(S_OK)
        }
        "StgIsStorageILockBytes" => {
            let has_root = ctx
                .shared
                .borrow()
                .storage
                .lockbytes
                .get(&args[0])
                .map(|(_, r)| r.is_some())
                .unwrap_or(false);
            ok(if has_root { S_OK } else { S_FALSE })
        }
        "CreateStreamOnHGlobal" => {
            let initial = if args[0] != 0 {
                let size = ctx.shared.borrow().heap.size_of(args[0]).unwrap_or(0);
                memory
                    .read_bytes(args[0], size as usize)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let stream = stream_on_bytes(ctx, memory, initial);
            out(memory, args[2], stream);
            ok(if stream == 0 { E_OUTOFMEMORY } else { S_OK })
        }
        "GetHGlobalFromStream" | "GetHGlobalFromILockBytes" => {
            let bytes = if name == "GetHGlobalFromStream" {
                stream_bytes(ctx, args[0])
            } else {
                ctx.shared
                    .borrow()
                    .storage
                    .lockbytes
                    .get(&args[0])
                    .map(|(b, _)| b.borrow().clone())
            };
            let Some(bytes) = bytes else {
                return ok(0x8007_0057); // E_INVALIDARG
            };
            let global = ctx
                .shared
                .borrow_mut()
                .heap
                .alloc(bytes.len().max(1) as u32)
                .unwrap_or(0);
            let _ = memory.write_bytes(global, &bytes);
            out(memory, args[1], global);
            ok(S_OK)
        }
        "WriteClassStg" | "ReadClassStg" => {
            let node = ctx
                .shared
                .borrow()
                .storage
                .storages
                .get(&args[0])
                .map(|(n, _)| n.clone());
            let Some(node) = node else {
                return ok(STG_E_INVALIDFUNCTION);
            };
            if name == "WriteClassStg" {
                if let Some(clsid) = memory.read_bytes(args[1], 16) {
                    node.borrow_mut().clsid.copy_from_slice(&clsid);
                }
            } else {
                let clsid = node.borrow().clsid;
                let _ = memory.write_bytes(args[1], &clsid);
            }
            ok(S_OK)
        }
        "WriteClassStm" | "ReadClassStm" => {
            let mut shared = ctx.shared.borrow_mut();
            let Some(stream) = shared.storage.streams.get_mut(&args[0]) else {
                return ok(STG_E_INVALIDFUNCTION);
            };
            if name == "WriteClassStm" {
                drop(shared);
                let clsid = memory
                    .read_bytes(args[1], 16)
                    .unwrap_or_else(|| vec![0; 16]);
                let mut shared = ctx.shared.borrow_mut();
                let stream = shared.storage.streams.get_mut(&args[0]).unwrap();
                let pos = stream.pos as usize;
                let mut data = stream.data.borrow_mut();
                if data.len() < pos + 16 {
                    data.resize(pos + 16, 0);
                }
                data[pos..pos + 16].copy_from_slice(&clsid);
                drop(data);
                stream.pos += 16;
            } else {
                let pos = stream.pos as usize;
                let data = stream.data.borrow();
                if data.len() < pos + 16 {
                    return ok(0x8003_001E); // STG_E_READFAULT
                }
                let clsid = data[pos..pos + 16].to_vec();
                drop(data);
                stream.pos += 16;
                drop(shared);
                let _ = memory.write_bytes(args[1], &clsid);
            }
            ok(S_OK)
        }
        "WriteFmtUserTypeStg" => ok(S_OK),
        "ReadFmtUserTypeStg" => {
            out(memory, args[1], 0);
            out(memory, args[2], 0);
            ok(S_OK)
        }

        // ── ILockBytes ──
        "Lkb_QueryInterface" | "Stg_QueryInterface" | "Stm_QueryInterface" => {
            out(memory, args[2], args[0]);
            ok(S_OK)
        }
        "Lkb_AddRef" | "Lkb_Release" | "Stg_AddRef" | "Stg_Release" | "Stm_AddRef"
        | "Stm_Release" => ok(1),
        "Lkb_ReadAt" | "Lkb_WriteAt" => {
            let offset = args[1] as usize;
            let buffer = ctx
                .shared
                .borrow()
                .storage
                .lockbytes
                .get(&args[0])
                .map(|(b, _)| b.clone());
            let Some(buffer) = buffer else {
                return ok(STG_E_INVALIDFUNCTION);
            };
            let done = if name == "Lkb_ReadAt" {
                let data = buffer.borrow();
                let end = (offset + args[4] as usize).min(data.len());
                let chunk = if offset < end {
                    data[offset..end].to_vec()
                } else {
                    Vec::new()
                };
                drop(data);
                let _ = memory.write_bytes(args[3], &chunk);
                chunk.len()
            } else {
                let chunk = memory
                    .read_bytes(args[3], args[4] as usize)
                    .unwrap_or_default();
                let mut data = buffer.borrow_mut();
                if data.len() < offset + chunk.len() {
                    data.resize(offset + chunk.len(), 0);
                }
                data[offset..offset + chunk.len()].copy_from_slice(&chunk);
                chunk.len()
            };
            out(memory, args[5], done as u32);
            ok(S_OK)
        }
        "Lkb_Flush" | "Lkb_LockRegion" | "Lkb_UnlockRegion" => ok(S_OK),
        "Lkb_SetSize" => {
            if let Some((buffer, _)) = ctx.shared.borrow().storage.lockbytes.get(&args[0]) {
                buffer.borrow_mut().resize(args[1] as usize, 0);
            }
            ok(S_OK)
        }
        "Lkb_Stat" => {
            let size = ctx
                .shared
                .borrow()
                .storage
                .lockbytes
                .get(&args[0])
                .map(|(b, _)| b.borrow().len())
                .unwrap_or(0);
            write_stat(ctx, memory, args[1], "", 3, size as u64, [0; 16], args[2]);
            ok(S_OK)
        }

        // ── IStorage ──
        "Stg_CreateStream" | "Stg_OpenStream" => {
            let element = read_wide(memory, args[1]);
            let mode = if name == "Stg_CreateStream" {
                args[2]
            } else {
                args[3]
            };
            let node = ctx
                .shared
                .borrow()
                .storage
                .storages
                .get(&args[0])
                .map(|(n, _)| n.clone());
            let Some(node) = node else {
                return ok(STG_E_INVALIDFUNCTION);
            };
            let existing = node.borrow().streams.get(&element).cloned();
            let data = match (existing, name == "Stg_CreateStream") {
                (Some(data), false) => data,
                (None, false) => {
                    out(memory, args[5], 0);
                    return ok(STG_E_FILENOTFOUND);
                }
                (Some(_), true) if mode & STGM_CREATE == 0 => {
                    out(memory, args[5], 0);
                    return ok(STG_E_FILEALREADYEXISTS);
                }
                _ => {
                    let data: Buffer = Rc::new(RefCell::new(Vec::new()));
                    node.borrow_mut()
                        .streams
                        .insert(element.clone(), data.clone());
                    data
                }
            };
            let stream = new_stream(ctx, memory, data, &element);
            out(memory, args[5], stream);
            ok(if stream == 0 { E_OUTOFMEMORY } else { S_OK })
        }
        "Stg_CreateStorage" | "Stg_OpenStorage" => {
            let element = read_wide(memory, args[1]);
            let out_index = if name == "Stg_CreateStorage" { 5 } else { 6 };
            let node = ctx
                .shared
                .borrow()
                .storage
                .storages
                .get(&args[0])
                .map(|(n, _)| n.clone());
            let Some(node) = node else {
                return ok(STG_E_INVALIDFUNCTION);
            };
            let existing = node.borrow().children.get(&element).cloned();
            let child = match (existing, name == "Stg_CreateStorage") {
                (Some(child), false) => child,
                (None, false) => {
                    out(memory, args[out_index], 0);
                    return ok(STG_E_FILENOTFOUND);
                }
                (Some(_), true) if args[2] & STGM_CREATE == 0 => {
                    out(memory, args[out_index], 0);
                    return ok(STG_E_FILEALREADYEXISTS);
                }
                _ => {
                    let child = Rc::new(RefCell::new(Node::default()));
                    node.borrow_mut()
                        .children
                        .insert(element.clone(), child.clone());
                    child
                }
            };
            let storage = new_storage(ctx, memory, child, &element);
            out(memory, args[out_index], storage);
            ok(if storage == 0 { E_OUTOFMEMORY } else { S_OK })
        }
        "Stg_CopyTo" => {
            let shared = ctx.shared.borrow();
            let from = shared
                .storage
                .storages
                .get(&args[0])
                .map(|(n, _)| n.clone());
            let to = shared
                .storage
                .storages
                .get(&args[4])
                .map(|(n, _)| n.clone());
            drop(shared);
            match (from, to) {
                (Some(from), Some(to)) => {
                    if !Rc::ptr_eq(&from, &to) {
                        deep_copy(&from.borrow(), &mut to.borrow_mut());
                    }
                    ok(S_OK)
                }
                _ => ok(STG_E_INVALIDFUNCTION),
            }
        }
        "Stg_MoveElementTo" | "Stg_EnumElements" => {
            if name == "Stg_EnumElements" {
                out(memory, args[4], 0);
            }
            ok(E_NOTIMPL)
        }
        "Stg_Commit" | "Stg_Revert" | "Stg_SetElementTimes" => ok(S_OK),
        "Stg_DestroyElement" => {
            let element = read_wide(memory, args[1]);
            if let Some((node, _)) = ctx.shared.borrow().storage.storages.get(&args[0]) {
                let mut node = node.borrow_mut();
                let removed = node.streams.remove(&element).is_some()
                    | node.children.remove(&element).is_some();
                return ok(if removed { S_OK } else { STG_E_FILENOTFOUND });
            }
            ok(STG_E_INVALIDFUNCTION)
        }
        "Stg_RenameElement" => {
            let (from, to) = (read_wide(memory, args[1]), read_wide(memory, args[2]));
            if let Some((node, _)) = ctx.shared.borrow().storage.storages.get(&args[0]) {
                let mut node = node.borrow_mut();
                if let Some(stream) = node.streams.remove(&from) {
                    node.streams.insert(to, stream);
                } else if let Some(child) = node.children.remove(&from) {
                    node.children.insert(to, child);
                } else {
                    return ok(STG_E_FILENOTFOUND);
                }
                return ok(S_OK);
            }
            ok(STG_E_INVALIDFUNCTION)
        }
        "Stg_SetClass" => {
            let clsid = memory
                .read_bytes(args[1], 16)
                .unwrap_or_else(|| vec![0; 16]);
            if let Some((node, _)) = ctx.shared.borrow().storage.storages.get(&args[0]) {
                node.borrow_mut().clsid.copy_from_slice(&clsid);
            }
            ok(S_OK)
        }
        "Stg_SetStateBits" => {
            if let Some((node, _)) = ctx.shared.borrow().storage.storages.get(&args[0]) {
                let mut node = node.borrow_mut();
                node.state_bits = (node.state_bits & !args[2]) | (args[1] & args[2]);
            }
            ok(S_OK)
        }
        "Stg_Stat" => {
            let entry = ctx
                .shared
                .borrow()
                .storage
                .storages
                .get(&args[0])
                .map(|(n, name)| (n.borrow().clsid, name.clone()));
            let Some((clsid, element)) = entry else {
                return ok(STG_E_INVALIDFUNCTION);
            };
            write_stat(ctx, memory, args[1], &element, 1, 0, clsid, args[2]);
            ok(S_OK)
        }

        // ── IStream ──
        "Stm_Read" | "Stm_Write" => {
            let (buffer, pos) = {
                let shared = ctx.shared.borrow();
                let Some(stream) = shared.storage.streams.get(&args[0]) else {
                    return ok(STG_E_INVALIDFUNCTION);
                };
                (stream.data.clone(), stream.pos as usize)
            };
            let done = if name == "Stm_Read" {
                let data = buffer.borrow();
                let end = (pos + args[2] as usize).min(data.len());
                let chunk = if pos < end {
                    data[pos..end].to_vec()
                } else {
                    Vec::new()
                };
                drop(data);
                let _ = memory.write_bytes(args[1], &chunk);
                chunk.len()
            } else {
                let chunk = memory
                    .read_bytes(args[1], args[2] as usize)
                    .unwrap_or_default();
                let mut data = buffer.borrow_mut();
                if data.len() < pos + chunk.len() {
                    data.resize(pos + chunk.len(), 0);
                }
                data[pos..pos + chunk.len()].copy_from_slice(&chunk);
                chunk.len()
            };
            if let Some(stream) = ctx.shared.borrow_mut().storage.streams.get_mut(&args[0]) {
                stream.pos += done as u64;
            }
            out(memory, args[3], done as u32);
            ok(if name == "Stm_Read" && done < args[2] as usize {
                S_FALSE
            } else {
                S_OK
            })
        }
        "Stm_Seek" => {
            let offset = (args[1] as u64 | (args[2] as u64) << 32) as i64;
            let mut shared = ctx.shared.borrow_mut();
            let Some(stream) = shared.storage.streams.get_mut(&args[0]) else {
                return ok(STG_E_INVALIDFUNCTION);
            };
            let base = match args[3] {
                0 => 0,
                1 => stream.pos as i64,
                _ => stream.data.borrow().len() as i64,
            };
            let next = (base + offset).max(0) as u64;
            stream.pos = next;
            drop(shared);
            if args[4] != 0 {
                let _ = memory.write_u32(args[4], next as u32);
                let _ = memory.write_u32(args[4] + 4, (next >> 32) as u32);
            }
            ok(S_OK)
        }
        "Stm_SetSize" => {
            if let Some(stream) = ctx.shared.borrow().storage.streams.get(&args[0]) {
                stream.data.borrow_mut().resize(args[1] as usize, 0);
            }
            ok(S_OK)
        }
        "Stm_CopyTo" => {
            let (source, pos) = {
                let shared = ctx.shared.borrow();
                let Some(stream) = shared.storage.streams.get(&args[0]) else {
                    return ok(STG_E_INVALIDFUNCTION);
                };
                (stream.data.clone(), stream.pos as usize)
            };
            let wanted = args[2] as usize;
            let chunk: Vec<u8> = {
                let data = source.borrow();
                let end = (pos + wanted).min(data.len());
                if pos < end {
                    data[pos..end].to_vec()
                } else {
                    Vec::new()
                }
            };
            if let Some(stream) = ctx.shared.borrow_mut().storage.streams.get_mut(&args[0]) {
                stream.pos += chunk.len() as u64;
            }
            // Write through the destination's own Write method.
            let scratch = ctx
                .shared
                .borrow_mut()
                .heap
                .alloc(chunk.len().max(1) as u32 + 4)
                .unwrap_or(0);
            let _ = memory.write_bytes(scratch, &chunk);
            crate::win32::clipboard::com_call(
                memory,
                args[1],
                4,
                &[scratch, chunk.len() as u32, 0],
            );
            ctx.shared.borrow_mut().heap.free(scratch);
            for (index, value) in [(4usize, chunk.len()), (5, chunk.len())] {
                if args[index] != 0 {
                    let _ = memory.write_u32(args[index], value as u32);
                    let _ = memory.write_u32(args[index] + 4, 0);
                }
            }
            ok(S_OK)
        }
        "Stm_Commit" | "Stm_Revert" | "Stm_LockRegion" | "Stm_UnlockRegion" => ok(S_OK),
        "Stm_Stat" => {
            let entry = ctx
                .shared
                .borrow()
                .storage
                .streams
                .get(&args[0])
                .map(|s| (s.data.borrow().len(), s.name.clone()));
            let Some((size, element)) = entry else {
                return ok(STG_E_INVALIDFUNCTION);
            };
            write_stat(
                ctx,
                memory,
                args[1],
                &element,
                2,
                size as u64,
                [0; 16],
                args[2],
            );
            ok(S_OK)
        }
        "Stm_Clone" => {
            let entry = ctx
                .shared
                .borrow()
                .storage
                .streams
                .get(&args[0])
                .map(|s| (s.data.clone(), s.pos, s.name.clone()));
            let Some((data, pos, element)) = entry else {
                return ok(STG_E_INVALIDFUNCTION);
            };
            let clone = new_stream(ctx, memory, data, &element);
            if let Some(stream) = ctx.shared.borrow_mut().storage.streams.get_mut(&clone) {
                stream.pos = pos;
            }
            out(memory, args[1], clone);
            ok(S_OK)
        }
        _ => None,
    }
}
