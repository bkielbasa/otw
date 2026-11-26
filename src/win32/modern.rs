//! Functions from Windows versions after 95 that current programs import
//! (Notepad++ among them): fiber-local storage, slim locks and condition
//! variables, one-time initialisation, thread-pool work, the `*Ex` locale
//! functions taking locale names, and many small USER32, SHLWAPI, theme,
//! DWM, crypto and shell functions.
//!
//! Most map onto what the subsystem already has. Where Windows 95 had no
//! such thing (visual styles, the DWM, code signing, cryptographic
//! providers), the functions report what an unthemed system without those
//! services reports, so programs take their fallback paths.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::{
    kernel::pe::ImportName,
    win32::{unicode, ApiContext, ApiOutcome, GuestMemory},
};

const S_OK: u32 = 0;
const E_FAIL: u32 = 0x8000_4005;
const E_NOTIMPL: u32 = 0x8000_4001;
const E_INVALIDARG: u32 = 0x8007_0057;
const INVALID: u32 = u32::MAX;

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "GetTickCount64" | "AreFileApisANSI" | "GetCaretBlinkTime" | "ImageList_EndDrag" | "IsThemeActive" | "IsAppThemed" | "BufferedPaintInit" | "BufferedPaintUnInit" => 0,
        "FlsAlloc" | "FlsGetValue" | "FlsFree" | "InitializeSListHead" | "AcquireSRWLockExclusive" | "AcquireSRWLockShared" | "ReleaseSRWLockExclusive" | "ReleaseSRWLockShared" | "TryAcquireSRWLockExclusive" | "TryAcquireSRWLockShared" | "InitializeSRWLock" | "InitializeConditionVariable" | "WakeAllConditionVariable" | "WakeConditionVariable" | "InitOnceInitialize" | "SubmitThreadpoolWork" | "CloseThreadpoolWork" | "CancelIo" | "UnregisterApplicationRestart" | "IsCharLowerW" | "IsCharAlphaNumericW" | "IsCharAlphaW" | "IsCharUpperW" | "AddClipboardFormatListener" | "RemoveClipboardFormatListener" | "FlashWindowEx" | "PathIsRelativeW" | "PathGetDriveNumberW" | "PathIsNetworkPathW" | "PathStripPathW" | "PathRemoveExtensionW" | "CloseThemeData" | "ImageNtHeader" | "IsNetworkAlive" | "_TrackMouseEvent" | "BufferedPaintStopAllAnimations" | "ImageList_DragShowNolock" | "BCryptDestroyHash" | "CryptMsgClose" | "DwmIsCompositionEnabled" | "GetSidSubAuthorityCount" | "CryptDestroyHash" => 1,
        "FlsSetValue" | "RegisterApplicationRestart" | "GetAncestor" | "SetDCBrushColor" | "WaitForThreadpoolWorkCallbacks" | "FreeLibraryWhenCallbackReturns" | "PathMatchSpecW" | "PathCanonicalizeW" | "OpenThemeData" | "DragQueryPoint" | "Shell_NotifyIconW" | "IsDestinationReachableW" | "ImmSetCompositionFontW" | "DwmGetColorizationColor" | "CertCloseStore" | "Process32FirstW" | "Process32NextW" | "ChangeWindowMessageFilter" | "CryptReleaseContext" | "GetSidSubAuthority" | "BCryptCloseAlgorithmProvider" | "ImageList_DragMove" | "EndBufferedPaint" | "BufferedPaintRenderAnimation" | "EnableThemeDialogTexture" => 2,
        "InitOnceComplete" | "SetWindowTheme" | "ColorAdjustLuma" | "ColorHLSToRGB" | "GetScrollBarInfo" | "WinVerifyTrust" | "GetLongPathNameW" | "CreateThreadpoolWork" | "CheckTokenMembership" | "DrawThemeParentBackground" | "SleepConditionVariableCS" | "ImageList_DragEnter" => 3,
        "ColorRGBToHLS" | "PathCompactPathExW" | "GetLocaleInfoEx" | "GetLocaleInfoW" | "SetLayeredWindowAttributes" | "ChangeWindowMessageFilterEx" | "NotifyWinEvent" | "GetFinalPathNameByHandleW" | "FindFirstStreamW" | "QueryFullProcessImageNameW" | "GetFileInformationByHandleEx" | "InitOnceBeginInitialize" | "InitOnceExecuteOnce" | "SleepConditionVariableSRW" | "PathCreateFromUrlW" | "SHOpenFolderAndSelectItems" | "DwmSetWindowAttribute" | "DwmGetWindowAttribute" | "BCryptOpenAlgorithmProvider" | "BCryptHashData" | "CryptHashData" | "ConvertStringSecurityDescriptorToSecurityDescriptorW" | "LoadIconMetric" | "TaskDialogIndirect" | "InternetCrackUrlW" | "BCryptFinishHash" | "ImageList_BeginDrag" | "CertGetCertificateContextProperty" | "GetApplicationRestartSettings" => 4,
        "CompareStringOrdinal" | "GetProductInfo" | "mouse_event" | "CryptAcquireContextW" | "CryptCreateHash" | "CryptGetHashParam" | "GetThemeColor" | "LoadIconWithScaleDown" | "SHParseDisplayName" | "CryptMsgGetParam" | "CertNameToStrW" | "BeginBufferedPaint" => 5,
        "CopyFileExW" | "ReplaceFileW" | "GetTimeFormatEx" | "AssocQueryStringW" | "DrawThemeBackground" | "GetThemeFont" | "GetThemeTransitionDuration" | "ImmSetCompositionStringW" | "CertGetNameStringW" | "CertFindCertificateInStore" | "GetThemeBackgroundContentRect" | "BCryptGetProperty" => 6,
        "GetDateFormatEx" | "GetThemePartSize" | "GetTextExtentExPointW" | "GetTextExtentExPointA" | "RegGetValueW" | "BCryptCreateHash" => 7,
        "ReadDirectoryChangesW" | "BeginBufferedAnimation" => 8,
        "CompareStringEx" | "LCMapStringEx" | "DrawThemeTextEx" => 9,
        "AlphaBlend" | "GdiAlphaBlend" | "CryptQueryObject" => 11,
        "ReportEventW" | "ReportEventA" => 9,
        "DeregisterEventSource" => 1,
        _ => return None,
    })
}

/// Ordinal-only COMCTL32 exports (version 6).
pub fn comctl32_ordinal(ordinal: u16) -> Option<&'static str> {
    Some(match ordinal {
        345 => "TaskDialogIndirect",
        380 => "LoadIconMetric",
        381 => "LoadIconWithScaleDown",
        _ => return None,
    })
}

fn call<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, dll: &str, name: &str, args: &[u32]) -> u32 {
    crate::win32::dispatch_with_memory(ctx, memory, dll, &ImportName::ByName(name.into()), args, 0)
        .map(|o| o.value)
        .unwrap_or(0)
}

fn set_error(ctx: &ApiContext, code: u32) {
    ctx.shared.borrow_mut().last_error = code;
}

fn counted<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, count: u32) -> Vec<u16> {
    if count as i32 == -1 {
        return unicode::read_wstr(memory, va);
    }
    let bytes = memory.read_bytes(va, count as usize * 2).unwrap_or_default();
    bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

fn write_wide<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, text: &[u16]) {
    unicode::write_wide(memory, va, text, true);
}

fn lower(unit: u16) -> u16 {
    char::from_u32(unit as u32).map(|c| c.to_lowercase().next().unwrap_or(c) as u32 as u16).unwrap_or(unit)
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    let lcid = 0x0409;
    match name {
        // ── KERNEL32 ──
        "GetTickCount64" => {
            let ticks = ctx.shared.borrow().ticks();
            return Some(ApiOutcome::wide(ticks, count));
        }
        "AreFileApisANSI" => ok(1),
        // Fiber-local storage is thread-local storage (no fibers).
        "FlsAlloc" => ok(call(ctx, memory, "KERNEL32.dll", "TlsAlloc", &[])),
        "FlsGetValue" => ok(call(ctx, memory, "KERNEL32.dll", "TlsGetValue", &[args[0]])),
        "FlsSetValue" => ok(call(ctx, memory, "KERNEL32.dll", "TlsSetValue", &[args[0], args[1]])),
        "FlsFree" => ok(call(ctx, memory, "KERNEL32.dll", "TlsFree", &[args[0]])),
        "InitializeSListHead" => {
            let _ = memory.write_bytes(args[0], &[0u8; 16]);
            ok(0)
        }
        // Slim reader/writer locks and condition variables. Threads only
        // change hands when one waits, so a lock is never contended; a
        // condition variable wait lets the other threads run and then
        // returns (a spurious wake-up, which callers must handle anyway).
        "InitializeSRWLock" | "InitializeConditionVariable" => {
            let _ = memory.write_ptr(args[0], 0);
            ok(0)
        }
        "AcquireSRWLockExclusive" | "AcquireSRWLockShared" => {
            crate::win32::sync::srw_acquire(ctx, memory, args[0], name.ends_with("Exclusive"));
            ok(0)
        }
        "ReleaseSRWLockExclusive" | "ReleaseSRWLockShared" => {
            crate::win32::sync::srw_release(ctx, args[0], name.ends_with("Exclusive"));
            ok(0)
        }
        "WakeAllConditionVariable" | "WakeConditionVariable" => {
            crate::win32::sync::wake(ctx, args[0], name == "WakeAllConditionVariable");
            ok(0)
        }
        "TryAcquireSRWLockExclusive" | "TryAcquireSRWLockShared" => {
            let thread = memory.current_thread();
            ok(u32::from(crate::win32::sync::srw_try(ctx, thread, args[0], name.ends_with("Exclusive"))))
        }
        "SleepConditionVariableSRW" | "SleepConditionVariableCS" => {
            // (condition, lock, timeout[, flags]): give up the lock, sleep
            // until woken, take the lock again.
            let shared_mode = name == "SleepConditionVariableSRW" && args[3] & 1 != 0;
            if args[1] != 0 {
                if name == "SleepConditionVariableCS" {
                    call(ctx, memory, "KERNEL32.dll", "LeaveCriticalSection", &[args[1]]);
                } else {
                    crate::win32::sync::srw_release(ctx, args[1], !shared_mode);
                }
            }
            let woken = crate::win32::sync::sleep_on(ctx, memory, args[0], args[2]);
            if args[1] != 0 {
                if name == "SleepConditionVariableCS" {
                    call(ctx, memory, "KERNEL32.dll", "EnterCriticalSection", &[args[1]]);
                } else {
                    crate::win32::sync::srw_acquire(ctx, memory, args[1], !shared_mode);
                }
            }
            if !woken {
                ctx.shared.borrow_mut().last_error = 1460; // ERROR_TIMEOUT
            }
            ok(u32::from(woken))
        }
        // One-time initialisation: the INIT_ONCE holds 0 (not started), 1
        // (running) or 2 (done).
        "InitOnceInitialize" => {
            let _ = memory.write_ptr(args[0], 0);
            ok(0)
        }
        "InitOnceBeginInitialize" => {
            // Another thread's initialisation runs: wait for its end.
            while memory.read_ptr(args[0]).unwrap_or(0) == 1 && args[1] & 2 == 0 && memory.others_running() {
                call(ctx, memory, "KERNEL32.dll", "Sleep", &[1]);
            }
            let state = memory.read_ptr(args[0]).unwrap_or(0);
            let pending = state != 2;
            if pending {
                let _ = memory.write_ptr(args[0], 1);
            }
            let _ = memory.write_u32(args[2], u32::from(pending));
            if args[3] != 0 {
                let _ = memory.write_ptr(args[3], 0);
            }
            ok(1)
        }
        "InitOnceComplete" => {
            let done = args[1] & 4 == 0; // INIT_ONCE_INIT_FAILED
            let _ = memory.write_ptr(args[0], if done { 2 } else { 0 });
            ok(1)
        }
        "InitOnceExecuteOnce" => {
            loop {
                match memory.read_ptr(args[0]).unwrap_or(0) {
                    2 => return ok(1),
                    // Another thread runs the initialisation.
                    1 if memory.others_running() => {
                        call(ctx, memory, "KERNEL32.dll", "Sleep", &[1]);
                    }
                    _ => break,
                }
            }
            let _ = memory.write_ptr(args[0], 1);
            let result = memory.call_guest(args[1], &[args[0], args[2], args[3]]).unwrap_or(0);
            let _ = memory.write_ptr(args[0], if result != 0 { 2 } else { 0 });
            ok(result)
        }
        // Thread-pool work runs at once on the submitting thread.
        "CreateThreadpoolWork" => {
            let work = ctx.shared.borrow_mut().heap.alloc(16).unwrap_or(0);
            let _ = memory.write_u32(work, args[0]);
            let _ = memory.write_u32(work + 4, args[1]);
            ok(work)
        }
        "SubmitThreadpoolWork" => {
            let callback = memory.read_u32(args[0]).unwrap_or(0);
            let context = memory.read_u32(args[0] + 4).unwrap_or(0);
            if callback != 0 {
                memory.call_guest(callback, &[0, context, args[0]]);
            }
            ok(0)
        }
        "CloseThreadpoolWork" => {
            ctx.shared.borrow_mut().heap.free(args[0]);
            ok(0)
        }
        "WaitForThreadpoolWorkCallbacks" | "FreeLibraryWhenCallbackReturns" => ok(0),
        // Locale-name forms of the locale functions: the user's locale.
        "CompareStringEx" => ok(call(ctx, memory, "KERNEL32.dll", "CompareStringW", &[lcid, args[1], args[2], args[3], args[4], args[5]])),
        "LCMapStringEx" => ok(call(ctx, memory, "KERNEL32.dll", "LCMapStringW", &[lcid, args[1], args[2], args[3], args[4], args[5]])),
        "GetLocaleInfoEx" | "GetLocaleInfoW" => ok(get_locale_info(ctx, memory, args[1], args[2], args[3], true)),
        "GetDateFormatEx" => ok(call(ctx, memory, "KERNEL32.dll", "GetDateFormatW", &[lcid, args[1], args[2], args[3], args[4], args[5]])),
        "GetTimeFormatEx" => ok(call(ctx, memory, "KERNEL32.dll", "GetTimeFormatW", &[lcid, args[1], args[2], args[3], args[4], args[5]])),
        "CompareStringOrdinal" => {
            let mut a = counted(memory, args[0], args[1]);
            let mut b = counted(memory, args[2], args[3]);
            if args[4] != 0 {
                a = a.into_iter().map(lower).collect();
                b = b.into_iter().map(lower).collect();
            }
            ok(match a.cmp(&b) {
                std::cmp::Ordering::Less => 1,
                std::cmp::Ordering::Equal => 2,
                std::cmp::Ordering::Greater => 3,
            })
        }
        "GetProductInfo" => {
            let _ = memory.write_u32(args[4], 0x30); // PRODUCT_PROFESSIONAL
            ok(1)
        }
        "RegisterApplicationRestart" | "UnregisterApplicationRestart" => ok(S_OK),
        "GetApplicationRestartSettings" => ok(0x8007_0490), // HRESULT(ERROR_NOT_FOUND)
        "QueryFullProcessImageNameW" => {
            let capacity = memory.read_u32(args[3]).unwrap_or(0);
            let length = call(ctx, memory, "KERNEL32.dll", "GetModuleFileNameW", &[0, args[2], capacity]);
            let _ = memory.write_u32(args[3], length);
            ok(u32::from(length != 0))
        }
        "GetFinalPathNameByHandleW" | "GetFileInformationByHandleEx" => {
            set_error(ctx, 50); // ERROR_NOT_SUPPORTED
            ok(0)
        }
        "FindFirstStreamW" => {
            set_error(ctx, 38); // ERROR_HANDLE_EOF: no alternate streams
            ok(INVALID)
        }
        "CopyFileExW" => ok(call(ctx, memory, "KERNEL32.dll", "CopyFileW", &[args[0], args[1], args[5] & 1])),
        // ReplaceFile(replaced, replacement, backup, ...): the replacement
        // takes the replaced file's place.
        "ReplaceFileW" => ok(call(ctx, memory, "KERNEL32.dll", "MoveFileExW", &[args[1], args[0], 1])),
        "GetLongPathNameW" => {
            let path = unicode::read_wstr(memory, args[0]);
            if args[1] != 0 && (path.len() as u32) < args[2] {
                write_wide(memory, args[1], &path);
                ok(path.len() as u32)
            } else {
                ok(path.len() as u32 + 1)
            }
        }
        "CancelIo" => ok(1),
        "ReadDirectoryChangesW" => {
            set_error(ctx, 50);
            ok(0)
        }
        "Process32FirstW" | "Process32NextW" => {
            set_error(ctx, 18); // ERROR_NO_MORE_FILES
            ok(0)
        }
        // ReportEvent(source, type, category, id, sid, strings, data size,
        // strings, data): shown on the console, as an event log would keep it.
        "ReportEventW" | "ReportEventA" => {
            let size = memory.ptr_size();
            let mut texts = Vec::new();
            for index in 0..(args[5] & 0xFFFF).min(16) {
                let text = memory.read_ptr(args[7] + index * size).unwrap_or(0) as u32;
                texts.push(if name == "ReportEventW" {
                    String::from_utf16_lossy(&unicode::read_wstr(memory, text))
                } else {
                    String::from_utf8_lossy(&crate::win32::read_cstr(memory, text)).into_owned()
                });
            }
            eprintln!("[event log] type {:#x} id {:#x}: {}", args[1], args[3], texts.join(" | "));
            ok(1)
        }
        "DeregisterEventSource" => ok(1),
        // ── USER32 ──
        "GetCaretBlinkTime" => ok(530),
        "ChangeWindowMessageFilterEx" | "ChangeWindowMessageFilter" | "SetLayeredWindowAttributes"
        | "AddClipboardFormatListener" | "RemoveClipboardFormatListener" => ok(1),
        "NotifyWinEvent" | "mouse_event" | "FlashWindowEx" | "GetScrollBarInfo" => ok(0),
        "IsCharLowerW" | "IsCharUpperW" | "IsCharAlphaW" | "IsCharAlphaNumericW" => {
            let c = char::from_u32(args[0] & 0xFFFF).unwrap_or('\0');
            ok(u32::from(match name {
                "IsCharLowerW" => c.is_lowercase(),
                "IsCharUpperW" => c.is_uppercase(),
                "IsCharAlphaW" => c.is_alphabetic(),
                _ => c.is_alphanumeric(),
            }))
        }
        // GetAncestor(window, GA_PARENT 1 / GA_ROOT 2 / GA_ROOTOWNER 3).
        "GetAncestor" => {
            let shared = ctx.shared.borrow();
            let parent_of = |hwnd: u32| shared.gui.window(hwnd).map(|w| w.parent).unwrap_or(0);
            let owner_of = |hwnd: u32| shared.gui.window(hwnd).map(|w| w.owner).unwrap_or(0);
            if shared.gui.window(args[0]).is_none() {
                return ok(0);
            }
            let mut root = args[0];
            while parent_of(root) != 0 {
                root = parent_of(root);
            }
            ok(match args[1] {
                1 => parent_of(args[0]),
                2 => root,
                3 => {
                    let mut top = root;
                    while owner_of(top) != 0 {
                        top = owner_of(top);
                    }
                    top
                }
                _ => 0,
            })
        }
        // ── GDI32 ──
        "SetDCBrushColor" => ok(0),
        // GetTextExtentExPoint(dc, text, length, max width, &fit, extents, &size)
        "GetTextExtentExPointW" | "GetTextExtentExPointA" => {
            let wide_text = name.ends_with('W');
            let extent_of = if wide_text { "GetTextExtentPoint32W" } else { "GetTextExtentPoint32A" };
            let size = ctx.shared.borrow_mut().heap.alloc(8).unwrap_or(0);
            let length = args[2];
            let mut fit = 0;
            for index in 1..=length {
                call(ctx, memory, "GDI32.dll", extent_of, &[args[0], args[1], index, size]);
                let width = memory.read_u32(size).unwrap_or(0);
                if args[5] != 0 {
                    let _ = memory.write_u32(args[5] + (index - 1) * 4, width);
                }
                if (width as i32) <= args[3] as i32 {
                    fit = index;
                }
            }
            let result = call(ctx, memory, "GDI32.dll", extent_of, &[args[0], args[1], length, size]);
            if args[4] != 0 {
                let _ = memory.write_u32(args[4], fit);
            }
            if args[6] != 0 {
                let bytes = memory.read_bytes(size, 8).unwrap_or_else(|| vec![0; 8]);
                let _ = memory.write_bytes(args[6], &bytes);
            }
            ctx.shared.borrow_mut().heap.free(size);
            ok(result)
        }
        // AlphaBlend without the blending: a stretched copy.
        "AlphaBlend" | "GdiAlphaBlend" => ok(call(
            ctx,
            memory,
            "GDI32.dll",
            "StretchBlt",
            &[args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7], args[8], args[9], 0x00CC_0020],
        )),
        // ── ADVAPI32 / crypto: no providers ──
        "RegGetValueW" => {
            // RegGetValue(key, subkey, value, flags, &type, data, &size)
            let key = if args[1] != 0 {
                let slot = ctx.shared.borrow_mut().heap.alloc(8).unwrap_or(0);
                let status = call(ctx, memory, "ADVAPI32.dll", "RegOpenKeyExW", &[args[0], args[1], 0, 0x2_0019, slot]);
                let key = memory.read_u32(slot).unwrap_or(0);
                ctx.shared.borrow_mut().heap.free(slot);
                if status != 0 {
                    return ok(status);
                }
                key
            } else {
                args[0]
            };
            let status = call(ctx, memory, "ADVAPI32.dll", "RegQueryValueExW", &[key, args[2], 0, args[4], args[5], args[6]]);
            if args[1] != 0 {
                call(ctx, memory, "ADVAPI32.dll", "RegCloseKey", &[key]);
            }
            ok(status)
        }
        "CryptAcquireContextW" => {
            set_error(ctx, 0x8009_0016); // NTE_BAD_KEYSET
            ok(0)
        }
        "CryptCreateHash" | "CryptHashData" | "CryptGetHashParam" | "CryptDestroyHash" | "CryptReleaseContext"
        | "ConvertStringSecurityDescriptorToSecurityDescriptorW" | "CryptQueryObject" | "CryptMsgGetParam"
        | "CertGetCertificateContextProperty" | "CertFindCertificateInStore" => ok(0),
        "CertCloseStore" | "CryptMsgClose" => ok(1),
        "CertGetNameStringW" | "CertNameToStrW" => ok(1),
        "CheckTokenMembership" => {
            let _ = memory.write_u32(args[2], 0);
            ok(1)
        }
        "GetSidSubAuthority" | "GetSidSubAuthorityCount" => {
            let block = ctx.shared.borrow_mut().heap.alloc(8).unwrap_or(0);
            let _ = memory.write_bytes(block, &[0; 8]);
            ok(block)
        }
        "BCryptOpenAlgorithmProvider" | "BCryptGetProperty" | "BCryptCreateHash" | "BCryptHashData"
        | "BCryptFinishHash" => ok(0xC000_0225), // STATUS_NOT_FOUND
        "BCryptCloseAlgorithmProvider" | "BCryptDestroyHash" => ok(0),
        "WinVerifyTrust" => ok(0x800B_0100), // TRUST_E_NOSIGNATURE
        // ── SHLWAPI ──
        "PathIsRelativeW" => {
            let path = unicode::read_wstr(memory, args[0]);
            let absolute = path.first() == Some(&(b'\\' as u16)) || path.get(1) == Some(&(b':' as u16));
            ok(u32::from(!absolute))
        }
        "PathGetDriveNumberW" => {
            let path = unicode::read_wstr(memory, args[0]);
            match (path.first(), path.get(1)) {
                (Some(&d), Some(&c)) if c == b':' as u16 && (d as u8).is_ascii_alphabetic() => {
                    ok(((d as u8).to_ascii_uppercase() - b'A') as u32)
                }
                _ => ok(INVALID),
            }
        }
        "PathIsNetworkPathW" => ok(0),
        "PathStripPathW" => {
            let path = unicode::read_wstr(memory, args[0]);
            let start = path.iter().rposition(|&c| c == b'\\' as u16 || c == b'/' as u16).map(|i| i + 1).unwrap_or(0);
            write_wide(memory, args[0], &path[start..]);
            ok(0)
        }
        "PathRemoveExtensionW" => {
            let path = unicode::read_wstr(memory, args[0]);
            let name_start = path.iter().rposition(|&c| c == b'\\' as u16).map(|i| i + 1).unwrap_or(0);
            if let Some(dot) = path[name_start..].iter().rposition(|&c| c == b'.' as u16) {
                write_wide(memory, args[0], &path[..name_start + dot]);
            }
            ok(0)
        }
        "PathCanonicalizeW" => {
            let path = String::from_utf16_lossy(&unicode::read_wstr(memory, args[1]));
            let mut parts: Vec<&str> = Vec::new();
            for part in path.split('\\') {
                match part {
                    "." => {}
                    ".." => {
                        if parts.len() > 1 {
                            parts.pop();
                        }
                    }
                    other => parts.push(other),
                }
            }
            let units: Vec<u16> = parts.join("\\").encode_utf16().collect();
            write_wide(memory, args[0], &units);
            ok(1)
        }
        "PathMatchSpecW" => {
            let name_text = String::from_utf16_lossy(&unicode::read_wstr(memory, args[0])).to_lowercase();
            let specs = String::from_utf16_lossy(&unicode::read_wstr(memory, args[1])).to_lowercase();
            ok(u32::from(specs.split(';').any(|spec| matches_pattern(spec.trim(), &name_text))))
        }
        "PathCompactPathExW" => {
            let path = unicode::read_wstr(memory, args[1]);
            let max = args[2] as usize;
            let shown = if path.len() < max { path } else { path[path.len() + 1 - max..].to_vec() };
            write_wide(memory, args[0], &shown);
            ok(1)
        }
        "PathCreateFromUrlW" | "AssocQueryStringW" => ok(E_INVALIDARG),
        "ColorRGBToHLS" => {
            let (h, l, s) = rgb_to_hls(args[0]);
            for (va, value) in [(args[1], h), (args[2], l), (args[3], s)] {
                if va != 0 {
                    let _ = memory.write_bytes(va, &(value as u16).to_le_bytes());
                }
            }
            ok(0)
        }
        "ColorHLSToRGB" => ok(hls_to_rgb(args[0] as u16, args[1] as u16, args[2] as u16)),
        "ColorAdjustLuma" => {
            let (h, l, s) = rgb_to_hls(args[0]);
            let delta = args[1] as i32;
            let l = if args[2] != 0 { (l as i32 + delta * 240 / 1000).clamp(0, 240) } else { (l as i32 + delta).clamp(0, 240) };
            ok(hls_to_rgb(h as u16, l as u16, s as u16))
        }
        // ── Visual styles, the DWM: not there (classic look) ──
        "OpenThemeData" => ok(0),
        "IsThemeActive" | "IsAppThemed" => ok(0),
        "CloseThemeData" | "SetWindowTheme" | "EnableThemeDialogTexture" | "BufferedPaintInit"
        | "BufferedPaintUnInit" | "BufferedPaintStopAllAnimations" => ok(S_OK),
        "GetThemeColor" | "GetThemeFont" | "GetThemeBackgroundContentRect" | "DrawThemeBackground"
        | "DrawThemeTextEx" | "GetThemePartSize" | "GetThemeTransitionDuration" | "DrawThemeParentBackground" => ok(E_FAIL),
        "BeginBufferedAnimation" | "BeginBufferedPaint" | "BufferedPaintRenderAnimation" | "EndBufferedPaint" => ok(0),
        "DwmGetColorizationColor" | "DwmSetWindowAttribute" => ok(E_FAIL),
        // (window, attribute, buffer, size): nothing set (not cloaked, no
        // special colours); the frame bounds are the window's rectangle.
        // (Firefox takes a window it reads as cloaked for hidden, and then
        // stops drawing it.)
        "DwmGetWindowAttribute" => {
            let _ = memory.write_bytes(args[2], &vec![0u8; args[3].min(256) as usize]);
            const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
            if args[1] == DWMWA_EXTENDED_FRAME_BOUNDS && args[3] >= 16 {
                call(ctx, memory, "USER32.dll", "GetWindowRect", &[args[0], args[2]]);
            }
            ok(S_OK)
        }
        // Composition is on for 64-bit programs (Windows 8 and later
        // cannot turn it off), whose frame pacing waits on DwmFlush.
        "DwmIsCompositionEnabled" => {
            let _ = memory.write_u32(args[0], u32::from(ctx.shared.borrow().x64));
            ok(S_OK)
        }
        // ── COMCTL32 v6 ──
        "_TrackMouseEvent" => ok(1),
        "ImageList_BeginDrag" | "ImageList_EndDrag" | "ImageList_DragEnter" | "ImageList_DragMove"
        | "ImageList_DragShowNolock" => ok(1),
        "LoadIconMetric" | "LoadIconWithScaleDown" => {
            let icon = call(ctx, memory, "USER32.dll", "LoadIconW", &[args[0], args[1]]);
            let out = if name == "LoadIconMetric" { args[3] } else { args[4] };
            let _ = memory.write_ptr(out, u64::from(icon));
            ok(if icon != 0 { S_OK } else { E_FAIL })
        }
        // TaskDialogIndirect(config, &button, &radio, &verified): OK.
        "TaskDialogIndirect" => {
            for (va, value) in [(args[1], 1), (args[2], 0), (args[3], 0)] {
                if va != 0 {
                    let _ = memory.write_u32(va, value);
                }
            }
            ok(S_OK)
        }
        // ── SHELL32, IMM32, WININET, network ──
        "SHParseDisplayName" | "SHOpenFolderAndSelectItems" => {
            if name == "SHParseDisplayName" {
                let _ = memory.write_ptr(args[2], 0);
            }
            ok(E_NOTIMPL)
        }
        "DragQueryPoint" => ok(0),
        "Shell_NotifyIconW" => ok(1),
        "ImmSetCompositionStringW" | "ImmSetCompositionFontW" => ok(1),
        "InternetCrackUrlW" => {
            set_error(ctx, 12006); // ERROR_INTERNET_UNRECOGNIZED_SCHEME
            ok(0)
        }
        "IsNetworkAlive" => {
            let _ = memory.write_u32(args[0], 1); // NETWORK_ALIVE_LAN
            ok(1)
        }
        "IsDestinationReachableW" => ok(0),
        // ImageNtHeader(base): the PE header.
        "ImageNtHeader" => {
            let offset = memory.read_u32(args[0] + 0x3C).unwrap_or(0);
            ok(if offset != 0 { args[0] + offset } else { 0 })
        }
        _ => None,
    }
}

/// A `*`/`?` file pattern match (lower-cased already).
fn matches_pattern(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some(c) => n.first() == Some(c) && go(&p[1..], &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

/// RGB (COLORREF) to hue, luminance, saturation on SHLWAPI's 0-240 scale.
fn rgb_to_hls(color: u32) -> (u32, u32, u32) {
    let (r, g, b) = ((color & 0xFF) as f64 / 255.0, ((color >> 8) & 0xFF) as f64 / 255.0, ((color >> 16) & 0xFF) as f64 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    if max == min {
        return (160, (l * 240.0).round() as u32, 0);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    ((h * 240.0).round() as u32, (l * 240.0).round() as u32, (s * 240.0).round() as u32)
}

fn hls_to_rgb(h: u16, l: u16, s: u16) -> u32 {
    let (h, l, s) = (h as f64 / 240.0, l as f64 / 240.0, s as f64 / 240.0);
    let channel = |p: f64, q: f64, mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    let (r, g, b) = if s == 0.0 {
        (l, l, l)
    } else {
        let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
        let p = 2.0 * l - q;
        (channel(p, q, h + 1.0 / 3.0), channel(p, q, h), channel(p, q, h - 1.0 / 3.0))
    };
    let to = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u32;
    to(r) | to(g) << 8 | to(b) << 16
}


/// The user's locale, English (United States): the value of a LOCALE_*
/// type as text.
pub fn locale_value(lctype: u32) -> Option<String> {
    const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November",
        "December",
    ];
    let text = match lctype {
        0x01 | 0x09 => "0409",
        0x02 => "English (United States)",
        0x03 => "ENU",
        0x04 => "English",
        0x05 | 0x0A => "1",
        0x06 | 0x08 | 0x1002 => "United States",
        0x07 | 0x68 => "USA",
        0x0B => "437",
        0x0C | 0x0F | 0x17 => ",",
        0x0D => "1",
        0x0E | 0x16 => ".",
        0x10 | 0x18 => "3;0",
        0x11 | 0x19 | 0x1A => "2",
        0x12 | 0x24 => "1",
        0x13 => "0123456789",
        0x14 => "$",
        0x15 => "USD",
        0x1B | 0x1C | 0x21 | 0x22 | 0x23 | 0x25 | 0x26 | 0x27 => "0",
        0x1D => "/",
        0x1E => ":",
        0x1F => "M/d/yyyy",
        0x20 => "dddd, MMMM d, yyyy",
        0x28 => "AM",
        0x29 => "PM",
        0x2A..=0x30 => return Some(DAYS[(lctype - 0x2A) as usize].into()),
        0x31..=0x37 => return Some(DAYS[(lctype - 0x31) as usize][..3].into()),
        0x38..=0x43 => return Some(MONTHS[(lctype - 0x38) as usize].into()),
        0x44..=0x4F => return Some(MONTHS[(lctype - 0x44) as usize][..3].into()),
        0x60..=0x66 => return Some(DAYS[(lctype - 0x60) as usize][..2].into()),
        0x50 => "",
        0x51 => "-",
        0x52 => "3",
        0x53 | 0x55 | 0x57 | 0x70 | 0x71 | 0x1005 | 0x100B | 0x100D => "0",
        0x54 | 0x56 | 0x1009 | 0x100A | 0x1010 | 0x1015 | 0x74 | 0x75 => "1",
        0x59 | 0x6D => "en",
        0x5A => "US",
        0x5C | 0x6E | 0x7B => "en-US",
        0x5D => "h:mm:ss",
        0x67 => "eng",
        0x69 => "NaN",
        0x6A => "Infinity",
        0x6B => "-Infinity",
        0x6C => "Latn;",
        0x6F | 0x72 | 0x73 => "English (United States)",
        0x76 => "%",
        0x77 => "\u{2030}",
        0x78 => "MMMM d",
        0x79 => "h:mm tt",
        0x7A => "ENG",
        0x1001 => "English",
        0x1003 => "h:mm:ss tt",
        0x1004 => "1252",
        0x1006 => "MMMM yyyy",
        0x1007 | 0x1008 => "US Dollar",
        0x100C => "6",
        0x1011 => "10000",
        0x1012 => "037",
        0x1014 => "Default",
        _ => return None,
    };
    Some(text.into())
}

/// GetLocaleInfoA/W/Ex(type, buffer, capacity): characters written (or
/// needed, with no buffer); LOCALE_RETURN_NUMBER gives a DWORD.
pub fn get_locale_info<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, lctype: u32, buffer: u32, capacity: u32, wide: bool) -> u32 {
    const RETURN_NUMBER: u32 = 0x2000_0000;
    let Some(text) = locale_value(lctype & 0x0FFF_FFFF) else {
        set_error(ctx, 1004); // ERROR_INVALID_FLAGS
        return 0;
    };
    if lctype & RETURN_NUMBER != 0 {
        let number: u32 = text.parse().unwrap_or(0);
        let needed = if wide { 2 } else { 4 };
        if capacity == 0 {
            return needed;
        }
        if capacity < needed || buffer == 0 {
            set_error(ctx, 122);
            return 0;
        }
        let _ = memory.write_u32(buffer, number);
        return needed;
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let bytes = unicode::wide_to_ansi(&units);
    let needed = if wide { units.len() } else { bytes.len() } as u32 + 1;
    if capacity == 0 {
        return needed;
    }
    if capacity < needed || buffer == 0 {
        set_error(ctx, 122); // ERROR_INSUFFICIENT_BUFFER
        return 0;
    }
    if wide {
        unicode::write_wide(memory, buffer, &units, true);
    } else {
        let mut bytes = bytes;
        bytes.push(0);
        let _ = memory.write_bytes(buffer, &bytes);
    }
    needed
}
