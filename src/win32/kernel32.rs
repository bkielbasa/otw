//! KERNEL32 memory, file, resource, atom and time services.

use crate::{
    win32::crt,
    win32::user::{self, ResName},
    win32::{read_cstr, write_cstr, ApiContext, ApiOutcome, GuestMemory},
};

const INVALID: u32 = u32::MAX;

/// Synthetic module handles for system DLLs implemented in Rust.
fn system_module(name: &str) -> Option<u32> {
    let base = name.to_ascii_lowercase();
    let base = base.trim_end_matches(".dll");
    Some(match base {
        "kernel32" => 0xBFF7_0000,
        "user32" => 0xBFF6_0000,
        "gdi32" => 0xBFF3_0000,
        "advapi32" => 0xBFEF_0000,
        "shell32" => 0xBFE0_0000,
        "comdlg32" => 0xBFE8_0000,
        "ole32" => 0xBFD0_0000,
        "oleaut32" => 0xBFC8_0000,
        "msvcrt20" => 0xBFC0_0000,
        _ => return None,
    })
}

/// DLL name behind a synthetic system module handle.
pub fn system_module_name(handle: u32) -> Option<&'static str> {
    Some(match handle {
        0xBFF7_0000 => "KERNEL32.dll",
        0xBFF6_0000 => "USER32.dll",
        0xBFF3_0000 => "GDI32.dll",
        0xBFEF_0000 => "ADVAPI32.dll",
        0xBFE0_0000 => "SHELL32.dll",
        0xBFE8_0000 => "COMDLG32.dll",
        0xBFD0_0000 => "OLE32.dll",
        0xBFC8_0000 => "OLEAUT32.dll",
        0xBFC0_0000 => "MSVCRT20.dll",
        _ => return None,
    })
}

/// Number of dynamic thunk slots reserved by the VM.
pub const DYNAMIC_SLOTS: usize = 256;

/// Give a Rust-implemented function a guest-callable address for
/// GetProcAddress, using a reserved dynamic thunk slot.
pub fn dynamic_thunk(ctx: &ApiContext, dll: &str, name: &str) -> Option<u32> {
    let import = crate::kernel::pe::ImportName::ByName(name.to_owned());
    crate::win32::arg_count(dll, &import)?;
    let base = ctx.builtin.get("__dynamic_base").copied()?;
    let mut shared = ctx.shared.borrow_mut();
    if let Some(index) = shared
        .dynamic_imports
        .iter()
        .position(|(d, n)| d.eq_ignore_ascii_case(dll) && n == name)
    {
        return Some(base + index as u32 * crate::kernel::iat::THUNK_STRIDE);
    }
    if shared.dynamic_imports.len() >= DYNAMIC_SLOTS {
        return None;
    }
    shared
        .dynamic_imports
        .push((dll.to_owned(), name.to_owned()));
    Some(base + (shared.dynamic_imports.len() as u32 - 1) * crate::kernel::iat::THUNK_STRIDE)
}

fn set_error(ctx: &ApiContext, code: u32) {
    ctx.shared.borrow_mut().last_error = code;
}

/// 100-ns intervals since 1601 for a UNIX time.
fn filetime_from_unix(secs: u64, nanos: u32) -> u64 {
    (secs + 11_644_473_600) * 10_000_000 + nanos as u64 / 100
}

fn write_filetime<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, value: u64) {
    if va != 0 {
        let _ = memory.write_u32(va, value as u32);
        let _ = memory.write_u32(va + 4, (value >> 32) as u32);
    }
}

fn read_filetime<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> u64 {
    let lo = memory.read_u32(va).unwrap_or(0) as u64;
    let hi = memory.read_u32(va + 4).unwrap_or(0) as u64;
    lo | (hi << 32)
}

fn metadata_times(metadata: &std::fs::Metadata) -> (u64, u64, u64) {
    let convert = |time: std::io::Result<std::time::SystemTime>| {
        time.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| filetime_from_unix(d.as_secs(), d.subsec_nanos()))
            .unwrap_or(filetime_from_unix(820_454_400, 0))
    };
    (
        convert(metadata.created()),
        convert(metadata.accessed()),
        convert(metadata.modified()),
    )
}

fn wildcard_match(pattern: &[u8], name: &[u8]) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    // "*.*" matches names without a dot too, as on FAT.
    if pattern == b"*.*" || pattern == b"*" {
        return true;
    }
    fn go(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(&pattern, &name)
}

/// Fill a WIN32_FIND_DATAA for a host directory entry.
fn write_find_data<M: GuestMemory + ?Sized>(
    memory: &mut M,
    va: u32,
    name: &[u8],
    metadata: &std::fs::Metadata,
) {
    let mut bytes = vec![0u8; 320];
    let attributes: u32 = if metadata.is_dir() { 0x10 } else { 0x20 };
    bytes[0..4].copy_from_slice(&attributes.to_le_bytes());
    let (created, accessed, written) = metadata_times(metadata);
    bytes[4..12].copy_from_slice(&created.to_le_bytes());
    bytes[12..20].copy_from_slice(&accessed.to_le_bytes());
    bytes[20..28].copy_from_slice(&written.to_le_bytes());
    let size = if metadata.is_dir() { 0 } else { metadata.len() };
    bytes[28..32].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
    bytes[32..36].copy_from_slice(&(size as u32).to_le_bytes());
    let n = name.len().min(259);
    bytes[44..44 + n].copy_from_slice(&name[..n]);
    let short: Vec<u8> = name.iter().take(13).copied().collect();
    bytes[304..304 + short.len()].copy_from_slice(&short);
    let _ = memory.write_bytes(va, &bytes);
}

fn module_path(ctx: &ApiContext, module: u32) -> Vec<u8> {
    let name = if module == 0 || module == ctx.image_base {
        ctx.modules
            .first()
            .map(|m| m.name.clone())
            .unwrap_or_else(|| "APP.EXE".into())
    } else {
        ctx.modules
            .iter()
            .find(|m| m.image_base == module)
            .map(|m| m.name.clone())
            .unwrap_or_default()
    };
    let name = std::path::Path::new(&name)
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_uppercase())
        .unwrap_or_default();
    format!("C:\\WINDOWS\\{name}").into_bytes()
}

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "GetTickCount"
        | "GetCurrentProcess"
        | "GetCurrentThread"
        | "GetCurrentProcessId"
        | "GetACP"
        | "GetOEMCP"
        | "GetProcessHeap" => 0,
        "Sleep"
        | "FreeLibrary"
        | "SetLastError"
        | "IsDBCSLeadByte"
        | "GlobalHandle"
        | "GlobalSize"
        | "LocalSize"
        | "GlobalAddAtomA"
        | "GlobalDeleteAtom"
        | "GlobalFindAtomA"
        | "AddAtomA"
        | "DeleteAtom"
        | "FindAtomA"
        | "LockResource"
        | "FreeResource"
        | "OutputDebugStringA"
        | "ResumeThread"
        | "InterlockedIncrement"
        | "InterlockedDecrement"
        | "CloseHandle"
        | "DeleteFileA"
        | "GetFileAttributesA"
        | "FindClose"
        | "_lclose"
        | "FlushFileBuffers"
        | "SetEndOfFile"
        | "LocalFree"
        | "GlobalFree"
        | "LocalLock"
        | "GlobalLock"
        | "LocalUnlock"
        | "GlobalUnlock"
        | "GlobalFlags"
        | "LocalFlags"
        | "LocalHandle"
        | "RemoveDirectoryA"
        | "GetModuleHandleA"
        | "GlobalCompact"
        | "LoadLibraryA"
        | "FreeLibraryAndExitThread"
        | "SetCurrentDirectoryA" => 1,
        "GetCurrentDirectoryA"
        | "GetTempPathA"
        | "LoadResource"
        | "SetFileAttributesA"
        | "MoveFileA"
        | "SetThreadPriority"
        | "IsBadReadPtr"
        | "IsBadWritePtr"
        | "IsBadStringPtrA"
        | "IsBadStringPtrW"
        | "FileTimeToSystemTime"
        | "SystemTimeToFileTime"
        | "FileTimeToLocalFileTime"
        | "LocalFileTimeToFileTime"
        | "GetFileSize"
        | "LocalAlloc"
        | "GlobalAlloc"
        | "FindFirstFileA"
        | "FindNextFileA"
        | "_lcreat"
        | "_lopen"
        | "SizeofResource"
        | "CreateDirectoryA"
        | "GetSystemTime"
        | "InterlockedExchange"
        | "CompareFileTime" => 2,
        "MulDiv" | "GetModuleFileNameA" | "GetShortPathNameA" | "FindResourceA"
        | "GlobalReAlloc" | "LocalReAlloc" | "RtlMoveMemory" | "RtlFillMemory"
        | "RtlZeroMemory" | "_lread" | "_lwrite" | "_llseek" | "GlobalGetAtomNameA"
        | "GetAtomNameA" | "CopyFileA" | "HeapAlloc" | "HeapFree" | "HeapSize" => match name {
            "RtlZeroMemory" | "HeapSize" => 2,
            _ => 3,
        },
        "SetFilePointer" | "GetFileTime" | "SetFileTime" | "GetFullPathNameA"
        | "GetTempFileNameA" | "HeapReAlloc" => 4,
        "LockFile" | "UnlockFile" | "ReadFile" | "WriteFile" | "GetDiskFreeSpaceA" => 5,
        "RaiseException" => 4,
        "SearchPathA" => 6,
        "DuplicateHandle" | "CreateFileA" => 7,
        "WideCharToMultiByte" => 8,
        _ => return None,
    })
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
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    match name {
        "GetTickCount" => ok(ctx.shared.borrow().ticks() as u32),
        "RaiseException" => {
            let count = args[2].min(15);
            let params: Vec<u32> = (0..count)
                .map(|i| memory.read_u32(args[3] + i * 4).unwrap_or(0))
                .collect();
            Some(crate::kernel::eh::raise(
                memory,
                args[0],
                args[1] & 1,
                &params,
                4,
            ))
        }
        "Sleep" => {
            std::thread::sleep(std::time::Duration::from_millis(args[0].min(50) as u64));
            ok(0)
        }
        "GetCurrentProcess" => ok(0xFFFF_FFFF),
        "GetCurrentThread" => ok(0xFFFF_FFFE),
        "GetCurrentProcessId" => ok(1),
        "GetACP" => ok(1252),
        "GetOEMCP" => ok(437),
        "GetProcessHeap" => ok(0x0050_0001),
        "ResumeThread" => ok(0),
        "SetThreadPriority" => ok(1),
        "FreeLibrary" | "FreeLibraryAndExitThread" => ok(1),
        "SetLastError" => {
            set_error(ctx, args[0]);
            ok(0)
        }
        "IsDBCSLeadByte" => ok(0),
        "OutputDebugStringA" => {
            let text = read_cstr(memory, args[0]);
            if std::env::var_os("OTW_DEBUG_STRINGS").is_some() {
                eprint!("[debug] {}", String::from_utf8_lossy(&text));
            }
            ok(0)
        }
        "IsBadReadPtr" | "IsBadWritePtr" => ok((args[0] == 0 && args[1] != 0) as u32),
        "IsBadStringPtrA" | "IsBadStringPtrW" => ok((args[0] == 0) as u32),
        "InterlockedIncrement" | "InterlockedDecrement" => {
            let value = memory.read_u32(args[0]).unwrap_or(0) as i32;
            let next = if name == "InterlockedIncrement" {
                value.wrapping_add(1)
            } else {
                value.wrapping_sub(1)
            };
            let _ = memory.write_u32(args[0], next as u32);
            ok(next as u32)
        }
        "InterlockedExchange" => {
            let previous = memory.read_u32(args[0]).unwrap_or(0);
            let _ = memory.write_u32(args[0], args[1]);
            ok(previous)
        }
        "MulDiv" => {
            let (a, b, c) = (
                args[0] as i32 as i64,
                args[1] as i32 as i64,
                args[2] as i32 as i64,
            );
            if c == 0 {
                return ok(u32::MAX);
            }
            let product = a * b;
            let half = c.abs() / 2;
            let result = if (product < 0) != (c < 0) {
                (product - if product < 0 { half } else { -half }) / c
            } else {
                (product + if product < 0 { -half } else { half }) / c
            };
            ok(result as i32 as u32)
        }
        "RtlMoveMemory" => {
            if let Some(bytes) = memory.read_bytes(args[1], args[2] as usize) {
                let _ = memory.write_bytes(args[0], &bytes);
            }
            ok(0)
        }
        "RtlFillMemory" => {
            let _ = memory.write_bytes(args[0], &vec![args[2] as u8; args[1] as usize]);
            ok(0)
        }
        "RtlZeroMemory" => {
            let _ = memory.write_bytes(args[0], &vec![0u8; args[1] as usize]);
            ok(0)
        }

        // ── Memory ──
        "LocalAlloc" | "GlobalAlloc" | "HeapAlloc" => {
            let (flags, size) = if name == "HeapAlloc" {
                (if args[1] & 0x8 != 0 { 0x40 } else { 0 }, args[2])
            } else {
                (args[0], args[1])
            };
            let address = ctx.shared.borrow_mut().heap.alloc(size.max(1)).unwrap_or(0);
            if address != 0 && flags & 0x40 != 0 {
                let _ = memory.write_bytes(address, &vec![0u8; size as usize]);
            }
            if address == 0 {
                set_error(ctx, 8);
            }
            ok(address)
        }
        "LocalLock" | "GlobalLock" | "GlobalHandle" | "LocalHandle" => {
            let address = if name.ends_with("Handle") {
                ctx.shared
                    .borrow()
                    .heap
                    .block_containing(args[0])
                    .unwrap_or(args[0])
            } else {
                args[0]
            };
            ok(address)
        }
        "LocalUnlock" | "GlobalUnlock" => ok(0),
        "LocalFlags" | "GlobalFlags" => ok(0),
        "GlobalCompact" => ok(0x0100_0000),
        "LocalFree" | "GlobalFree" | "HeapFree" => {
            let address = if name == "HeapFree" { args[2] } else { args[0] };
            if address != 0 {
                ctx.shared.borrow_mut().heap.free(address);
            }
            ok(if name == "HeapFree" { 1 } else { 0 })
        }
        "LocalSize" | "GlobalSize" | "HeapSize" => {
            let address = if name == "HeapSize" { args[1] } else { args[0] };
            ok(ctx.shared.borrow().heap.size_of(address).unwrap_or(0))
        }
        "LocalReAlloc" | "GlobalReAlloc" | "HeapReAlloc" => {
            let (handle, size, flags) = if name == "HeapReAlloc" {
                (args[2], args[3], if args[1] & 0x8 != 0 { 0x40 } else { 0 })
            } else {
                (args[0], args[1], args[2])
            };
            if flags & 0x80 != 0 {
                return ok(handle);
            }
            let old_size = ctx.shared.borrow().heap.size_of(handle).unwrap_or(0);
            if handle != 0 && old_size >= size && size > 0 {
                return ok(handle);
            }
            let address = ctx.shared.borrow_mut().heap.alloc(size.max(1)).unwrap_or(0);
            if address == 0 {
                return ok(0);
            }
            if flags & 0x40 != 0 {
                let _ = memory.write_bytes(address, &vec![0u8; size as usize]);
            }
            if handle != 0 {
                if let Some(bytes) = memory.read_bytes(handle, old_size.min(size) as usize) {
                    let _ = memory.write_bytes(address, &bytes);
                }
                ctx.shared.borrow_mut().heap.free(handle);
            }
            ok(address)
        }

        // ── Modules ──
        "GetModuleHandleA" => {
            if args[0] == 0 {
                return ok(ctx.image_base);
            }
            let requested =
                String::from_utf8_lossy(&read_cstr(memory, args[0])).to_ascii_lowercase();
            let requested_base = requested
                .trim_end_matches(".dll")
                .trim_end_matches(".exe")
                .to_string();
            let found = ctx.modules.iter().find(|m| {
                let n = m.name.to_ascii_lowercase();
                let n = std::path::Path::new(&n)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or(n);
                n == requested
                    || n.trim_end_matches(".dll").trim_end_matches(".exe") == requested_base
            });
            match found {
                Some(module) => ok(module.image_base),
                None => match system_module(&requested) {
                    Some(handle) => ok(handle),
                    None => {
                        set_error(ctx, 126);
                        ok(0)
                    }
                },
            }
        }
        "LoadLibraryA" => {
            let requested = String::from_utf8_lossy(&read_cstr(memory, args[0])).into_owned();
            let file = std::path::Path::new(&requested.replace('\\', "/"))
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or(requested.clone());
            if let Some(module) = ctx
                .modules
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case(&file))
            {
                return ok(module.image_base);
            }
            match system_module(&file) {
                Some(handle) => ok(handle),
                None => {
                    set_error(ctx, 126);
                    ok(0)
                }
            }
        }
        "GetModuleFileNameA" => {
            let path = module_path(ctx, args[0]);
            ok(crate::win32::copy_path(memory, args[1], args[2], &path)
                .min(args[2].saturating_sub(1)))
        }

        // ── Resources ──
        "FindResourceA" => {
            let name_id = user::res_name(memory, args[1]);
            let kind = user::res_name(memory, args[2]);
            match user::find_resource_any(ctx, memory, args[0], &kind, &name_id) {
                Some((va, size)) => {
                    ctx.shared.borrow_mut().resource_sizes.insert(va, size);
                    ok(va)
                }
                None => {
                    set_error(ctx, 1814);
                    ok(0)
                }
            }
        }
        "LoadResource" => ok(args[1]),
        "LockResource" => ok(args[0]),
        "FreeResource" => ok(0),
        "SizeofResource" => ok(ctx
            .shared
            .borrow()
            .resource_sizes
            .get(&args[1])
            .copied()
            .unwrap_or(0)),

        // ── Atoms ──
        "GlobalAddAtomA" | "AddAtomA" | "GlobalFindAtomA" | "FindAtomA" => {
            if args[0] < 0x10000 {
                return ok(args[0]);
            }
            let text = String::from_utf8_lossy(&read_cstr(memory, args[0])).into_owned();
            if let Some(number) = text.strip_prefix('#').and_then(|n| n.parse::<u32>().ok()) {
                return ok(number);
            }
            let mut shared = ctx.shared.borrow_mut();
            let position = shared
                .global_atoms
                .iter()
                .position(|a| a.eq_ignore_ascii_case(&text));
            match position {
                Some(index) => ok(0xC000 + index as u32),
                None if name.contains("Find") => ok(0),
                None => {
                    shared.global_atoms.push(text);
                    ok(0xC000 + shared.global_atoms.len() as u32 - 1)
                }
            }
        }
        "GlobalGetAtomNameA" | "GetAtomNameA" => {
            let atom = args[0];
            let text = if atom < 0xC000 {
                Some(format!("#{atom}"))
            } else {
                ctx.shared
                    .borrow()
                    .global_atoms
                    .get((atom - 0xC000) as usize)
                    .cloned()
            };
            match text {
                Some(text) => {
                    let mut bytes = text.into_bytes();
                    bytes.truncate(args[2].saturating_sub(1) as usize);
                    let _ = write_cstr(memory, args[1], &bytes);
                    ok(bytes.len() as u32)
                }
                None => ok(0),
            }
        }
        "GlobalDeleteAtom" | "DeleteAtom" => ok(0),

        // ── Files ──
        "CreateFileA" => {
            let path = read_cstr(memory, args[0]);
            let access = args[1];
            let disposition = args[4];
            let read = access & 0x8000_0000 != 0 || access == 0;
            let write = access & 0x4000_0000 != 0;
            let (create, truncate, must_not_exist) = match disposition {
                1 => (true, false, true),
                2 => (true, true, false),
                3 => (false, false, false),
                4 => (true, false, false),
                5 => (false, true, false),
                _ => (false, false, false),
            };
            let host = crt::host_path(&path);
            if !create && host.as_ref().map(|h| !h.exists()).unwrap_or(true) {
                set_error(ctx, 2);
                return ok(INVALID);
            }
            match crt::open_file(
                ctx,
                &path,
                read,
                write || create || truncate,
                create,
                truncate,
                must_not_exist,
            ) {
                Ok(handle) => {
                    ctx.shared.borrow_mut().file_paths.insert(handle, path);
                    set_error(ctx, 0);
                    ok(handle)
                }
                Err(code) => {
                    set_error(ctx, code);
                    ok(INVALID)
                }
            }
        }
        "_lopen" | "_lcreat" => {
            let path = read_cstr(memory, args[0]);
            let result = if name == "_lcreat" {
                crt::open_file(ctx, &path, true, true, true, true, false)
            } else {
                let mode = args[1] & 3;
                crt::open_file(ctx, &path, mode != 1, mode != 0, false, false, false)
            };
            match result {
                Ok(handle) => {
                    ctx.shared.borrow_mut().file_paths.insert(handle, path);
                    ok(handle)
                }
                Err(code) => {
                    set_error(ctx, code);
                    ok(INVALID)
                }
            }
        }
        "CloseHandle" | "_lclose" => {
            ctx.shared.borrow_mut().file_paths.remove(&args[0]);
            let closed = crt::close_file(ctx, args[0]);
            ok(if name == "_lclose" {
                if closed {
                    0
                } else {
                    INVALID
                }
            } else {
                1
            })
        }
        "ReadFile" | "_lread" => {
            let bytes = crt::read_file(ctx, args[0], args[2] as usize);
            match bytes {
                Some(bytes) => {
                    let _ = memory.write_bytes(args[1], &bytes);
                    if name == "ReadFile" {
                        if args[3] != 0 {
                            let _ = memory.write_u32(args[3], bytes.len() as u32);
                        }
                        ok(1)
                    } else {
                        ok(bytes.len() as u32)
                    }
                }
                None => {
                    set_error(ctx, 6);
                    ok(if name == "ReadFile" { 0 } else { INVALID })
                }
            }
        }
        "WriteFile" | "_lwrite" => {
            let Some(bytes) = memory.read_bytes(args[1], args[2] as usize) else {
                return ok(0);
            };
            let written = if (args[0] as i32) <= -10 && (args[0] as i32) >= -12 {
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(&bytes);
                let _ = stdout.flush();
                Some(bytes.len())
            } else {
                crt::write_file(ctx, args[0], &bytes)
            };
            match written {
                Some(count) => {
                    if name == "WriteFile" {
                        if args[3] != 0 {
                            let _ = memory.write_u32(args[3], count as u32);
                        }
                        ok(1)
                    } else {
                        ok(count as u32)
                    }
                }
                None => {
                    set_error(ctx, 6);
                    ok(if name == "WriteFile" { 0 } else { INVALID })
                }
            }
        }
        "SetFilePointer" | "_llseek" => {
            let (offset, method) = if name == "_llseek" {
                (args[1] as i32 as i64, args[2])
            } else {
                let high = if args[2] != 0 {
                    memory.read_u32(args[2]).unwrap_or(0) as i32 as i64
                } else {
                    if (args[1] as i32) < 0 {
                        -1
                    } else {
                        0
                    }
                };
                ((high << 32) | args[1] as i64, args[3])
            };
            match crt::seek_file(ctx, args[0], offset, method) {
                Some(position) => {
                    if name == "SetFilePointer" && args[2] != 0 {
                        let _ = memory.write_u32(args[2], (position >> 32) as u32);
                    }
                    ok(position as u32)
                }
                None => {
                    set_error(ctx, 6);
                    ok(INVALID)
                }
            }
        }
        "GetFileSize" => match crt::file_size(ctx, args[0]) {
            Some(size) => {
                if args[1] != 0 {
                    let _ = memory.write_u32(args[1], (size >> 32) as u32);
                }
                ok(size as u32)
            }
            None => ok(INVALID),
        },
        "SetEndOfFile" => {
            let position = crt::seek_file(ctx, args[0], 0, 1);
            let done = {
                let shared = ctx.shared.borrow();
                match (shared.files.get(&args[0]), position) {
                    (Some(file), Some(position)) => file.set_len(position).is_ok(),
                    _ => false,
                }
            };
            ok(done as u32)
        }
        "FlushFileBuffers" | "LockFile" | "UnlockFile" => ok(1),
        "GetFileTime" | "SetFileTime" => {
            if name == "SetFileTime" {
                return ok(1);
            }
            let metadata = ctx
                .shared
                .borrow()
                .files
                .get(&args[0])
                .and_then(|f| f.metadata().ok());
            let Some(metadata) = metadata else {
                return ok(0);
            };
            let (created, accessed, written) = metadata_times(&metadata);
            write_filetime(memory, args[1], created);
            write_filetime(memory, args[2], accessed);
            write_filetime(memory, args[3], written);
            ok(1)
        }
        "DuplicateHandle" => {
            let duplicate = {
                let mut shared = ctx.shared.borrow_mut();
                let clone = shared.files.get(&args[1]).and_then(|f| f.try_clone().ok());
                match clone {
                    Some(file) => {
                        let handle = shared.alloc_handle() | 0x0070_0000;
                        shared.files.insert(handle, file);
                        handle
                    }
                    None => args[1],
                }
            };
            if args[3] != 0 {
                let _ = memory.write_u32(args[3], duplicate);
            }
            ok(1)
        }
        "GetFileAttributesA" => {
            let path = read_cstr(memory, args[0]);
            match crt::host_path(&path).and_then(|p| std::fs::metadata(p).ok()) {
                Some(metadata) => ok(if metadata.is_dir() { 0x10 } else { 0x20 }),
                None => {
                    set_error(ctx, 2);
                    ok(INVALID)
                }
            }
        }
        "SetFileAttributesA" => ok(1),
        "DeleteFileA" => {
            let path = read_cstr(memory, args[0]);
            let removed = crt::host_path(&path)
                .map(|p| std::fs::remove_file(p).is_ok())
                .unwrap_or(false);
            if !removed {
                set_error(ctx, 2);
            }
            ok(removed as u32)
        }
        "MoveFileA" | "CopyFileA" => {
            let from = read_cstr(memory, args[0]);
            let to = read_cstr(memory, args[1]);
            let done = match (crt::host_path(&from), crt::host_path(&to)) {
                (Some(from), Some(to)) => {
                    if name == "MoveFileA" {
                        std::fs::rename(from, to).is_ok()
                    } else {
                        if args[2] != 0 && to.exists() {
                            false
                        } else {
                            std::fs::copy(from, to).is_ok()
                        }
                    }
                }
                _ => false,
            };
            ok(done as u32)
        }
        "CreateDirectoryA" | "RemoveDirectoryA" => {
            let path = read_cstr(memory, args[0]);
            let done = crt::host_path(&path)
                .map(|p| {
                    if name == "CreateDirectoryA" {
                        std::fs::create_dir(p).is_ok()
                    } else {
                        std::fs::remove_dir(p).is_ok()
                    }
                })
                .unwrap_or(false);
            ok(done as u32)
        }
        "FindFirstFileA" | "FindNextFileA" => {
            let (pattern_dir, pattern, index) = if name == "FindFirstFileA" {
                let path = read_cstr(memory, args[0]);
                let split = path.iter().rposition(|b| *b == b'\\' || *b == b'/');
                let (dir, pattern) = match split {
                    Some(p) => (path[..p].to_vec(), path[p + 1..].to_vec()),
                    None => (Vec::new(), path.clone()),
                };
                (dir, pattern, 0usize)
            } else {
                let search = ctx.shared.borrow().finds.get(&args[0]).cloned();
                match search {
                    Some((dir, pattern, index)) => (dir, pattern, index),
                    None => return ok(0),
                }
            };
            let host_dir = crt::host_path(if pattern_dir.is_empty() {
                b"\\"
            } else {
                &pattern_dir
            });
            let mut entries: Vec<(Vec<u8>, std::fs::Metadata)> = host_dir
                .and_then(|d| std::fs::read_dir(d).ok())
                .map(|it| {
                    it.filter_map(Result::ok)
                        .filter_map(|e| {
                            Some((
                                e.file_name().to_string_lossy().as_bytes().to_vec(),
                                e.metadata().ok()?,
                            ))
                        })
                        .filter(|(n, _)| wildcard_match(&pattern, n))
                        .collect()
                })
                .unwrap_or_default();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let data_va = if name == "FindFirstFileA" {
                args[1]
            } else {
                args[1]
            };
            match entries.get(index) {
                Some((entry_name, metadata)) => {
                    write_find_data(memory, data_va, entry_name, metadata);
                    if name == "FindFirstFileA" {
                        let mut shared = ctx.shared.borrow_mut();
                        let handle = shared.alloc_handle() | 0x0071_0000;
                        shared.finds.insert(handle, (pattern_dir, pattern, 1));
                        ok(handle)
                    } else {
                        ctx.shared
                            .borrow_mut()
                            .finds
                            .insert(args[0], (pattern_dir, pattern, index + 1));
                        ok(1)
                    }
                }
                None => {
                    set_error(ctx, if name == "FindFirstFileA" { 2 } else { 18 });
                    ok(if name == "FindFirstFileA" { INVALID } else { 0 })
                }
            }
        }
        "FindClose" => {
            ctx.shared.borrow_mut().finds.remove(&args[0]);
            ok(1)
        }
        "GetCurrentDirectoryA" => ok(crate::win32::copy_path(memory, args[1], args[0], b"C:\\")),
        "SetCurrentDirectoryA" => ok(1),
        "GetTempPathA" => {
            if let Some(path) = crt::host_path(b"C:\\TEMP") {
                let _ = std::fs::create_dir_all(path);
            }
            ok(crate::win32::copy_path(
                memory,
                args[1],
                args[0],
                b"C:\\TEMP\\",
            ))
        }
        "GetTempFileNameA" => {
            let mut directory = read_cstr(memory, args[0]);
            if !directory.ends_with(b"\\") {
                directory.push(b'\\');
            }
            let mut prefix = read_cstr(memory, args[1]);
            prefix.truncate(3);
            let mut unique = args[2];
            if unique == 0 {
                unique = (ctx.shared.borrow().ticks() as u32 & 0xFFFF).max(1);
            }
            let mut path = directory.clone();
            path.extend_from_slice(&prefix);
            path.extend_from_slice(format!("{unique:04X}.TMP").as_bytes());
            if args[2] == 0 {
                if let Some(host) = crt::host_path(&path) {
                    let _ = std::fs::File::create(host);
                }
            }
            let _ = write_cstr(memory, args[3], &path);
            ok(unique)
        }
        "GetFullPathNameA" => {
            let path = read_cstr(memory, args[0]);
            let full = if path.len() >= 2 && path[1] == b':' {
                path.clone()
            } else if path.first() == Some(&b'\\') {
                [b"C:".as_slice(), &path].concat()
            } else {
                [b"C:\\".as_slice(), &path].concat()
            };
            let length = crate::win32::copy_path(memory, args[2], args[1], &full);
            if args[3] != 0 && args[2] != 0 && length < args[1] {
                let file_part = full
                    .iter()
                    .rposition(|b| *b == b'\\')
                    .map(|p| p + 1)
                    .unwrap_or(0);
                let _ = memory.write_u32(args[3], args[2] + file_part as u32);
            }
            ok(length)
        }
        "GetShortPathNameA" => {
            let path = read_cstr(memory, args[0]);
            ok(crate::win32::copy_path(memory, args[1], args[2], &path))
        }
        "SearchPathA" => {
            let file = read_cstr(memory, args[1]);
            let mut extended = file.clone();
            if args[2] != 0 && !file.contains(&b'.') {
                extended.extend_from_slice(&read_cstr(memory, args[2]));
            }
            for directory in [
                b"C:\\".as_slice(),
                b"C:\\WINDOWS\\",
                b"C:\\WINDOWS\\SYSTEM\\",
            ] {
                let candidate = [directory, extended.as_slice()].concat();
                if crt::host_path(&candidate)
                    .map(|p| p.exists())
                    .unwrap_or(false)
                {
                    return ok(crate::win32::copy_path(
                        memory, args[4], args[3], &candidate,
                    ));
                }
            }
            set_error(ctx, 2);
            ok(0)
        }
        "GetDiskFreeSpaceA" => {
            for (index, value) in [8u32, 512, 200_000, 250_000].iter().enumerate() {
                if args[index + 1] != 0 {
                    let _ = memory.write_u32(args[index + 1], *value);
                }
            }
            ok(1)
        }

        // ── Time ──
        "FileTimeToLocalFileTime" | "LocalFileTimeToFileTime" => {
            let value = read_filetime(memory, args[0]);
            write_filetime(memory, args[1], value);
            ok(1)
        }
        "CompareFileTime" => {
            let a = read_filetime(memory, args[0]);
            let b = read_filetime(memory, args[1]);
            ok(match a.cmp(&b) {
                std::cmp::Ordering::Less => u32::MAX,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            })
        }
        "FileTimeToSystemTime" => {
            let value = read_filetime(memory, args[0]);
            let secs = (value / 10_000_000) as i64 - 11_644_473_600;
            let millis = ((value / 10_000) % 1000) as u16;
            let days = secs.div_euclid(86400);
            let rem = secs.rem_euclid(86400);
            // Reuse the CRT calendar conversion via localtime-style fields.
            let z = days + 719468;
            let era = z.div_euclid(146097);
            let doe = z - era * 146097;
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let day = doy - (153 * mp + 2) / 5 + 1;
            let month = if mp < 10 { mp + 3 } else { mp - 9 };
            let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
            let weekday = (days + 4).rem_euclid(7);
            let fields: [u16; 8] = [
                year as u16,
                month as u16,
                weekday as u16,
                day as u16,
                (rem / 3600) as u16,
                ((rem % 3600) / 60) as u16,
                (rem % 60) as u16,
                millis,
            ];
            let bytes: Vec<u8> = fields.iter().flat_map(|f| f.to_le_bytes()).collect();
            let _ = memory.write_bytes(args[1], &bytes);
            ok(1)
        }
        "SystemTimeToFileTime" => {
            let bytes = memory
                .read_bytes(args[0], 16)
                .unwrap_or_else(|| vec![0; 16]);
            let f = |i: usize| u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]) as i64;
            let (year, month, day) = (f(0), f(1), f(3));
            let y = if month <= 2 { year - 1 } else { year };
            let era = y.div_euclid(400);
            let yoe = y - era * 400;
            let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
            let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
            let days = era * 146097 + doe - 719468;
            let secs = days * 86400 + f(4) * 3600 + f(5) * 60 + f(6);
            let value = ((secs + 11_644_473_600) as u64) * 10_000_000 + f(7) as u64 * 10_000;
            write_filetime(memory, args[1], value);
            ok(1)
        }
        "GetSystemTime" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let filetime = filetime_from_unix(now.as_secs(), now.subsec_nanos());
            let scratch = user::scratch(ctx, 8);
            write_filetime(memory, scratch, filetime);
            let result = dispatch(ctx, memory, "FileTimeToSystemTime", &[scratch, args[0]]);
            user::free_scratch(ctx, scratch);
            result.map(|_| ApiOutcome::new(0, count))
        }

        // ── Strings ──
        "WideCharToMultiByte" => {
            let length = args[3] as i32;
            let units: Vec<u16> = if length < 0 {
                let mut units = Vec::new();
                let mut offset = 0;
                while let Some(bytes) = memory.read_bytes(args[2] + offset, 2) {
                    let unit = u16::from_le_bytes([bytes[0], bytes[1]]);
                    units.push(unit);
                    offset += 2;
                    if unit == 0 || units.len() > 1 << 20 {
                        break;
                    }
                }
                units
            } else {
                memory
                    .read_bytes(args[2], length as usize * 2)
                    .unwrap_or_default()
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect()
            };
            let bytes: Vec<u8> = units
                .iter()
                .map(|u| if *u <= 0xFF { *u as u8 } else { b'?' })
                .collect();
            if args[5] == 0 {
                return ok(bytes.len() as u32);
            }
            let copied = bytes.len().min(args[5] as usize);
            let _ = memory.write_bytes(args[4], &bytes[..copied]);
            ok(copied as u32)
        }
        _ => {
            let _ = ResName::Id(0);
            None
        }
    }
}
