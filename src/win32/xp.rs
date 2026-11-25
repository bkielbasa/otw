//! KERNEL32 functions from later Windows (NT 4, 2000, XP) that programs
//! built for them import, mostly through their C runtime: timing, the
//! unhandled-exception filter, pointer encoding, code-page information and
//! console stubs.

use crate::{
    kernel::eh,
    win32::{ApiContext, ApiOutcome, GuestMemory},
};

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "GetUserDefaultUILanguage" | "GetSystemDefaultUILanguage" | "IsDebuggerPresent" | "GetConsoleCP"
        | "GetConsoleOutputCP" | "GetUserDefaultLangID" | "GetSystemDefaultLangID" | "GetUserDefaultLCID"
        | "GetSystemDefaultLCID" | "GetLogicalDrives" | "GetEnvironmentStrings" | "GetEnvironmentStringsA" => 0,
        "HeapDestroy" | "SetHandleCount" | "SetEvent" | "ResetEvent" | "PulseEvent" | "SetThreadExecutionState"
        | "UnmapViewOfFile" | "SetDllDirectoryW" | "SetDllDirectoryA" | "FreeEnvironmentStringsA" => 1,
        "IsValidLocale" | "EnumSystemLocalesA" | "EnumSystemLocalesW" | "InterlockedExchangeAdd" => 2,
        "HeapCreate" | "VirtualFree" | "SystemTimeToTzSpecificLocalTime" | "GetProcessAffinityMask" => 3,
        "VirtualAlloc" | "GetDiskFreeSpaceExW" | "GetDiskFreeSpaceExA" | "CreateEventW" | "VirtualQuery" => 4,
        "MapViewOfFile" => 5,
        "CreateFileMappingA" | "CreateFileMappingW" | "CreateThread" | "GetNumberFormatW" | "GetNumberFormatA" => 6,
        "CreateProcessW" | "CreateProcessA" => 10,
        "GetSystemTimeAsFileTime" | "IsProcessorFeaturePresent" | "QueryPerformanceCounter"
        | "QueryPerformanceFrequency" | "UnhandledExceptionFilter" | "SetUnhandledExceptionFilter"
        | "IsValidCodePage" | "GetTimeZoneInformation" | "EncodePointer" | "DecodePointer" | "GetFileType" => 1,
        "GetCPInfo" | "GetConsoleMode" | "SetStdHandle" | "TerminateProcess" | "InitializeCriticalSectionAndSpinCount" => 2,
        "DosDateTimeToFileTime" => 3,
        "RtlUnwind" | "VirtualProtect" => 4,
        "HeapQueryInformation" | "ReadConsoleW" | "WriteConsoleW" | "SetFilePointerEx" => 5,
        _ => return None,
    })
}

/// FILETIME (100 ns since 1601) of a Unix time.
fn filetime(duration: std::time::Duration) -> u64 {
    duration.as_secs() * 10_000_000 + duration.subsec_nanos() as u64 / 100 + 116_444_736_000_000_000
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    let write_u64 = |memory: &mut M, va: u32, value: u64| {
        let _ = memory.write_bytes(va, &value.to_le_bytes());
    };
    match name {
        "GetSystemTimeAsFileTime" => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            write_u64(memory, args[0], filetime(now));
            ok(0)
        }
        "QueryPerformanceCounter" => {
            let micros = ctx.shared.borrow().started.elapsed().as_micros() as u64;
            write_u64(memory, args[0], micros);
            ok(1)
        }
        "QueryPerformanceFrequency" => {
            write_u64(memory, args[0], 1_000_000);
            ok(1)
        }
        "IsDebuggerPresent" => ok(0),
        // The SSE2 and other feature probes: report none, so runtimes take
        // their plain x86 paths.
        "IsProcessorFeaturePresent" => ok(0),
        "UnhandledExceptionFilter" => ok(1), // EXCEPTION_EXECUTE_HANDLER: terminate
        "SetUnhandledExceptionFilter" => {
            let mut shared = ctx.shared.borrow_mut();
            ok(std::mem::replace(&mut shared.unhandled_filter, args[0]))
        }
        "TerminateProcess" => {
            if args[0] == u32::MAX {
                Some(ApiOutcome { wide: None, value: 1, arg_count: count, exit_code: Some(args[1]) })
            } else {
                ok(0)
            }
        }
        "IsValidCodePage" => ok(u32::from(matches!(args[0], 437 | 850 | 1252 | 20127 | 28591 | 65001))),
        "GetCPInfo" => {
            // CPINFO: MaxCharSize 1, DefaultChar '?', no lead bytes.
            let mut info = vec![0u8; 20];
            info[0] = 1;
            info[4] = b'?';
            let _ = memory.write_bytes(args[1], &info);
            ok(1)
        }
        "GetConsoleMode" | "ReadConsoleW" | "WriteConsoleW" => ok(0),
        "GetConsoleCP" | "GetConsoleOutputCP" => ok(437),
        "SetStdHandle" => ok(1),
        "GetUserDefaultUILanguage" | "GetSystemDefaultUILanguage" | "GetUserDefaultLangID" | "GetSystemDefaultLangID" => {
            ok(0x0409)
        }
        "GetTimeZoneInformation" => {
            let _ = memory.write_bytes(args[0], &[0u8; 172]);
            ok(0) // TIME_ZONE_ID_UNKNOWN
        }
        "DosDateTimeToFileTime" => {
            let (date, time) = (args[0] & 0xFFFF, args[1] & 0xFFFF);
            let (year, month, day) = (1980 + (date >> 9) as i64, ((date >> 5) & 15) as i64, (date & 31) as i64);
            let (hour, minute, second) = ((time >> 11) as i64, ((time >> 5) & 63) as i64, ((time & 31) * 2) as i64);
            // Days from 1970-01-01 (civil-from-days inverse).
            let (y, m) = if month <= 2 { (year - 1, month + 9) } else { (year, month - 3) };
            let era = y.div_euclid(400);
            let yoe = y - era * 400;
            let doy = (153 * m + 2) / 5 + day - 1;
            let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
            let days = era * 146097 + doe - 719468;
            let secs = days * 86400 + hour * 3600 + minute * 60 + second;
            write_u64(memory, args[2], filetime(std::time::Duration::from_secs(secs.max(0) as u64)));
            ok(1)
        }
        "VirtualProtect" => {
            let _ = memory.write_u32(args[3], 0x40); // was PAGE_EXECUTE_READWRITE
            ok(1)
        }
        "EncodePointer" | "DecodePointer" => ok(args[0]),
        "InitializeCriticalSectionAndSpinCount" => ok(1),
        "HeapQueryInformation" => ok(0),
        "GetFileType" => {
            // FILE_TYPE_CHAR for the standard handles, FILE_TYPE_DISK otherwise.
            let standard = matches!(args[0] as i32, -10 | -11 | -12);
            ok(if standard { 2 } else { 1 })
        }
        "SetFilePointerEx" => {
            // SetFilePointerEx(file, distance low, distance high, new position*, method)
            let high = ctx.shared.borrow_mut().heap.alloc(4).unwrap_or(0);
            let _ = memory.write_u32(high, args[2]);
            let low = crate::win32::dispatch_with_memory(
                ctx,
                memory,
                "KERNEL32.dll",
                &crate::kernel::pe::ImportName::ByName("SetFilePointer".into()),
                &[args[0], args[1], high, args[4]],
                0,
            )
            .map(|o| o.value)
            .unwrap_or(u32::MAX);
            let high_value = memory.read_u32(high).unwrap_or(0);
            ctx.shared.borrow_mut().heap.free(high);
            if args[3] != 0 {
                let _ = memory.write_u32(args[3], low);
                let _ = memory.write_u32(args[3] + 4, high_value);
            }
            ok(u32::from(low != u32::MAX))
        }
        "HeapCreate" => ok(0x0050_0001 + (ctx.shared.borrow_mut().alloc_handle() << 4)),
        "HeapDestroy" => ok(1),
        "VirtualAlloc" => {
            // VirtualAlloc(address, size, type, protect): reserving and
            // committing are one step; committing inside a region is a no-op.
            let (address, size) = (args[0], args[1]);
            let mut shared = ctx.shared.borrow_mut();
            if address != 0 {
                let inside = shared
                    .virtual_allocs
                    .iter()
                    .find(|(start, (_, length))| address >= **start && address < **start + *length)
                    .map(|(start, _)| *start);
                if inside.is_some() {
                    return ok(address & !0xFFF);
                }
            }
            let size = (size.max(1) + 0xFFF) & !0xFFF;
            let Some(block) = shared.heap.alloc(size + 0x1000) else {
                return ok(0);
            };
            let start = (block + 0xFFF) & !0xFFF;
            shared.virtual_allocs.insert(start, (block, size));
            drop(shared);
            let _ = memory.write_bytes(start, &vec![0u8; size as usize]);
            ok(start)
        }
        "VirtualFree" => {
            // MEM_RELEASE frees; MEM_DECOMMIT keeps the region.
            if args[2] & 0x8000 != 0 {
                let mut shared = ctx.shared.borrow_mut();
                if let Some((block, _)) = shared.virtual_allocs.remove(&args[0]) {
                    shared.heap.free(block);
                }
            }
            ok(1)
        }
        "VirtualQuery" => {
            // MEMORY_BASIC_INFORMATION for a committed read/write region.
            let region = ctx
                .shared
                .borrow()
                .virtual_allocs
                .iter()
                .find(|(start, (_, length))| args[0] >= **start && args[0] < **start + *length)
                .map(|(start, (_, length))| (*start, *length));
            let (base, size) = region.unwrap_or((args[0] & !0xFFF, 0x1000));
            let info = [base, base, 4, size, 0x1000, 4, 0x20000];
            let bytes: Vec<u8> = info.iter().flat_map(|v| v.to_le_bytes()).collect();
            let _ = memory.write_bytes(args[1], &bytes[..(args[2] as usize).min(28)]);
            ok(28)
        }
        "CreateFileMappingA" | "CreateFileMappingW" => {
            // CreateFileMapping(file, attributes, protect, size high, size low, name)
            let file = if args[0] == u32::MAX { 0 } else { args[0] };
            let size = if args[4] != 0 || file == 0 {
                args[4]
            } else {
                let shared = ctx.shared.borrow();
                shared.files.get(&file).and_then(|f| f.metadata().ok()).map(|m| m.len() as u32).unwrap_or(0)
            };
            let mut shared = ctx.shared.borrow_mut();
            let handle = shared.alloc_handle();
            shared.file_mappings.insert(handle, (file, size));
            ok(handle)
        }
        "MapViewOfFile" => {
            // MapViewOfFile(mapping, access, offset high, offset low, bytes)
            let Some((file, size)) = ctx.shared.borrow().file_mappings.get(&args[0]).copied() else {
                return ok(0);
            };
            let offset = args[3];
            let length = if args[4] != 0 { args[4] } else { size.saturating_sub(offset) };
            let Some(view) = ctx.shared.borrow_mut().heap.alloc(length.max(1)) else {
                return ok(0);
            };
            let mut contents = vec![0u8; length as usize];
            if file != 0 {
                use std::io::{Read, Seek, SeekFrom};
                let mut shared = ctx.shared.borrow_mut();
                if let Some(handle) = shared.files.get_mut(&file) {
                    let position = handle.stream_position().unwrap_or(0);
                    if handle.seek(SeekFrom::Start(offset as u64)).is_ok() {
                        let mut read = 0;
                        while read < contents.len() {
                            match handle.read(&mut contents[read..]) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => read += n,
                            }
                        }
                    }
                    let _ = handle.seek(SeekFrom::Start(position));
                }
            }
            let _ = memory.write_bytes(view, &contents);
            ctx.shared.borrow_mut().mapped_views.insert(view, (args[0], offset, length));
            ok(view)
        }
        "UnmapViewOfFile" => {
            let mut shared = ctx.shared.borrow_mut();
            let removed = shared.mapped_views.remove(&args[0]).is_some();
            if removed {
                shared.heap.free(args[0]);
            }
            ok(u32::from(removed))
        }
        "GetDiskFreeSpaceExW" | "GetDiskFreeSpaceExA" => {
            let (free, total) = (8u64 << 30, 16u64 << 30);
            for (index, value) in [(1, free), (2, total), (3, free)] {
                if args[index] != 0 {
                    write_u64(memory, args[index], value);
                }
            }
            ok(1)
        }
        "GetProcessAffinityMask" => {
            let _ = memory.write_u32(args[1], 1);
            let _ = memory.write_u32(args[2], 1);
            ok(1)
        }
        "GetUserDefaultLCID" | "GetSystemDefaultLCID" => ok(0x0409),
        "IsValidLocale" => ok(u32::from(args[0] & 0x3FF == 0x09)),
        "EnumSystemLocalesA" | "EnumSystemLocalesW" => ok(1),
        "GetLogicalDrives" => ok(1 << 2 | u32::from(crate::win32::crt::drive_d_root().is_some()) << 3), // C: (D:)
        "SetHandleCount" => ok(args[0]),
        "SetDllDirectoryW" | "SetDllDirectoryA" => ok(1),
        "SetThreadExecutionState" => ok(0x8000_0000), // ES_CONTINUOUS
        "InterlockedExchangeAdd" => {
            let old = memory.read_u32(args[0]).unwrap_or(0);
            let _ = memory.write_u32(args[0], old.wrapping_add(args[1]));
            ok(old)
        }
        "SystemTimeToTzSpecificLocalTime" => {
            let time = memory.read_bytes(args[1], 16).unwrap_or_else(|| vec![0; 16]);
            let _ = memory.write_bytes(args[2], &time);
            ok(1)
        }
        "GetEnvironmentStrings" | "GetEnvironmentStringsA" => {
            // An empty block: two NULs.
            let block = ctx.shared.borrow_mut().heap.alloc(2).unwrap_or(0);
            let _ = memory.write_bytes(block, &[0, 0]);
            ok(block)
        }
        "FreeEnvironmentStringsA" => {
            ctx.shared.borrow_mut().heap.free(args[0]);
            ok(1)
        }
        // Events: there is one thread, so nothing ever waits on them.
        "CreateEventW" => ok(ctx.shared.borrow_mut().alloc_handle()),
        "SetEvent" | "ResetEvent" | "PulseEvent" => ok(1),
        // No threads or child processes yet: creating one fails.
        "CreateThread" | "CreateProcessW" | "CreateProcessA" => {
            ctx.shared.borrow_mut().last_error = 8; // ERROR_NOT_ENOUGH_MEMORY
            ok(0)
        }
        "GetNumberFormatW" | "GetNumberFormatA" => {
            // GetNumberFormat(locale, flags, value, format, buffer, size): the
            // value as given.
            let wide = name.ends_with('W');
            let value = if wide {
                crate::win32::unicode::wide_to_ansi(&crate::win32::unicode::read_wstr(memory, args[2]))
            } else {
                crate::win32::read_cstr(memory, args[2])
            };
            let needed = value.len() as u32 + 1;
            if args[5] == 0 {
                return ok(needed);
            }
            if args[5] < needed {
                ctx.shared.borrow_mut().last_error = 122;
                return ok(0);
            }
            if wide {
                crate::win32::unicode::write_wide(memory, args[4], &crate::win32::unicode::ansi_to_wide(&value), true);
            } else {
                let _ = crate::win32::write_cstr(memory, args[4], &value);
            }
            ok(needed)
        }
        "RtlUnwind" => {
            // RtlUnwind(target frame, target IP, record, return value): run
            // the handlers above the target frame, then return.
            let record = if args[2] != 0 {
                args[2]
            } else {
                let block = ctx.shared.borrow_mut().heap.alloc(eh::RECORD_SIZE).unwrap_or(0);
                let _ = memory.write_bytes(block, &vec![0u8; eh::RECORD_SIZE as usize]);
                let _ = memory.write_u32(block, 0xC000_0027); // STATUS_UNWIND
                block
            };
            let context = ctx.shared.borrow_mut().heap.alloc(eh::CONTEXT_SIZE).unwrap_or(0);
            let _ = memory.write_bytes(context, &vec![0u8; eh::CONTEXT_SIZE as usize]);
            eh::global_unwind(memory, args[0], record, context);
            let mut shared = ctx.shared.borrow_mut();
            shared.heap.free(context);
            if args[2] == 0 {
                shared.heap.free(record);
            }
            ok(args[3])
        }
        _ => None,
    }
}
