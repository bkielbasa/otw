//! Functions 64-bit programs for Windows 7 and later call that Windows 95
//! never had: the native API (`ntdll`) entry points programs use directly,
//! and the KERNEL32 additions of Vista and later (completion ports, job
//! objects, thread-pool timers, process attribute lists, ...).
//!
//! Arguments arrive in full width. Most map onto what the subsystem
//! already has; services OTW does not provide (job objects, other
//! processes' memory, file mapping through the native API) report failure
//! the way Windows does, so programs take their fallback paths.

use std::collections::{HashMap, VecDeque};

use crate::{
    kernel::pe::ImportName,
    win32::{unicode, vmem, ApiContext, ApiOutcome, GuestMemory},
};

const STATUS_SUCCESS: u64 = 0;
const STATUS_NOT_IMPLEMENTED: u64 = 0xC000_0002;
const STATUS_INVALID_INFO_CLASS: u64 = 0xC000_0003;

#[derive(Debug, Default)]
pub struct State {
    /// Completion ports: queued (bytes, key, overlapped) packets.
    pub ports: HashMap<u32, VecDeque<(u32, u64, u64)>>,
    /// Vectored exception handlers, first to call first.
    pub vectored: Vec<(u32, u64)>,
    next_vectored: u32,
    /// Function tables added for generated code: (base, table, count).
    pub function_tables: Vec<(u64, u64, u32)>,
    /// The COM apartment of each thread that joined one: 0 single-threaded,
    /// 1 multithreaded.
    pub apartments: HashMap<u32, u32>,
    /// Each module's delay-loaded imports, (dll in lower case, name).
    pub delay_imports: HashMap<u32, std::collections::HashSet<(String, String)>>,
}

/// The delay-load imports a mapped module declares.
fn read_delay_imports<M: GuestMemory + ?Sized>(memory: &mut M, base: u32) -> std::collections::HashSet<(String, String)> {
    let mut found = std::collections::HashSet::new();
    let Some(pe) = memory.read_u32(base + 0x3C) else { return found };
    let x64 = memory.read_bytes(base + pe + 24, 2).is_some_and(|m| m == [0x0B, 0x02]);
    let directory = base + pe + 24 + if x64 { 112 } else { 96 } + 13 * 8;
    let rva = memory.read_u32(directory).unwrap_or(0);
    if rva == 0 {
        return found;
    }
    let width = if x64 { 8 } else { 4 };
    for entry in 0..256 {
        let at = base + rva + entry * 32;
        let name = memory.read_u32(at + 4).unwrap_or(0);
        if name == 0 {
            break;
        }
        let dll = String::from_utf8_lossy(&crate::win32::read_cstr(memory, base + name)).to_ascii_lowercase();
        let names = memory.read_u32(at + 16).unwrap_or(0);
        for index in 0..4096 {
            let value = if x64 { memory.read_u64(base + names + index * width).unwrap_or(0) } else { memory.read_u32(base + names + index * width).unwrap_or(0) as u64 };
            if value == 0 {
                break;
            }
            if value >> (width * 8 - 1) != 0 {
                found.insert((dll.clone(), format!("#{}", value & 0xFFFF)));
            } else {
                let function = crate::win32::read_cstr(memory, base + value as u32 + 2);
                found.insert((dll.clone(), String::from_utf8_lossy(&function).into_owned()));
            }
        }
    }
    found
}

/// Whether a loaded module declares `dll!name` as a delay-loaded import:
/// its loader helper asks GetProcAddress for it, and gets a thunk even
/// when the function is not implemented (calling it then stops the
/// program with its name, as a missing ordinary import does).
pub fn declared_delay_import<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, dll: &str, name: &str) -> bool {
    let mut bases: Vec<u32> = vec![ctx.image_base];
    bases.extend(ctx.modules.borrow().iter().filter(|m| !m.path.is_empty()).map(|m| m.image_base));
    let host = |dll: &str| crate::win32::api_set_host(dll).unwrap_or(dll).to_ascii_lowercase();
    let key = (host(dll), name.to_owned());
    for base in bases {
        let known = ctx.shared.borrow().nt.delay_imports.contains_key(&base);
        if !known {
            let set = read_delay_imports(memory, base);
            ctx.shared.borrow_mut().nt.delay_imports.insert(base, set);
        }
        if ctx.shared.borrow().nt.delay_imports.get(&base).is_some_and(|set| set.iter().any(|(d, n)| *n == key.1 && host(d) == key.0)) {
            return true;
        }
    }
    false
}

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "FlushProcessWriteBuffers" | "GetCurrentProcessorNumber" | "SwitchToThread" | "RtlGetLastWin32Error"
        | "GetCurrentProcessorNumberEx" => 0,
        "GetThreadId" | "QueryUnbiasedInterruptTime" | "QueryUnbiasedInterruptTimePrecise" | "RtlCaptureContext2"
        | "GetSystemTimePreciseAsFileTime" | "CloseThreadpoolTimer" | "CloseThreadpoolWait"
        | "DeleteProcThreadAttributeList" | "InterlockedFlushSList" | "InterlockedPopEntrySList" | "QueryDepthSList"
        | "RemoveVectoredExceptionHandler" | "NtClose" | "RtlSetLastWin32Error" | "RtlNtStatusToDosError"
        | "RtlQueryPerformanceCounter" | "RtlFreeUnicodeString" | "RtlFreeAnsiString" | "RtlAcquireSRWLockExclusive"
        | "RtlAcquireSRWLockShared" | "RtlReleaseSRWLockExclusive" | "RtlReleaseSRWLockShared" | "RtlInitializeSRWLock"
        | "RtlWakeAllConditionVariable" | "RtlWakeConditionVariable" | "RtlRunOnceInitialize" | "AttachConsole"
        | "WindowsDeleteString" | "SetDefaultDllDirectories"
        | "WakeByAddressAll" | "WakeByAddressSingle" | "RtlInitializeConditionVariable" => 1,
        "AddVectoredExceptionHandler" | "RemoveVectoredContinueHandler" | "AddVectoredContinueHandler"
        | "InterlockedPushEntrySList" | "RtlInitUnicodeString" | "RtlInitAnsiString" | "GetProcessHandleCount"
        | "GetProcessHeaps" | "IsWow64Process" | "SetThreadAffinityMask" | "CreateJobObjectW"
        | "AssignProcessToJobObject" | "TerminateJobObject" | "GetCurrentPackageFullName" | "GetUserDefaultLocaleName"
        | "UnregisterWaitEx" | "GetThreadContext" | "SetThreadContext" | "GetFileInformationByHandle"
        | "WindowsGetStringRawBuffer" | "RoInitialize" | "WerRegisterRuntimeExceptionModule"
        | "WerUnregisterRuntimeExceptionModule" | "GetProcessId" => 2,
        "FlushInstructionCache" | "RtlCompareMemory" | "RtlRunOnceComplete" | "RtlCompareUnicodeString" | "RtlEqualUnicodeString"
        | "RtlDuplicateUnicodeString" | "RtlAnsiStringToUnicodeString" | "RtlUnicodeStringToAnsiString"
        | "RtlAllocateHeap" | "RtlFreeHeap" | "CreateHardLinkW" | "CreateSymbolicLinkW" | "SetProcessMitigationPolicy" | "CreateThreadpoolTimer" | "CreateThreadpoolWait" | "SetThreadpoolWait"
        | "WaitForThreadpoolTimerCallbacks" | "WaitForThreadpoolWaitCallbacks" | "SetHandleInformation"
        | "RtlAddFunctionTable" | "RoGetActivationFactory" => 3,
        "RtlCaptureStackBackTrace" | "RtlRunOnceBeginInitialize" | "RtlRunOnceExecuteOnce" | "RtlReAllocateHeap"
        | "HeapSetInformation" | "GetProcessMitigationPolicy" | "EnumSystemLocalesEx" | "WaitOnAddress"
        | "SetThreadInformation" | "CreateEventExW" | "CreateEventExA" | "CreateWaitableTimerExW"
        | "CreateWaitableTimerExA" | "RtlSleepConditionVariableSRW" | "CreateIoCompletionPort" | "SetThreadpoolTimer"
        | "InitializeProcThreadAttributeList" | "SetFileInformationByHandle"
        | "PostQueuedCompletionStatus" | "SetInformationJobObject" | "VirtualFreeEx" | "CopyFile2" | "GetLogicalProcessorInformationEx"
        | "WindowsCreateStringReference" | "WindowsCreateString" => 4,
        "GetProcessTimes" | "VirtualAllocEx" | "VirtualProtectEx" | "WriteProcessMemory" | "ReadProcessMemory"
        | "GetQueuedCompletionStatus" | "QueryInformationJobObject" | "CreateFile2" | "NtQueryObject"
        | "NtReadVirtualMemory" | "NtQueryInformationProcess" | "NtWriteVirtualMemory" => 5,
        "SetWaitableTimerEx" | "CreateSemaphoreExW" | "SearchPathW" | "RegisterWaitForSingleObject"
        | "NtQueryVirtualMemory" | "UpdateProcThreadAttribute" | "SetWaitableTimer" => 6,
        "NtDuplicateObject" | "CreateRemoteThread" => 7,
        "NtOpenFile" => 6,
        "K32EnumProcessModules" | "EnumProcessModules" => 4,
        "NtUnmapViewOfSection" => 2,
        "NtMapViewOfSection" => 10,
        "memcmp" | "memcpy" | "memmove" | "memset" => 3,
        "InitializeAcl" => 3,
        "GetProcessWindowStation" => 0,
        "CoGetApartmentType" => 2,
        "CommandLineToArgvW" => 2,
        "GetFinalPathNameByHandleW" | "GetFileInformationByHandleEx" => 4,
        "GetDpiForMonitor" => 4,
        "GetDpiForWindow" | "EnableNonClientDpiScaling" | "SetThreadDpiAwarenessContext" | "SetProcessDpiAwarenessContext"
        | "IsValidDpiAwarenessContext" | "GetAwarenessFromDpiAwarenessContext" | "SetProcessDpiAwareness" => 1,
        "GetDpiForSystem" | "GetThreadDpiAwarenessContext" | "SetProcessDPIAware" | "IsProcessDPIAware" => 0,
        "AreDpiAwarenessContextsEqual" | "GetSystemMetricsForDpi" | "GetProcessDpiAwareness" => 2,
        "AdjustWindowRectExForDpi" => 5,
        "NtQuerySection" | "NtQueryInformationThread" | "NtSetInformationProcess" | "NtQuerySystemInformation"
        | "NtOpenDirectoryObject" | "NtQueryDirectoryObject" | "NtCreateFile" | "NtReadFile" | "NtWriteFile"
        | "NtDeviceIoControlFile" | "NtFsControlFile" | "NtCancelIoFileEx" | "LdrResolveDelayLoadedAPI"
        | "NtCreateSection" => 10,
        "RtlGetLastNtStatus" => 0,
        "LdrLoadDll" => 4,
        "LdrUnloadDll" => 1,
        "CreateDXGIFactory" | "CreateDXGIFactory1" => 2,
        "CreateDXGIFactory2" => 3,
        "D3D11CreateDevice" => 10,
        "D3D10CreateDevice1" => 7,
        "Direct3DCreate9" => 1,
        "Direct3DCreate9Ex" => 2,
        "D2D1CreateFactory" => 4,
        "DwmFlush" => 0,
        "DwmGetCompositionTimingInfo" => 2,
        "MsgWaitForMultipleObjectsEx" => 5,
        "LockFileEx" | "UnlockFileEx" => 6,
        "VirtualQueryEx" => 4,
        "GetNamedSecurityInfoW" => 8,
        "SetupDiGetClassDevsW" => 4,
        "SetupDiDestroyDeviceInfoList" => 1,
        "SetupDiEnumDeviceInfo" => 3,
        "SetWinEventHook" => 7,
        "UnhookWinEvent" | "UnregisterDeviceNotification" => 1,
        "RegisterDeviceNotificationW" => 3,
        "EnumDisplayDevicesW" => 4,
        "SHGetPropertyStoreForWindow" | "SetCurrentProcessExplicitAppUserModelID" | "GetCurrentProcessExplicitAppUserModelID" => 3,
        "GetComputerNameExA" | "GetComputerNameExW" => 3,
        "CreateWaitableTimerW" | "CreateWaitableTimerA" => 3,
        "CancelWaitableTimer" => 1,
        "RtlAddGrowableFunctionTable" => 6,
        "RtlGrowFunctionTable" => 2,
        "RtlDeleteGrowableFunctionTable" | "RtlDeleteFunctionTable" => 1,
        "GetSecurityDescriptorControl" => 3,
        "RegisterTraceGuidsW" => 8,
        "GetDynamicTimeZoneInformation" => 1,
        "GlobalMemoryStatusEx" | "IsValidSecurityDescriptor" => 1,
        "GetSystemCpuSetInformation" => 5,
        "GetUserPreferredUILanguages" | "GetThreadPreferredUILanguages" | "GetUserDefaultPreferredUILanguages" => 4,
        "LCIDToLocaleName" => 4,
        "LocaleNameToLCID" => 2,
        "GetDisplayConfigBufferSizes" => 3,
        "RoActivateInstance" => 2,
        "VirtualAlloc" | "VirtualProtect" => 4,
        "VirtualFree" | "VirtualQuery" => 3,
        "timeGetTime" => 0,
        "timeBeginPeriod" | "timeEndPeriod" => 1,
        "timeGetDevCaps" => 2,
        "ProcessPrng" | "SystemFunction036" => 2,
        "SetThreadStackGuarantee" | "EventUnregister" => 1,
        "EventRegister" => 4,
        "EventSetInformation" | "EventWriteTransfer" => 5,
        "GetSecurityInfo" => 8,
        "GetThreadDesktop" | "CloseDesktop" | "CloseWindowStation" => 1,
        "OpenInputDesktop" | "OpenDesktopW" => 4,
        "GetUserObjectInformationW" | "GetUserObjectInformationA" => 5,
        "AddAccessAllowedAce" | "CreateWellKnownSid" => 4,
        "SetSecurityDescriptorGroup" | "SetSecurityDescriptorOwner" => 3,
        "GetLengthSid" | "IsValidSid" => 1,
        "CopySid" => 3,
        _ => return None,
    })
}

fn win32(ctx: &ApiContext, memory: &mut dyn GuestMemory, dll: &str, name: &str, args: &[u32]) -> u32 {
    crate::win32::dispatch_with_memory(ctx, memory, dll, &ImportName::ByName(name.into()), args, 0)
        .map(|o| o.value)
        .unwrap_or(0)
}

fn set_error(ctx: &ApiContext, code: u32) {
    ctx.shared.borrow_mut().last_error = code;
}

fn alloc(ctx: &ApiContext, size: u32) -> u32 {
    ctx.shared.borrow_mut().heap.alloc(size).unwrap_or(0)
}

/// A UNICODE_STRING / ANSI_STRING: Length, MaximumLength, Buffer.
fn read_counted(memory: &mut dyn GuestMemory, va: u32) -> (u16, u16, u32) {
    let bytes = memory.read_bytes(va, 4).unwrap_or_else(|| vec![0; 4]);
    let buffer = memory.read_ptr(va + memory.ptr_size()).unwrap_or(0) as u32;
    (u16::from_le_bytes([bytes[0], bytes[1]]), u16::from_le_bytes([bytes[2], bytes[3]]), buffer)
}

fn write_counted(memory: &mut dyn GuestMemory, va: u32, length: u16, maximum: u16, buffer: u32) {
    let mut bytes = length.to_le_bytes().to_vec();
    bytes.extend_from_slice(&maximum.to_le_bytes());
    let _ = memory.write_bytes(va, &bytes);
    let size = memory.ptr_size();
    let _ = memory.write_ptr(va + size, buffer as u64);
}

fn unicode_string(memory: &mut dyn GuestMemory, va: u32) -> Vec<u16> {
    let (length, _, buffer) = read_counted(memory, va);
    let bytes = memory.read_bytes(buffer, length as usize).unwrap_or_default();
    bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

/// SLIST_HEADER: the first entry and the depth. In a 64-bit process the
/// header is 16 bytes, the entry address (shifted by four) in the second
/// half; in a 32-bit one the entry is the first pointer.
fn slist(memory: &mut dyn GuestMemory, head: u32) -> (u32, u16) {
    if memory.ptr_size() == 8 {
        let low = memory.read_u64(head).unwrap_or(0);
        let high = memory.read_u64(head + 8).unwrap_or(0);
        ((high & !0xF) as u32, low as u16)
    } else {
        let first = memory.read_u32(head).unwrap_or(0);
        let depth = memory.read_bytes(head + 4, 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
        (first, depth)
    }
}

fn set_slist(memory: &mut dyn GuestMemory, head: u32, first: u32, depth: u16) {
    if memory.ptr_size() == 8 {
        let low = memory.read_u64(head).unwrap_or(0);
        let sequence = ((low >> 16) + 1) & 0xFFFF_FFFF_FFFF;
        let _ = memory.write_u64(head, depth as u64 | sequence << 16);
        let _ = memory.write_u64(head + 8, (first as u64 & !0xF) | 1); // header-8 bit marks the x64 format
    } else {
        let _ = memory.write_u32(head, first);
        let _ = memory.write_bytes(head + 4, &depth.to_le_bytes());
    }
}

pub fn call(ctx: &ApiContext, memory: &mut dyn GuestMemory, name: &str, a: &[u64]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    let arg = |i: usize| a.get(i).copied().unwrap_or(0);
    let p = |i: usize| arg(i) as u32;
    let ok = |value: u64| Some(ApiOutcome::wide(value, count));
    let size = memory.ptr_size();
    match name {
        // ── C library functions NTDLL exports ──
        "memcpy" | "memmove" => {
            let bytes = memory.read_bytes(p(1), arg(2) as usize).unwrap_or_default();
            let _ = memory.write_bytes(p(0), &bytes);
            ok(arg(0))
        }
        "memset" => {
            let _ = memory.write_bytes(p(0), &vec![p(1) as u8; arg(2) as usize]);
            ok(arg(0))
        }
        "memcmp" => {
            let x = memory.read_bytes(p(0), arg(2) as usize).unwrap_or_default();
            let y = memory.read_bytes(p(1), arg(2) as usize).unwrap_or_default();
            ok(match x.cmp(&y) {
                std::cmp::Ordering::Less => u32::MAX as u64,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            })
        }
        "RtlCompareMemory" => {
            let x = memory.read_bytes(p(0), arg(2) as usize).unwrap_or_default();
            let y = memory.read_bytes(p(1), arg(2) as usize).unwrap_or_default();
            ok(x.iter().zip(&y).take_while(|(a, b)| a == b).count() as u64)
        }
        // ── Processor, time, processes ──
        "FlushProcessWriteBuffers" | "FlushInstructionCache" | "SwitchToThread" => {
            if name == "SwitchToThread" {
                win32(ctx, memory, "KERNEL32.dll", "Sleep", &[0]);
            }
            ok(1)
        }
        "GetCurrentProcessorNumber" => ok(0),
        "GetCurrentProcessorNumberEx" => {
            let _ = memory.write_bytes(p(0), &[0u8; 4]);
            ok(0)
        }
        "GetThreadId" => ok(crate::win32::sync::thread_id_of(ctx, memory, p(0)).unwrap_or(0) as u64),
        "GetProcessId" => ok(1),
        "QueryUnbiasedInterruptTime" | "QueryUnbiasedInterruptTimePrecise" => {
            let ticks = ctx.shared.borrow().ticks();
            let _ = memory.write_u64(p(0), ticks * 10_000);
            ok(1)
        }
        "GetSystemTimePreciseAsFileTime" => {
            win32(ctx, memory, "KERNEL32.dll", "GetSystemTimeAsFileTime", &[p(0)]);
            ok(0)
        }
        "GetProcessTimes" => {
            win32(ctx, memory, "KERNEL32.dll", "GetSystemTimeAsFileTime", &[p(1)]);
            for index in 2..5 {
                let _ = memory.write_u64(p(index), 0);
            }
            let start = memory.read_u64(p(1)).unwrap_or(0).saturating_sub(ctx.shared.borrow().ticks() * 10_000);
            let _ = memory.write_u64(p(1), start);
            ok(1)
        }
        "GetProcessHandleCount" => {
            let _ = memory.write_u32(p(1), 32);
            ok(1)
        }
        "GetProcessHeaps" => {
            let heap = win32(ctx, memory, "KERNEL32.dll", "GetProcessHeap", &[]);
            if arg(0) >= 1 && p(1) != 0 {
                let _ = memory.write_ptr(p(1), crate::kernel::syscall::widen(heap));
            }
            ok(1)
        }
        "IsWow64Process" => {
            let _ = memory.write_u32(p(1), 0);
            ok(1)
        }
        "SetThreadAffinityMask" => ok(1),
        "SetThreadInformation" | "SetProcessMitigationPolicy" | "HeapSetInformation" | "SetDefaultDllDirectories"
        | "SetHandleInformation" | "SetFileInformationByHandle" | "SetInformationJobObject" | "TerminateJobObject"
        | "AssignProcessToJobObject"
        | "UnregisterWaitEx" | "EnumSystemLocalesEx" => ok(1),
        "GetProcessMitigationPolicy" | "QueryInformationJobObject" | "GetLogicalProcessorInformationEx"
        | "GetThreadContext" | "SetThreadContext" | "CreateHardLinkW" | "CreateSymbolicLinkW" | "AttachConsole"
        | "ReadProcessMemory" => {
            set_error(ctx, 50); // ERROR_NOT_SUPPORTED
            ok(0)
        }
        "CreateRemoteThread" => {
            set_error(ctx, 5);
            ok(0)
        }
        "GetCurrentPackageFullName" => ok(15700), // APPMODEL_ERROR_NO_PACKAGE
        "GetUserDefaultLocaleName" => {
            let name: Vec<u16> = "en-US".encode_utf16().collect();
            if (arg(1) as usize) <= name.len() {
                return ok(0);
            }
            unicode::write_wide(memory, p(0), &name, true);
            ok(name.len() as u64 + 1)
        }
        "CreateJobObjectW" => {
            let handle = win32(ctx, memory, "KERNEL32.dll", "CreateEventW", &[0, 1, 0, 0]);
            ok(handle as u64)
        }
        "InitializeProcThreadAttributeList" => {
            // (list, count, flags, &size)
            let needed = 48 + 24 * p(1).max(1);
            if p(0) == 0 {
                let _ = memory.write_ptr(p(3), needed as u64);
                set_error(ctx, 122); // ERROR_INSUFFICIENT_BUFFER
                return ok(0);
            }
            let _ = memory.write_bytes(p(0), &vec![0u8; needed as usize]);
            ok(1)
        }
        "UpdateProcThreadAttribute" => {
            // (list, flags, attribute, value, size, ...): kept as a count
            // and (attribute, value, size) entries after the list's
            // first 8 bytes, for CreateProcess to read.
            let entries = memory.read_u32(p(0) + 4).unwrap_or(0);
            let entry = p(0) + 8 + entries * 24;
            let _ = memory.write_u64(entry, arg(2));
            let _ = memory.write_u64(entry + 8, arg(3));
            let _ = memory.write_u64(entry + 16, arg(4));
            let _ = memory.write_u32(p(0), 0x4C57_544F);
            let _ = memory.write_u32(p(0) + 4, entries + 1);
            ok(1)
        }
        "DeleteProcThreadAttributeList" => ok(0),
        "WerRegisterRuntimeExceptionModule" | "WerUnregisterRuntimeExceptionModule" | "RoInitialize" => ok(0),
        // ── Memory of a process (only this one) ──
        "VirtualAllocEx" => ok(win32(ctx, memory, "KERNEL32.dll", "VirtualAlloc", &[p(1), p(2), p(3), p(4)]) as u64),
        "VirtualFreeEx" => ok(win32(ctx, memory, "KERNEL32.dll", "VirtualFree", &[p(1), p(2), p(3)]) as u64),
        "VirtualProtectEx" => ok(win32(ctx, memory, "KERNEL32.dll", "VirtualProtect", &[p(1), p(2), p(3), p(4)]) as u64),
        "WriteProcessMemory" | "NtWriteVirtualMemory" | "NtReadVirtualMemory" => {
            // (process, base, buffer, size, &done): this process only.
            let (from, to) = if name == "NtReadVirtualMemory" { (p(1), p(2)) } else { (p(2), p(1)) };
            let bytes = memory.read_bytes(from, arg(3) as usize).unwrap_or_default();
            let _ = memory.write_bytes(to, &bytes);
            if p(4) != 0 {
                let _ = memory.write_ptr(p(4), bytes.len() as u64);
            }
            ok(if name.starts_with("Nt") { STATUS_SUCCESS } else { 1 })
        }
        "NtQueryVirtualMemory" => {
            // (process, address, class, buffer, length, &returned)
            if arg(2) != 0 {
                return ok(STATUS_INVALID_INFO_CLASS);
            }
            let returned = call(ctx, memory, "VirtualQuery", &[arg(1), arg(3), arg(4)]).map(|o| o.value).unwrap_or(0);
            if p(5) != 0 {
                let _ = memory.write_ptr(p(5), returned as u64);
            }
            ok(if returned != 0 { STATUS_SUCCESS } else { 0xC000_000D })
        }
        "NtQueryInformationProcess" => {
            // (process, class, buffer, length, &returned): basic information.
            if arg(1) != 0 {
                return ok(STATUS_INVALID_INFO_CLASS);
            }
            let teb = memory.current_tib();
            let peb = if size == 8 { memory.read_u64(teb + 0x60).unwrap_or(0) } else { memory.read_u32(teb + 0x30).unwrap_or(0) as u64 };
            let mut values = vec![0u64, peb, 1, 8, 1, 0];
            values.truncate(6);
            for (index, value) in values.into_iter().enumerate() {
                let _ = memory.write_ptr(p(2) + index as u32 * size, value);
            }
            if p(4) != 0 {
                let _ = memory.write_u32(p(4), 6 * size);
            }
            ok(STATUS_SUCCESS)
        }
        "NtClose" => {
            win32(ctx, memory, "KERNEL32.dll", "CloseHandle", &[p(0)]);
            ok(STATUS_SUCCESS)
        }
        "NtDuplicateObject" | "NtMapViewOfSection" | "NtUnmapViewOfSection" | "NtOpenFile" | "NtQueryObject" => {
            ok(STATUS_NOT_IMPLEMENTED)
        }
        // ── Heaps, errors, strings of the native API ──
        "RtlAllocateHeap" => ok(win32(ctx, memory, "KERNEL32.dll", "HeapAlloc", &[p(0), p(1), p(2)]) as u64),
        "RtlFreeHeap" => ok(win32(ctx, memory, "KERNEL32.dll", "HeapFree", &[p(0), p(1), p(2)]) as u64),
        "RtlReAllocateHeap" => ok(win32(ctx, memory, "KERNEL32.dll", "HeapReAlloc", &[p(0), p(1), p(2), p(3)]) as u64),
        "RtlGetLastWin32Error" => ok(ctx.shared.borrow().last_error as u64),
        "RtlSetLastWin32Error" => {
            set_error(ctx, p(0));
            ok(0)
        }
        "RtlNtStatusToDosError" => ok(match p(0) {
            0 => 0,
            0xC000_0034 | 0xC000_000F => 2,
            0xC000_003A => 3,
            0xC000_0022 => 5,
            0xC000_0008 => 6,
            0xC000_0017 => 8,
            0xC000_0002 => 50,
            0xC000_000D => 87,
            0xC000_0023 => 122,
            0x0000_0102 => 258,
            _ => 317,
        }),
        "RtlQueryPerformanceCounter" => {
            win32(ctx, memory, "KERNEL32.dll", "QueryPerformanceCounter", &[p(0)]);
            ok(1)
        }
        "RtlInitUnicodeString" | "RtlInitAnsiString" => {
            let length = if p(1) == 0 {
                0
            } else if name == "RtlInitUnicodeString" {
                unicode::read_wstr(memory, p(1)).len() * 2
            } else {
                crate::win32::read_cstr(memory, p(1)).len()
            };
            let unit = if name == "RtlInitUnicodeString" { 2 } else { 1 };
            let maximum = if p(1) == 0 { 0 } else { length + unit };
            write_counted(memory, p(0), length as u16, maximum as u16, p(1));
            ok(0)
        }
        "RtlFreeUnicodeString" | "RtlFreeAnsiString" => {
            let (_, _, buffer) = read_counted(memory, p(0));
            if buffer != 0 {
                ctx.shared.borrow_mut().heap.free(buffer);
            }
            write_counted(memory, p(0), 0, 0, 0);
            ok(0)
        }
        "RtlDuplicateUnicodeString" => {
            // (flags, source, destination): 1 = terminate.
            let text = unicode_string(memory, p(1));
            let terminate = arg(0) & 1 != 0;
            let bytes = text.len() as u32 * 2 + if terminate { 2 } else { 0 };
            let buffer = alloc(ctx, bytes.max(2));
            unicode::write_wide(memory, buffer, &text, terminate);
            write_counted(memory, p(2), (text.len() * 2) as u16, bytes as u16, buffer);
            ok(STATUS_SUCCESS)
        }
        "RtlAnsiStringToUnicodeString" => {
            // (destination, source, allocate)
            let (length, _, buffer) = read_counted(memory, p(1));
            let bytes = memory.read_bytes(buffer, length as usize).unwrap_or_default();
            let text = unicode::ansi_to_wide(&bytes);
            let target = if arg(2) != 0 {
                let block = alloc(ctx, text.len() as u32 * 2 + 2);
                write_counted(memory, p(0), (text.len() * 2) as u16, (text.len() * 2 + 2) as u16, block);
                block
            } else {
                let (_, maximum, block) = read_counted(memory, p(0));
                if (maximum as usize) < text.len() * 2 {
                    return ok(0x8000_0005); // STATUS_BUFFER_OVERFLOW
                }
                let _ = memory.write_bytes(p(0), &((text.len() * 2) as u16).to_le_bytes());
                block
            };
            unicode::write_wide(memory, target, &text, true);
            ok(STATUS_SUCCESS)
        }
        "RtlUnicodeStringToAnsiString" => {
            let text = unicode::wide_to_ansi(&unicode_string(memory, p(1)));
            let target = if arg(2) != 0 {
                let block = alloc(ctx, text.len() as u32 + 1);
                write_counted(memory, p(0), text.len() as u16, text.len() as u16 + 1, block);
                block
            } else {
                let (_, _, block) = read_counted(memory, p(0));
                let _ = memory.write_bytes(p(0), &(text.len() as u16).to_le_bytes());
                block
            };
            let mut bytes = text;
            bytes.push(0);
            let _ = memory.write_bytes(target, &bytes);
            ok(STATUS_SUCCESS)
        }
        "RtlCompareUnicodeString" | "RtlEqualUnicodeString" => {
            let fold = |u: u16| if arg(2) != 0 { unicode_upper(u) } else { u };
            let x: Vec<u16> = unicode_string(memory, p(0)).into_iter().map(fold).collect();
            let y: Vec<u16> = unicode_string(memory, p(1)).into_iter().map(fold).collect();
            if name == "RtlEqualUnicodeString" {
                return ok(u64::from(x == y));
            }
            ok(match x.cmp(&y) {
                std::cmp::Ordering::Less => u32::MAX as u64,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            })
        }
        // ── Locks, one-time initialisation (the native names) ──
        "RtlAcquireSRWLockExclusive" | "RtlAcquireSRWLockShared" | "RtlReleaseSRWLockExclusive"
        | "RtlReleaseSRWLockShared" | "RtlWakeAllConditionVariable" | "RtlWakeConditionVariable"
        | "WakeByAddressAll" | "WakeByAddressSingle" => ok(0),
        "RtlInitializeSRWLock" | "RtlInitializeConditionVariable" | "RtlRunOnceInitialize" => {
            let _ = memory.write_ptr(p(0), 0);
            ok(0)
        }
        "RtlSleepConditionVariableSRW" => {
            // (condition, lock, timeout*, flags): a spurious wake-up after
            // the others ran.
            let timeout = if p(2) == 0 { u32::MAX } else { (memory.read_u64(p(2)).unwrap_or(0) as i64).unsigned_abs() as u32 / 10_000 };
            win32(ctx, memory, "KERNEL32.dll", "Sleep", &[timeout.min(10)]);
            ok(STATUS_SUCCESS)
        }
        "RtlRunOnceBeginInitialize" => {
            let state = memory.read_ptr(p(0)).unwrap_or(0);
            if state == 2 {
                return ok(STATUS_SUCCESS);
            }
            let _ = memory.write_ptr(p(0), 1);
            ok(0x0000_0103) // STATUS_PENDING
        }
        "RtlRunOnceComplete" => {
            let _ = memory.write_ptr(p(0), if arg(1) & 4 == 0 { 2 } else { 0 });
            ok(STATUS_SUCCESS)
        }
        "RtlRunOnceExecuteOnce" => {
            if memory.read_ptr(p(0)).unwrap_or(0) == 2 {
                return ok(STATUS_SUCCESS);
            }
            let result = memory.call_guest_wide(p(1), &[arg(0), arg(2), arg(3)]).unwrap_or(0) as u32;
            if result != 0 {
                let _ = memory.write_ptr(p(0), 2);
                ok(STATUS_SUCCESS)
            } else {
                ok(0xC000_0001)
            }
        }
        "WaitOnAddress" => {
            // (address, compare, size, timeout): until the value differs.
            let (address, compare, length, timeout) = (p(0), p(1), arg(2) as usize, p(3));
            // The thread waits until the value changes (WakeByAddress* need
            // do nothing: the waiting thread looks at the value).
            let expected = memory.read_bytes(compare, length).unwrap_or_default();
            if memory.read_bytes(address, length).unwrap_or_default() != expected {
                return ok(1);
            }
            if timeout == 0 {
                set_error(ctx, 1460); // ERROR_TIMEOUT
                return ok(0);
            }
            let changed = crate::win32::sync::wait_on_address(ctx, memory, address, expected, timeout);
            if !changed {
                set_error(ctx, 1460);
            }
            ok(u64::from(changed))
        }
        // ── Interlocked singly linked lists ──
        "InterlockedPushEntrySList" => {
            let (first, depth) = slist(memory, p(0));
            let _ = memory.write_ptr(p(1), first as u64);
            set_slist(memory, p(0), p(1), depth.wrapping_add(1));
            ok(first as u64)
        }
        "InterlockedPopEntrySList" => {
            let (first, depth) = slist(memory, p(0));
            if first == 0 {
                return ok(0);
            }
            let next = memory.read_ptr(first).unwrap_or(0) as u32;
            set_slist(memory, p(0), next, depth.saturating_sub(1));
            ok(first as u64)
        }
        "InterlockedFlushSList" => {
            let (first, _) = slist(memory, p(0));
            set_slist(memory, p(0), 0, 0);
            ok(first as u64)
        }
        "QueryDepthSList" => ok(slist(memory, p(0)).1 as u64),
        // ── Synchronisation objects with the Vista signatures ──
        "CreateEventExW" | "CreateEventExA" => {
            // (security, name, flags: 1 manual reset, 2 initially set, access)
            let function = if name.ends_with('W') { "CreateEventW" } else { "CreateEventA" };
            ok(win32(ctx, memory, "KERNEL32.dll", function, &[p(0), p(2) & 1, (p(2) >> 1) & 1, p(1)]) as u64)
        }
        "CreateSemaphoreExW" => {
            ok(win32(ctx, memory, "KERNEL32.dll", "CreateSemaphoreW", &[p(0), p(1), p(2), p(3)]) as u64)
        }
        // (security, name, flags: 1 manual reset, access)
        "CreateWaitableTimerExW" | "CreateWaitableTimerExA" => ok(crate::win32::sync::create_timer(ctx, p(2) & 1 != 0) as u64),
        "CreateWaitableTimerW" | "CreateWaitableTimerA" => ok(crate::win32::sync::create_timer(ctx, p(1) != 0) as u64),
        "CancelWaitableTimer" => ok(u64::from(crate::win32::sync::cancel_timer(ctx, p(0)))),
        "SetWaitableTimerEx" | "SetWaitableTimer" => {
            // (timer, &due time, period ms, ...): a negative due time is
            // relative in 100 ns units, a positive one a FILETIME.
            let due = memory.read_u64(p(1)).unwrap_or(0) as i64;
            let delay = if due < 0 {
                due.unsigned_abs() / 10_000
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64 / 100 + 116_444_736_000_000_000)
                    .unwrap_or(0);
                (due as u64).saturating_sub(now) / 10_000
            };
            ok(u64::from(crate::win32::sync::set_timer(ctx, p(0), delay, p(2) as u64)))
        }
        // ── Completion ports ──
        "CreateIoCompletionPort" => {
            // (file, existing port, key, threads): files are not associated.
            if p(1) != 0 {
                return ok(arg(1));
            }
            let port = win32(ctx, memory, "KERNEL32.dll", "CreateEventW", &[0, 1, 0, 0]);
            ctx.shared.borrow_mut().nt.ports.insert(port, VecDeque::new());
            ok(port as u64)
        }
        "PostQueuedCompletionStatus" => {
            // (port, bytes, key, overlapped)
            let queued = {
                let mut shared = ctx.shared.borrow_mut();
                match shared.nt.ports.get_mut(&p(0)) {
                    Some(queue) => {
                        queue.push_back((p(1), arg(2), arg(3)));
                        true
                    }
                    None => {
                        shared.last_error = 6;
                        false
                    }
                }
            };
            if queued {
                // The port's event wakes a thread waiting for a packet.
                win32(ctx, memory, "KERNEL32.dll", "SetEvent", &[p(0)]);
            }
            ok(u64::from(queued))
        }
        "GetQueuedCompletionStatus" => {
            // (port, &bytes, &key, &overlapped, timeout)
            let port = p(0);
            let empty = |ctx: &ApiContext| ctx.shared.borrow().nt.ports.get(&port).is_none_or(|q| q.is_empty());
            if empty(ctx) {
                win32(ctx, memory, "KERNEL32.dll", "ResetEvent", &[port]);
                win32(ctx, memory, "KERNEL32.dll", "WaitForSingleObject", &[port, p(4)]);
            }
            let packet = ctx.shared.borrow_mut().nt.ports.get_mut(&port).and_then(VecDeque::pop_front);
            match packet {
                Some((bytes, key, overlapped)) => {
                    let _ = memory.write_u32(p(1), bytes);
                    let _ = memory.write_ptr(p(2), key);
                    let _ = memory.write_ptr(p(3), overlapped);
                    ok(1)
                }
                None => {
                    let _ = memory.write_ptr(p(3), 0);
                    set_error(ctx, 258); // WAIT_TIMEOUT
                    ok(0)
                }
            }
        }
        // ── Thread-pool timers and waits: accepted, never fire ──
        "CreateThreadpoolTimer" | "CreateThreadpoolWait" => {
            let object = alloc(ctx, 32);
            let _ = memory.write_bytes(object, &[0u8; 32]);
            ok(object as u64)
        }
        "SetThreadpoolTimer" | "SetThreadpoolWait" | "WaitForThreadpoolTimerCallbacks" | "WaitForThreadpoolWaitCallbacks" => ok(0),
        "CloseThreadpoolTimer" | "CloseThreadpoolWait" => {
            ctx.shared.borrow_mut().heap.free(p(0));
            ok(0)
        }
        "RegisterWaitForSingleObject" => {
            let _ = memory.write_ptr(p(0), 0x5A17);
            ok(1)
        }
        // ── Files ──
        "CreateFile2" => {
            // (name, access, share, disposition, parameters)
            let attributes = if p(4) != 0 { memory.read_u32(p(4) + 4).unwrap_or(0x80) } else { 0x80 };
            let handle = win32(ctx, memory, "KERNEL32.dll", "CreateFileW", &[p(0), p(1), p(2), 0, p(3), attributes, 0]);
            ok(crate::kernel::syscall::widen(handle))
        }
        "CopyFile2" => {
            let done = win32(ctx, memory, "KERNEL32.dll", "CopyFileW", &[p(0), p(1), 0]);
            ok(if done != 0 { 0 } else { 0x8007_0002 })
        }
        "GetFileInformationByHandle" => {
            // BY_HANDLE_FILE_INFORMATION: attributes, three times, volume
            // serial, size high/low, links, index high/low.
            let Some(length) = crate::win32::crt::file_size(ctx, p(0)) else {
                set_error(ctx, 6);
                return ok(0);
            };
            let directory = ctx.shared.borrow().files.get(&p(0)).and_then(|f| f.metadata().ok()).is_some_and(|m| m.is_dir());
            let mut bytes = (if directory { 0x10u32 } else { 0x80 }).to_le_bytes().to_vec();
            bytes.extend_from_slice(&[0u8; 24]);
            bytes.extend_from_slice(&0x4F54_5700u32.to_le_bytes());
            bytes.extend_from_slice(&((length >> 32) as u32).to_le_bytes());
            bytes.extend_from_slice(&(length as u32).to_le_bytes());
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&p(0).to_le_bytes());
            let _ = memory.write_bytes(p(1), &bytes);
            ok(1)
        }
        "SearchPathW" => {
            set_error(ctx, 2);
            ok(0)
        }
        "K32EnumProcessModules" | "EnumProcessModules" => {
            // (process, modules, bytes, &needed)
            let mut bases: Vec<u32> = vec![ctx.image_base];
            bases.extend(ctx.modules.borrow().iter().map(|m| m.image_base).filter(|&b| b != ctx.image_base));
            let needed = bases.len() as u32 * size;
            for (index, base) in bases.iter().enumerate() {
                if (index as u32 + 1) * size <= p(2) {
                    let _ = memory.write_ptr(p(1) + index as u32 * size, *base as u64);
                }
            }
            let _ = memory.write_u32(p(3), needed);
            ok(1)
        }
        // ── Exceptions ──
        "AddVectoredExceptionHandler" | "AddVectoredContinueHandler" => {
            let mut shared = ctx.shared.borrow_mut();
            shared.nt.next_vectored += 1;
            let handle = 0x7EC0_0000 + shared.nt.next_vectored;
            if name == "AddVectoredExceptionHandler" {
                if arg(0) != 0 {
                    shared.nt.vectored.insert(0, (handle, arg(1)));
                } else {
                    shared.nt.vectored.push((handle, arg(1)));
                }
            }
            ok(handle as u64)
        }
        "RemoveVectoredExceptionHandler" | "RemoveVectoredContinueHandler" => {
            ctx.shared.borrow_mut().nt.vectored.retain(|(h, _)| *h != p(0));
            ok(1)
        }
        "RtlAddFunctionTable" => {
            // (table, entry count, base)
            ctx.shared.borrow_mut().nt.function_tables.push((arg(2), arg(0), p(1)));
            ok(1)
        }
        "RtlCaptureStackBackTrace" => ok(0),
        "RtlAddGrowableFunctionTable" => {
            // (&handle, table, count, maximum, base, limit)
            let index = {
                let mut shared = ctx.shared.borrow_mut();
                shared.nt.function_tables.push((arg(4), arg(1), p(2)));
                shared.nt.function_tables.len() as u64
            };
            let _ = memory.write_ptr(p(0), 0x6F00_0000 + index);
            ok(STATUS_SUCCESS)
        }
        "RtlGrowFunctionTable" => {
            let index = (arg(0).wrapping_sub(0x6F00_0000)) as usize;
            if let Some(entry) = ctx.shared.borrow_mut().nt.function_tables.get_mut(index.wrapping_sub(1)) {
                entry.2 = p(1);
            }
            ok(0)
        }
        "RtlDeleteGrowableFunctionTable" | "RtlDeleteFunctionTable" => ok(1),
        "GetSecurityDescriptorControl" => {
            let _ = memory.write_bytes(p(1), &0x8004u16.to_le_bytes()); // SE_SELF_RELATIVE | SE_DACL_PRESENT
            let _ = memory.write_u32(p(2), 1);
            ok(1)
        }
        "RegisterTraceGuidsW" => {
            let _ = memory.write_u64(p(7), 0);
            ok(0)
        }
        "GetDynamicTimeZoneInformation" => {
            // UTC: all zero, "Coordinated Universal Time".
            let _ = memory.write_bytes(p(0), &[0u8; 432]);
            let name: Vec<u16> = "Coordinated Universal Time".encode_utf16().collect();
            unicode::write_wide(memory, p(0) + 4, &name, true);
            unicode::write_wide(memory, p(0) + 88, &name, true);
            let key: Vec<u16> = "UTC".encode_utf16().collect();
            unicode::write_wide(memory, p(0) + 172, &key, true);
            ok(0) // TIME_ZONE_ID_UNKNOWN
        }
        "ProcessPrng" | "SystemFunction036" => {
            // Random bytes from the host.
            let mut bytes = vec![0u8; arg(1) as usize];
            let mut state = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() as u64
                ^ (p(0) as u64).rotate_left(29);
            if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
                use std::io::Read;
                let _ = file.read_exact(&mut bytes);
            } else {
                for byte in &mut bytes {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *byte = state as u8;
                }
            }
            let _ = memory.write_bytes(p(0), &bytes);
            ok(1)
        }
        "SetThreadStackGuarantee" => ok(1),
        // (file, buffer, length, flags): \\?\C:\... for a file OTW opened.
        "GetFinalPathNameByHandleW" => {
            let Some(path) = ctx.shared.borrow().file_paths.get(&p(0)).cloned() else {
                set_error(ctx, 6);
                return ok(0);
            };
            let absolute = crate::win32::crt::absolute(ctx, &path);
            let mut text: Vec<u16> = if p(3) & 0xF == 0 { r"\\?\".encode_utf16().collect() } else { Vec::new() };
            text.extend(unicode::ansi_to_wide(&absolute));
            if (p(2) as usize) <= text.len() {
                return ok(text.len() as u64 + 1);
            }
            unicode::write_wide(memory, p(1), &text, true);
            ok(text.len() as u64)
        }
        // (file, class, buffer, size): basic, standard, attribute-tag and
        // id information from the host file.
        "GetFileInformationByHandleEx" => {
            let metadata = ctx.shared.borrow().files.get(&p(0)).and_then(|f| f.metadata().ok());
            let Some(metadata) = metadata else {
                set_error(ctx, 6);
                return ok(0);
            };
            let filetime = |time: std::io::Result<std::time::SystemTime>| {
                time.ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64 / 100 + 116_444_736_000_000_000)
                    .unwrap_or(116_444_736_000_000_000)
            };
            let attributes: u32 = if metadata.is_dir() { 0x10 } else { 0x20 };
            let mut bytes = Vec::new();
            match p(1) {
                0 => {
                    let modified = filetime(metadata.modified());
                    for value in [filetime(metadata.created()), filetime(metadata.accessed()), modified, modified] {
                        bytes.extend_from_slice(&value.to_le_bytes());
                    }
                    bytes.extend_from_slice(&attributes.to_le_bytes());
                    bytes.extend_from_slice(&[0; 4]);
                }
                1 => {
                    let size = if metadata.is_dir() { 0 } else { metadata.len() };
                    bytes.extend_from_slice(&((size + 4095) & !4095).to_le_bytes());
                    bytes.extend_from_slice(&size.to_le_bytes());
                    bytes.extend_from_slice(&1u32.to_le_bytes());
                    bytes.extend_from_slice(&[0, u8::from(metadata.is_dir()), 0, 0]);
                }
                9 => {
                    bytes.extend_from_slice(&attributes.to_le_bytes());
                    bytes.extend_from_slice(&0u32.to_le_bytes());
                }
                18 => {
                    use std::os::unix::fs::MetadataExt;
                    bytes.extend_from_slice(&0x4F54_5700u64.to_le_bytes());
                    bytes.extend_from_slice(&metadata.ino().to_le_bytes());
                    bytes.extend_from_slice(&0u64.to_le_bytes());
                }
                _ => {
                    set_error(ctx, 50);
                    return ok(0);
                }
            }
            if (p(3) as usize) < bytes.len() {
                set_error(ctx, 24); // ERROR_BAD_LENGTH
                return ok(0);
            }
            let _ = memory.write_bytes(p(2), &bytes);
            ok(1)
        }
        // One display at 96 dots per inch.
        "GetDpiForMonitor" => {
            let _ = memory.write_u32(p(2), 96);
            let _ = memory.write_u32(p(3), 96);
            ok(0)
        }
        "GetDpiForWindow" | "GetDpiForSystem" => ok(96),
        "EnableNonClientDpiScaling" | "SetProcessDPIAware" | "IsProcessDPIAware" | "AreDpiAwarenessContextsEqual"
        | "IsValidDpiAwarenessContext" | "SetProcessDpiAwarenessContext" => ok(1),
        // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 (-4); awareness 2.
        "SetThreadDpiAwarenessContext" | "GetThreadDpiAwarenessContext" => ok(u64::MAX - 3),
        "GetAwarenessFromDpiAwarenessContext" => ok(2),
        "SetProcessDpiAwareness" => ok(0),
        "GetProcessDpiAwareness" => {
            let _ = memory.write_u32(p(1), 2);
            ok(0)
        }
        "GetSystemMetricsForDpi" => ok(win32(ctx, memory, "USER32.dll", "GetSystemMetrics", &[p(0)]) as u64),
        "AdjustWindowRectExForDpi" => ok(win32(ctx, memory, "USER32.dll", "AdjustWindowRectEx", &[p(0), p(1), p(2), p(3)]) as u64),
        // NtQuerySection(section, class, buffer, length, &returned): basic
        // information (base, attributes SEC_COMMIT, maximum size).
        "NtQuerySection" if p(1) == 0 => {
            let Some((_, size)) = ctx.shared.borrow().file_mappings.get(&p(0)).copied() else {
                return ok(0xC000_0008); // STATUS_INVALID_HANDLE
            };
            let mut bytes = vec![0u8; 24];
            bytes[8..12].copy_from_slice(&0x0800_0000u32.to_le_bytes());
            bytes[16..24].copy_from_slice(&(size as u64).to_le_bytes());
            let _ = memory.write_bytes(p(2), &bytes);
            if p(4) != 0 {
                let _ = memory.write_u32(p(4), 24);
            }
            ok(STATUS_SUCCESS)
        }
        // Native calls for what OTW does not offer through them.
        "NtQuerySection" | "NtQueryInformationThread" | "NtSetInformationProcess" | "NtQuerySystemInformation"
        | "NtOpenDirectoryObject" | "NtQueryDirectoryObject" | "NtCreateFile" | "NtReadFile" | "NtWriteFile"
        | "NtDeviceIoControlFile" | "NtFsControlFile" | "NtCancelIoFileEx" | "LdrResolveDelayLoadedAPI"
        | "NtCreateSection" => ok(STATUS_NOT_IMPLEMENTED),
        "RtlGetLastNtStatus" => ok(0),
        "LdrLoadDll" => {
            // (search path, flags, &name, &handle)
            let name = unicode_string(memory, p(2));
            let text = alloc(ctx, name.len() as u32 * 2 + 2);
            unicode::write_wide(memory, text, &name, true);
            let handle = win32(ctx, memory, "KERNEL32.dll", "LoadLibraryW", &[text]);
            ctx.shared.borrow_mut().heap.free(text);
            let _ = memory.write_ptr(p(3), handle as u64);
            ok(if handle != 0 { STATUS_SUCCESS } else { 0xC000_0135 })
        }
        "LdrUnloadDll" => ok(STATUS_SUCCESS),
        // No Direct3D or DXGI: programs draw in software.
        "CreateDXGIFactory" | "CreateDXGIFactory1" | "CreateDXGIFactory2" => {
            let out = if name == "CreateDXGIFactory2" { p(2) } else { p(1) };
            let _ = memory.write_ptr(out, 0);
            ok(0x887A_0004) // DXGI_ERROR_UNSUPPORTED
        }
        "D3D11CreateDevice" => {
            for index in [7, 9] {
                if p(index) != 0 {
                    let _ = memory.write_ptr(p(index), 0);
                }
            }
            ok(0x887A_0004)
        }
        "D3D10CreateDevice1" | "Direct3DCreate9Ex" | "D2D1CreateFactory" => ok(0x8000_4005),
        "Direct3DCreate9" => ok(0),
        // DWM_TIMING_INFO (packed) of a 60 Hz display: rates, the refresh
        // period and the last vertical blank in performance-counter ticks
        // (microseconds).
        "DwmGetCompositionTimingInfo" => {
            let now = ctx.shared.borrow().started.elapsed().as_micros() as u64;
            let period = 16_667u64;
            let vblank = now - now % period;
            let mut bytes = vec![0u8; 0x120];
            bytes[0..4].copy_from_slice(&0x120u32.to_le_bytes());
            bytes[4..8].copy_from_slice(&60u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
            bytes[12..20].copy_from_slice(&period.to_le_bytes());
            bytes[20..24].copy_from_slice(&60u32.to_le_bytes());
            bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
            bytes[28..36].copy_from_slice(&vblank.to_le_bytes());
            bytes[36..44].copy_from_slice(&(now / period).to_le_bytes());
            bytes[48..56].copy_from_slice(&vblank.to_le_bytes());
            let size = memory.read_u32(p(1)).unwrap_or(0).clamp(0, 0x120) as usize;
            let _ = memory.write_bytes(p(1), &bytes[..size.max(44)]);
            ok(0)
        }
        // The next frame of a 60 Hz display.
        "DwmFlush" => {
            let now = ctx.shared.borrow().ticks();
            let wait = 16 - (now % 16).min(15);
            win32(ctx, memory, "KERNEL32.dll", "Sleep", &[wait as u32]);
            ok(0)
        }
        // (count, handles, timeout, wake mask, flags: 1 = wait for all)
        "MsgWaitForMultipleObjectsEx" => {
            ok(win32(ctx, memory, "USER32.dll", "MsgWaitForMultipleObjects", &[p(0), p(1), p(4) & 1, p(2), p(3)]) as u64)
        }
        "LockFileEx" | "UnlockFileEx" => ok(1),
        "VirtualQueryEx" => ok(win32(ctx, memory, "KERNEL32.dll", "VirtualQuery", &[p(1), p(2), p(3), 0]) as u64),
        "GetNamedSecurityInfoW" => {
            for index in 3..7 {
                if p(index) != 0 {
                    let _ = memory.write_ptr(p(index), 0);
                }
            }
            if p(7) != 0 {
                let descriptor = alloc(ctx, 40);
                let _ = memory.write_bytes(descriptor, &[1, 0, 0x04, 0x80, 0, 0, 0, 0]);
                let _ = memory.write_ptr(p(7), descriptor as u64);
            }
            ok(0)
        }
        // No devices to enumerate.
        "SetupDiGetClassDevsW" => ok(u64::MAX),
        "SetupDiDestroyDeviceInfoList" => ok(1),
        "SetupDiEnumDeviceInfo" => {
            set_error(ctx, 259); // ERROR_NO_MORE_ITEMS
            ok(0)
        }
        "SetWinEventHook" => ok(0x3E70),
        "UnhookWinEvent" | "UnregisterDeviceNotification" => ok(1),
        "RegisterDeviceNotificationW" => ok(0xDE70),
        "EnumDisplayDevicesW" => {
            // (device, index, DISPLAY_DEVICEW, flags): one display.
            if p(0) != 0 || p(1) != 0 {
                return ok(0);
            }
            let info = p(2);
            let size = memory.read_u32(info).unwrap_or(840);
            let _ = memory.write_bytes(info + 4, &vec![0u8; size.saturating_sub(4).min(836) as usize]);
            let name: Vec<u16> = r"\\.\DISPLAY1".encode_utf16().collect();
            unicode::write_wide(memory, info + 4, &name, true);
            let string: Vec<u16> = "OTW Display".encode_utf16().collect();
            unicode::write_wide(memory, info + 68, &string, true);
            let _ = memory.write_u32(info + 324, 0x5); // ATTACHED_TO_DESKTOP | PRIMARY_DEVICE
            ok(1)
        }
        "SHGetPropertyStoreForWindow" => {
            let _ = memory.write_ptr(p(2), 0);
            ok(0x8000_4001)
        }
        "SetCurrentProcessExplicitAppUserModelID" => ok(0),
        "GetCurrentProcessExplicitAppUserModelID" => {
            let _ = memory.write_ptr(p(0), 0);
            ok(0x8000_4005)
        }
        "GetComputerNameExA" | "GetComputerNameExW" => {
            // (format, buffer, &size)
            let text = "OTW";
            let capacity = memory.read_u32(p(2)).unwrap_or(0);
            let _ = memory.write_u32(p(2), if (capacity as usize) > text.len() { text.len() } else { text.len() + 1 } as u32);
            if p(1) == 0 || (capacity as usize) <= text.len() {
                set_error(ctx, 234); // ERROR_MORE_DATA
                return ok(0);
            }
            if name.ends_with('W') {
                unicode::write_wide(memory, p(1), &text.encode_utf16().collect::<Vec<_>>(), true);
            } else {
                let _ = memory.write_bytes(p(1), b"OTW\0");
            }
            ok(1)
        }
        // A 64-bit request: reservations of 4 GiB or more go above 4 GiB.
        "VirtualAlloc" => {
            let trace = std::env::var_os("OTW_TRACE_VMEM").is_some();
            let high = if vmem::owns_high(ctx, arg(0)) {
                Some(vmem::commit_high(ctx, memory, arg(0), arg(1)))
            } else if arg(1) >= vmem::HIGH_THRESHOLD && p(2) & 0x2000 != 0 {
                Some(vmem::reserve_high(ctx, memory, arg(0), arg(1), p(2)))
            } else {
                None
            };
            if let Some(result) = high {
                if trace {
                    eprintln!("[vmem] high alloc {:#x} size {:#x} type {:#x} -> {result:x?}", arg(0), arg(1), p(2));
                }
                return match result {
                    Ok(address) => ok(address),
                    Err(code) => {
                        set_error(ctx, code);
                        ok(0)
                    }
                };
            }
            if arg(1) > u32::MAX as u64 || arg(0) > u32::MAX as u64 {
                if std::env::var_os("OTW_TRACE_VMEM").is_some() {
                    eprintln!("[vmem] alloc {:#x} size {:#x} type {:#x} too large", arg(0), arg(1), p(2));
                }
                set_error(ctx, 8);
                return ok(0);
            }
            ok(win32(ctx, memory, "KERNEL32.dll", "VirtualAlloc", &[p(0), p(1), p(2), p(3)]) as u64)
        }
        "VirtualFree" if vmem::owns_high(ctx, arg(0)) => {
            // Decommitting leaves the pages mapped; releasing unmaps them.
            if p(2) & 0x8000 != 0 {
                if let Err(code) = vmem::release_high(ctx, memory, arg(0)) {
                    set_error(ctx, code);
                    return ok(0);
                }
            }
            ok(1)
        }
        "VirtualFree" => ok(win32(ctx, memory, "KERNEL32.dll", "VirtualFree", &[p(0), p(1), p(2)]) as u64),
        "VirtualProtect" if vmem::owns_high(ctx, arg(0)) => {
            let _ = memory.write_u32(p(3), 4);
            ok(1)
        }
        "VirtualProtect" => ok(win32(ctx, memory, "KERNEL32.dll", "VirtualProtect", &[p(0), p(1), p(2), p(3)]) as u64),
        "VirtualQuery" => match vmem::query_high(ctx, arg(0)) {
            Some(values) => {
                if arg(2) < 48 {
                    set_error(ctx, 24);
                    return ok(0);
                }
                let mut bytes = [0u8; 48];
                for (offset, value, wide) in [(0, values[0], true), (8, values[1], true), (16, values[2], false), (24, values[3], true), (32, values[4], false), (36, values[5], false), (40, values[6], false)] {
                    let width = if wide { 8 } else { 4 };
                    bytes[offset..offset + width].copy_from_slice(&value.to_le_bytes()[..width]);
                }
                let _ = memory.write_bytes(p(1), &bytes);
                ok(48)
            }
            None => ok(win32(ctx, memory, "KERNEL32.dll", "VirtualQuery", &[p(0), p(1), p(2), 0]) as u64),
        },
        "GlobalMemoryStatusEx" => {
            // MEMORYSTATUSEX: load, physical, page file, virtual (total, free).
            let mut bytes = 64u32.to_le_bytes().to_vec();
            bytes.extend_from_slice(&10u32.to_le_bytes());
            for value in [2u64 << 30, 1 << 30, 4 << 30, 3 << 30, 0x7FFE_0000, 0x6000_0000, 0] {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            let _ = memory.write_bytes(p(0), &bytes);
            ok(1)
        }
        "IsValidSecurityDescriptor" => ok(1),
        "GetSystemCpuSetInformation" | "GetDisplayConfigBufferSizes" => {
            if name == "GetDisplayConfigBufferSizes" {
                return ok(50); // ERROR_NOT_SUPPORTED
            }
            set_error(ctx, 50);
            ok(0)
        }
        "GetUserPreferredUILanguages" | "GetThreadPreferredUILanguages" | "GetUserDefaultPreferredUILanguages" => {
            // (flags, &count, buffer, &size): "en-US", a double-NUL list.
            let mut list: Vec<u16> = "en-US".encode_utf16().collect();
            list.extend([0, 0]);
            let _ = memory.write_u32(p(1), 1);
            let capacity = memory.read_u32(p(3)).unwrap_or(0);
            let _ = memory.write_u32(p(3), list.len() as u32);
            if p(2) != 0 {
                if capacity < list.len() as u32 {
                    set_error(ctx, 122);
                    return ok(0);
                }
                unicode::write_wide(memory, p(2), &list, false);
            }
            ok(1)
        }
        "LCIDToLocaleName" => {
            let text: Vec<u16> = "en-US".encode_utf16().collect();
            if p(1) == 0 || p(2) == 0 {
                return ok(text.len() as u64 + 1);
            }
            unicode::write_wide(memory, p(1), &text, true);
            ok(text.len() as u64 + 1)
        }
        "LocaleNameToLCID" => ok(0x409),
        "RoActivateInstance" => {
            let _ = memory.write_ptr(p(1), 0);
            ok(0x8004_0154)
        }
        // WINMM's timer: milliseconds since start.
        "timeGetTime" => ok(ctx.shared.borrow().ticks() & 0xFFFF_FFFF),
        "timeBeginPeriod" | "timeEndPeriod" => ok(0),
        "timeGetDevCaps" => {
            let _ = memory.write_u32(p(0), 1);
            let _ = memory.write_u32(p(0) + 4, 1_000_000);
            ok(0)
        }
        // Event tracing: registered, nothing is written.
        "EventRegister" => {
            let _ = memory.write_u64(p(3), 0x4554_5700);
            ok(0)
        }
        "EventUnregister" | "EventSetInformation" | "EventWriteTransfer" => ok(0),
        "GetSecurityInfo" => {
            // (object, type, information, &owner, &group, &dacl, &sacl, &descriptor):
            // an empty descriptor (everyone may do everything).
            for index in 3..7 {
                if p(index) != 0 {
                    let _ = memory.write_ptr(p(index), 0);
                }
            }
            if p(7) != 0 {
                let descriptor = alloc(ctx, 40);
                let _ = memory.write_bytes(descriptor, &[1, 0, 0x04, 0x80, 0, 0, 0, 0]);
                let _ = memory.write_ptr(p(7), descriptor as u64);
            }
            ok(0)
        }
        "CommandLineToArgvW" => {
            // (line, &count): one block of pointers then the strings.
            let line = unicode::read_wstr(memory, p(0));
            let args = crate::win32::ucrt::split_command_line(&line);
            let table = (args.len() as u32 + 1) * size;
            let total = table + args.iter().map(|a| a.len() as u32 * 2 + 2).sum::<u32>();
            let block = alloc(ctx, total);
            let mut text = block + table;
            for (index, arg) in args.iter().enumerate() {
                let _ = memory.write_ptr(block + index as u32 * size, text as u64);
                unicode::write_wide(memory, text, arg, true);
                text += arg.len() as u32 * 2 + 2;
            }
            let _ = memory.write_ptr(block + args.len() as u32 * size, 0);
            let _ = memory.write_u32(p(1), args.len() as u32);
            ok(block as u64)
        }
        "CoGetApartmentType" => {
            let thread = memory.current_thread();
            let apartment = ctx.shared.borrow().nt.apartments.get(&thread).copied();
            match apartment {
                Some(kind) => {
                    // APTTYPE_STA 0 (APTTYPE_MAINSTA 3 on the first thread), APTTYPE_MTA 1.
                    let kind = if kind == 0 && thread == 1 { 3 } else { kind };
                    let _ = memory.write_u32(p(0), kind);
                    let _ = memory.write_u32(p(1), 0);
                    ok(0)
                }
                None => {
                    let _ = memory.write_u32(p(0), u32::MAX);
                    ok(0x8004_01F0) // CO_E_NOTINITIALIZED
                }
            }
        }
        // ── Window stations and desktops: the one interactive desktop ──
        "GetProcessWindowStation" => ok(0x5715),
        "GetThreadDesktop" | "OpenInputDesktop" | "OpenDesktopW" => ok(0x5D35),
        "CloseDesktop" | "CloseWindowStation" => ok(1),
        "GetUserObjectInformationW" | "GetUserObjectInformationA" => {
            // (object, index, buffer, length, &needed): UOI_FLAGS 1, UOI_NAME 2.
            match p(1) {
                1 => {
                    let _ = memory.write_u32(p(4), 12);
                    if p(3) < 12 {
                        set_error(ctx, 122);
                        return ok(0);
                    }
                    let _ = memory.write_bytes(p(2), &[1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0]);
                    ok(1)
                }
                2 => {
                    let text: Vec<u16> = (if p(0) == 0x5715 { "WinSta0" } else { "Default" }).encode_utf16().collect();
                    let wide = name.ends_with('W');
                    let needed = (text.len() as u32 + 1) * if wide { 2 } else { 1 };
                    let _ = memory.write_u32(p(4), needed);
                    if p(3) < needed {
                        set_error(ctx, 122);
                        return ok(0);
                    }
                    if wide {
                        unicode::write_wide(memory, p(2), &text, true);
                    } else {
                        let mut bytes: Vec<u8> = text.iter().map(|&u| u as u8).collect();
                        bytes.push(0);
                        let _ = memory.write_bytes(p(2), &bytes);
                    }
                    ok(1)
                }
                _ => {
                    set_error(ctx, 87);
                    ok(0)
                }
            }
        }
        // ── Security: descriptors are accepted, nothing is enforced ──
        "InitializeAcl" => {
            // ACL header: revision, size, ACE count.
            let mut header = vec![p(2) as u8, 0];
            header.extend_from_slice(&(p(1) as u16).to_le_bytes());
            header.extend_from_slice(&[0, 0, 0, 0]);
            let _ = memory.write_bytes(p(0), &header);
            ok(1)
        }
        "AddAccessAllowedAce" | "SetSecurityDescriptorGroup" | "SetSecurityDescriptorOwner" | "IsValidSid" => ok(1),
        "CreateWellKnownSid" => {
            // (type, domain, sid, &size): S-1-5-<type>, or S-1-1-0 for
            // Everyone.
            let needed = 12u32;
            let capacity = memory.read_u32(p(3)).unwrap_or(0);
            let _ = memory.write_u32(p(3), needed);
            if p(2) == 0 || capacity < needed {
                set_error(ctx, 122);
                return ok(0);
            }
            let (authority, sub) = if p(0) == 1 { (1u8, 0u32) } else { (5, p(0)) };
            let mut sid = vec![1u8, 1, 0, 0, 0, 0, 0, authority];
            sid.extend_from_slice(&sub.to_le_bytes());
            let _ = memory.write_bytes(p(2), &sid);
            ok(1)
        }
        "GetLengthSid" => {
            let count = memory.read_bytes(p(0) + 1, 1).and_then(|b| b.first().copied()).unwrap_or(0) as u64;
            ok(8 + 4 * count)
        }
        "CopySid" => {
            let count = memory.read_bytes(p(2) + 1, 1).and_then(|b| b.first().copied()).unwrap_or(0) as u32;
            let length = 8 + 4 * count;
            if p(0) < length {
                set_error(ctx, 122);
                return ok(0);
            }
            let bytes = memory.read_bytes(p(2), length as usize).unwrap_or_default();
            let _ = memory.write_bytes(p(1), &bytes);
            ok(1)
        }
        // ── Windows Runtime: not there ──
        // HSTRING: a header holding the length and the characters' address.
        "WindowsCreateStringReference" | "WindowsCreateString" => {
            // (text, length, header, &string) / (text, length, &string)
            let (header, out) = if name == "WindowsCreateString" {
                let block = alloc(ctx, 32 + p(1) * 2 + 2);
                let text = memory.read_bytes(p(0), p(1) as usize * 2).unwrap_or_default();
                let _ = memory.write_bytes(block + 32, &text);
                let _ = memory.write_bytes(block + 32 + p(1) * 2, &[0, 0]);
                let _ = memory.write_u32(block + 4, p(1));
                let _ = memory.write_ptr(block + 8, (block + 32) as u64);
                (block, p(2))
            } else {
                let _ = memory.write_u32(p(2), 1);
                let _ = memory.write_u32(p(2) + 4, p(1));
                let _ = memory.write_ptr(p(2) + 8, arg(0));
                (p(2), p(3))
            };
            let _ = memory.write_ptr(out, if p(1) == 0 { 0 } else { header as u64 });
            ok(0)
        }
        "WindowsGetStringRawBuffer" => {
            if p(0) == 0 {
                if p(1) != 0 {
                    let _ = memory.write_u32(p(1), 0);
                }
                return ok(static_empty(ctx, memory) as u64);
            }
            if p(1) != 0 {
                let length = memory.read_u32(p(0) + 4).unwrap_or(0);
                let _ = memory.write_u32(p(1), length);
            }
            ok(memory.read_ptr(p(0) + 8).unwrap_or(0))
        }
        "WindowsDeleteString" => ok(0),
        "RoGetActivationFactory" => ok(0x8004_0154), // REGDB_E_CLASSNOTREG
        _ => None,
    }
}

fn static_empty(ctx: &ApiContext, memory: &mut dyn GuestMemory) -> u32 {
    let block = alloc(ctx, 4);
    let _ = memory.write_u32(block, 0);
    block
}

fn unicode_upper(unit: u16) -> u16 {
    char::from_u32(unit as u32)
        .map(|c| {
            let mut upper = c.to_uppercase();
            if upper.len() == 1 { upper.next().unwrap_or(c) as u32 as u16 } else { unit }
        })
        .unwrap_or(unit)
}
