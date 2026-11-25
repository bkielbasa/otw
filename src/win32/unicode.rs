//! The Unicode ("W") half of the Win32 API, on top of the ANSI core.
//!
//! Windows 95 is an ANSI system; programs built for Windows NT/2000/XP call
//! the W functions. As Microsoft's Layer for Unicode did on Windows 9x, a W
//! function here converts its UTF-16 string arguments to ANSI, calls the A
//! function, and converts what it returns back. Windows whose procedures
//! were registered through W functions (`RegisterClassW`,
//! `SetWindowLongW`, `CreateDialogIndirectParamW`, ...) receive their
//! messages with UTF-16 text: [`call_proc`] converts the string parameters
//! of messages around the call.
//!
//! The ANSI code page is Latin-1 (the UI font's character set): characters
//! outside it become `?`.

use crate::{
    kernel::pe::ImportName,
    win32::{dispatch_with_memory, read_cstr, ApiContext, ApiOutcome, GuestMemory},
};

// ── Text ─────────────────────────────────────────────────────────────────

pub fn wide_to_ansi(units: &[u16]) -> Vec<u8> {
    units
        .iter()
        .filter(|&&unit| unit != 0xFEFF) // byte-order mark / zero-width no-break space
        .map(|&unit| if unit < 0x100 { unit as u8 } else { b'?' })
        .collect()
}

pub fn ansi_to_wide(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&byte| byte as u16).collect()
}

/// Read a NUL-terminated UTF-16 string (at most 32K characters).
pub fn read_wstr<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> Vec<u16> {
    let mut units = Vec::new();
    if va == 0 {
        return units;
    }
    let mut offset = 0u32;
    while units.len() < 32768 {
        let Some(bytes) = memory.read_bytes(va.wrapping_add(offset), 64) else {
            break;
        };
        for pair in bytes.chunks_exact(2) {
            let unit = u16::from_le_bytes([pair[0], pair[1]]);
            if unit == 0 {
                return units;
            }
            units.push(unit);
        }
        offset += 64;
    }
    units
}

fn read_wide_n<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, count: usize) -> Vec<u16> {
    memory
        .read_bytes(va, count.min(32768) * 2)
        .unwrap_or_default()
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// Write UTF-16 units, followed by a NUL when `nul`.
pub fn write_wide<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, units: &[u16], nul: bool) {
    let mut bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    if nul {
        bytes.extend_from_slice(&[0, 0]);
    }
    let _ = memory.write_bytes(va, &bytes);
}

/// Copy an ANSI string to a UTF-16 buffer of `capacity` characters
/// (including the NUL), truncating like Win32.
fn store_wide<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, ansi: &[u8], capacity: u32) {
    if va == 0 || capacity == 0 {
        return;
    }
    let count = ansi.len().min(capacity as usize - 1);
    write_wide(memory, va, &ansi_to_wide(&ansi[..count]), true);
}

/// Guest heap blocks that live for one call.
#[derive(Default)]
struct Temps(Vec<u32>);

impl Temps {
    fn alloc(&mut self, ctx: &ApiContext, size: u32) -> u32 {
        let block = ctx.shared.borrow_mut().heap.alloc(size.max(4)).unwrap_or(0);
        if block != 0 {
            self.0.push(block);
        }
        block
    }

    fn bytes<M: GuestMemory + ?Sized>(&mut self, ctx: &ApiContext, memory: &mut M, bytes: &[u8]) -> u32 {
        let block = self.alloc(ctx, bytes.len() as u32 + 2);
        let _ = memory.write_bytes(block, bytes);
        let _ = memory.write_bytes(block + bytes.len() as u32, &[0, 0]);
        block
    }

    fn wide<M: GuestMemory + ?Sized>(&mut self, ctx: &ApiContext, memory: &mut M, units: &[u16]) -> u32 {
        let block = self.alloc(ctx, units.len() as u32 * 2 + 2);
        write_wide(memory, block, units, true);
        block
    }

    fn zeroed<M: GuestMemory + ?Sized>(&mut self, ctx: &ApiContext, memory: &mut M, size: u32) -> u32 {
        let block = self.alloc(ctx, size);
        let _ = memory.write_bytes(block, &vec![0; size.max(4) as usize]);
        block
    }

    fn free(self, ctx: &ApiContext) {
        let mut shared = ctx.shared.borrow_mut();
        for block in self.0 {
            shared.heap.free(block);
        }
    }
}

/// A string argument (`LPCWSTR`) converted to an ANSI copy; resource and
/// atom names (`MAKEINTRESOURCE`) pass through.
fn ansi_arg<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, temps: &mut Temps, va: u32) -> u32 {
    if va < 0x10000 {
        return va;
    }
    let text = wide_to_ansi(&read_wstr(memory, va));
    temps.bytes(ctx, memory, &text)
}

// ── Generic wrappers ────────────────────────────────────────────────────

/// How a W function's argument differs from its A function's.
#[derive(Clone, Copy, Debug)]
enum P {
    /// A NUL-terminated input string.
    In(usize),
    /// An input string of `count` characters (or NUL-terminated if -1).
    Counted(usize, usize),
    /// An output buffer whose size in characters is in another argument.
    Out(usize, usize),
}

/// What a W function becomes.
enum Plan {
    /// The A function with converted strings.
    Wrap(&'static str, &'static [P]),
    /// The same, for a W function taking more arguments than its A
    /// function (the extra ones are ignored).
    WrapN(&'static str, &'static [P], usize),
    /// Implemented here, taking this many stack arguments.
    Native(usize),
}

fn plan(name: &str) -> Option<Plan> {
    use P::*;
    use Plan::*;
    Some(match name {
        // KERNEL32
        "CreateFileW" => Wrap("CreateFileA", &[In(0)]),
        "DeleteFileW" => Wrap("DeleteFileA", &[In(0)]),
        "CreateDirectoryW" => Wrap("CreateDirectoryA", &[In(0)]),
        "SetCurrentDirectoryW" => Wrap("SetCurrentDirectoryA", &[In(0)]),
        "GetCurrentDirectoryW" => Wrap("GetCurrentDirectoryA", &[Out(1, 0)]),
        "GetDriveTypeW" => Wrap("GetDriveTypeA", &[In(0)]),
        "GetVolumeInformationW" => Wrap("GetVolumeInformationA", &[In(0), Out(1, 2), Out(6, 7)]),
        "OutputDebugStringW" => Wrap("OutputDebugStringA", &[In(0)]),
        "GlobalAddAtomW" => Wrap("GlobalAddAtomA", &[In(0)]),
        "GlobalFindAtomW" => Wrap("GlobalFindAtomA", &[In(0)]),
        "LoadLibraryW" => Wrap("LoadLibraryA", &[In(0)]),
        "LoadLibraryExW" => Wrap("LoadLibraryExA", &[In(0)]),
        "GetTempPathW" => Wrap("GetTempPathA", &[Out(1, 0)]),
        "GetModuleFileNameW" => Wrap("GetModuleFileNameA", &[Out(1, 2)]),
        "GetModuleHandleW" => Wrap("GetModuleHandleA", &[In(0)]),
        "MoveFileExW" => WrapN("MoveFileA", &[In(0), In(1)], 3),
        "MoveFileW" => Wrap("MoveFileA", &[In(0), In(1)]),
        "CopyFileW" => Wrap("CopyFileA", &[In(0), In(1)]),
        "FindResourceW" => Wrap("FindResourceA", &[In(1), In(2)]),
        "GetDateFormatW" => Wrap("GetDateFormatA", &[In(3), Out(4, 5)]),
        "GetTimeFormatW" => Wrap("GetTimeFormatA", &[In(3), Out(4, 5)]),
        "GetSystemDirectoryW" => Wrap("GetSystemDirectoryA", &[Out(0, 1)]),
        "GetWindowsDirectoryW" => Wrap("GetWindowsDirectoryA", &[Out(0, 1)]),
        "GetFileAttributesW" => Wrap("GetFileAttributesA", &[In(0)]),
        "SetFileAttributesW" => Wrap("SetFileAttributesA", &[In(0)]),
        "RemoveDirectoryW" => Wrap("RemoveDirectoryA", &[In(0)]),
        "FindResourceExW" | "GetFullPathNameW" | "FindFirstFileW" | "FindNextFileW" | "GetFileAttributesExW"
        | "GetModuleHandleExW" => Native(match name {
            "GetFullPathNameW" | "FindResourceExW" => 4,
            "FindFirstFileW" | "FindNextFileW" => 2,
            _ => 3,
        }),
        "lstrcmpW" | "lstrcmpiW" | "lstrcpyW" | "lstrcatW" => Native(2),
        "lstrcpynW" => Native(3),
        "GetCommandLineW" | "GetEnvironmentStringsW" => Native(0),
        "FreeEnvironmentStringsW" | "GetStartupInfoW" | "GetVersionExW" | "GetVersionExA" => Native(1),
        "GetEnvironmentVariableW" | "GetEnvironmentVariableA" => Native(3),
        "SetEnvironmentVariableW" | "SetEnvironmentVariableA" => Native(2),
        "CompareStringW" | "LCMapStringW" | "CompareStringA" | "LCMapStringA" => Native(6),
        "GetStringTypeA" => Native(5),
        "GetStringTypeW" => Native(4),
        "FormatMessageW" => Native(7),
        "WinExec" => Native(2),
        // USER32
        "CreateWindowExW" => Wrap("CreateWindowExA", &[In(1), In(2)]),
        "SetWindowTextW" => Wrap("SetWindowTextA", &[In(1)]),
        "GetWindowTextW" => Wrap("GetWindowTextA", &[Out(1, 2)]),
        "GetWindowTextLengthW" => Wrap("GetWindowTextLengthA", &[]),
        "GetClassNameW" => Wrap("GetClassNameA", &[Out(1, 2)]),
        "SetDlgItemTextW" => Wrap("SetDlgItemTextA", &[In(2)]),
        "GetDlgItemTextW" => Wrap("GetDlgItemTextA", &[Out(2, 3)]),
        "SetPropW" => Wrap("SetPropA", &[In(1)]),
        "GetPropW" => Wrap("GetPropA", &[In(1)]),
        "RemovePropW" => Wrap("RemovePropA", &[In(1)]),
        "UnregisterClassW" => Wrap("UnregisterClassA", &[In(0)]),
        "RegisterWindowMessageW" => Wrap("RegisterWindowMessageA", &[In(0)]),
        "MessageBoxW" => Wrap("MessageBoxA", &[In(1), In(2)]),
        "FindWindowW" => Wrap("FindWindowA", &[In(0), In(1)]),
        "WinHelpW" => Wrap("WinHelpA", &[In(1)]),
        "LoadIconW" => Wrap("LoadIconA", &[In(1)]),
        "LoadCursorW" => Wrap("LoadCursorA", &[In(1)]),
        "LoadBitmapW" => Wrap("LoadBitmapA", &[In(1)]),
        "LoadImageW" => Wrap("LoadImageA", &[In(1)]),
        "LoadMenuW" => Wrap("LoadMenuA", &[In(1)]),
        "LoadAcceleratorsW" => Wrap("LoadAcceleratorsA", &[In(1)]),
        "LoadStringW" => Wrap("LoadStringA", &[Out(2, 3)]),
        "DrawTextW" => Wrap("DrawTextA", &[Counted(1, 2)]),
        "DrawTextExW" => Wrap("DrawTextExA", &[Counted(1, 2)]),
        "TabbedTextOutW" => Wrap("TabbedTextOutA", &[Counted(3, 4)]),
        "GetMessageW" => Wrap("GetMessageA", &[]),
        "PeekMessageW" => Wrap("PeekMessageA", &[]),
        "DispatchMessageW" => Wrap("DispatchMessageA", &[]),
        "PostMessageW" => Wrap("PostMessageA", &[]),
        "PostThreadMessageW" => Wrap("PostThreadMessageA", &[]),
        "IsDialogMessageW" => Wrap("IsDialogMessageA", &[]),
        "GetWindowLongW" => Wrap("GetWindowLongA", &[]),
        "GetMenuStringW" => Wrap("GetMenuStringA", &[Out(2, 3)]),
        "CopyAcceleratorTableW" => Wrap("CopyAcceleratorTableA", &[]),
        "TranslateAcceleratorW" => Wrap("TranslateAcceleratorA", &[]),
        "CharNextW" | "CharPrevW" => Native(if name == "CharNextW" { 1 } else { 2 }),
        "CharUpperW" | "CharLowerW" => Native(1),
        "RealChildWindowFromPoint" => Wrap("ChildWindowFromPoint", &[]),
        "SetWindowLongW" | "CallWindowProcW" | "DefWindowProcW" | "SendMessageW" | "SendDlgItemMessageW"
        | "RegisterClassW" | "RegisterClassExW" | "GetClassInfoW" | "GetClassInfoExW" | "CreateDialogIndirectParamW"
        | "SetWindowsHookExW" | "SystemParametersInfoW" | "GrayStringW" | "GetClassLongW" | "GetClassLongA"
        | "FindWindowExW" | "FindWindowExA" | "SetMenuItemInfoW" | "MonitorFromWindow" | "GetMonitorInfoW"
        | "GetMonitorInfoA" | "CopyIcon" | "DefDlgProcW" | "DialogBoxIndirectParamW" => Native(match name {
            "CallWindowProcW" | "SendDlgItemMessageW" | "CreateDialogIndirectParamW" | "DialogBoxIndirectParamW" => 5,
            "DefWindowProcW" | "SendMessageW" | "SetWindowsHookExW" | "SystemParametersInfoW" | "FindWindowExW"
            | "FindWindowExA" | "SetMenuItemInfoW" | "DefDlgProcW" => 4,
            "SetWindowLongW" | "GetClassInfoW" | "GetClassInfoExW" => 3,
            "GrayStringW" => 9,
            "GetClassLongW" | "GetClassLongA" | "MonitorFromWindow" | "GetMonitorInfoW" | "GetMonitorInfoA" => 2,
            _ => 1,
        }),
        // GDI32
        "TextOutW" => Wrap("TextOutA", &[Counted(3, 4)]),
        "GetTextExtentPoint32W" | "GetTextExtentPointW" => Wrap("GetTextExtentPoint32A", &[Counted(1, 2)]),
        "CreateDCW" => Wrap("CreateDCA", &[In(0), In(1), In(2)]),
        "CreateICW" => Wrap("CreateICA", &[In(0), In(1), In(2)]),
        // COMDLG32: CHOOSECOLOR holds no strings the dialog uses.
        "ChooseColorW" => Wrap("ChooseColorA", &[]),
        "CreateFontIndirectW" | "GetObjectW" | "EnumFontFamiliesExW" => {
            Native(if name == "CreateFontIndirectW" { 1 } else if name == "GetObjectW" { 3 } else { 5 })
        }
        // SHLWAPI
        "PathFindFileNameW" | "PathFindExtensionW" | "PathIsUNCW" | "PathStripToRootW" | "PathFileExistsW"
        | "PathRemoveFileSpecW" | "PathAddBackslashW" => Native(1),
        "PathAppendW" | "PathCombineW" => Native(if name == "PathAppendW" { 2 } else { 3 }),
        // SHELL32
        "SHGetSpecialFolderLocation" | "SHGetPathFromIDListW" | "SHGetSpecialFolderPathW" | "SHBrowseForFolderW"
        | "ShellExecuteW" | "SHChangeNotify" | "SHGetFolderPathW" | "ILFree" => Native(match name {
            "SHGetSpecialFolderLocation" => 3,
            "SHGetPathFromIDListW" => 2,
            "SHGetSpecialFolderPathW" | "SHChangeNotify" => 4,
            "SHGetFolderPathW" => 5,
            "ShellExecuteW" => 6,
            _ => 1,
        }),
        // OLE32, OLEACC, WINSPOOL
        "CoCreateGuid" => Native(1),
        "LresultFromObject" | "CreateStdAccessibleObject" => Native(if name == "LresultFromObject" { 3 } else { 4 }),
        "OpenPrinterW" | "DocumentPropertiesW" => Native(if name == "OpenPrinterW" { 3 } else { 6 }),
        // COMCTL32
        "InitCommonControlsEx" | "InitCommonControls" => Native(if name == "InitCommonControls" { 0 } else { 1 }),
        _ => return None,
    })
}

/// Argument count of a function implemented here (`None` if not).
pub fn arg_count(dll: &str, name: &str) -> Option<usize> {
    match plan(name)? {
        Plan::Wrap(a_name, _) => super::arg_count(dll, &ImportName::ByName(a_name.into())),
        Plan::WrapN(a_name, _, count) => super::arg_count(dll, &ImportName::ByName(a_name.into())).map(|_| count),
        Plan::Native(count) => Some(count),
    }
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dll: &str,
    name: &str,
    args: &[u32],
    caller: u32,
) -> Option<ApiOutcome> {
    let count = arg_count(dll, name)?;
    if args.len() < count {
        return None;
    }
    match plan(name)? {
        Plan::Wrap(a_name, params) | Plan::WrapN(a_name, params, _) => wrap(ctx, memory, dll, a_name, args, params, count, caller),
        Plan::Native(_) => native(ctx, memory, dll, name, args, count, caller),
    }
}

/// Call `a_name` with the converted arguments, then convert outputs back.
#[allow(clippy::too_many_arguments)]
fn wrap<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dll: &str,
    a_name: &str,
    args: &[u32],
    params: &[P],
    count: usize,
    caller: u32,
) -> Option<ApiOutcome> {
    let mut temps = Temps::default();
    let mut a_args = args.to_vec();
    let mut outputs = Vec::new();
    for param in params {
        // Parameters past the arguments the A function reads are absent.
        let highest = match *param {
            P::In(index) => index,
            P::Counted(index, length) | P::Out(index, length) => index.max(length),
        };
        if highest >= args.len() {
            continue;
        }
        match *param {
            P::In(index) => a_args[index] = ansi_arg(ctx, memory, &mut temps, args[index]),
            P::Counted(index, length) => {
                let va = args[index];
                if va >= 0x10000 {
                    let units = if args[length] as i32 >= 0 {
                        read_wide_n(memory, va, args[length] as usize)
                    } else {
                        read_wstr(memory, va)
                    };
                    a_args[index] = temps.bytes(ctx, memory, &wide_to_ansi(&units));
                }
            }
            P::Out(index, size) => {
                if args[index] != 0 {
                    let capacity = args[size];
                    a_args[index] = temps.zeroed(ctx, memory, capacity + 1);
                    outputs.push((args[index], a_args[index], capacity));
                }
            }
        }
    }
    let mut outcome = dispatch_with_memory(ctx, memory, dll, &ImportName::ByName(a_name.into()), &a_args, caller)?;
    for (original, temp, capacity) in outputs {
        let ansi = read_cstr(memory, temp);
        store_wide(memory, original, &ansi, capacity);
    }
    temps.free(ctx);
    outcome.arg_count = count;
    Some(outcome)
}

fn call_a<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dll: &str,
    a_name: &str,
    args: &[u32],
    caller: u32,
) -> u32 {
    dispatch_with_memory(ctx, memory, dll, &ImportName::ByName(a_name.into()), args, caller)
        .map(|outcome| outcome.value)
        .unwrap_or(0)
}

fn set_last_error(ctx: &ApiContext, code: u32) {
    ctx.shared.borrow_mut().last_error = code;
}

/// Whether a window procedure expects UTF-16 messages.
pub fn is_unicode_proc(ctx: &ApiContext, proc_va: u32) -> bool {
    ctx.shared.borrow().gui.unicode_procs.contains(&proc_va)
}

pub fn mark_unicode(ctx: &ApiContext, proc_va: u32) {
    if proc_va != 0 && !ctx.procs.contains_key(&proc_va) {
        ctx.shared.borrow_mut().gui.unicode_procs.insert(proc_va);
    }
}

// ── Messages ────────────────────────────────────────────────────────────

const WM_CREATE: u32 = 0x0001;
const WM_SETTEXT: u32 = 0x000C;
const WM_GETTEXT: u32 = 0x000D;
const WM_SETTINGCHANGE: u32 = 0x001A;
const WM_NCCREATE: u32 = 0x0081;
const EM_GETLINE: u32 = 0x00C4;
const EM_REPLACESEL: u32 = 0x00C2;
const CB_GETLBTEXT: u32 = 0x0148;
const LB_GETTEXT: u32 = 0x0189;

/// List and combo box messages whose lParam is a string — unless the box is
/// owner-drawn without `HASSTRINGS`, when it is item data.
fn list_string_message(msg: u32) -> bool {
    matches!(msg, 0x143 | 0x14A | 0x14C | 0x14D | 0x158 | 0x180 | 0x181 | 0x18C | 0x18F | 0x1A2)
}

fn holds_strings(ctx: &ApiContext, hwnd: u32, msg: u32) -> bool {
    let style = ctx.shared.borrow().gui.window(hwnd).map(|w| w.style).unwrap_or(0);
    if msg < 0x180 {
        // CBS_OWNERDRAWFIXED/VARIABLE and CBS_HASSTRINGS
        style & 0x30 == 0 || style & 0x200 != 0
    } else {
        // LBS_OWNERDRAWFIXED/VARIABLE and LBS_HASSTRINGS
        style & 0x30 == 0 || style & 0x40 != 0
    }
}

/// Call `call` with a message's text converted: to UTF-16 when `to_wide`
/// (an ANSI message reaching a Unicode procedure), to ANSI otherwise.
#[allow(clippy::too_many_arguments)]
pub fn convert_message<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    to_wide: bool,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
    call: impl FnOnce(&mut M, u32, u32) -> u32,
) -> u32 {
    let mut temps = Temps::default();
    let read = |memory: &mut M, va: u32| -> Vec<u16> {
        if to_wide {
            ansi_to_wide(&read_cstr(memory, va))
        } else {
            read_wstr(memory, va)
        }
    };
    let result = if lparam >= 0x10000
        && (matches!(msg, WM_SETTEXT | WM_SETTINGCHANGE | EM_REPLACESEL)
            || (list_string_message(msg) && holds_strings(ctx, hwnd, msg)))
    {
        let text = read(memory, lparam);
        let converted = if to_wide { temps.wide(ctx, memory, &text) } else { temps.bytes(ctx, memory, &wide_to_ansi(&text)) };
        call(memory, wparam, converted)
    } else if lparam != 0 && matches!(msg, WM_GETTEXT | EM_GETLINE | LB_GETTEXT | CB_GETLBTEXT) {
        let capacity = match msg {
            WM_GETTEXT => wparam,
            EM_GETLINE => memory.read_bytes(lparam, 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u32).unwrap_or(0),
            _ => 1024,
        };
        let unit = if to_wide { 2 } else { 1 };
        let temp = temps.zeroed(ctx, memory, (capacity.max(1) + 1) * unit);
        if msg == EM_GETLINE {
            let _ = memory.write_bytes(temp, &(capacity as u16).to_le_bytes());
        }
        let copied = call(memory, wparam, temp);
        if to_wide {
            let units = read_wide_n(memory, temp, copied.min(capacity.max(1)) as usize);
            let bytes = wide_to_ansi(&units);
            let mut out = bytes.clone();
            if msg != EM_GETLINE {
                out.push(0);
            }
            let _ = memory.write_bytes(lparam, &out);
        } else {
            let bytes = memory.read_bytes(temp, copied.min(capacity.max(1)) as usize).unwrap_or_default();
            write_wide(memory, lparam, &ansi_to_wide(&bytes), msg != EM_GETLINE);
        }
        copied
    } else if lparam != 0 && matches!(msg, WM_CREATE | WM_NCCREATE) {
        // CREATESTRUCT (in the process's layout): lpszName and lpszClass,
        // fields 9 and 10.
        let layout = crate::win32::x64::CREATESTRUCT;
        let size = layout.size(memory.ptr_size() == 8) as usize;
        let block = memory.read_bytes(lparam, size).unwrap_or_else(|| vec![0; size]);
        let copy = temps.bytes(ctx, memory, &block);
        let view = crate::win32::x64::View::new(memory, &layout, copy);
        for index in [9, 10] {
            let va = view.get32(memory, index);
            if va >= 0x10000 {
                let text = read(memory, va);
                let converted = if to_wide { temps.wide(ctx, memory, &text) } else { temps.bytes(ctx, memory, &wide_to_ansi(&text)) };
                view.set(memory, index, converted);
            }
        }
        call(memory, wparam, copy)
    } else {
        call(memory, wparam, lparam)
    };
    temps.free(ctx);
    result
}

/// Call a guest window or dialog procedure, giving a Unicode procedure its
/// messages' text as UTF-16.
pub fn call_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    proc_va: u32,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> Option<u32> {
    let x64 = ctx.shared.borrow().x64;
    let call = |memory: &mut M, wparam: u32, lparam: u32| {
        if x64 {
            crate::win32::x64::call_proc(ctx, memory, proc_va, hwnd, msg, wparam, lparam)
        } else {
            memory.call_guest(proc_va, &[hwnd, msg, wparam, lparam])
        }
    };
    if !is_unicode_proc(ctx, proc_va) {
        return call(memory, wparam, lparam);
    }
    let mut result = None;
    convert_message(ctx, memory, true, hwnd, msg, wparam, lparam, |memory, wparam, lparam| {
        result = call(memory, wparam, lparam);
        result.unwrap_or(0)
    });
    result
}

// ── Native W functions ──────────────────────────────────────────────────

/// LOGFONTA is 60 bytes (32-byte face name); LOGFONTW is 92 (32 WCHARs).
fn logfont_w_to_a(wide: &[u8]) -> Vec<u8> {
    let mut out = wide[..28].to_vec();
    let face: Vec<u16> = wide[28..92].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let mut face = wide_to_ansi(&face);
    face.resize(32, 0);
    out.extend_from_slice(&face);
    out
}

fn logfont_a_to_w(ansi: &[u8]) -> Vec<u8> {
    let mut out = ansi[..28].to_vec();
    for &byte in &ansi[28..60] {
        out.extend_from_slice(&(byte as u16).to_le_bytes());
    }
    out
}

fn string_type(unit: u16) -> u16 {
    // CT_CTYPE1 flags for Latin-1.
    let ch = char::from_u32(unit as u32).unwrap_or('\0');
    let mut flags = 0u16;
    if ch.is_uppercase() {
        flags |= 0x1;
    }
    if ch.is_lowercase() {
        flags |= 0x2;
    }
    if ch.is_ascii_digit() {
        flags |= 0x4;
    }
    if ch.is_whitespace() {
        flags |= 0x8;
    }
    if ch.is_ascii_punctuation() {
        flags |= 0x10;
    }
    if ch.is_control() {
        flags |= 0x20;
    }
    if ch == ' ' || ch == '\t' {
        flags |= 0x40;
    }
    if ch.is_ascii_hexdigit() {
        flags |= 0x80;
    }
    if ch.is_alphabetic() {
        flags |= 0x100;
    }
    flags
}

fn case_map(unit: u16, upper: bool) -> u16 {
    let Some(ch) = char::from_u32(unit as u32) else {
        return unit;
    };
    let mapped = if upper { ch.to_uppercase().next() } else { ch.to_lowercase().next() };
    mapped.map(|c| c as u32).filter(|&c| c < 0x10000).unwrap_or(unit as u32) as u16
}

/// A counted UTF-16 argument (`-1` = NUL-terminated).
fn counted<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, count: u32) -> Vec<u16> {
    if count as i32 >= 0 {
        read_wide_n(memory, va, count as usize)
    } else {
        read_wstr(memory, va)
    }
}

/// A string allocated once per process and kept (GetCommandLineW etc.).
fn cached_wide<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, key: &str, units: impl FnOnce(&mut M) -> Vec<u16>) -> u32 {
    if let Some(&va) = ctx.shared.borrow().wide_strings.get(key) {
        return va;
    }
    let units = units(memory);
    let va = ctx.shared.borrow_mut().heap.alloc(units.len() as u32 * 2 + 4).unwrap_or(0);
    write_wide(memory, va, &units, true);
    let _ = memory.write_bytes(va + units.len() as u32 * 2, &[0, 0, 0, 0]);
    ctx.shared.borrow_mut().wide_strings.insert(key.to_string(), va);
    va
}

fn native<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dll: &str,
    name: &str,
    args: &[u32],
    count: usize,
    caller: u32,
) -> Option<ApiOutcome> {
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    let mut temps = Temps::default();
    let value = match name {
        // ── KERNEL32 ──
        "lstrcmpW" | "lstrcmpiW" => {
            let (mut a, mut b) = (read_wstr(memory, args[0]), read_wstr(memory, args[1]));
            if name == "lstrcmpiW" {
                a = a.into_iter().map(|u| case_map(u, false)).collect();
                b = b.into_iter().map(|u| case_map(u, false)).collect();
            }
            match a.cmp(&b) {
                std::cmp::Ordering::Less => u32::MAX,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }
        }
        "lstrcpyW" => {
            let text = read_wstr(memory, args[1]);
            write_wide(memory, args[0], &text, true);
            args[0]
        }
        "lstrcpynW" => {
            let text = read_wstr(memory, args[1]);
            let max = (args[2] as usize).saturating_sub(1);
            write_wide(memory, args[0], &text[..text.len().min(max)], true);
            args[0]
        }
        "lstrcatW" => {
            let mut text = read_wstr(memory, args[0]);
            text.extend(read_wstr(memory, args[1]));
            write_wide(memory, args[0], &text, true);
            args[0]
        }
        "GetCommandLineW" => {
            let va = ctx.command_line_va;
            cached_wide(ctx, memory, "command line", |memory| ansi_to_wide(&read_cstr(memory, va)))
        }
        "GetEnvironmentStringsW" => cached_wide(ctx, memory, "environment", |_| Vec::new()),
        "FreeEnvironmentStringsW" => 1,
        "GetEnvironmentVariableW" | "GetEnvironmentVariableA" => {
            set_last_error(ctx, 203); // ERROR_ENVVAR_NOT_FOUND
            0
        }
        "SetEnvironmentVariableW" | "SetEnvironmentVariableA" => 1,
        "GetStartupInfoW" => {
            crate::win32::x64::write_struct(memory, &crate::win32::x64::STARTUPINFO, args[0], &[]);
            0
        }
        "GetVersionExW" | "GetVersionExA" => {
            // OSVERSIONINFO: Windows 95 (4.0.950), or for 64-bit programs a
            // 64-bit Windows (see `os_version`).
            let size = memory.read_u32(args[0]).unwrap_or(0);
            let version = os_version(ctx);
            for (offset, value) in [(4, version.major), (8, version.minor), (12, version.build), (16, version.platform)] {
                let _ = memory.write_u32(args[0] + offset, value);
            }
            let csd_size = if name == "GetVersionExW" { 256 } else { 128 };
            let _ = memory.write_bytes(args[0] + 20, &vec![0u8; csd_size]);
            if size >= 20 + csd_size as u32 + 8 {
                // OSVERSIONINFOEX: service pack, suite, product type.
                let at = args[0] + 20 + csd_size as u32;
                let _ = memory.write_bytes(at, &[version.service_pack as u8, 0, 0, 0, 0, 0, 1, 0]);
            }
            1
        }
        "CompareStringW" => {
            let ignore_case = args[1] & 1 != 0;
            let mut a = counted(memory, args[2], args[3]);
            let mut b = counted(memory, args[4], args[5]);
            if ignore_case {
                a = a.into_iter().map(|u| case_map(u, false)).collect();
                b = b.into_iter().map(|u| case_map(u, false)).collect();
            }
            match a.cmp(&b) {
                std::cmp::Ordering::Less => 1,
                std::cmp::Ordering::Equal => 2,
                std::cmp::Ordering::Greater => 3,
            }
        }
        "LCMapStringW" => {
            let mut text = counted(memory, args[2], args[3]);
            if args[3] as i32 == -1 {
                text.push(0);
            }
            if args[1] & 0x200 != 0 || args[1] & 0x100 != 0 {
                let upper = args[1] & 0x200 != 0;
                text = text.into_iter().map(|u| case_map(u, upper)).collect();
            }
            if args[5] == 0 {
                text.len() as u32
            } else if (text.len() as u32) > args[5] {
                set_last_error(ctx, 122);
                0
            } else {
                write_wide(memory, args[4], &text, false);
                text.len() as u32
            }
        }
        "CompareStringA" | "LCMapStringA" | "GetStringTypeA" => {
            // The ANSI forms over the wide implementations (Latin-1).
            let narrow = |memory: &mut M, va: u32, count: u32| -> Vec<u16> {
                let bytes = if count as i32 == -1 {
                    read_cstr(memory, va)
                } else {
                    memory.read_bytes(va, count as usize).unwrap_or_default()
                };
                ansi_to_wide(&bytes)
            };
            match name {
                "CompareStringA" => {
                    let ignore_case = args[1] & 1 != 0;
                    let mut a = narrow(memory, args[2], args[3]);
                    let mut b = narrow(memory, args[4], args[5]);
                    // A NUL ends a counted string too.
                    for text in [&mut a, &mut b] {
                        if let Some(end) = text.iter().position(|&u| u == 0) {
                            text.truncate(end);
                        }
                        if ignore_case {
                            *text = text.iter().map(|&u| case_map(u, false)).collect();
                        }
                    }
                    match a.cmp(&b) {
                        std::cmp::Ordering::Less => 1,
                        std::cmp::Ordering::Equal => 2,
                        std::cmp::Ordering::Greater => 3,
                    }
                }
                "LCMapStringA" => {
                    let mut text = narrow(memory, args[2], args[3]);
                    if args[3] as i32 == -1 {
                        text.push(0);
                    }
                    if args[1] & 0x300 != 0 {
                        let upper = args[1] & 0x200 != 0;
                        text = text.into_iter().map(|u| case_map(u, upper)).collect();
                    }
                    let bytes = wide_to_ansi(&text);
                    if args[5] == 0 {
                        bytes.len() as u32
                    } else if bytes.len() as u32 > args[5] {
                        set_last_error(ctx, 122);
                        0
                    } else {
                        let _ = memory.write_bytes(args[4], &bytes);
                        bytes.len() as u32
                    }
                }
                _ => {
                    // GetStringTypeA(locale, info type, text, count, types)
                    let text = narrow(memory, args[2], args[3]);
                    let flags: Vec<u8> = text.iter().flat_map(|&u| string_type(u).to_le_bytes()).collect();
                    let _ = memory.write_bytes(args[4], &flags);
                    1
                }
            }
        }
        "GetStringTypeW" => {
            let text = counted(memory, args[1], args[2]);
            let flags: Vec<u8> = text.iter().flat_map(|&u| string_type(u).to_le_bytes()).collect();
            let _ = memory.write_bytes(args[3], &flags);
            1
        }
        "FormatMessageW" => {
            // Only system messages: "Error N." (FORMAT_MESSAGE_ALLOCATE_BUFFER
            // puts the text in a LocalAlloc block).
            let text: Vec<u16> = format!("Error {}.\r\n", args[2]).encode_utf16().collect();
            let target = if args[0] & 0x100 != 0 {
                let block = ctx.shared.borrow_mut().heap.alloc(text.len() as u32 * 2 + 2).unwrap_or(0);
                let _ = memory.write_u32(args[4], block);
                block
            } else if args[5] as usize > text.len() {
                args[4]
            } else {
                0
            };
            if target == 0 {
                0
            } else {
                write_wide(memory, target, &text, true);
                text.len() as u32
            }
        }
        "WinExec" => 33,
        "GetFullPathNameW" => {
            let result = wrap(ctx, memory, dll, "GetFullPathNameA", args, &[P::In(0), P::Out(2, 1)], count, caller)?.value;
            if args[3] != 0 && result != 0 && result < args[1] {
                // lpFilePart: the last path component.
                let path = read_wstr(memory, args[2]);
                let start = path.iter().rposition(|&u| u == b'\\' as u16).map(|i| i + 1).unwrap_or(0);
                let _ = memory.write_u32(args[3], args[2] + start as u32 * 2);
            }
            result
        }
        "FindResourceExW" => {
            // FindResourceExW(module, type, name, language) -> FindResourceA(module, name, type)
            let kind = ansi_arg(ctx, memory, &mut temps, args[1]);
            let resource = ansi_arg(ctx, memory, &mut temps, args[2]);
            call_a(ctx, memory, dll, "FindResourceA", &[args[0], resource, kind], caller)
        }
        "FindFirstFileW" | "FindNextFileW" => {
            // WIN32_FIND_DATAA (320 bytes) to WIN32_FIND_DATAW (592 bytes).
            let data = temps.zeroed(ctx, memory, 320);
            let (a_name, first) = if name == "FindFirstFileW" {
                ("FindFirstFileA", ansi_arg(ctx, memory, &mut temps, args[0]))
            } else {
                ("FindNextFileA", args[0])
            };
            let result = call_a(ctx, memory, dll, a_name, &[first, data], caller);
            if result != 0 && result != u32::MAX {
                let ansi = memory.read_bytes(data, 320).unwrap_or_else(|| vec![0; 320]);
                let mut wide = ansi[..44].to_vec();
                for (start, length) in [(44usize, 260usize), (304, 14)] {
                    for &byte in &ansi[start..start + length] {
                        wide.extend_from_slice(&(byte as u16).to_le_bytes());
                    }
                }
                let _ = memory.write_bytes(args[1], &wide);
            }
            result
        }
        "GetFileAttributesExW" => {
            let path = wide_to_ansi(&read_wstr(memory, args[0]));
            match crate::win32::crt::host_path_in(ctx, &path).and_then(|p| std::fs::metadata(p).ok()) {
                Some(meta) => {
                    let attributes: u32 = if meta.is_dir() { 0x10 } else { 0x20 };
                    let modified = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() * 10_000_000 + 116_444_736_000_000_000)
                        .unwrap_or(0);
                    let mut data = attributes.to_le_bytes().to_vec();
                    for _ in 0..3 {
                        data.extend_from_slice(&modified.to_le_bytes());
                    }
                    data.extend_from_slice(&((meta.len() >> 32) as u32).to_le_bytes());
                    data.extend_from_slice(&(meta.len() as u32).to_le_bytes());
                    let _ = memory.write_bytes(args[2], &data);
                    1
                }
                None => {
                    set_last_error(ctx, 2);
                    0
                }
            }
        }
        "GetModuleHandleExW" => {
            // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: the module holding an address.
            let module = if args[0] & 4 != 0 {
                let address = args[1];
                std::iter::once((ctx.image_base, u32::MAX))
                    .chain(ctx.modules.borrow().iter().map(|m| (m.image_base, m.size)))
                    .filter(|(base, _)| address >= *base)
                    .max_by_key(|(base, _)| *base)
                    .map(|(base, _)| base)
                    .unwrap_or(ctx.image_base)
            } else {
                let module = ansi_arg(ctx, memory, &mut temps, args[1]);
                call_a(ctx, memory, dll, "GetModuleHandleA", &[module], caller)
            };
            let _ = memory.write_ptr(args[2], u64::from(module));
            u32::from(module != 0)
        }
        // ── USER32 ──
        "CharUpperW" | "CharLowerW" => {
            let upper = name == "CharUpperW";
            if args[0] < 0x10000 {
                case_map(args[0] as u16, upper) as u32
            } else {
                let text: Vec<u16> = read_wstr(memory, args[0]).into_iter().map(|u| case_map(u, upper)).collect();
                write_wide(memory, args[0], &text, true);
                args[0]
            }
        }
        "CharNextW" => {
            let unit = read_wide_n(memory, args[0], 1);
            if unit.first().copied().unwrap_or(0) == 0 {
                args[0]
            } else {
                args[0] + 2
            }
        }
        "CharPrevW" => {
            if args[1] > args[0] {
                args[1] - 2
            } else {
                args[0]
            }
        }
        "SetWindowLongW" => {
            // GWL_WNDPROC (and DWL_DLGPROC on dialogs) set a Unicode procedure.
            if args[1] as i32 == -4 || args[1] == 4 {
                mark_unicode(ctx, args[2]);
            }
            call_a(ctx, memory, dll, "SetWindowLongA", args, caller)
        }
        "CallWindowProcW" => {
            let (proc_va, hwnd, msg) = (args[0], args[1], args[2]);
            convert_message(ctx, memory, false, hwnd, msg, args[3], args[4], |memory, wparam, lparam| {
                call_a(ctx, memory, dll, "CallWindowProcA", &[proc_va, hwnd, msg, wparam, lparam], caller)
            })
        }
        "DefWindowProcW" | "SendMessageW" | "DefDlgProcW" => {
            let a_name = match name {
                "DefWindowProcW" => "DefWindowProcA",
                "SendMessageW" => "SendMessageA",
                _ => "DefDlgProcA",
            };
            let (hwnd, msg) = (args[0], args[1]);
            convert_message(ctx, memory, false, hwnd, msg, args[2], args[3], |memory, wparam, lparam| {
                call_a(ctx, memory, dll, a_name, &[hwnd, msg, wparam, lparam], caller)
            })
        }
        "SendDlgItemMessageW" => {
            let (dialog, id, msg) = (args[0], args[1], args[2]);
            let control = call_a(ctx, memory, dll, "GetDlgItem", &[dialog, id], caller);
            convert_message(ctx, memory, false, control, msg, args[3], args[4], |memory, wparam, lparam| {
                call_a(ctx, memory, dll, "SendDlgItemMessageA", &[dialog, id, msg, wparam, lparam], caller)
            })
        }
        "RegisterClassW" | "RegisterClassExW" => {
            // A copy with ANSI menu and class names (WNDCLASS(EX) fields
            // 8 and 9, after cbSize in the Ex form).
            let ex = name == "RegisterClassExW";
            let (layout, first) = crate::win32::x64::window_class(ex);
            let size = layout.size(memory.ptr_size() == 8) as usize;
            let class = memory.read_bytes(args[0], size).unwrap_or_else(|| vec![0; size]);
            let copy = temps.bytes(ctx, memory, &class);
            let view = crate::win32::x64::View::new(memory, &layout, copy);
            mark_unicode(ctx, view.get32(memory, first + 1));
            for index in [first + 8, first + 9] {
                let va = view.get32(memory, index);
                let converted = ansi_arg(ctx, memory, &mut temps, va);
                view.set(memory, index, converted);
            }
            call_a(ctx, memory, dll, if ex { "RegisterClassExA" } else { "RegisterClassA" }, &[copy], caller)
        }
        "GetClassInfoW" | "GetClassInfoExW" => {
            let class_name = ansi_arg(ctx, memory, &mut temps, args[1]);
            let ex = name == "GetClassInfoExW";
            let a_name = if ex { "GetClassInfoExA" } else { "GetClassInfoA" };
            let result = call_a(ctx, memory, dll, a_name, &[args[0], class_name, args[2]], caller);
            if result != 0 {
                // Point the names back at the caller's string; no menu name.
                let (layout, first) = crate::win32::x64::window_class(ex);
                let view = crate::win32::x64::View::new(memory, &layout, args[2]);
                view.set(memory, first + 8, 0);
                view.set(memory, first + 9, args[1]);
            }
            result
        }
        "CreateDialogIndirectParamW" | "DialogBoxIndirectParamW" => {
            mark_unicode(ctx, args[3]);
            let a_name = if name == "CreateDialogIndirectParamW" { "CreateDialogIndirectParamA" } else { "DialogBoxIndirectParamA" };
            call_a(ctx, memory, dll, a_name, args, caller)
        }
        "SetWindowsHookExW" => {
            mark_unicode(ctx, args[1]);
            call_a(ctx, memory, dll, "SetWindowsHookExA", args, caller)
        }
        "SystemParametersInfoW" => match args[0] {
            // SPI_GETNONCLIENTMETRICS: five LOGFONTs in NONCLIENTMETRICS.
            0x29 => {
                let size = memory.read_u32(args[2]).unwrap_or(500);
                let temp = temps.zeroed(ctx, memory, 340);
                let _ = memory.write_u32(temp, 340);
                let result = call_a(ctx, memory, dll, "SystemParametersInfoA", &[0x29, 340, temp, 0], caller);
                let ansi = memory.read_bytes(temp, 340).unwrap_or_else(|| vec![0; 340]);
                let mut wide = ansi[..24].to_vec();
                wide[..4].copy_from_slice(&size.to_le_bytes());
                wide.extend(logfont_a_to_w(&ansi[24..84]));
                wide.extend_from_slice(&ansi[84..92]);
                wide.extend(logfont_a_to_w(&ansi[92..152]));
                wide.extend_from_slice(&ansi[152..160]);
                for start in [160, 220, 280] {
                    wide.extend(logfont_a_to_w(&ansi[start..start + 60]));
                }
                wide.resize(size.clamp(500, 504) as usize, 0);
                let _ = memory.write_bytes(args[2], &wide);
                result
            }
            // SPI_GETICONTITLELOGFONT
            0x1F => {
                let temp = temps.zeroed(ctx, memory, 60);
                let result = call_a(ctx, memory, dll, "SystemParametersInfoA", &[0x1F, 60, temp, 0], caller);
                let ansi = memory.read_bytes(temp, 60).unwrap_or_else(|| vec![0; 60]);
                let _ = memory.write_bytes(args[2], &logfont_a_to_w(&ansi));
                result
            }
            _ => call_a(ctx, memory, dll, "SystemParametersInfoA", args, caller),
        },
        "GrayStringW" => {
            // lpData is a string only without an output procedure.
            let mut a_args = args.to_vec();
            if args[2] == 0 && args[3] >= 0x10000 {
                let text = counted(memory, args[3], if args[4] == 0 { u32::MAX } else { args[4] });
                a_args[3] = temps.bytes(ctx, memory, &wide_to_ansi(&text));
            }
            call_a(ctx, memory, dll, "GrayStringA", &a_args, caller)
        }
        "GetClassLongW" | "GetClassLongA" => {
            let shared = ctx.shared.borrow();
            let class = shared
                .gui
                .window(args[0])
                .and_then(|w| shared.gui.classes.iter().find(|c| c.name.eq_ignore_ascii_case(&w.class)));
            match class {
                Some(class) => match args[1] as i32 {
                    -8 => class.menu_name,
                    -10 => class.background,
                    -12 => class.cursor,
                    -14 | -34 => class.icon,
                    -16 => class.instance,
                    -18 => class.wnd_extra,
                    -20 => class.cls_extra,
                    -24 => class.wndproc,
                    -26 => class.style,
                    -32 => class.atom,
                    _ => 0,
                },
                None => 0,
            }
        }
        "FindWindowExW" | "FindWindowExA" => {
            // Top-level windows only, like FindWindow.
            if args[0] != 0 || args[1] != 0 {
                0
            } else if name == "FindWindowExW" {
                let class = ansi_arg(ctx, memory, &mut temps, args[2]);
                let title = ansi_arg(ctx, memory, &mut temps, args[3]);
                call_a(ctx, memory, dll, "FindWindowA", &[class, title], caller)
            } else {
                call_a(ctx, memory, dll, "FindWindowA", &[args[2], args[3]], caller)
            }
        }
        "SetMenuItemInfoW" => {
            // MENUITEMINFOW in the process's layout: fMask, fState, wID, dwTypeData.
            let mask = crate::win32::xp_user::mii(memory, args[3], 1);
            let state = crate::win32::xp_user::mii(memory, args[3], 3);
            let id = crate::win32::xp_user::mii(memory, args[3], 4);
            let text_va = crate::win32::xp_user::mii(memory, args[3], 9);
            let text = (mask & 0x50 != 0 && text_va >= 0x10000).then(|| wide_to_ansi(&read_wstr(memory, text_va)));
            let mut shared = ctx.shared.borrow_mut();
            let item = shared.gui.menus.get_mut(&args[0]).and_then(|menu| {
                if args[2] != 0 {
                    menu.items.get_mut(args[1] as usize)
                } else {
                    menu.items.iter_mut().find(|item| item.id == args[1])
                }
            });
            match item {
                Some(item) => {
                    if let Some(text) = text {
                        item.text = text;
                    }
                    if mask & 1 != 0 {
                        item.flags = (item.flags & !0x8B) | (state & 0x8B);
                    }
                    if mask & 2 != 0 {
                        item.id = id;
                    }
                    shared.gui.dirty = true;
                    1
                }
                None => 0,
            }
        }
        "MonitorFromWindow" => 0x0001_0001,
        "GetMonitorInfoW" | "GetMonitorInfoA" => {
            // MONITORINFO: rcMonitor, rcWork, dwFlags (MONITORINFOF_PRIMARY).
            let (width, height) = ctx.shared.borrow().gui.screen;
            let size = memory.read_u32(args[1]).unwrap_or(40);
            let mut info = Vec::new();
            for value in [0, 0, width, height, 0, 0, width, height - crate::win32::user::TASKBAR_HEIGHT, 1] {
                info.extend_from_slice(&(value as u32).to_le_bytes());
            }
            let _ = memory.write_bytes(args[1] + 4, &info);
            if size >= 72 {
                if name == "GetMonitorInfoW" {
                    write_wide(memory, args[1] + 40, &"\\\\.\\DISPLAY1".encode_utf16().collect::<Vec<_>>(), true);
                } else {
                    let _ = memory.write_bytes(args[1] + 40, b"\\\\.\\DISPLAY1\0");
                }
            }
            1
        }
        "CopyIcon" => args[0],
        // ── GDI32 ──
        "CreateFontIndirectW" => {
            let wide = memory.read_bytes(args[0], 92).unwrap_or_else(|| vec![0; 92]);
            let font = temps.bytes(ctx, memory, &logfont_w_to_a(&wide));
            call_a(ctx, memory, dll, "CreateFontIndirectA", &[font], caller)
        }
        "GetObjectW" => {
            // Fonts return a LOGFONTW; other objects are the same in A and W.
            let size = call_a(ctx, memory, dll, "GetObjectA", &[args[0], 0, 0], caller);
            if size == 60 {
                if args[2] == 0 {
                    92
                } else {
                    let temp = temps.zeroed(ctx, memory, 60);
                    call_a(ctx, memory, dll, "GetObjectA", &[args[0], 60, temp], caller);
                    let ansi = memory.read_bytes(temp, 60).unwrap_or_else(|| vec![0; 60]);
                    let wide = logfont_a_to_w(&ansi);
                    let _ = memory.write_bytes(args[2], &wide[..wide.len().min(args[1] as usize)]);
                    (args[1]).min(92)
                }
            } else {
                call_a(ctx, memory, dll, "GetObjectA", args, caller)
            }
        }
        "EnumFontFamiliesExW" => {
            let filter = (args[1] != 0).then(|| {
                let face = memory.read_bytes(args[1] + 28, 64).unwrap_or_default();
                let units: Vec<u16> = face.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
                wide_to_ansi(&units)
            });
            crate::win32::gdi::enum_font_families(ctx, memory, filter, args[2], args[3], true)
        }
        // ── SHLWAPI ──
        "PathFindFileNameW" => {
            let path = read_wstr(memory, args[0]);
            let start = path.iter().rposition(|&u| u == b'\\' as u16 || u == b'/' as u16 || u == b':' as u16).map(|i| i + 1).unwrap_or(0);
            args[0] + start as u32 * 2
        }
        "PathFindExtensionW" => {
            let path = read_wstr(memory, args[0]);
            let name_start = path.iter().rposition(|&u| u == b'\\' as u16).map(|i| i + 1).unwrap_or(0);
            let dot = path[name_start..].iter().rposition(|&u| u == b'.' as u16).map(|i| i + name_start);
            args[0] + dot.unwrap_or(path.len()) as u32 * 2
        }
        "PathIsUNCW" => {
            let path = read_wstr(memory, args[0]);
            u32::from(path.starts_with(&[b'\\' as u16, b'\\' as u16]))
        }
        "PathStripToRootW" => {
            let path = read_wstr(memory, args[0]);
            let root: Vec<u16> = if path.len() >= 2 && path[1] == b':' as u16 {
                let mut root = path[..2].to_vec();
                root.push(b'\\' as u16);
                root
            } else {
                path.iter().take_while(|&&u| u != b'\\' as u16).copied().collect()
            };
            write_wide(memory, args[0], &root, true);
            u32::from(!root.is_empty())
        }
        "PathFileExistsW" => {
            let path = wide_to_ansi(&read_wstr(memory, args[0]));
            u32::from(crate::win32::crt::host_path_in(ctx, &path).is_some_and(|p| p.exists()))
        }
        "PathRemoveFileSpecW" => {
            let path = read_wstr(memory, args[0]);
            match path.iter().rposition(|&u| u == b'\\' as u16) {
                Some(index) => {
                    let keep = if index == 2 && path.get(1) == Some(&(b':' as u16)) { 3 } else { index };
                    write_wide(memory, args[0], &path[..keep], true);
                    u32::from(keep < path.len())
                }
                None => 0,
            }
        }
        "PathAddBackslashW" => {
            let mut path = read_wstr(memory, args[0]);
            if path.last() != Some(&(b'\\' as u16)) {
                path.push(b'\\' as u16);
                write_wide(memory, args[0], &path, true);
            }
            args[0] + path.len() as u32 * 2
        }
        "PathAppendW" | "PathCombineW" => {
            let (target, first, second) = if name == "PathAppendW" { (args[0], args[0], args[1]) } else { (args[0], args[1], args[2]) };
            let mut path = read_wstr(memory, first);
            let more = read_wstr(memory, second);
            if !path.is_empty() && path.last() != Some(&(b'\\' as u16)) && more.first() != Some(&(b'\\' as u16)) {
                path.push(b'\\' as u16);
            }
            path.extend(more);
            write_wide(memory, target, &path, true);
            if name == "PathAppendW" { 1 } else { target }
        }
        // ── SHELL32 ──
        "SHGetSpecialFolderLocation" => {
            // The desktop and virtual folders as class items; file-system
            // folders by path.
            match crate::win32::shellfolder::special_items(args[1]).or_else(|| {
                special_folder(args[1]).and_then(|path| crate::win32::shellfolder::path_items("", path))
            }) {
                Some(list) => {
                    let pidl = crate::win32::shellfolder::alloc_list(ctx, memory, &list);
                    let _ = memory.write_ptr(args[2], u64::from(pidl));
                    0
                }
                None => {
                    let _ = memory.write_ptr(args[2], 0);
                    0x8000_4005 // E_FAIL
                }
            }
        }
        "SHGetPathFromIDListW" => match pidl_path(memory, args[0]) {
            Some(path) => {
                write_wide(memory, args[1], &ansi_to_wide(&path), true);
                1
            }
            None => 0,
        },
        "SHGetSpecialFolderPathW" | "SHGetFolderPathW" => {
            let (csidl, out) = if name == "SHGetFolderPathW" { (args[1] & 0xFF, args[4]) } else { (args[2] & 0xFF, args[1]) };
            match special_folder(csidl) {
                Some(path) => {
                    let _ = crate::win32::crt::host_path_in(ctx, path.as_bytes()).map(std::fs::create_dir_all);
                    write_wide(memory, out, &ansi_to_wide(path.as_bytes()), true);
                    if name == "SHGetFolderPathW" { 0 } else { 1 }
                }
                None => {
                    if name == "SHGetFolderPathW" { 0x8000_4005 } else { 0 }
                }
            }
        }
        "ILFree" => {
            ctx.shared.borrow_mut().heap.free(args[0]);
            0
        }
        "SHBrowseForFolderW" => 0, // cancelled
        "SHChangeNotify" => 0,
        "ShellExecuteW" => {
            let operation = String::from_utf8_lossy(&wide_to_ansi(&read_wstr(memory, args[1]))).into_owned();
            let file = String::from_utf8_lossy(&wide_to_ansi(&read_wstr(memory, args[2]))).into_owned();
            eprintln!("[shell] ShellExecute {operation:?} {file:?} (not started)");
            42 // > 32: success
        }
        // ── OLE32, OLEACC, WINSPOOL ──
        "CoCreateGuid" => {
            let seed = ctx.shared.borrow().started.elapsed().as_nanos() as u64 ^ 0x9E37_79B9_7F4A_7C15;
            let mut bytes = Vec::with_capacity(16);
            let mut state = seed;
            for _ in 0..16 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                bytes.push(state as u8);
            }
            let _ = memory.write_bytes(args[0], &bytes);
            0
        }
        "LresultFromObject" => 0,
        "CreateStdAccessibleObject" => 0x8000_4001, // E_NOTIMPL
        "OpenPrinterW" => 0,
        "DocumentPropertiesW" => u32::MAX,
        // ── COMCTL32 ──
        "InitCommonControlsEx" => 1,
        "InitCommonControls" => 0,
        _ => {
            temps.free(ctx);
            return None;
        }
    };
    temps.free(ctx);
    ok(value)
}

/// Paths of the special folders (`CSIDL_*`).
pub(crate) fn special_folder(csidl: u32) -> Option<&'static str> {
    Some(match csidl & 0xFF {
        0x00 | 0x10 | 0x19 => "C:\\WINDOWS\\Desktop",
        0x02 | 0x17 => "C:\\WINDOWS\\Start Menu\\Programs",
        0x05 | 0x0C | 0x2E => "C:\\My Documents",
        0x07 | 0x18 => "C:\\WINDOWS\\Start Menu\\Programs\\StartUp",
        0x0B | 0x16 => "C:\\WINDOWS\\Start Menu",
        0x1A | 0x1C | 0x23 => "C:\\WINDOWS\\Application Data",
        0x06 => "C:\\WINDOWS\\Favorites",
        0x08 => "C:\\WINDOWS\\Recent",
        0x09 => "C:\\WINDOWS\\SendTo",
        0x13 => "C:\\WINDOWS\\NetHood",
        0x14 => "C:\\WINDOWS\\Fonts",
        0x15 => "C:\\WINDOWS\\ShellNew",
        0x20 => "C:\\WINDOWS\\Temporary Internet Files",
        0x21 => "C:\\WINDOWS\\Cookies",
        0x22 => "C:\\WINDOWS\\History",
        0x24 => "C:\\WINDOWS",
        0x25 => "C:\\WINDOWS\\SYSTEM",
        0x26 => "C:\\Program Files",
        0x2B => "C:\\Program Files\\Common Files",
        0x28 => "C:\\WINDOWS",
        _ => return None,
    })
}

/// An item ID list for a file-system path (see `shellfolder`).
pub(crate) fn make_pidl<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, path: &str) -> u32 {
    let list = crate::win32::shellfolder::path_items("", path).unwrap_or_default();
    crate::win32::shellfolder::alloc_list(ctx, memory, &list)
}

pub(crate) fn pidl_path<M: GuestMemory + ?Sized>(memory: &mut M, pidl: u32) -> Option<Vec<u8>> {
    crate::win32::shellfolder::pidl_path(memory, pidl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wrapper_has_an_a_function() {
        // The Wrap plans must point at implemented A functions.
        let names = [
            "CreateFileW", "DeleteFileW", "CreateDirectoryW", "SetCurrentDirectoryW", "GetCurrentDirectoryW", "GetDriveTypeW",
            "GetVolumeInformationW", "OutputDebugStringW",
            "GlobalAddAtomW", "GlobalFindAtomW", "LoadLibraryW",
            "LoadLibraryExW", "GetTempPathW", "GetModuleFileNameW", "GetModuleHandleW", "MoveFileExW", "FindResourceW",
            "GetDateFormatW", "GetSystemDirectoryW", "GetWindowsDirectoryW", "CreateWindowExW", "SetWindowTextW",
            "GetWindowTextW", "GetWindowTextLengthW", "GetClassNameW", "SetDlgItemTextW", "GetDlgItemTextW", "SetPropW",
            "GetPropW", "RemovePropW", "UnregisterClassW", "RegisterWindowMessageW", "MessageBoxW", "FindWindowW", "WinHelpW",
            "LoadIconW", "LoadCursorW", "LoadBitmapW", "LoadStringW", "DrawTextW", "DrawTextExW", "TabbedTextOutW",
            "GetMessageW", "PeekMessageW", "DispatchMessageW", "PostMessageW", "IsDialogMessageW", "GetWindowLongW",
            "RealChildWindowFromPoint", "TextOutW", "GetTextExtentPoint32W", "CreateDCW",
        ];
        let dll_of = |name: &str| {
            if ["TextOutW", "GetTextExtentPoint32W", "CreateDCW"].contains(&name) {
                "GDI32.dll"
            } else if name.starts_with("Get") && ["GetMessageW", "GetWindowTextW", "GetWindowTextLengthW", "GetClassNameW", "GetDlgItemTextW", "GetPropW", "GetWindowLongW"].contains(&name)
                || ["CreateWindowExW", "SetWindowTextW", "SetDlgItemTextW", "SetPropW", "RemovePropW", "UnregisterClassW", "RegisterWindowMessageW", "MessageBoxW", "FindWindowW", "WinHelpW", "LoadIconW", "LoadCursorW", "LoadBitmapW", "LoadStringW", "DrawTextW", "DrawTextExW", "TabbedTextOutW", "PeekMessageW", "DispatchMessageW", "PostMessageW", "IsDialogMessageW", "RealChildWindowFromPoint"].contains(&name)
            {
                "USER32.dll"
            } else {
                "KERNEL32.dll"
            }
        };
        let missing: Vec<&str> = names.iter().copied().filter(|name| arg_count(dll_of(name), name).is_none()).collect();
        assert!(missing.is_empty(), "W wrappers without an A function: {missing:?}");
    }

    #[test]
    fn text_converts_through_latin_1() {
        assert_eq!(wide_to_ansi(&"Grüße €".encode_utf16().collect::<Vec<_>>()), b"Gr\xfc\xdfe ?");
        assert_eq!(ansi_to_wide(b"A\xe9"), vec![0x41, 0xE9]);
        assert_eq!(logfont_a_to_w(&logfont_w_to_a(&{
            let mut font = vec![0u8; 92];
            font[28] = b'T';
            font
        }))[28..30], [b'T', 0]);
    }
}

/// The Windows version programs are told.
pub struct OsVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
    pub platform: u32,
    pub service_pack: u32,
}

/// Windows 95 (4.0.950) for 32-bit programs. A 64-bit program cannot run
/// on Windows 95, so it is told Windows 7 SP1 (6.1.7601), the oldest
/// 64-bit Windows such programs commonly require.
pub fn os_version(ctx: &ApiContext) -> OsVersion {
    if ctx.shared.borrow().x64 {
        OsVersion { major: 6, minor: 1, build: 7601, platform: 2, service_pack: 1 }
    } else {
        OsVersion { major: 4, minor: 0, build: 950, platform: 1, service_pack: 0 }
    }
}

/// `VerifyVersionInfo(info, type mask, condition mask low, high)`.
pub fn verify_version<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, args: &[u32], wide: bool) -> u32 {
    let (info, types) = (args[0], args[1]);
    let mask = args[2] as u64 | (args[3] as u64) << 32;
    let condition = |bit: u32| ((mask >> (bit.trailing_zeros() * 3)) & 7) as u32;
    let ours = os_version(ctx);
    let csd = if wide { 256 } else { 128 };
    let read = |memory: &mut M, offset: u32, size: u32| -> u32 {
        let bytes = memory.read_bytes(info + offset, size as usize).unwrap_or_default();
        bytes.iter().rev().fold(0, |value, &b| value << 8 | b as u32)
    };
    let compare = |ours: u64, theirs: u64, how: u32| match how {
        1 => ours == theirs,
        2 => ours > theirs,
        3 => ours >= theirs,
        4 => ours < theirs,
        5 => ours <= theirs,
        _ => true,
    };
    // Major, minor and service pack compare as one version number, with
    // the condition of the most significant part asked about.
    let parts = [
        (2u32, ours.major, read(memory, 4, 4)),
        (1, ours.minor, read(memory, 8, 4)),
        (0x20, ours.service_pack, read(memory, 20 + csd, 2)),
        (0x10, 0, read(memory, 22 + csd, 2)),
    ];
    let asked: Vec<_> = parts.iter().filter(|(bit, _, _)| types & bit != 0).collect();
    let mut ok = true;
    if let Some(first) = asked.first() {
        let pack = |values: &[u32]| values.iter().fold(0u64, |acc, &v| acc << 16 | v as u64 & 0xFFFF);
        let mine: Vec<u32> = asked.iter().map(|p| p.1).collect();
        let theirs: Vec<u32> = asked.iter().map(|p| p.2).collect();
        ok &= compare(pack(&mine), pack(&theirs), condition(first.0));
    }
    if types & 4 != 0 {
        ok &= compare(ours.build as u64, read(memory, 12, 4) as u64, condition(4));
    }
    if types & 8 != 0 {
        ok &= compare(ours.platform as u64, read(memory, 16, 4) as u64, condition(8));
    }
    if types & 0x80 != 0 {
        ok &= compare(1, read(memory, 26 + csd, 1) as u64, condition(0x80));
    }
    if !ok {
        ctx.shared.borrow_mut().last_error = 0x47E; // ERROR_OLD_WIN_VERSION
    }
    u32::from(ok)
}
