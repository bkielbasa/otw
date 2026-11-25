//! OLE32 / OLEAUT32 / WINSPOOL and remaining COMDLG32 / SHELL32 entry points.
//!
//! There is no COM runtime in the VM. OLE initialisation, class-object and
//! drag-and-drop registration succeed so MFC applications start as ordinary
//! OLE servers and containers; embedding, linking, monikers and structured
//! storage report `E_NOTIMPL`, which MFC turns into graceful failures.

use crate::{
    kernel::pe::ImportName,
    win32::{ApiContext, ApiOutcome, GuestMemory},
};

const S_OK: u32 = 0;
const S_FALSE: u32 = 1;
const E_NOTIMPL: u32 = 0x8000_4001;
const E_OUTOFMEMORY: u32 = 0x8007_000E;

fn ole32_args(name: &str) -> Option<usize> {
    Some(match name {
        "IMalloc_AddRef" | "IMalloc_Release" | "IMalloc_HeapMinimize" => 1,
        "IMalloc_Alloc" | "IMalloc_Free" | "IMalloc_GetSize" | "IMalloc_DidAlloc" => 2,
        "IMalloc_QueryInterface" | "IMalloc_Realloc" => 3,
        "Moniker_AddRef" | "Moniker_Release" | "Moniker_IsDirty" | "Rot_AddRef" | "Rot_Release" => {
            1
        }
        "Moniker_GetClassID"
        | "Moniker_Load"
        | "Moniker_GetSizeMax"
        | "Moniker_IsEqual"
        | "Moniker_Hash"
        | "Moniker_Inverse"
        | "Moniker_IsSystemMoniker"
        | "Rot_Revoke"
        | "Rot_IsRunning"
        | "Rot_EnumRunning" => 2,
        "Moniker_QueryInterface"
        | "Moniker_Save"
        | "Moniker_Enum"
        | "Moniker_CommonPrefixWith"
        | "Moniker_RelativePathTo"
        | "Rot_QueryInterface"
        | "Rot_GetObject"
        | "Rot_NoteChangeTime"
        | "Rot_GetTimeOfLastChange" => 3,
        "Moniker_ComposeWith"
        | "Moniker_IsRunning"
        | "Moniker_GetTimeOfLastChange"
        | "Moniker_GetDisplayName" => 4,
        "Moniker_BindToObject" | "Moniker_BindToStorage" | "Moniker_Reduce" | "Rot_Register" => 5,
        "Moniker_ParseDisplayName" => 6,
        "DataObj_AddRef" | "DataObj_Release" | "EnumFmt_AddRef" | "EnumFmt_Release"
        | "EnumFmt_Reset" => 1,
        "DataObj_QueryGetData"
        | "DataObj_DUnadvise"
        | "DataObj_EnumDAdvise"
        | "EnumFmt_Skip"
        | "EnumFmt_Clone" => 2,
        "DataObj_QueryInterface"
        | "DataObj_GetData"
        | "DataObj_GetDataHere"
        | "DataObj_GetCanonicalFormatEtc"
        | "DataObj_EnumFormatEtc"
        | "EnumFmt_QueryInterface" => 3,
        "DataObj_SetData" | "EnumFmt_Next" => 4,
        "DataObj_DAdvise" => 5,
        "CoFreeUnusedLibraries" | "OleFlushClipboard" | "OleUninitialize" | "CoUninitialize" => 0,
        "CoInitializeEx" => 2,
        "CoRevokeClassObject"
        | "CoTaskMemAlloc"
        | "CoTaskMemFree"
        | "CreateAntiMoniker"
        | "CreateDataAdviseHolder"
        | "CreateOleAdviseHolder"
        | "GetConvertStg"
        | "OleDestroyMenuDescriptor"
        | "OleGetClipboard"
        | "OleInitialize"
        | "OleIsCurrentClipboard"
        | "OleIsRunning"
        | "OleQueryCreateFromData"
        | "OleQueryLinkFromData"
        | "OleRun"
        | "OleSetClipboard"
        | "ReleaseStgMedium"
        | "RevokeDragDrop"
        | "StgIsStorageFile"
        | "StgIsStorageILockBytes"
        | "CoInitialize" => 1,
        "CLSIDFromProgID"
        | "CLSIDFromString"
        | "CoDisconnectObject"
        | "CoGetMalloc"
        | "CoLoadLibrary"
        | "CoRegisterMessageFilter"
        | "CoTreatAsClass"
        | "CreateBindCtx"
        | "CreateFileMoniker"
        | "CreatePointerMoniker"
        | "GetClassFile"
        | "GetHGlobalFromILockBytes"
        | "GetHGlobalFromStream"
        | "GetRunningObjectTable"
        | "IIDFromString"
        | "OleConvertIStorageToOLESTREAM"
        | "OleCreateMenuDescriptor"
        | "OleDoAutoConvert"
        | "OleGetIconOfFile"
        | "OleNoteObjectVisible"
        | "OleRegEnumVerbs"
        | "OleSaveToStream"
        | "OleSetContainedObject"
        | "ProgIDFromCLSID"
        | "ReadClassStg"
        | "ReadClassStm"
        | "RegisterDragDrop"
        | "SetConvertStg"
        | "StringFromCLSID"
        | "StringFromIID"
        | "WriteClassStg"
        | "WriteClassStm" => 2,
        "CoLockObjectExternal"
        | "CoUnmarshalInterface"
        | "CreateGenericComposite"
        | "CreateILockBytesOnHGlobal"
        | "CreateItemMoniker"
        | "CreateStreamOnHGlobal"
        | "MonikerCommonPrefixWith"
        | "OleConvertOLESTREAMToIStorage"
        | "OleDuplicateData"
        | "OleGetIconOfClass"
        | "OleLoadFromStream"
        | "OleLockRunning"
        | "OleRegEnumFormatEtc"
        | "OleRegGetMiscStatus"
        | "OleRegGetUserType"
        | "OleSave"
        | "OleTranslateAccelerator"
        | "ReadFmtUserTypeStg"
        | "StringFromGUID2"
        | "WriteFmtUserTypeStg" => 3,
        "BindMoniker"
        | "CreateDataCache"
        | "DoDragDrop"
        | "IsAccelerator"
        | "MkParseDisplayName"
        | "MonikerRelativePathTo"
        | "OleCreateDefaultHandler"
        | "OleDraw"
        | "OleLoad"
        | "OleMetafilePictFromIconAndLabel"
        | "StgCreateDocfile"
        | "StgCreateDocfileOnILockBytes"
        | "StgSetTimes" => 4,
        "CoCreateInstance"
        | "CoGetClassObject"
        | "CoRegisterClassObject"
        | "OleSetMenuDescriptor" => 5,
        "CoMarshalInterface" | "StgOpenStorage" | "StgOpenStorageOnILockBytes" => 6,
        "OleCreate"
        | "OleCreateFromData"
        | "OleCreateLink"
        | "OleCreateLinkFromData"
        | "OleCreateLinkToFile"
        | "OleCreateStaticFromData" => 7,
        "OleCreateFromFile" => 8,
        _ => return None,
    })
}

fn oleaut32_args(ordinal: u16) -> Option<usize> {
    Some(match ordinal {
        2 | 6 | 7 | 8 | 9 | 16 | 17 | 18 | 21 | 22 | 24 | 37 | 38 | 39 | 149 | 150 => 1,
        4 | 10 | 11 | 23 | 27 | 36 | 40 => 2,
        15 | 19 | 20 | 25 | 26 | 31 | 35 => 3,
        12 | 29 | 32 | 33 => 4,
        28 => 5,
        30 => 8,
        _ => return None,
    })
}

/// The process-wide task allocator object: one vtable pointer followed by
/// the vtable of Rust-backed IMalloc methods.
fn malloc_object<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) -> u32 {
    let existing = ctx.shared.borrow().imalloc;
    if existing != 0 {
        return existing;
    }
    let block = ctx
        .shared
        .borrow_mut()
        .heap
        .alloc(4 + 4 * crate::win32::BUILTIN_COM.len() as u32)
        .unwrap_or(0);
    if block == 0 {
        return 0;
    }
    crate::win32::set_vtable(ctx, memory, block, crate::win32::BUILTIN_COM);
    ctx.shared.borrow_mut().imalloc = block;
    block
}

/// Build a COM object whose vtable is every BUILTIN_COM method with the
/// given prefix, followed by `extra` bytes of instance data.
fn com_object<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    prefix: &str,
    extra: u32,
) -> u32 {
    let methods: Vec<&str> = crate::win32::BUILTIN_COM
        .iter()
        .copied()
        .filter(|name| name.starts_with(prefix))
        .collect();
    let block = ctx
        .shared
        .borrow_mut()
        .heap
        .alloc(4 + extra + 4 * methods.len() as u32)
        .unwrap_or(0);
    if block == 0 {
        return 0;
    }
    let vtable = block + 4 + extra;
    let _ = memory.write_u32(block, vtable);
    for (index, name) in methods.iter().enumerate() {
        let _ = memory.write_u32(vtable + index as u32 * 4, ctx.builtin_proc(name));
    }
    block
}

/// The IDataObject handed out by OleGetClipboard (one per process).
fn data_object<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) -> u32 {
    let existing = ctx.shared.borrow().clipboard_object;
    if existing != 0 {
        return existing;
    }
    let object = com_object(ctx, memory, "DataObj_", 0);
    ctx.shared.borrow_mut().clipboard_object = object;
    object
}

/// An IEnumFORMATETC over `formats`: instance data is the cursor, count
/// and the format list.
fn format_enumerator<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    formats: &[u32],
    index: u32,
) -> u32 {
    let formats = &formats[..formats.len().min(32)];
    let object = com_object(ctx, memory, "EnumFmt_", 8 + 4 * 32);
    if object == 0 {
        return 0;
    }
    let _ = memory.write_u32(object + 4, index);
    let _ = memory.write_u32(object + 8, formats.len() as u32);
    for (i, cf) in formats.iter().enumerate() {
        let _ = memory.write_u32(object + 12 + i as u32 * 4, *cf);
    }
    object
}

pub fn arg_count(dll: &str, name: &ImportName) -> Option<usize> {
    let dll = dll.to_ascii_lowercase();
    match (dll.as_str(), name) {
        ("ole32.dll", ImportName::ByName(name)) => crate::win32::storage::method_args(name)
            .or_else(|| crate::win32::shelllink::arg_count(name))
            .or_else(|| crate::win32::shellfolder::arg_count(name))
            .or_else(|| crate::win32::bindctx::arg_count(name))
            .or_else(|| crate::win32::catmgr::arg_count(name))
            .or_else(|| crate::win32::storage::api_args(name))
            .or_else(|| crate::win32::com::arg_count(name))
            .or_else(|| ole32_args(name)),
        ("oleaut32.dll", ImportName::Ordinal(ordinal)) => oleaut32_args(*ordinal),
        ("winspool.drv", ImportName::ByName(name)) => match name.as_str() {
            "ClosePrinter" => Some(1),
            "OpenPrinterA" => Some(3),
            "DocumentPropertiesA" => Some(6),
            _ => None,
        },
        ("comdlg32.dll", ImportName::ByName(name)) => match name.as_str() {
            "ChooseColorA" | "ChooseFontA" | "ReplaceTextA" | "PrintDlgA" => Some(1),
            _ => None,
        },
        ("shell32.dll", ImportName::ByName(name)) => match name.as_str() {
            "ExtractIconA" => Some(3),
            _ => None,
        },
        _ => None,
    }
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dll: &str,
    name: &ImportName,
    args: &[u32],
) -> Option<ApiOutcome> {
    let count = arg_count(dll, name)?;
    if args.len() < count {
        return None;
    }
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    // A pointer-sized result (an interface pointer, a handle) and a 32-bit one.
    let out = |memory: &mut M, va: u32, value: u32| {
        if va != 0 {
            let _ = memory.write_ptr(va, u64::from(value));
        }
    };
    let out32 = |memory: &mut M, va: u32, value: u32| {
        if va != 0 {
            let _ = memory.write_u32(va, value);
        }
    };
    match name {
        ImportName::Ordinal(ordinal) => match ordinal {
            2 | 4 => {
                // SysAllocString(Len): length-prefixed wide string.
                let length = if *ordinal == 4 {
                    args[1] as usize
                } else {
                    let mut n = 0usize;
                    while let Some(b) = memory.read_bytes(args[0] + n as u32 * 2, 2) {
                        if b == [0, 0] || n > 1 << 16 {
                            break;
                        }
                        n += 1;
                    }
                    n
                };
                let block = ctx
                    .shared
                    .borrow_mut()
                    .heap
                    .alloc(length as u32 * 2 + 6)
                    .unwrap_or(0);
                if block == 0 {
                    return ok(0);
                }
                let _ = memory.write_u32(block, length as u32 * 2);
                if args[0] != 0 {
                    if let Some(bytes) = memory.read_bytes(args[0], length * 2) {
                        let _ = memory.write_bytes(block + 4, &bytes);
                    }
                }
                let _ = memory.write_bytes(block + 4 + length as u32 * 2, &[0, 0]);
                ok(block + 4)
            }
            6 => {
                if args[0] != 0 {
                    ctx.shared.borrow_mut().heap.free(args[0] - 4);
                }
                ok(0)
            }
            7 => ok(if args[0] == 0 {
                0
            } else {
                memory.read_u32(args[0] - 4).unwrap_or(0) / 2
            }),
            8 => {
                let _ = memory.write_bytes(args[0], &[0u8; 16]);
                ok(0)
            }
            9 => {
                let _ = memory.write_bytes(args[0], &[0u8; 2]);
                ok(S_OK)
            }
            _ => ok(E_NOTIMPL),
        },
        ImportName::ByName(name) => {
            let lower = dll.to_ascii_lowercase();
            if lower == "ole32.dll"
                && (crate::win32::storage::method_args(name).is_some()
                    || crate::win32::storage::api_args(name).is_some())
            {
                return crate::win32::storage::dispatch(ctx, memory, name, args, count);
            }
            if lower == "ole32.dll" && crate::win32::catmgr::arg_count(name).is_some() {
                return crate::win32::catmgr::dispatch(ctx, memory, name, args);
            }
            if lower == "ole32.dll" && crate::win32::bindctx::arg_count(name).is_some() {
                return crate::win32::bindctx::dispatch(ctx, memory, name, args);
            }
            if lower == "ole32.dll" && crate::win32::shellfolder::arg_count(name).is_some() {
                return crate::win32::shellfolder::dispatch(ctx, memory, name, args);
            }
            if lower == "ole32.dll" && crate::win32::shelllink::arg_count(name).is_some() {
                return crate::win32::shelllink::dispatch(ctx, memory, name, args);
            }
            if lower == "ole32.dll" && crate::win32::com::arg_count(name).is_some() {
                return crate::win32::com::dispatch(ctx, memory, name, args);
            }
            if lower == "ole32.dll" && name == "CoCreateInstance" {
                // CoCreateInstance(clsid, outer, context, iid, out)
                let clsid = memory.read_bytes(args[0], 16).unwrap_or_default();
                if clsid == crate::win32::shelllink::CLSID_SHELL_LINK && args[1] == 0 {
                    let iid = memory.read_bytes(args[3], 16).unwrap_or_default();
                    return ok(crate::win32::shelllink::create(ctx, memory, &iid, args[4]));
                }
            }
            if lower == "winspool.drv" {
                return ok(match name.as_str() {
                    "OpenPrinterA" => {
                        out(memory, args[1], 0);
                        0
                    }
                    "DocumentPropertiesA" => u32::MAX,
                    _ => 0,
                });
            }
            if lower == "comdlg32.dll" && name == "ChooseColorA" {
                return ok(crate::win32::commdlg::choose_color(ctx, memory, args[0]));
            }
            if lower == "comdlg32.dll" || lower == "shell32.dll" {
                return ok(0);
            }
            match name.as_str() {
                "IMalloc_QueryInterface" => {
                    out(memory, args[2], args[0]);
                    ok(S_OK)
                }
                "IMalloc_AddRef" | "IMalloc_Release" => ok(1),
                "IMalloc_HeapMinimize" => ok(0),
                "IMalloc_Alloc" => ok(ctx.shared.borrow_mut().heap.alloc(args[1]).unwrap_or(0)),
                "IMalloc_Free" => {
                    if args[1] != 0 {
                        ctx.shared.borrow_mut().heap.free(args[1]);
                    }
                    ok(0)
                }
                "IMalloc_GetSize" => ok(ctx
                    .shared
                    .borrow()
                    .heap
                    .size_of(args[1])
                    .unwrap_or(u32::MAX)),
                "IMalloc_DidAlloc" => {
                    ok(ctx.shared.borrow().heap.size_of(args[1]).is_some() as u32)
                }
                "IMalloc_Realloc" => {
                    let result =
                        crate::win32::crt::dispatch(ctx, memory, "realloc", &[args[1], args[2]]);
                    ok(result.map(|r| r.value).unwrap_or(0))
                }
                "Moniker_QueryInterface" | "Rot_QueryInterface" => {
                    // Every interface request is answered by the object itself.
                    out(memory, args[2], args[0]);
                    ok(S_OK)
                }
                "Moniker_AddRef" | "Moniker_Release" | "Rot_AddRef" | "Rot_Release" => ok(1),
                "Moniker_IsDirty" | "Moniker_IsEqual" | "Moniker_IsRunning" | "Rot_IsRunning" => {
                    ok(S_FALSE)
                }
                "Moniker_Hash" => {
                    out32(memory, args[1], args[0]);
                    ok(S_OK)
                }
                "Moniker_IsSystemMoniker" => {
                    out32(memory, args[1], 2); // MKSYS_FILEMONIKER
                    ok(S_OK)
                }
                "Moniker_GetDisplayName" => {
                    let path = memory.read_u32(args[0] + 4).unwrap_or(0);
                    let mut length = 0u32;
                    while memory
                        .read_bytes(path + length * 2, 2)
                        .map(|b| b != [0, 0])
                        .unwrap_or(false)
                        && length < 1024
                    {
                        length += 1;
                    }
                    let copy = ctx
                        .shared
                        .borrow_mut()
                        .heap
                        .alloc(length * 2 + 2)
                        .unwrap_or(0);
                    if let Some(bytes) = memory.read_bytes(path, length as usize * 2 + 2) {
                        let _ = memory.write_bytes(copy, &bytes);
                    }
                    out(memory, args[3], copy);
                    ok(S_OK)
                }
                "Moniker_Reduce" => {
                    out(memory, args[4], 0);
                    ok(0x0004_01E2) // MK_S_REDUCED_TO_SELF
                }
                "Moniker_GetClassID"
                | "Moniker_Load"
                | "Moniker_Save"
                | "Moniker_GetSizeMax"
                | "Moniker_BindToObject"
                | "Moniker_BindToStorage"
                | "Moniker_ComposeWith"
                | "Moniker_Enum"
                | "Moniker_GetTimeOfLastChange"
                | "Moniker_Inverse"
                | "Moniker_CommonPrefixWith"
                | "Moniker_RelativePathTo"
                | "Moniker_ParseDisplayName"
                | "Rot_EnumRunning" => ok(E_NOTIMPL),
                "Rot_Register" => {
                    let mut shared = ctx.shared.borrow_mut();
                    let cookie = shared.alloc_handle();
                    drop(shared);
                    out32(memory, args[4], cookie);
                    ok(S_OK)
                }
                "Rot_Revoke" | "Rot_NoteChangeTime" => ok(S_OK),
                "Rot_GetObject" | "Rot_GetTimeOfLastChange" => ok(0x8004_01E3), // MK_E_UNAVAILABLE
                "CreateFileMoniker" => {
                    let object = com_object(ctx, memory, "Moniker_", 4);
                    if object == 0 {
                        return ok(E_OUTOFMEMORY);
                    }
                    // Keep a private copy of the wide path after the vtable pointer.
                    let mut length = 0u32;
                    while memory
                        .read_bytes(args[0] + length * 2, 2)
                        .map(|b| b != [0, 0])
                        .unwrap_or(false)
                        && length < 1024
                    {
                        length += 1;
                    }
                    let path = ctx
                        .shared
                        .borrow_mut()
                        .heap
                        .alloc(length * 2 + 2)
                        .unwrap_or(0);
                    if let Some(bytes) = memory.read_bytes(args[0], length as usize * 2 + 2) {
                        let _ = memory.write_bytes(path, &bytes);
                    }
                    let _ = memory.write_u32(object + 4, path);
                    out(memory, args[1], object);
                    ok(S_OK)
                }
                "GetRunningObjectTable" => {
                    let existing = ctx.shared.borrow().rot;
                    let object = if existing != 0 {
                        existing
                    } else {
                        com_object(ctx, memory, "Rot_", 0)
                    };
                    ctx.shared.borrow_mut().rot = object;
                    out(memory, args[1], object);
                    ok(if object == 0 { E_OUTOFMEMORY } else { S_OK })
                }
                "OleSetClipboard" => {
                    crate::win32::clipboard::empty(ctx, memory);
                    if args[0] != 0 {
                        crate::win32::clipboard::com_call(memory, args[0], 1, &[]);
                        crate::win32::clipboard::capture_ole(ctx, memory, args[0]);
                    }
                    ok(S_OK)
                }
                "OleGetClipboard" => {
                    let object = data_object(ctx, memory);
                    out(memory, args[0], object);
                    ok(if object == 0 { E_OUTOFMEMORY } else { S_OK })
                }
                "OleIsCurrentClipboard" => {
                    let source = ctx.shared.borrow().clipboard.ole_source;
                    ok(if args[0] != 0 && args[0] == source {
                        S_OK
                    } else {
                        S_FALSE
                    })
                }
                "OleFlushClipboard" => {
                    // The data is already rendered; just let go of the source.
                    let source = std::mem::take(&mut ctx.shared.borrow_mut().clipboard.ole_source);
                    if source != 0 {
                        crate::win32::clipboard::com_call(memory, source, 2, &[]);
                    }
                    ok(S_OK)
                }
                "ReleaseStgMedium" => {
                    crate::win32::clipboard::release_medium(ctx, memory, args[0]);
                    ok(0)
                }
                "DataObj_QueryInterface" | "EnumFmt_QueryInterface" => {
                    out(memory, args[2], args[0]);
                    ok(S_OK)
                }
                "DataObj_AddRef" | "DataObj_Release" | "EnumFmt_AddRef" | "EnumFmt_Release" => {
                    ok(1)
                }
                "DataObj_GetData" | "DataObj_QueryGetData" => {
                    let cf = memory
                        .read_bytes(args[1], 2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]) as u32)
                        .unwrap_or(0);
                    let tymed = memory.read_u32(args[1] + 16).unwrap_or(0);
                    let want = if cf == crate::win32::clipboard::CF_BITMAP {
                        16
                    } else {
                        1
                    };
                    if !crate::win32::clipboard::is_available(ctx, memory, cf) {
                        return ok(0x8004_0064); // DV_E_FORMATETC
                    }
                    if tymed & want == 0 {
                        return ok(0x8004_0069); // DV_E_TYMED
                    }
                    if name == "DataObj_QueryGetData" {
                        return ok(S_OK);
                    }
                    match crate::win32::clipboard::render(ctx, memory, cf) {
                        Some(handle) => {
                            let _ = memory.write_u32(args[2], want);
                            let _ = memory.write_u32(args[2] + 4, handle);
                            let _ = memory.write_u32(args[2] + 8, 0);
                            ok(S_OK)
                        }
                        None => ok(0x8004_0064),
                    }
                }
                "DataObj_GetCanonicalFormatEtc" => {
                    if args[2] != 0 {
                        if let Some(bytes) = memory.read_bytes(args[1], 20) {
                            let _ = memory.write_bytes(args[2], &bytes);
                            let _ = memory.write_u32(args[2] + 4, 0);
                        }
                    }
                    ok(0x0004_0130) // DATA_S_SAMEFORMATETC
                }
                "DataObj_GetDataHere" | "DataObj_SetData" => ok(E_NOTIMPL),
                "DataObj_DAdvise" | "DataObj_DUnadvise" | "DataObj_EnumDAdvise" => ok(0x8004_0003),
                "DataObj_EnumFormatEtc" => {
                    if args[1] != 1 {
                        out(memory, args[2], 0);
                        return ok(E_NOTIMPL);
                    }
                    let formats = crate::win32::clipboard::available_formats(ctx, memory);
                    let object = format_enumerator(ctx, memory, &formats, 0);
                    out(memory, args[2], object);
                    ok(if object == 0 { E_OUTOFMEMORY } else { S_OK })
                }
                "EnumFmt_Next" => {
                    let (index, count) = (
                        memory.read_u32(args[0] + 4).unwrap_or(0),
                        memory.read_u32(args[0] + 8).unwrap_or(0),
                    );
                    let mut fetched = 0;
                    while fetched < args[1] && index + fetched < count {
                        let cf = memory
                            .read_u32(args[0] + 12 + (index + fetched) * 4)
                            .unwrap_or(0);
                        let tymed = if cf == crate::win32::clipboard::CF_BITMAP {
                            16u32
                        } else {
                            1
                        };
                        let mut entry = Vec::with_capacity(20);
                        entry.extend_from_slice(&(cf as u16).to_le_bytes());
                        entry.extend_from_slice(&[0, 0]);
                        for value in [0u32, 1, u32::MAX, tymed] {
                            entry.extend_from_slice(&value.to_le_bytes());
                        }
                        let _ = memory.write_bytes(args[2] + fetched * 20, &entry);
                        fetched += 1;
                    }
                    let _ = memory.write_u32(args[0] + 4, index + fetched);
                    if args[3] != 0 {
                        let _ = memory.write_u32(args[3], fetched);
                    }
                    ok(if fetched == args[1] { S_OK } else { S_FALSE })
                }
                "EnumFmt_Skip" => {
                    let (index, count) = (
                        memory.read_u32(args[0] + 4).unwrap_or(0),
                        memory.read_u32(args[0] + 8).unwrap_or(0),
                    );
                    let next = (index + args[1]).min(count);
                    let _ = memory.write_u32(args[0] + 4, next);
                    ok(if index + args[1] <= count {
                        S_OK
                    } else {
                        S_FALSE
                    })
                }
                "EnumFmt_Reset" => {
                    let _ = memory.write_u32(args[0] + 4, 0);
                    ok(S_OK)
                }
                "EnumFmt_Clone" => {
                    let (index, count) = (
                        memory.read_u32(args[0] + 4).unwrap_or(0),
                        memory.read_u32(args[0] + 8).unwrap_or(0),
                    );
                    let formats: Vec<u32> = (0..count)
                        .map(|i| memory.read_u32(args[0] + 12 + i * 4).unwrap_or(0))
                        .collect();
                    let object = format_enumerator(ctx, memory, &formats, index);
                    out(memory, args[1], object);
                    ok(if object == 0 { E_OUTOFMEMORY } else { S_OK })
                }
                "CoGetMalloc" => {
                    let object = malloc_object(ctx, memory);
                    out(memory, args[1], object);
                    ok(if object == 0 { E_OUTOFMEMORY } else { S_OK })
                }
                "OleInitialize" | "CoInitialize" | "CoInitializeEx" => ok(S_OK),
                "OleUninitialize" | "CoUninitialize" | "CoFreeUnusedLibraries" => ok(0),
                "CoRegisterMessageFilter" => {
                    out(memory, args[1], 0);
                    ok(S_OK)
                }
                "CoRegisterClassObject" => {
                    let mut shared = ctx.shared.borrow_mut();
                    let cookie = shared.alloc_handle();
                    drop(shared);
                    out32(memory, args[4], cookie);
                    ok(S_OK)
                }
                "CoRevokeClassObject"
                | "CoDisconnectObject"
                | "CoLockObjectExternal"
                | "RegisterDragDrop"
                | "RevokeDragDrop"
                | "OleSetMenuDescriptor"
                | "OleNoteObjectVisible"
                | "OleSetContainedObject"
                | "OleLockRunning" => ok(S_OK),
                "OleIsRunning" => ok(S_FALSE),
                "OleQueryCreateFromData" | "OleQueryLinkFromData" => ok(S_FALSE),
                "IsAccelerator" => ok(0),
                "OleTranslateAccelerator" => ok(S_FALSE),
                "OleCreateMenuDescriptor" => {
                    let mut shared = ctx.shared.borrow_mut();
                    ok(shared.alloc_handle())
                }
                "OleDestroyMenuDescriptor" => ok(0),
                "CoTaskMemAlloc" => ok(ctx.shared.borrow_mut().heap.alloc(args[0]).unwrap_or(0)),
                "CoTaskMemFree" => {
                    if args[0] != 0 {
                        ctx.shared.borrow_mut().heap.free(args[0]);
                    }
                    ok(0)
                }
                "CreateBindCtx"
                | "CreateItemMoniker"
                | "CreateAntiMoniker"
                | "CreatePointerMoniker"
                | "CreateGenericComposite"
                | "CreateOleAdviseHolder"
                | "CreateDataAdviseHolder"
                | "CreateStreamOnHGlobal"
                | "CreateILockBytesOnHGlobal"
                | "CoGetClassObject"
                | "CoCreateInstance" => {
                    let out_index = match name.as_str() {
                        "OleGetClipboard"
                        | "CreateAntiMoniker"
                        | "CreateOleAdviseHolder"
                        | "CreateDataAdviseHolder" => 0,
                        "CoGetMalloc"
                        | "CreateBindCtx"
                        | "GetRunningObjectTable"
                        | "CreateFileMoniker"
                        | "CreatePointerMoniker" => 1,
                        "CreateItemMoniker"
                        | "CreateGenericComposite"
                        | "CreateStreamOnHGlobal"
                        | "CreateILockBytesOnHGlobal" => 2,
                        _ => 4,
                    };
                    out(memory, args[out_index], 0);
                    ok(E_NOTIMPL)
                }
                "StgIsStorageFile" | "StgIsStorageILockBytes" => ok(S_FALSE),
                "StringFromGUID2" => {
                    let guid = memory
                        .read_bytes(args[0], 16)
                        .unwrap_or_else(|| vec![0; 16]);
                    let d1 = u32::from_le_bytes([guid[0], guid[1], guid[2], guid[3]]);
                    let d2 = u16::from_le_bytes([guid[4], guid[5]]);
                    let d3 = u16::from_le_bytes([guid[6], guid[7]]);
                    let text = format!(
                        "{{{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
                        guid[8], guid[9], guid[10], guid[11], guid[12], guid[13], guid[14], guid[15]
                    );
                    if (args[2] as usize) < text.len() + 1 {
                        return ok(0);
                    }
                    let wide: Vec<u8> = text
                        .bytes()
                        .chain(std::iter::once(0))
                        .flat_map(|b| [b, 0])
                        .collect();
                    let _ = memory.write_bytes(args[1], &wide);
                    ok(text.len() as u32 + 1)
                }
                _ => {
                    // Output parameters of the remaining calls are cleared
                    // where their position is unambiguous.
                    ok(E_NOTIMPL)
                }
            }
        }
    }
}
