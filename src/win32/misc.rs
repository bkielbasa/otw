//! KERNEL32, USER32 and GDI32 functions first needed by Internet Explorer's
//! DLLs and the genuine COMCTL32 it installs.
//!
//! A program runs on one thread, so mutexes and semaphores are always free
//! and never block. DDE has no other programs to talk to: conversations fail
//! to connect. Window regions are not modelled (windows stay rectangular).

use crate::{
    kernel::pe::ImportName,
    win32::{unicode, ApiContext, ApiOutcome, GuestMemory},
};

/// Replace `%NAME%` with the Windows 95 environment's value.
fn expand_environment(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.iter().position(|&b| b == b'%') {
        out.extend_from_slice(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.iter().position(|&b| b == b'%') else {
            out.extend_from_slice(&rest[start..]);
            return out;
        };
        let name = String::from_utf8_lossy(&after[..end]).to_ascii_uppercase();
        let value: Option<&[u8]> = match name.as_str() {
            "WINDIR" | "SYSTEMROOT" => Some(b"C:\\WINDOWS"),
            "TEMP" | "TMP" => Some(b"C:\\WINDOWS\\TEMP"),
            "PROGRAMFILES" => Some(b"C:\\Program Files"),
            "COMSPEC" => Some(b"C:\\COMMAND.COM"),
            "SYSTEMDRIVE" => Some(b"C:"),
            _ => None,
        };
        match value {
            Some(value) => out.extend_from_slice(value),
            None => out.extend_from_slice(&rest[start..start + end + 2]),
        }
        rest = &after[end + 1..];
    }
    out.extend_from_slice(rest);
    out
}

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "DebugBreak" | "GetThreadLocale" => 0,
        "GlobalMemoryStatus" | "AddFontResourceA" | "SetThreadLocale" => 1,
        "IsDBCSLeadByteEx" | "TerminateThread" | "UnhookWindowsHook" | "WaitForInputIdle" => 2,
        "OpenEventA" | "OpenEventW" | "OpenProcess" | "CharNextExA" | "CoGetInterfaceAndReleaseStream" => 3,
        "FormatMessageA" => 7,
        "WNetGetConnectionA" | "WNetGetConnectionW" | "QueryDosDeviceA" | "OpenProcessToken"
        | "LookupPrivilegeValueA" | "ModifyWorldTransform" => 3,
        "EqualSid" | "SetGraphicsMode" | "GetLogicalDriveStringsA" | "RegUnLoadKeyA" | "ExcludeUpdateRgn"
        | "GetWindowWord" | "DdeAccessData" | "ExitWindowsEx" | "WNetConnectionDialog" | "WNetCancelConnectionA"
        | "LZCopy" => 2,
        "MakeCriticalSectionGlobal" => 1,
        "VerifyVersionInfoW" | "VerifyVersionInfoA" => 4,
        "GetFileSizeEx" | "MonitorFromRect" => 2,
        "MonitorFromPoint" => 3,
        "FindFirstFileExW" | "FindFirstFileExA" => 6,
        "EnumDisplayMonitors" | "SHGetKnownFolderPath" | "SHCreateItemFromParsingName" => 4,
        "CreateActCtxW" | "CreateActCtxA" | "ReleaseActCtx" | "GetCurrentActCtx" | "AddRefActCtx" => 1,
        "ActivateActCtx" | "DeactivateActCtx" => 2,
        "FindActCtxSectionStringW" | "FindActCtxSectionStringA" => 5,
        "QueryActCtxW" => 7,
        "ImmGetContext" | "ImmIsIME" | "ImmGetOpenStatus" | "ImmGetVirtualKey" => 1,
        "ImmReleaseContext" | "ImmAssociateContext" | "ImmSetOpenStatus" | "ImmSetCompositionWindow"
        | "ImmSetCandidateWindow" | "ImmSetCompositionFontA" | "ImmGetProperty" => 2,
        "ImmGetConversionStatus" | "ImmGetCandidateWindow" => 3,
        "ImmGetCompositionStringA" | "ImmGetCompositionStringW" | "ImmNotifyIME" | "ImmEscapeA" | "ImmEscapeW" => 4,
        "RegisterEventSourceW" | "RegisterEventSourceA" | "RegisterServiceCtrlHandlerW"
        | "RegisterServiceCtrlHandlerA" | "CreateToolhelp32Snapshot" | "Process32First" | "Process32Next" => 2,
        "RegSetKeySecurity" | "SetProcessWorkingSetSize" => 3,
        "ReadProcessMemory" => 5,
        "GetKeyboardType" | "DdeFreeDataHandle" | "DdeUnaccessData" | "DdeGetLastError" | "RemoveFontResourceA"
        | "LZClose" => 1,
        "OpenServiceA" | "RegLoadKeyA" | "RegSaveKeyA" | "MoveFileExA" | "EnumResourceTypesA" | "SetWindowWord"
        | "CreateDiscardableBitmap" | "WNetCancelConnection2A" | "WNetAddConnectionA" | "LZOpenFileA" => 3,
        "QueryServiceConfigA" | "CreateScalableFontResourceA" | "WNetAddConnection2A" => 4,
        "WNetGetLastErrorA" => 5,
        "VerFindFileA" => 8,
        "ChangeServiceConfigA" => 11,
        "FreeSid" => 1,
        "GetTokenInformation" => 5,
        "AdjustTokenPrivileges" => 6,
        "AllocateAndInitializeSid" => 11,
        "GetQueueStatus" | "WindowFromDC" | "CoFileTimeNow" => 1,
        "GetExitCodeThread" | "ValidateRgn" | "SetEnhMetaFileBits" | "SetMetaFileBitsEx" => 2,
        "OpenFileMappingA" | "OpenFileMappingW" | "OpenFile" | "GetRegionData"
        | "CoMarshalInterThreadInterfaceInStream" => 3,
        "EnumFontsW" | "EnumObjects" | "GetCharABCWidthsA" | "GetCharABCWidthsW" => 4,
        "ToAscii" => 5,
        "CoGetMarshalSizeMax" => 6,
        "ScrollDC" => 7,
        "MaskBlt" => 12,
        "GetSystemPowerStatus" | "CreateMetaFileW" | "RegisterClipboardFormatW" | "IsWindowUnicode"
        | "VkKeyScanA" | "VkKeyScanW" | "MessageBoxIndirectA" | "MessageBoxIndirectW"
        | "CloseServiceHandle" => 1,
        "SleepEx" | "FlushViewOfFile" | "GetComputerNameA" | "CallMsgFilterW" | "CharToOemW"
        | "OemToCharW" | "SetMenuContextHelpId" | "CryptReleaseContext" => 2,
        "ExpandEnvironmentStringsA" | "ExpandEnvironmentStringsW" | "QueueUserAPC"
        | "FileTimeToDosDateTime" | "GetClipboardFormatNameW" | "GetMenuDefaultItem"
        | "GetTextFaceW" | "OpenSCManagerA" => 3,
        "EnumFontFamiliesW" | "CryptSetProvParam" | "CreateIconFromResource" => 4,
        "GetStringTypeExW" | "WaitForMultipleObjectsEx" | "BroadcastSystemMessage"
        | "LookupIconIdFromDirectoryEx" | "CryptAcquireContextA" | "CryptGetProvParam" => 5,
        "MapViewOfFileEx" | "GetCharacterPlacementA" | "GetCharacterPlacementW" => 6,
        "CreateIconFromResourceEx" => 7,
        "DeviceIoControl" | "EnumServicesStatusA" => 8,
        "CreateFontW" => 14,
        "GetUserNameA" | "GetUserNameW" | "InitializeSecurityDescriptor" | "GetKeyboardLayoutList" => 2,
        "SetSecurityDescriptorDacl" => 4,
        "ReleaseMutex" | "GetSystemInfo" | "GetNativeSystemInfo" | "IsBadCodePtr" | "ReinitializeCriticalSection"
        | "SetClipboardViewer" | "DdeDisconnect" | "DdeUninitialize" | "SetKeyboardState"
        | "CreateBitmapIndirect" => 1,
        "ChangeClipboardChain" | "DdeFreeStringHandle" | "GetWindowRgn" | "ShowWindowAsync"
        | "SetObjectOwner" => 2,
        "CreateMutexA" | "CreateMutexW" | "OpenMutexA" | "OpenMutexW" | "OpenSemaphoreA"
        | "OpenSemaphoreW" | "ReleaseSemaphore" | "DdeCreateStringHandleA"
        | "DdeCreateStringHandleW" | "GetUpdateRgn" | "InvalidateRgn" | "SetMenuDefaultItem"
        | "SetWindowRgn" | "GetBitmapBits" | "GetTextCharsetInfo" | "TranslateCharsetInfo" => 3,
        "CreateSemaphoreA" | "CreateSemaphoreW" | "FindResourceExA" | "DdeInitializeA"
        | "DdeInitializeW" | "DdeConnect" | "DdeGetData" | "DdeNameService"
        | "ChildWindowFromPointEx" | "GetCharWidthW" | "GetCharWidth32W" => 4,
        "GetStringTypeExA" | "EnumResourceLanguagesA" | "EnumResourceLanguagesW"
        | "DdeQueryStringA" | "DdeQueryStringW" | "CopyImage" | "FrameRgn" => 5,
        "ThunkConnect32" | "CreateRoundRectRgn" => 6,
        "SendMessageTimeoutA" | "SendMessageTimeoutW" | "DdeCreateDataHandle" => 7,
        "DdeClientTransaction" => 8,
        _ => return None,
    })
}

fn call<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dll: &str,
    name: &str,
    args: &[u32],
) -> u32 {
    crate::win32::dispatch_with_memory(ctx, memory, dll, &ImportName::ByName(name.into()), args, 0)
        .map(|o| o.value)
        .unwrap_or(0)
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
    let value = match name {
        // ── KERNEL32 ──
        // The Windows 95 link to a DLL's 16-bit half; nothing calls through it.
        "ThunkConnect32" => 1,
        "CreateMutexA" | "CreateMutexW" | "CreateSemaphoreA" | "CreateSemaphoreW" => {
            ctx.shared.borrow_mut().alloc_handle()
        }
        // Nothing else has created it.
        "OpenMutexA" | "OpenMutexW" | "OpenSemaphoreA" | "OpenSemaphoreW" => {
            ctx.shared.borrow_mut().last_error = 2;
            0
        }
        "ReleaseMutex" => 1,
        "ReleaseSemaphore" => {
            if args[2] != 0 {
                let _ = memory.write_u32(args[2], 0);
            }
            1
        }
        "DebugBreak" | "ReinitializeCriticalSection" => 0,
        "GlobalMemoryStatus" => {
            // MEMORYSTATUS: 64 MiB of memory, half free.
            let mut status = Vec::new();
            for value in [32u32, 50, 64 << 20, 32 << 20, 128 << 20, 96 << 20, 0x7FFE_0000, 0x7000_0000] {
                status.extend_from_slice(&value.to_le_bytes());
            }
            let _ = memory.write_bytes(args[0], &status);
            0
        }
        "IsDBCSLeadByteEx" => 0,
        "OpenEventA" | "OpenEventW" | "OpenProcess" => {
            ctx.shared.borrow_mut().last_error = 2;
            0
        }
        "TerminateThread" => 0,
        // Windows 95 has no security: these NT functions are not
        // implemented there.
        "OpenProcessToken" | "LookupPrivilegeValueA" | "GetTokenInformation"
        | "AdjustTokenPrivileges" | "AllocateAndInitializeSid" => {
            ctx.shared.borrow_mut().last_error = 120; // ERROR_CALL_NOT_IMPLEMENTED
            0
        }
        "EqualSid" | "FreeSid" => 0,
        "GetFileSizeEx" => match crate::win32::crt::file_size(ctx, args[0]) {
            Some(size) => {
                let _ = memory.write_bytes(args[1], &size.to_le_bytes());
                1
            }
            None => 0,
        },
        // FindFirstFileEx(name, info level, data, search op, filter, flags):
        // the standard information, every match.
        "FindFirstFileExW" | "FindFirstFileExA" => {
            let simple = if name.ends_with('W') { "FindFirstFileW" } else { "FindFirstFileA" };
            call(ctx, memory, "KERNEL32.dll", simple, &[args[0], args[2]])
        }
        // One monitor: the screen.
        "MonitorFromPoint" | "MonitorFromRect" => 0x0001_0001,
        // EnumDisplayMonitors(hdc, clip, proc, data): the one monitor.
        "EnumDisplayMonitors" => {
            let (width, height) = ctx.shared.borrow().gui.screen;
            let rect = ctx.shared.borrow_mut().heap.alloc(16).unwrap_or(0);
            let bytes: Vec<u8> = [0, 0, width, height].iter().flat_map(|v| (*v as u32).to_le_bytes()).collect();
            let _ = memory.write_bytes(rect, &bytes);
            if args[2] != 0 {
                memory.call_guest(args[2], &[0x0001_0001, args[0], rect, args[3]]);
            }
            ctx.shared.borrow_mut().heap.free(rect);
            1
        }
        // SHGetKnownFolderPath(folder id, flags, token, &path): the folders
        // that have a CSIDL, as a CoTaskMemAlloc'd string.
        "SHGetKnownFolderPath" => {
            let id = memory.read_bytes(args[0], 16).unwrap_or_default();
            let known = [
                ("B4BFCC3A-DB2C-424C-B029-7FE99A87C641", 0x10), // Desktop
                ("FDD39AD0-238F-46AF-ADB4-6C85480369C7", 0x05), // Documents
                ("33E28130-4E1E-4676-835A-98395C3BC3BB", 0x05), // Pictures
                ("3EB685DB-65F9-4CF6-A03A-E3EF65729F3D", 0x1A), // RoamingAppData
                ("F1B32785-6FBA-4FCF-9D55-7B8E7F157091", 0x1C), // LocalAppData
                ("62AB5D82-FDC1-4DC3-A9DD-070D1D495D97", 0x23), // ProgramData
                ("905E63B6-C1BF-494E-B29C-65B732D3D21A", 0x26), // ProgramFiles
                ("6D809377-6AF0-444B-8957-A3773F02200E", 0x26), // ProgramFilesX64
                ("F38BF404-1D43-42F2-9305-67DE0B28FC23", 0x24), // Windows
                ("1AC14E77-02E7-4E5D-B744-2EB1AE5198B7", 0x25), // System
                ("A77F5D77-2E2B-44C3-A6A2-ABA601054A51", 0x02), // Programs
                ("625B53C3-AB48-4EC1-BA1F-A1EF4146FC19", 0x0B), // StartMenu
                ("B97D20BB-F46A-4C97-BA10-5E3608430854", 0x07), // Startup
                ("FD228CB7-AE11-4AE3-864C-16F3910AB8FE", 0x14), // Fonts
            ];
            let csidl = known.iter().find(|(text, _)| crate::win32::shell::parse_guid(text).is_some_and(|g| g.as_slice() == id.as_slice())).map(|(_, c)| *c);
            match csidl.and_then(crate::win32::unicode::special_folder) {
                Some(path) => {
                    let units: Vec<u16> = path.encode_utf16().chain([0]).collect();
                    let block = ctx.shared.borrow_mut().heap.alloc(units.len() as u32 * 2).unwrap_or(0);
                    crate::win32::unicode::write_wide(memory, block, &units, false);
                    if let Some(host) = crate::win32::crt::host_path(path.as_bytes()) {
                        let _ = std::fs::create_dir_all(host);
                    }
                    let _ = memory.write_ptr(args[3], u64::from(block));
                    0
                }
                None => {
                    let _ = memory.write_ptr(args[3], 0);
                    0x8000_4005 // E_FAIL
                }
            }
        }
        // Shell items (IShellItem) are not there: callers fall back.
        "SHCreateItemFromParsingName" => {
            let _ = memory.write_ptr(args[3], 0);
            0x8000_4001 // E_NOTIMPL
        }
        "VerifyVersionInfoW" | "VerifyVersionInfoA" => {
            crate::win32::unicode::verify_version(ctx, memory, args, name.ends_with('W'))
        }
        // Activation contexts (side-by-side manifests): none, as on
        // Windows before XP; programs then use the system's DLLs.
        "CreateActCtxW" | "CreateActCtxA" => {
            ctx.shared.borrow_mut().last_error = 2; // ERROR_FILE_NOT_FOUND
            u32::MAX
        }
        "ActivateActCtx" | "GetCurrentActCtx" => {
            let out = if name == "ActivateActCtx" { args[1] } else { args[0] };
            if out != 0 {
                let _ = memory.write_ptr(out, 0);
            }
            1
        }
        "DeactivateActCtx" | "AddRefActCtx" => 1,
        "ReleaseActCtx" => 0,
        // QueryActCtxW(flags, context, subinstance, class, buffer, size,
        // &needed): a module has no activation context of its own.
        "QueryActCtxW" => {
            let size = args[5].min(64);
            if args[4] != 0 && size > 0 {
                let _ = memory.write_bytes(args[4], &vec![0u8; size as usize]);
            }
            if args[6] != 0 {
                let _ = memory.write_u32(args[6], size);
            }
            1
        }
        "FindActCtxSectionStringW" | "FindActCtxSectionStringA" => {
            ctx.shared.borrow_mut().last_error = 0x36B7; // ERROR_SXS_KEY_NOT_FOUND
            0
        }
        // IMM32: no input method editors (Western Windows 95).
        "ImmGetContext" | "ImmIsIME" | "ImmGetOpenStatus" | "ImmAssociateContext" | "ImmGetProperty"
        | "ImmGetConversionStatus" | "ImmGetCandidateWindow" | "ImmNotifyIME" | "ImmEscapeA" | "ImmEscapeW" => 0,
        "ImmGetVirtualKey" => 0,
        "ImmReleaseContext" | "ImmSetOpenStatus" | "ImmSetCompositionWindow" | "ImmSetCandidateWindow"
        | "ImmSetCompositionFontA" => 1,
        "ImmGetCompositionStringA" | "ImmGetCompositionStringW" => (-1i32) as u32, // IMM_ERROR_NODATA
        // One process: no other process to list or read.
        "MakeCriticalSectionGlobal" => 0,
        "CreateToolhelp32Snapshot" => u32::MAX,
        "Process32First" | "Process32Next" | "ReadProcessMemory" => {
            ctx.shared.borrow_mut().last_error = 18; // ERROR_NO_MORE_FILES
            0
        }
        "SetProcessWorkingSetSize" => 1,
        // No event log or service control manager.
        "RegisterEventSourceW" | "RegisterEventSourceA" => 0x5E0,
        "RegisterServiceCtrlHandlerW" | "RegisterServiceCtrlHandlerA" => 0,
        "RegSetKeySecurity" => 0,
        // No services; registry hives cannot be loaded or saved.
        "OpenServiceA" | "QueryServiceConfigA" | "ChangeServiceConfigA" => {
            ctx.shared.borrow_mut().last_error = 120;
            0
        }
        "RegLoadKeyA" | "RegUnLoadKeyA" | "RegSaveKeyA" => 120,
        "MoveFileExA" => {
            // (from, to, flags): a delayed move (until restart) is not
            // available on Windows 95.
            const MOVEFILE_REPLACE_EXISTING: u32 = 1;
            const MOVEFILE_DELAY_UNTIL_REBOOT: u32 = 4;
            if args[2] & MOVEFILE_DELAY_UNTIL_REBOOT != 0 {
                ctx.shared.borrow_mut().last_error = 120;
                0
            } else {
                let from = crate::win32::read_cstr(memory, args[0]);
                let to = if args[1] == 0 { Vec::new() } else { crate::win32::read_cstr(memory, args[1]) };
                let from_host = crate::win32::crt::host_path_in(ctx, &from);
                let result = match (from_host, to.is_empty()) {
                    (Some(from), true) => std::fs::remove_file(from).is_ok(),
                    (Some(from), false) => match crate::win32::crt::host_path_in(ctx, &to) {
                        Some(to) if to.exists() && args[2] & MOVEFILE_REPLACE_EXISTING == 0 => false,
                        Some(to) => std::fs::rename(from, to).is_ok(),
                        None => false,
                    },
                    _ => false,
                };
                if !result {
                    ctx.shared.borrow_mut().last_error = 5;
                }
                u32::from(result)
            }
        }
        "EnumResourceTypesA" => 1,
        // Drive C: (and D:).
        "GetLogicalDriveStringsA" => {
            let list: &[u8] = if crate::win32::crt::drive_d_root().is_some() { b"C:\\\0D:\\\0\0" } else { b"C:\\\0\0" };
            if args[0] as usize >= list.len() {
                let _ = memory.write_bytes(args[1], list);
            }
            (list.len() - 1) as u32
        }
        "SetGraphicsMode" => 1, // GM_COMPATIBLE
        "ModifyWorldTransform" => 0,
        // No device names are defined.
        "QueryDosDeviceA" => {
            ctx.shared.borrow_mut().last_error = 2;
            0
        }
        // No network drives.
        "WNetGetConnectionA" | "WNetGetConnectionW" => 2250, // ERROR_NOT_CONNECTED
        "GetExitCodeThread" => {
            let _ = memory.write_u32(args[1], 0);
            1
        }
        "OpenFileMappingA" | "OpenFileMappingW" => {
            ctx.shared.borrow_mut().last_error = 2;
            0
        }
        "OpenFile" => {
            // OpenFile(path, OFSTRUCT*, style): OFSTRUCT { cBytes, fFixedDisk,
            // nErrCode, reserved[4], szPathName[128] }.
            const OF_EXIST: u32 = 0x4000;
            const OF_CREATE: u32 = 0x1000;
            let path = crate::win32::read_cstr(memory, args[0]);
            let full = crate::win32::crt::absolute(ctx, &path);
            if args[1] != 0 {
                let mut record = vec![0u8; 136];
                record[0] = 136;
                record[1] = 1;
                let length = full.len().min(127);
                record[8..8 + length].copy_from_slice(&full[..length]);
                let _ = memory.write_bytes(args[1], &record);
            }
            let exists = crate::win32::crt::host_path_in(ctx, &path).is_some_and(|host| host.exists());
            if args[2] & OF_EXIST != 0 {
                if exists { 1 } else { u32::MAX }
            } else {
                let access = match args[2] & 3 {
                    0 => 0x8000_0000,
                    1 => 0x4000_0000,
                    _ => 0xC000_0000,
                };
                let disposition = if args[2] & OF_CREATE != 0 { 2 } else { 3 };
                let temp = ctx.shared.borrow_mut().heap.alloc(path.len() as u32 + 1).unwrap_or(0);
                let _ = memory.write_bytes(temp, &[path.as_slice(), &[0]].concat());
                let handle = call(ctx, memory, "KERNEL32.dll", "CreateFileA", &[temp, access, 3, 0, disposition, 0x80, 0]);
                ctx.shared.borrow_mut().heap.free(temp);
                handle
            }
        }
        "CoFileTimeNow" => call(ctx, memory, "KERNEL32.dll", "GetSystemTimeAsFileTime", &args[..1]),
        "CoGetMarshalSizeMax" => {
            let _ = memory.write_u32(args[0], 0);
            0x8000_4001
        }
        "CoMarshalInterThreadInterfaceInStream" => {
            let _ = memory.write_u32(args[2], 0);
            0x8000_4001
        }
        "ExpandEnvironmentStringsA" | "ExpandEnvironmentStringsW" => {
            let wide = name.ends_with('W');
            let source = if wide {
                unicode::wide_to_ansi(&unicode::read_wstr(memory, args[0]))
            } else {
                crate::win32::read_cstr(memory, args[0])
            };
            let mut text = expand_environment(&source);
            text.push(0);
            let needed = text.len() as u32;
            if args[1] != 0 && args[2] >= needed {
                if wide {
                    let units = unicode::ansi_to_wide(&text);
                    unicode::write_wide(memory, args[1], &units, false);
                } else {
                    let _ = memory.write_bytes(args[1], &text);
                }
            }
            needed
        }
        "GetStringTypeExW" => call(ctx, memory, "KERNEL32.dll", "GetStringTypeW", &args[1..5]),
        "SleepEx" => {
            call(ctx, memory, "KERNEL32.dll", "Sleep", &args[..1]);
            0
        }
        "WaitForMultipleObjectsEx" => 0,
        "QueueUserAPC" | "DeviceIoControl" => 0,
        "GetSystemPowerStatus" => {
            // AC power, no battery.
            let _ = memory.write_bytes(args[0], &[1, 128, 255, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
            1
        }
        "FileTimeToDosDateTime" => {
            // (FILETIME*, &date, &time)
            let low = memory.read_u32(args[0]).unwrap_or(0) as u64;
            let high = memory.read_u32(args[0] + 4).unwrap_or(0) as u64;
            let seconds = ((high << 32 | low) / 10_000_000).saturating_sub(11_644_473_600) as i64;
            let days = seconds.div_euclid(86_400);
            let rest = seconds.rem_euclid(86_400);
            // Civil date from days since 1970 (Howard Hinnant's algorithm).
            let z = days + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z - era * 146_097;
            let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let day = doy - (153 * mp + 2) / 5 + 1;
            let month = if mp < 10 { mp + 3 } else { mp - 9 };
            let year = yoe + era * 400 + i64::from(month <= 2);
            if year < 1980 {
                return Some(ApiOutcome::new(0, count));
            }
            let date = ((year - 1980) << 9 | month << 5 | day) as u32;
            let time = ((rest / 3600) << 11 | (rest / 60 % 60) << 5 | (rest % 60) / 2) as u32;
            let _ = memory.write_bytes(args[1], &(date as u16).to_le_bytes());
            let _ = memory.write_bytes(args[2], &(time as u16).to_le_bytes());
            1
        }
        "FlushViewOfFile" => 1,
        "MapViewOfFileEx" => call(ctx, memory, "KERNEL32.dll", "MapViewOfFile", &args[..5]),
        "GetComputerNameA" => {
            let capacity = memory.read_u32(args[1]).unwrap_or(0);
            if capacity < 4 {
                let _ = memory.write_u32(args[1], 4);
                ctx.shared.borrow_mut().last_error = 111; // ERROR_BUFFER_OVERFLOW
                0
            } else {
                let _ = memory.write_bytes(args[0], b"OTW\0");
                let _ = memory.write_u32(args[1], 3);
                1
            }
        }
        // No services, no cryptographic providers.
        "OpenSCManagerA" => {
            ctx.shared.borrow_mut().last_error = 120; // ERROR_CALL_NOT_IMPLEMENTED
            0
        }
        "CloseServiceHandle" | "CryptReleaseContext" => 1,
        "EnumServicesStatusA" | "CryptGetProvParam" | "CryptSetProvParam" => 0,
        "CryptAcquireContextA" => {
            let _ = memory.write_u32(args[0], 0);
            ctx.shared.borrow_mut().last_error = 0x8009_0016; // NTE_BAD_KEYSET
            0
        }
        // ── ADVAPI32 ──
        "GetUserNameA" | "GetUserNameW" => {
            // (buffer, &size in characters, including the terminator)
            let user = b"OTW";
            let capacity = memory.read_u32(args[1]).unwrap_or(0);
            let _ = memory.write_u32(args[1], user.len() as u32 + 1);
            if capacity <= user.len() as u32 {
                ctx.shared.borrow_mut().last_error = 122;
                0
            } else {
                let data: Vec<u8> = if name.ends_with('W') {
                    user.iter().flat_map(|&b| [b, 0]).chain([0, 0]).collect()
                } else {
                    user.iter().copied().chain([0]).collect()
                };
                let _ = memory.write_bytes(args[0], &data);
                1
            }
        }
        "InitializeSecurityDescriptor" | "SetSecurityDescriptorDacl" => 1,
        "FormatMessageA" => format_message(ctx, memory, args),
        "GetThreadLocale" => 0x0409,
        "SetThreadLocale" => 1,
        "IsBadCodePtr" => 0,
        "GetSystemInfo" | "GetNativeSystemInfo" => {
            // SYSTEM_INFO in the process's layout: a Pentium for 32-bit
            // programs, an x64 processor for 64-bit ones.
            let (architecture, kind, level) = if memory.ptr_size() == 8 { (9, 8664, 6) } else { (0, 586, 5) };
            let values = [architecture, 0, 0x1000, 0x1_0000, 0x7FFE_FFFF, 1, 1, kind, 0x1_0000, level, 0];
            crate::win32::x64::write_struct(memory, &crate::win32::x64::SYSTEM_INFO, args[0], &values);
            0
        }
        "GetStringTypeExA" => call(ctx, memory, "KERNEL32.dll", "GetStringTypeA", args),
        // FindResourceEx(module, type, name, language)
        "FindResourceExA" => call(ctx, memory, "KERNEL32.dll", "FindResourceA", &[args[0], args[2], args[1]]),
        "EnumResourceLanguagesA" | "EnumResourceLanguagesW" => 0,

        // ── USER32 ──
        "SetClipboardViewer" => 0,
        "CharNextExA" => {
            let byte = memory.read_bytes(args[1], 1).unwrap_or_default();
            if byte.first().copied().unwrap_or(0) == 0 { args[1] } else { args[1] + 1 }
        }
        "UnhookWindowsHook" => 1,
        // An enhanced 101/102-key keyboard with 12 function keys.
        "GetKeyboardType" => match args[0] {
            0 => 4,
            1 => 0,
            _ => 12,
        },
        "ExcludeUpdateRgn" => 1, // NULLREGION
        "GetWindowWord" => call(ctx, memory, "USER32.dll", "GetWindowLongA", &args[..2]) & 0xFFFF,
        "SetWindowWord" => {
            let old = call(ctx, memory, "USER32.dll", "GetWindowLongA", &args[..2]);
            call(ctx, memory, "USER32.dll", "SetWindowLongA", &[args[0], args[1], (old & 0xFFFF_0000) | (args[2] & 0xFFFF)]);
            old & 0xFFFF
        }
        "DdeFreeDataHandle" | "DdeUnaccessData" => 1,
        "DdeAccessData" | "DdeGetLastError" => 0,
        "ExitWindowsEx" => {
            // Windows restarts: what programs asked to run once at the next
            // start (installers registering their DLLs) runs now.
            eprintln!("[process] ExitWindowsEx {:#x}: restart requested", args[0]);
            ctx.shared.borrow_mut().restart_requested = true;
            run_once(ctx, memory);
            1
        }
        // No network connections.
        "WNetCancelConnection2A" | "WNetCancelConnectionA" => 2250,
        "WNetAddConnectionA" | "WNetAddConnection2A" => 50, // ERROR_NOT_SUPPORTED
        "WNetConnectionDialog" => 1223, // ERROR_CANCELLED
        "WNetGetLastErrorA" => 0,
        // LZ32 on uncompressed files: plain file handles.
        "LZOpenFileA" => call(ctx, memory, "KERNEL32.dll", "OpenFile", &args[..3]),
        "LZClose" => call(ctx, memory, "KERNEL32.dll", "CloseHandle", &args[..1]),
        "LZCopy" => {
            let buffer = ctx.shared.borrow_mut().heap.alloc(0x10000 + 4).unwrap_or(0);
            let read = buffer + 0x10000;
            let mut total = 0u32;
            loop {
                let _ = memory.write_u32(read, 0);
                call(ctx, memory, "KERNEL32.dll", "ReadFile", &[args[0], buffer, 0x10000, read, 0]);
                let count = memory.read_u32(read).unwrap_or(0);
                if count == 0 {
                    break;
                }
                call(ctx, memory, "KERNEL32.dll", "WriteFile", &[args[1], buffer, count, read, 0]);
                total += count;
            }
            ctx.shared.borrow_mut().heap.free(buffer);
            total
        }
        "GetQueueStatus" => 0,
        "WindowFromDC" => 0,
        "ValidateRgn" => call(ctx, memory, "USER32.dll", "ValidateRect", &[args[0], 0]),
        "ScrollDC" => {
            if args[6] != 0 {
                let _ = memory.write_bytes(args[6], &[0; 16]);
            }
            1
        }
        "ToAscii" => {
            // (virtual key, scan code, key state, &char, flags)
            let vk = args[0] as u8;
            let shift = memory.read_bytes(args[2] + 0x10, 1).map(|b| b[0] & 0x80 != 0).unwrap_or(false);
            let c = match vk {
                b'A'..=b'Z' if !shift => vk + 32,
                b'A'..=b'Z' | b'0'..=b'9' | b' ' => vk,
                0x0D => b'\r',
                0x08 => 8,
                0x09 => b'\t',
                0x1B => 0x1B,
                _ => 0,
            };
            if c == 0 {
                0
            } else {
                let _ = memory.write_bytes(args[3], &[c, 0]);
                1
            }
        }
        "GetKeyboardLayoutList" => {
            if args[0] > 0 && args[1] != 0 {
                let _ = memory.write_u32(args[1], 0x0409_0409);
            }
            1
        }
        "BroadcastSystemMessage" => 1,
        "CallMsgFilterW" => 0,
        "CharToOemW" => {
            let text = unicode::wide_to_ansi(&unicode::read_wstr(memory, args[0]));
            let _ = memory.write_bytes(args[1], &[text.as_slice(), &[0]].concat());
            1
        }
        "OemToCharW" => {
            let text = crate::win32::read_cstr(memory, args[0]);
            unicode::write_wide(memory, args[1], &unicode::ansi_to_wide(&text), true);
            1
        }
        "RegisterClipboardFormatW" => {
            let text = unicode::wide_to_ansi(&unicode::read_wstr(memory, args[0]));
            let temp = ctx.shared.borrow_mut().heap.alloc(text.len() as u32 + 1).unwrap_or(0);
            let _ = memory.write_bytes(temp, &[text.as_slice(), &[0]].concat());
            let format = call(ctx, memory, "USER32.dll", "RegisterClipboardFormatA", &[temp]);
            ctx.shared.borrow_mut().heap.free(temp);
            format
        }
        "GetClipboardFormatNameW" => 0,
        "IsWindowUnicode" => 0,
        "GetMenuDefaultItem" => u32::MAX,
        "SetMenuContextHelpId" => 1,
        "VkKeyScanA" | "VkKeyScanW" => {
            let c = args[0] as u8;
            match c {
                b'a'..=b'z' => (c - 32) as u32,
                b'A'..=b'Z' => c as u32 | 0x100,
                b'0'..=b'9' | b' ' => c as u32,
                b'\r' => 0x0D,
                b'\t' => 0x09,
                8 => 0x08,
                _ => 0xFFFF,
            }
        }
        "MessageBoxIndirectA" | "MessageBoxIndirectW" => {
            // MSGBOXPARAMS: owner 4, instance 8, text 12, caption 16, style 20.
            // MSGBOXPARAMS (process layout): owner, text, caption, style.
            let params = crate::win32::x64::View::new(memory, &crate::win32::x64::MSGBOXPARAMS, args[0]);
            let (owner, text, caption, style) =
                (params.get32(memory, 1), params.get32(memory, 3), params.get32(memory, 4), params.get32(memory, 5));
            let call_name = if name.ends_with('W') { "MessageBoxW" } else { "MessageBoxA" };
            call(ctx, memory, "USER32.dll", call_name, &[owner, text, caption, style & 0xFFFF_FF7F])
        }
        "LookupIconIdFromDirectoryEx" => {
            // (GRPICONDIR*, icon, cx, cy, flags): the entry nearest the size.
            let count = u16::from_le_bytes(
                memory.read_bytes(args[0] + 4, 2).map(|b| [b[0], b[1]]).unwrap_or([0, 0]),
            ) as u32;
            let want = if args[2] == 0 { 32 } else { args[2] } as i32;
            let mut best: Option<(i32, u32)> = None;
            for index in 0..count {
                let entry = memory.read_bytes(args[0] + 6 + index * 14, 14).unwrap_or_else(|| vec![0; 14]);
                let width = if entry[0] == 0 { 256 } else { entry[0] as i32 };
                let id = u16::from_le_bytes([entry[12], entry[13]]) as u32;
                let distance = (width - want).abs();
                if best.is_none_or(|(d, _)| distance < d) {
                    best = Some((distance, id));
                }
            }
            best.map(|(_, id)| id).unwrap_or(0)
        }
        "CreateIconFromResource" | "CreateIconFromResourceEx" => 0,
        "WaitForInputIdle" => 0,
        "CoGetInterfaceAndReleaseStream" => {
            let _ = memory.write_u32(args[2], 0);
            0x8000_4001 // E_NOTIMPL: nothing is marshalled
        }
        "AddFontResourceA" => 0,
        "ChangeClipboardChain" => 0,
        "DdeInitializeA" | "DdeInitializeW" => {
            let _ = memory.write_u32(args[0], 1);
            0 // DMLERR_NO_ERROR
        }
        "DdeCreateStringHandleA" | "DdeCreateStringHandleW" => 1,
        "DdeFreeStringHandle" | "DdeDisconnect" | "DdeUninitialize" => 1,
        "DdeConnect" | "DdeClientTransaction" | "DdeCreateDataHandle" | "DdeGetData"
        | "DdeNameService" | "DdeQueryStringA" | "DdeQueryStringW" => 0,
        "SendMessageTimeoutA" | "SendMessageTimeoutW" => {
            // (window, message, wParam, lParam, flags, timeout, &result)
            let send = if name.ends_with('W') { "SendMessageW" } else { "SendMessageA" };
            let result = call(ctx, memory, "USER32.dll", send, &args[..4]);
            if args[6] != 0 {
                let _ = memory.write_u32(args[6], result);
            }
            1
        }
        "ShowWindowAsync" => call(ctx, memory, "USER32.dll", "ShowWindow", &args[..2]),
        // ChildWindowFromPointEx(parent, POINT, flags)
        "ChildWindowFromPointEx" => {
            call(ctx, memory, "USER32.dll", "ChildWindowFromPoint", &args[..3])
        }
        "InvalidateRgn" => call(ctx, memory, "USER32.dll", "InvalidateRect", &[args[0], 0, args[2]]),
        "GetUpdateRgn" => 1, // NULLREGION
        "GetWindowRgn" => 0, // ERROR: no region
        "SetWindowRgn" | "SetMenuDefaultItem" | "SetKeyboardState" => 1,
        // CopyImage(image, type, cx, cy, flags): the same image.
        "CopyImage" => args[0],

        // ── GDI32 ──
        "CreateBitmapIndirect" => {
            // BITMAP { type, width, height, widthBytes, planes:16, bitsPixel:16, bits }
            let data = memory.read_bytes(args[0], 24).unwrap_or_else(|| vec![0; 24]);
            let u32_at = |at: usize| u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
            let planes = u16::from_le_bytes([data[16], data[17]]) as u32;
            let bits = u16::from_le_bytes([data[18], data[19]]) as u32;
            call(ctx, memory, "GDI32.dll", "CreateBitmap", &[u32_at(4), u32_at(8), planes, bits, u32_at(20)])
        }
        "CreateRoundRectRgn" => call(ctx, memory, "GDI32.dll", "CreateRectRgn", &args[..4]),
        "FrameRgn" => 1,
        "GetBitmapBits" => 0,
        "GetCharWidthW" | "GetCharWidth32W" => call(ctx, memory, "GDI32.dll", "GetCharWidthA", args),
        "GetTextCharsetInfo" => {
            if args[1] != 0 {
                let _ = memory.write_bytes(args[1], &[0; 24]);
            }
            0 // ANSI_CHARSET
        }
        "TranslateCharsetInfo" => {
            // (source, CHARSETINFO*, flags): everything is Western (1252).
            let mut info = Vec::with_capacity(32);
            info.extend_from_slice(&0u32.to_le_bytes()); // ciCharset: ANSI_CHARSET
            info.extend_from_slice(&1252u32.to_le_bytes()); // ciACP
            info.extend_from_slice(&[0; 24]);
            info[8] = 1; // FONTSIGNATURE: Latin 1
            let _ = memory.write_bytes(args[1], &info);
            1
        }
        "SetObjectOwner" => 1,
        "CreateDiscardableBitmap" => call(ctx, memory, "GDI32.dll", "CreateCompatibleBitmap", &args[..3]),
        "CreateScalableFontResourceA" | "RemoveFontResourceA" => 0,
        "SetEnhMetaFileBits" | "SetMetaFileBitsEx" | "GetRegionData" | "EnumObjects" => 0,
        "EnumFontsW" => {
            let filter = (args[1] != 0).then(|| unicode::wide_to_ansi(&unicode::read_wstr(memory, args[1])));
            crate::win32::gdi::enum_font_families(ctx, memory, filter, args[2], args[3], true)
        }
        "GetCharABCWidthsA" | "GetCharABCWidthsW" => {
            // (dc, first, last, ABC*): each width as the B part.
            let (first, last) = (args[1], args[2].min(args[1] + 0xFFFF));
            let widths = ctx.shared.borrow_mut().heap.alloc((last - first + 1) * 4).unwrap_or(0);
            call(ctx, memory, "GDI32.dll", "GetCharWidthA", &[args[0], first.min(255), last.min(255), widths]);
            for index in 0..=(last - first) {
                let width = if first + index <= 255 { memory.read_u32(widths + index * 4).unwrap_or(0) } else { 8 };
                let abc = [0u32, width, 0];
                let bytes: Vec<u8> = abc.iter().flat_map(|v| v.to_le_bytes()).collect();
                let _ = memory.write_bytes(args[3] + index * 12, &bytes);
            }
            ctx.shared.borrow_mut().heap.free(widths);
            1
        }
        "MaskBlt" => {
            // Without the mask: a BitBlt with the foreground operation.
            let rop = args[11] & 0x00FF_FFFF;
            call(ctx, memory, "GDI32.dll", "BitBlt", &[args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7], rop])
        }
        "CreateMetaFileW" => 0,
        "CreateFontW" => {
            let face = unicode::wide_to_ansi(&unicode::read_wstr(memory, args[13]));
            let temp = ctx.shared.borrow_mut().heap.alloc(face.len() as u32 + 1).unwrap_or(0);
            let _ = memory.write_bytes(temp, &[face.as_slice(), &[0]].concat());
            let mut font_args = args[..14].to_vec();
            font_args[13] = if args[13] == 0 { 0 } else { temp };
            let font = call(ctx, memory, "GDI32.dll", "CreateFontA", &font_args);
            ctx.shared.borrow_mut().heap.free(temp);
            font
        }
        "GetTextFaceW" => {
            let temp = ctx.shared.borrow_mut().heap.alloc(64).unwrap_or(0);
            let length = call(ctx, memory, "GDI32.dll", "GetTextFaceA", &[args[0], 64, temp]);
            let face = crate::win32::read_cstr(memory, temp);
            ctx.shared.borrow_mut().heap.free(temp);
            if args[2] != 0 && args[1] > face.len() as u32 {
                unicode::write_wide(memory, args[2], &unicode::ansi_to_wide(&face), true);
            }
            length
        }
        "EnumFontFamiliesW" => {
            let filter = (args[1] != 0).then(|| unicode::wide_to_ansi(&unicode::read_wstr(memory, args[1])));
            crate::win32::gdi::enum_font_families(ctx, memory, filter, args[2], args[3], true)
        }
        "GetCharacterPlacementA" | "GetCharacterPlacementW" => {
            // (dc, text, count, max extent, GCP_RESULTS*, flags): the extent,
            // and each character's advance in lpDx.
            let text: Vec<u8> = if name.ends_with('W') {
                unicode::wide_to_ansi(
                    &memory
                        .read_bytes(args[1], args[2] as usize * 2)
                        .unwrap_or_default()
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect::<Vec<_>>(),
                )
            } else {
                memory.read_bytes(args[1], args[2] as usize).unwrap_or_default()
            };
            let temp = ctx.shared.borrow_mut().heap.alloc(text.len() as u32 + 16).unwrap_or(0);
            let size = temp + text.len() as u32 + 4;
            let _ = memory.write_bytes(temp, &text);
            call(ctx, memory, "GDI32.dll", "GetTextExtentPoint32A", &[args[0], temp, text.len() as u32, size]);
            let width = memory.read_u32(size).unwrap_or(0);
            let height = memory.read_u32(size + 4).unwrap_or(0);
            if args[4] != 0 {
                let dx = memory.read_u32(args[4] + 16).unwrap_or(0);
                for index in 0..text.len() as u32 {
                    let _ = memory.write_bytes(temp, &[text[index as usize]]);
                    call(ctx, memory, "GDI32.dll", "GetTextExtentPoint32A", &[args[0], temp, 1, size]);
                    let advance = memory.read_u32(size).unwrap_or(0);
                    if dx != 0 {
                        let _ = memory.write_u32(dx + index * 4, advance);
                    }
                }
                // nGlyphs at 28 and nMaxFit at 32.
                let _ = memory.write_u32(args[4] + 28, text.len() as u32);
                let _ = memory.write_u32(args[4] + 32, text.len() as u32);
            }
            ctx.shared.borrow_mut().heap.free(temp);
            (height << 16) | (width & 0xFFFF)
        }
        _ => return None,
    };
    Some(ApiOutcome::new(value, count))
}

/// FormatMessageA(flags, source, id, language, buffer, size, arguments):
/// strings from the caller (`%1`... inserted from an argument array) or a
/// generic text for system messages.
fn format_message<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, args: &[u32]) -> u32 {
    const ALLOCATE_BUFFER: u32 = 0x100;
    const IGNORE_INSERTS: u32 = 0x200;
    const FROM_STRING: u32 = 0x400;
    const ARGUMENT_ARRAY: u32 = 0x2000;
    let flags = args[0];
    let pattern = if flags & FROM_STRING != 0 {
        crate::win32::read_cstr(memory, args[1])
    } else {
        format!("Error {}.\r\n", args[2]).into_bytes()
    };
    let mut text = Vec::new();
    let mut index = 0;
    while index < pattern.len() {
        let byte = pattern[index];
        if byte == b'%' && flags & IGNORE_INSERTS == 0 && index + 1 < pattern.len() {
            let digits: Vec<u8> = pattern[index + 1..].iter().take_while(|b| b.is_ascii_digit()).copied().collect();
            if !digits.is_empty() {
                let number: u32 = String::from_utf8_lossy(&digits).parse().unwrap_or(1);
                index += 1 + digits.len();
                // An optional printf format such as !ls! or !d! (the
                // default is !s!).
                let mut spec = b"s".to_vec();
                if pattern.get(index) == Some(&b'!') {
                    if let Some(end) = pattern[index + 1..].iter().position(|&b| b == b'!') {
                        spec = pattern[index + 1..index + 1 + end].to_vec();
                        index += end + 2;
                    }
                }
                let slot = if args[6] == 0 {
                    0
                } else if flags & ARGUMENT_ARRAY != 0 {
                    memory.read_u32(args[6] + (number - 1) * 4).unwrap_or(0)
                } else {
                    // A va_list: a pointer to the arguments.
                    let list = memory.read_u32(args[6]).unwrap_or(0);
                    memory.read_u32(list + (number - 1) * 4).unwrap_or(0)
                };
                let format = [b"%".as_slice(), &spec].concat();
                text.extend(crate::win32::controls::format_printf(memory, &format, &[slot], None));
                continue;
            }
            match pattern[index + 1] {
                b'n' => text.extend_from_slice(b"\r\n"),
                other => text.push(other),
            }
            index += 2;
            continue;
        }
        text.push(byte);
        index += 1;
    }
    let length = text.len() as u32;
    text.push(0);
    if flags & ALLOCATE_BUFFER != 0 {
        let block = ctx.shared.borrow_mut().heap.alloc(text.len() as u32).unwrap_or(0);
        let _ = memory.write_bytes(block, &text);
        let _ = memory.write_u32(args[4], block);
    } else {
        if text.len() as u32 > args[5] {
            ctx.shared.borrow_mut().last_error = 122;
            return 0;
        }
        let _ = memory.write_bytes(args[4], &text);
    }
    length
}

/// Run and remove the commands in `HKLM\...\RunOnce`, as Windows does when
/// it starts.
pub fn run_once<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) {
    const KEY: &str = "HKEY_LOCAL_MACHINE\\Software\\Microsoft\\Windows\\CurrentVersion\\RunOnce";
    const EX: &str = "HKEY_LOCAL_MACHINE\\Software\\Microsoft\\Windows\\CurrentVersion\\RunOnceEx";
    // A RunOnce command may stop for another restart (IE's RunOnceEx
    // processor does when a DLL it registered must replace one in use);
    // Windows then restarts again, and IE's shell resumes RunOnceEx.
    for boot in 0..5 {
        apply_wininit(ctx);
        let mut commands = crate::win32::registry::values(KEY);
        if commands.is_empty() && boot > 0 && !crate::win32::registry::subkeys(EX).is_empty() {
            commands.push(("RunOnceEx".into(), 1, b"rundll32.exe iernonce.dll,RunOnceExProcess".to_vec()));
        }
        if commands.is_empty() {
            break;
        }
        commands.sort_by(|a, b| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()));
        for (name, _, command) in commands {
            crate::win32::registry::delete_value(KEY, &name);
            let command = String::from_utf8_lossy(&command).into_owned();
            eprintln!("[process] RunOnce {name}: {command}");
            let text = ctx.shared.borrow_mut().heap.alloc(command.len() as u32 + 1).unwrap_or(0);
            let _ = memory.write_bytes(text, &[command.as_bytes(), &[0]].concat());
            let info = ctx.shared.borrow_mut().heap.alloc(16 + 68).unwrap_or(0);
            let _ = memory.write_bytes(info, &[0; 84]);
            let started = call(ctx, memory, "KERNEL32.dll", "CreateProcessA", &[0, text, 0, 0, 0, 0, 0, 0, info + 16, info]);
            if started != 0 {
                let process = memory.read_u32(info).unwrap_or(0);
                call(ctx, memory, "KERNEL32.dll", "WaitForSingleObject", &[process, u32::MAX]);
            }
            ctx.shared.borrow_mut().heap.free(text);
            ctx.shared.borrow_mut().heap.free(info);
        }
        crate::win32::registry::flush();
    }
}

/// Carry out `C:\\WINDOWS\\WININIT.INI`'s `[Rename]` section, as Windows 95
/// does when it starts: files that were in use when an installer replaced
/// them (`target=source`, or `NUL=file` to delete).
fn apply_wininit(ctx: &ApiContext) {
    let Some(ini) = crate::win32::crt::host_path_in(ctx, b"C:\\WINDOWS\\WININIT.INI") else {
        return;
    };
    let Ok(text) = std::fs::read(&ini) else {
        return;
    };
    let text: String = text.iter().map(|&b| b as char).collect();
    let mut in_rename = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_rename = line.eq_ignore_ascii_case("[rename]");
            continue;
        }
        let Some((target, source)) = line.split_once('=').filter(|_| in_rename) else {
            continue;
        };
        let Some(source_host) = crate::win32::crt::host_path_in(ctx, source.trim().as_bytes()) else {
            continue;
        };
        if target.trim().eq_ignore_ascii_case("NUL") {
            let _ = if source_host.is_dir() { std::fs::remove_dir_all(&source_host) } else { std::fs::remove_file(&source_host) };
            eprintln!("[process] WININIT: delete {}", source.trim());
        } else if let Some(target_host) = crate::win32::crt::host_path_in(ctx, target.trim().as_bytes()) {
            let _ = std::fs::remove_file(&target_host);
            let moved = std::fs::rename(&source_host, &target_host).is_ok();
            eprintln!("[process] WININIT: {} -> {}: {}", source.trim(), target.trim(), if moved { "ok" } else { "failed" });
        }
    }
    let _ = std::fs::rename(&ini, ini.with_file_name("WININIT.BAK"));
}

/// `VxDCall` (KERNEL32 ordinals 1-9): Win32 services of VxDs. MSHTML
/// manages its own heap with the VMM's page services; they map onto the
/// subsystem's virtual memory. Other services fail.
pub fn vxd_call<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, args: &[u32]) -> u32 {
    let service = args.first().copied().unwrap_or(0);
    let arg = |i: usize| args.get(i + 1).copied().unwrap_or(0);
    if std::env::var_os("OTW_TRACE_API").is_some() {
        eprintln!("[vxd] service {service:#010x} args {:x?}", &args[1..]);
    }
    match service {
        // PageReserve(page, pages, flags): a page number, or PR_PRIVATE /
        // PR_SHARED / PR_SYSTEM for anywhere. Returns the address.
        0x0001_0000 => {
            let (page, pages) = (arg(0), arg(1));
            let address = if page >= 0x8000_0000 { 0 } else { page << 12 };
            let result = call(ctx, memory, "KERNEL32.dll", "VirtualAlloc", &[address, pages << 12, 0x2000, 4]);
            if result == 0 { u32::MAX } else { result }
        }
        // PageCommit(page, pages, hpd, pager data, flags): memory is
        // committed when reserved.
        0x0001_0001 => {
            let (page, pages) = (arg(0), arg(1));
            let _ = memory.write_bytes(page << 12, &vec![0u8; (pages as usize) << 12]);
            1
        }
        // PageDecommit, PageModifyPermissions.
        0x0001_0002 | 0x0001_000D => 1,
        // PageFree(address, flags).
        0x0001_000A => call(ctx, memory, "KERNEL32.dll", "VirtualFree", &[arg(0), 0, 0x8000]),
        // PageQuery(address, MEMORY_BASIC_INFORMATION, size).
        0x0001_000E => call(ctx, memory, "KERNEL32.dll", "VirtualQuery", &[arg(0), arg(1), arg(2)]),
        _ => 0,
    }
}
