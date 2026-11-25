//! 64-bit programs on the 32-bit subsystem.
//!
//! A 64-bit process lives below 4 GiB like a 32-bit one, so its pointers
//! and handles fit the subsystem's 32-bit values; what differs is how they
//! are laid out. The x64 calling convention passes one argument per 64-bit
//! register or stack slot (so a 64-bit value passed by value is one
//! argument, not two), and every structure holding a pointer, handle or
//! `*_PTR` value has wider fields and different offsets.
//!
//! Calls from 64-bit code come here first. Arguments that point to such
//! structures are copied into 32-bit scratch copies before the subsystem
//! runs and copied back after it, structures in window messages are
//! translated in both directions (the subsystem sending to a 64-bit window
//! procedure, and the program sending to the subsystem's), and results are
//! widened to 64 bits. The subsystem's functions themselves stay 32-bit.

use crate::{
    kernel::{pe::ImportName, syscall::widen},
    win32::{ApiContext, ApiOutcome, GuestMemory},
};

/// One structure field.
#[derive(Clone, Copy, Debug)]
pub enum F {
    /// 32-bit value.
    D,
    /// 16-bit value.
    W,
    /// Byte.
    B,
    /// Pointer, handle or `*_PTR` value: 4 or 8 bytes.
    P,
    /// 64-bit value on both.
    Q,
    /// `n` 32-bit values (a RECT is `A(4)`).
    A(u32),
    /// `n` bytes, byte aligned (character arrays).
    Bytes(u32),
    /// No data: align the next field as a pointer (where a nested
    /// structure holding pointers starts).
    AlignP,
}

impl F {
    fn size_align(self, x64: bool) -> (u32, u32) {
        match self {
            F::D => (4, 4),
            F::W => (2, 2),
            F::B => (1, 1),
            F::P if x64 => (8, 8),
            F::P => (4, 4),
            F::Q => (8, 8),
            F::A(n) => (4 * n, 4),
            F::Bytes(n) => (n, 1),
            F::AlignP if x64 => (0, 8),
            F::AlignP => (0, 4),
        }
    }
}

/// A structure: its fields and whether its first field is its size.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub fields: &'static [F],
    pub sized: bool,
}

impl Layout {
    const fn new(fields: &'static [F]) -> Self {
        Self { fields, sized: false }
    }

    const fn sized(fields: &'static [F]) -> Self {
        Self { fields, sized: true }
    }

    /// Field offsets and the structure's size.
    fn offsets(&self, x64: bool) -> (Vec<u32>, u32) {
        let mut offsets = Vec::with_capacity(self.fields.len());
        let (mut at, mut max_align) = (0u32, 1u32);
        for field in self.fields {
            let (size, align) = field.size_align(x64);
            at = (at + align - 1) & !(align - 1);
            offsets.push(at);
            at += size;
            max_align = max_align.max(align);
        }
        (offsets, (at + max_align - 1) & !(max_align - 1))
    }

    pub fn size(&self, x64: bool) -> u32 {
        self.offsets(x64).1
    }
}

/// A structure in guest memory read and written in the layout of the
/// process it belongs to: field `i` is at its 32-bit or 64-bit offset, and
/// pointer-sized fields are 4 or 8 bytes.
pub struct View {
    va: u32,
    offsets: Vec<u32>,
    fields: &'static [F],
    x64: bool,
}

impl View {
    pub fn new<M: GuestMemory + ?Sized>(memory: &M, layout: &Layout, va: u32) -> Self {
        let x64 = memory.ptr_size() == 8;
        Self { va, offsets: layout.offsets(x64).0, fields: layout.fields, x64 }
    }

    /// The address of field `index`.
    pub fn at(&self, index: usize) -> u32 {
        self.va + self.offsets[index]
    }

    /// Field `index` (pointer-sized fields in full).
    pub fn get<M: GuestMemory + ?Sized>(&self, memory: &mut M, index: usize) -> u64 {
        let at = self.va + self.offsets[index];
        match self.fields[index] {
            F::P if self.x64 => memory.read_u64(at).unwrap_or(0),
            F::Q => memory.read_u64(at).unwrap_or(0),
            F::W => memory.read_bytes(at, 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u64).unwrap_or(0),
            F::B => memory.read_bytes(at, 1).map(|b| b[0] as u64).unwrap_or(0),
            _ => memory.read_u32(at).unwrap_or(0) as u64,
        }
    }

    /// Field `index` as a 32-bit value: a pointer-sized field's low half.
    pub fn get32<M: GuestMemory + ?Sized>(&self, memory: &mut M, index: usize) -> u32 {
        self.get(memory, index) as u32
    }

    /// Set field `index`; a pointer-sized field is written in full (the
    /// value widened as a 32-bit result would be).
    pub fn set<M: GuestMemory + ?Sized>(&self, memory: &mut M, index: usize, value: u32) {
        let at = self.va + self.offsets[index];
        match self.fields[index] {
            F::P if self.x64 => {
                let _ = memory.write_u64(at, widen(value));
            }
            F::W => {
                let _ = memory.write_bytes(at, &(value as u16).to_le_bytes());
            }
            F::B => {
                let _ = memory.write_bytes(at, &[value as u8]);
            }
            _ => {
                let _ = memory.write_u32(at, value);
            }
        }
    }
}

/// Copy a structure from one layout to the other.
fn convert(layout: &Layout, from: &[u8], to_x64: bool) -> Vec<u8> {
    let (source, _) = layout.offsets(!to_x64);
    let (target, size) = layout.offsets(to_x64);
    let mut out = vec![0u8; size as usize];
    for (index, field) in layout.fields.iter().enumerate() {
        let (s, t) = (source[index] as usize, target[index] as usize);
        match field {
            F::P => {
                let value = u32::from_le_bytes(from.get(s..s + 4).and_then(|b| b.try_into().ok()).unwrap_or([0; 4]));
                if to_x64 {
                    out[t..t + 8].copy_from_slice(&widen(value).to_le_bytes());
                } else {
                    out[t..t + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
            other => {
                let (n, _) = other.size_align(to_x64);
                if let Some(bytes) = from.get(s..s + n as usize) {
                    out[t..t + n as usize].copy_from_slice(bytes);
                }
            }
        }
    }
    if layout.sized {
        out[0..4].copy_from_slice(&size.to_le_bytes());
    }
    out
}

/// A copy of the 64-bit structure at `va64` in 32-bit layout, in scratch
/// memory.
pub fn narrow<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, layout: &Layout, va64: u32) -> u32 {
    let from = memory.read_bytes(va64, layout.size(true) as usize).unwrap_or_default();
    let bytes = convert(layout, &from, false);
    let va = ctx.shared.borrow_mut().heap.alloc(bytes.len() as u32 + 16).unwrap_or(0);
    let _ = memory.write_bytes(va, &bytes);
    va
}

/// A copy of the 32-bit structure at `va32` in 64-bit layout, in scratch
/// memory.
pub fn widen_struct<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, layout: &Layout, va32: u32) -> u32 {
    let from = memory.read_bytes(va32, layout.size(false) as usize).unwrap_or_default();
    let bytes = convert(layout, &from, true);
    let va = ctx.shared.borrow_mut().heap.alloc(bytes.len() as u32 + 16).unwrap_or(0);
    let _ = memory.write_bytes(va, &bytes);
    va
}

/// Write a 32-bit scratch copy back over the 64-bit original (or the
/// reverse), keeping the original's size field.
pub fn copy_back<M: GuestMemory + ?Sized>(memory: &mut M, layout: &Layout, from: u32, from_x64: bool, to: u32) {
    let source = memory.read_bytes(from, layout.size(from_x64) as usize).unwrap_or_default();
    let mut bytes = convert(layout, &source, !from_x64);
    if layout.sized {
        if let Some(original) = memory.read_bytes(to, 4) {
            bytes[0..4].copy_from_slice(&original);
        }
    }
    let _ = memory.write_bytes(to, &bytes);
}

fn free(ctx: &ApiContext, va: u32) {
    if va != 0 {
        ctx.shared.borrow_mut().heap.free(va);
    }
}

// ── Structures ───────────────────────────────────────────────────────────

use F::*;

pub const MSG: Layout = Layout::new(&[P, D, P, P, D, D, D]);
pub const WNDCLASS: Layout = Layout::new(&[D, P, D, D, P, P, P, P, P, P]);
pub const WNDCLASSEX: Layout = Layout::sized(&[D, D, P, D, D, P, P, P, P, P, P, P]);
/// WNDCLASS or WNDCLASSEX, and the index of its first WNDCLASS field (the
/// Ex form starts with cbSize).
pub fn window_class(ex: bool) -> (Layout, usize) {
    if ex { (WNDCLASSEX, 1) } else { (WNDCLASS, 0) }
}

pub const CREATESTRUCT: Layout = Layout::new(&[P, P, P, P, D, D, D, D, D, P, P, D]);
pub const PAINTSTRUCT: Layout = Layout::new(&[P, D, A(4), D, D, Bytes(32)]);
pub const WINDOWPOS: Layout = Layout::new(&[P, P, D, D, D, D, D]);
pub const NMHDR: Layout = Layout::new(&[P, P, D]);
pub const MENUITEMINFO: Layout = Layout::sized(&[D, D, D, D, D, P, P, P, P, P, D, P]);
pub const STARTUPINFO: Layout = Layout::sized(&[D, P, P, P, D, D, D, D, D, D, D, D, W, W, P, P, P, P]);
pub const SYSTEM_INFO: Layout = Layout::new(&[W, W, D, P, P, P, D, D, D, W, W]);
pub const MEMORY_BASIC_INFORMATION: Layout = Layout::new(&[P, P, D, P, D, D, D]);
pub const PROCESS_INFORMATION: Layout = Layout::new(&[P, P, D, D]);
pub const DRAWITEMSTRUCT: Layout = Layout::new(&[D, D, D, D, D, P, P, A(4), P]);
pub const MEASUREITEMSTRUCT: Layout = Layout::new(&[D, D, D, D, D, P]);
pub const DELETEITEMSTRUCT: Layout = Layout::new(&[D, D, D, P, P]);
pub const COMPAREITEMSTRUCT: Layout = Layout::new(&[D, D, P, D, P, D, P, D]);
pub const HELPINFO: Layout = Layout::sized(&[D, D, D, P, P, D, D]);
pub const CWPSTRUCT: Layout = Layout::new(&[P, P, D, P]);
pub const CWPRETSTRUCT: Layout = Layout::new(&[P, P, P, D, P]);
pub const BITMAP: Layout = Layout::new(&[D, D, D, D, W, W, P]);
pub const MSGBOXPARAMS: Layout = Layout::sized(&[D, P, P, P, P, D, P, P, P, D]);
pub const SHELLEXECUTEINFO: Layout = Layout::sized(&[D, D, P, P, P, P, P, D, P, P, P, P, D, P, P]);
pub const OPENFILENAME: Layout = Layout::sized(&[
    D, P, P, P, P, D, D, P, D, P, D, P, P, D, W, W, P, P, P, P, P, D, D,
]);
pub const BROWSEINFO: Layout = Layout::new(&[P, P, P, P, D, P, P, D]);
pub const SECURITY_ATTRIBUTES: Layout = Layout::sized(&[D, P, D]);
pub const TCITEM: Layout = Layout::new(&[D, D, D, P, D, D, P]);
pub const TOOLINFO: Layout = Layout::sized(&[D, D, P, P, A(4), P, P, P, P]);
pub const LVITEM: Layout = Layout::new(&[D, D, D, D, D, P, D, D, P, D, D, D, P, D, D]);
pub const LVCOLUMN: Layout = Layout::new(&[D, D, D, P, D, D, D, D, D, D, D]);
pub const NMLISTVIEW: Layout = Layout::new(&[P, P, D, D, D, D, D, D, D, D, P]);
pub const NMTOOLBAR: Layout = Layout::new(&[P, P, D, D, AlignP, D, D, B, B, P, P, D, P, A(4)]);
pub const TVITEM: Layout = Layout::new(&[D, P, D, D, P, D, D, D, D, P]);
pub const TVINSERTSTRUCT: Layout = Layout::new(&[P, P, D, P, D, D, P, D, D, D, D, P, D, P, P]);
pub const NMTREEVIEW: Layout = Layout::new(&[
    P, P, D, D, D, P, D, D, P, D, D, D, D, P, D, P, D, D, P, D, D, D, D, P, D, D,
]);
pub const PSHNOTIFY: Layout = Layout::new(&[P, P, D, P]);
pub const COPYDATASTRUCT: Layout = Layout::new(&[P, D, P]);
pub const CBT_CREATEWND: Layout = Layout::new(&[P, P]);
pub const NCCALCSIZE_PARAMS: Layout = Layout::new(&[A(12), P]);

/// How a call uses a structure argument.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    In,
    Out,
    InOut,
}

/// The structure arguments of functions, by argument index.
fn struct_args(name: &str) -> &'static [(usize, Layout, Dir)] {
    use Dir::*;
    match name {
        "GetMenuItemInfoW" | "GetMenuItemInfoA" => &[(3, MENUITEMINFO, InOut)],
        "SetMenuItemInfoW" | "SetMenuItemInfoA" | "InsertMenuItemW" | "InsertMenuItemA" => &[(3, MENUITEMINFO, In)],
        "GetStartupInfoW" | "GetStartupInfoA" => &[(0, STARTUPINFO, Out)],
        "GetSystemInfo" | "GetNativeSystemInfo" => &[(0, SYSTEM_INFO, Out)],
        "VirtualQuery" => &[(1, MEMORY_BASIC_INFORMATION, Out)],
        "CreateProcessW" | "CreateProcessA" => &[(8, STARTUPINFO, In), (9, PROCESS_INFORMATION, Out)],
        "MessageBoxIndirectW" | "MessageBoxIndirectA" => &[(0, MSGBOXPARAMS, In)],
        "ShellExecuteExW" | "ShellExecuteExA" => &[(0, SHELLEXECUTEINFO, InOut)],
        "GetOpenFileNameW" | "GetOpenFileNameA" | "GetSaveFileNameW" | "GetSaveFileNameA" => &[(0, OPENFILENAME, InOut)],
        "SHBrowseForFolderW" | "SHBrowseForFolderA" => &[(0, BROWSEINFO, InOut)],
        _ => &[],
    }
}

/// Arguments passed by value as 64-bit quantities (two 32-bit arguments in
/// the subsystem's signature), by x64 argument index.
fn split_args(name: &str) -> &'static [usize] {
    match name {
        "SetFilePointerEx" => &[1],
        // A POINT by value: one 64-bit argument, two 32-bit ones.
        "MonitorFromPoint" | "WindowFromPoint" => &[0],
        "PtInRect" | "ChildWindowFromPoint" | "ChildWindowFromPointEx" | "RealChildWindowFromPoint"
        | "DragDetect" => &[1],
        "VerifyVersionInfoW" | "VerifyVersionInfoA" => &[2],
        "LockFileEx" | "UnlockFileEx" => &[],
        _ => &[],
    }
}

/// Window messages whose lParam points to a structure, and how it is used.
fn message_struct(msg: u32) -> Option<(Layout, Dir)> {
    use Dir::*;
    Some(match msg {
        0x002B => (DRAWITEMSTRUCT, In),                   // WM_DRAWITEM
        0x002C => (MEASUREITEMSTRUCT, InOut),             // WM_MEASUREITEM
        0x002D => (DELETEITEMSTRUCT, In),                 // WM_DELETEITEM
        0x0039 => (COMPAREITEMSTRUCT, In),                // WM_COMPAREITEM
        0x0053 => (HELPINFO, In),                         // WM_HELP
        0x004A => (COPYDATASTRUCT, In),                   // WM_COPYDATA
        0x1304 | 0x1306 | 0x133C | 0x133E => (TCITEM, InOut), // TCM_GET/SET/INSERTITEM(A/W)
        0x1305 | 0x1307 | 0x133D => (TCITEM, InOut),
        0x1005 | 0x1006 | 0x1007 | 0x102D | 0x104B | 0x104C | 0x104D | 0x1073 | 0x1074 => (LVITEM, InOut),
        0x1019 | 0x101A | 0x101B | 0x105F | 0x1060 | 0x1061 => (LVCOLUMN, InOut),
        0x0404 | 0x0432 | 0x0433 | 0x0434 | 0x0435 | 0x0438 | 0x0439 | 0x043A => (TOOLINFO, InOut),
        _ => return None,
    })
}

// ── Calls from 64-bit code ───────────────────────────────────────────────

/// What a function is called, under its x64 name.
fn alias(name: &str) -> &str {
    match name {
        "GetWindowLongPtrW" => "GetWindowLongW",
        "GetWindowLongPtrA" => "GetWindowLongA",
        "SetWindowLongPtrW" => "SetWindowLongW",
        "SetWindowLongPtrA" => "SetWindowLongA",
        "GetClassLongPtrW" => "GetClassLongW",
        "GetClassLongPtrA" => "GetClassLongA",
        "SetClassLongPtrW" => "SetClassLongW",
        "SetClassLongPtrA" => "SetClassLongA",
        other => other,
    }
}

/// Functions only 64-bit programs call, with their argument counts.
fn x64_only(name: &str) -> Option<usize> {
    Some(match name {
        "InitializeCriticalSectionEx" => 3,
        "VerSetConditionMask" => 3,
        "RtlCaptureContext" => 1,
        "RtlLookupFunctionEntry" => 3,
        "RtlPcToFileHeader" => 2,
        "RtlVirtualUnwind" => 8,
        "RtlUnwindEx" => 6,
        _ => return None,
    })
}

fn by_name(name: &ImportName) -> Option<&str> {
    name.as_str()
}

/// The x64 argument count of an import.
pub fn arg_count(ctx: &ApiContext, dll: &str, name: &ImportName) -> Option<usize> {
    if let Some(text) = by_name(name) {
        if let Some(count) = x64_only(text) {
            return Some(count);
        }
        let text = alias(text);
        let count = crate::kernel::syscall::SystemCalls::arg_count(ctx, dll, &ImportName::ByName(text.into()))?;
        return Some(count - split_args(text).len());
    }
    crate::kernel::syscall::SystemCalls::arg_count(ctx, dll, name)
}

fn trace() -> bool {
    std::env::var_os("OTW_TRACE_X64").is_some()
}

/// Perform a call from 64-bit code.
pub fn call(
    ctx: &ApiContext,
    memory: &mut dyn GuestMemory,
    dll: &str,
    name: &ImportName,
    args: &crate::kernel::syscall::Args,
    caller: u32,
) -> Option<ApiOutcome> {
    let count = args.len();
    let args = args.all_wide();
    let text = by_name(name).map(str::to_owned);
    if let Some(text) = text.as_deref() {
        if let Some(value) = x64_function(ctx, memory, text, args) {
            return Some(ApiOutcome::wide(value, count));
        }
    }
    let text = text.as_deref().map(alias);
    let name = match text {
        Some(text) => ImportName::ByName(text.to_owned()),
        None => name.clone(),
    };
    // 64-bit values by value become their two halves.
    let splits = text.map(split_args).unwrap_or(&[]);
    let mut narrow_args: Vec<u32> = Vec::with_capacity(args.len() + splits.len());
    for (index, &value) in args.iter().enumerate() {
        narrow_args.push(value as u32);
        if splits.contains(&index) {
            narrow_args.push((value >> 32) as u32);
        }
    }
    // Structures by pointer: 32-bit copies for the call.
    let mut copies: Vec<(usize, Layout, Dir, u32, u32)> = Vec::new();
    for &(index, layout, dir) in text.map(struct_args).unwrap_or(&[]) {
        let Some(&original) = narrow_args.get(index) else {
            continue;
        };
        if original == 0 {
            continue;
        }
        let copy = if dir == Dir::Out {
            let va = ctx.shared.borrow_mut().heap.alloc(layout.size(false) + 16).unwrap_or(0);
            let mut zero = vec![0u8; layout.size(false) as usize];
            if layout.sized {
                zero[0..4].copy_from_slice(&layout.size(false).to_le_bytes());
            }
            let _ = memory.write_bytes(va, &zero);
            va
        } else {
            narrow(ctx, memory, &layout, original)
        };
        narrow_args[index] = copy;
        copies.push((index, layout, dir, original, copy));
    }
    // Messages with structures, sent by the program to a window procedure
    // of the subsystem.
    let message_args = text.and_then(message_position);
    let mut message_copy = None;
    if let Some((msg_index, lparam_index)) = message_args {
        let (msg, lparam) = (narrow_args[msg_index], narrow_args[lparam_index]);
        if lparam != 0 {
            if let Some(copy) = message_to32(ctx, memory, msg, lparam) {
                narrow_args[lparam_index] = copy.1;
                message_copy = Some((msg, lparam, copy));
            }
        }
    }
    if trace() {
        eprintln!("[x64] {dll}!{} {:x?} -> {:x?}", name.display(), args, narrow_args);
    }
    let outcome: Option<ApiOutcome> = crate::win32::dispatch_with_memory(ctx, memory, dll, &name, &narrow_args, caller);
    if let Some((msg, original, copy)) = message_copy {
        message_back_to64(ctx, memory, msg, original, copy);
    }
    for (_, layout, dir, original, copy) in copies {
        if dir != Dir::In {
            copy_back(memory, &layout, copy, false, original);
        }
        free(ctx, copy);
    }
    let outcome = outcome?;
    let value = outcome.wide.unwrap_or_else(|| widen(outcome.value));
    Some(ApiOutcome { value: value as u32, wide: Some(value), arg_count: count, exit_code: outcome.exit_code })
}

/// Where the message and lParam are among a messaging function's arguments.
fn message_position(name: &str) -> Option<(usize, usize)> {
    Some(match name {
        "SendMessageW" | "SendMessageA" | "PostMessageW" | "PostMessageA" | "SendNotifyMessageW"
        | "DefWindowProcW" | "DefWindowProcA" | "DefDlgProcW" | "DefDlgProcA" | "DefMDIChildProcW"
        | "DefMDIChildProcA" => (1, 3),
        "SendDlgItemMessageW" | "SendDlgItemMessageA" | "CallWindowProcW" | "CallWindowProcA"
        | "DefFrameProcW" | "DefFrameProcA" | "SendMessageTimeoutW" => (2, 4),
        _ => return None,
    })
}

/// A message structure: (layout, scratch copy, extra copies to free).
type MessageCopy = (Layout, u32, Vec<(Layout, u32, u32)>);

/// A 32-bit copy of the structure a message from 64-bit code points to.
fn message_to32(ctx: &ApiContext, memory: &mut dyn GuestMemory, msg: u32, lparam: u32) -> Option<MessageCopy> {
    let (layout, _) = message_struct(msg)?;
    let copy = narrow(ctx, memory, &layout, lparam);
    let mut nested = Vec::new();
    // TVM_INSERTITEM's item text and TCITEM text pointers stay as they are;
    // nested structures only in a few messages.
    if msg == 0x0083 {
        // WM_NCCALCSIZE with wParam TRUE: the WINDOWPOS pointer.
        let pos64 = memory.read_u32(copy + 48).unwrap_or(0);
        if pos64 != 0 {
            let pos32 = narrow(ctx, memory, &WINDOWPOS, pos64);
            let _ = memory.write_u32(copy + 48, pos32);
            nested.push((WINDOWPOS, pos64, pos32));
        }
    }
    Some((layout, copy, nested))
}

fn message_back_to64(ctx: &ApiContext, memory: &mut dyn GuestMemory, msg: u32, original: u32, copy: MessageCopy) {
    let (layout, va, nested) = copy;
    let dir = message_struct(msg).map(|(_, d)| d).unwrap_or(Dir::In);
    if dir != Dir::In {
        copy_back(memory, &layout, va, false, original);
    }
    for (layout, original, copy) in nested {
        copy_back(memory, &layout, copy, false, original);
        free(ctx, copy);
    }
    free(ctx, va);
}

/// Functions with a 64-bit form of their own.
fn x64_function(ctx: &ApiContext, memory: &mut dyn GuestMemory, name: &str, args: &[u64]) -> Option<u64> {
    let call = |memory: &mut dyn GuestMemory, function: &str, narrow: &[u32]| {
        crate::win32::dispatch_with_memory(ctx, memory, "KERNEL32.dll", &ImportName::ByName(function.into()), narrow, 0)
            .map(|o| widen(o.value))
            .unwrap_or(0)
    };
    Some(match name {
        "InitializeCriticalSectionEx" => call(memory, "InitializeCriticalSection", &[args[0] as u32]).max(1),
        // VerSetConditionMask(mask, type bit, condition): three bits per type.
        "VerSetConditionMask" => {
            let (mut mask, types, condition) = (args[0], args[1] as u32, args[2] & 7);
            for bit in 0..8 {
                if types & (1 << bit) != 0 {
                    mask |= condition << (bit * 3);
                }
            }
            mask
        }
        // Exception unwinding information: none yet.
        "RtlCaptureContext" => {
            let _ = memory.write_bytes(args[0] as u32, &[0u8; 0x4D0]);
            0
        }
        "RtlLookupFunctionEntry" => {
            if args[1] != 0 {
                let base = module_base(ctx, args[0] as u32);
                let _ = memory.write_bytes(args[1] as u32, &(base as u64).to_le_bytes());
            }
            0
        }
        "RtlPcToFileHeader" => {
            let base = module_base(ctx, args[0] as u32);
            let _ = memory.write_bytes(args[1] as u32, &(base as u64).to_le_bytes());
            base as u64
        }
        "RtlVirtualUnwind" | "RtlUnwindEx" => 0,
        _ => return None,
    })
}

fn module_base(ctx: &ApiContext, address: u32) -> u32 {
    ctx.modules
        .borrow()
        .iter()
        .find(|m| address >= m.image_base && address < m.image_base + m.size)
        .map(|m| m.image_base)
        .unwrap_or(0)
}

// ── Calls into 64-bit code ───────────────────────────────────────────────

/// Call a 64-bit window procedure with a message from the subsystem:
/// structures the message points to are given to it in 64-bit layout.
pub fn call_proc<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    proc_va: u32,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> Option<u32> {
    let Some((layout, dir)) = (lparam != 0).then(|| message_struct(msg)).flatten() else {
        return memory.call_guest(proc_va, &[hwnd, msg, wparam, lparam]);
    };
    let copy = widen_struct(ctx, memory, &layout, lparam);
    let result = memory.call_guest(proc_va, &[hwnd, msg, wparam, copy]);
    if dir != Dir::In {
        copy_back(memory, &layout, copy, true, lparam);
    }
    free(ctx, copy);
    result
}

/// Call a 64-bit hook procedure: CBT window creation passes a structure
/// pointing to a CREATESTRUCT; window-procedure hooks pass the message.
pub fn call_hook<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    proc_va: u32,
    kind: i32,
    code: u32,
    wparam: u32,
    lparam: u32,
) -> Option<u32> {
    let pair = match (kind, code) {
        (4, _) => Some((CWPSTRUCT, None)),                   // WH_CALLWNDPROC
        (12, _) => Some((CWPRETSTRUCT, None)),               // WH_CALLWNDPROCRET
        (3, _) => Some((MSG, None)),                         // WH_GETMESSAGE
        _ => None,
    };
    let Some((outer, inner)) = pair.filter(|_| lparam != 0) else {
        return memory.call_guest(proc_va, &[code, wparam, lparam]);
    };
    let copy = widen_struct(ctx, memory, &outer, lparam);
    let mut inner_copy = None;
    if let Some(inner) = inner {
        let pointer = memory.read_u32(lparam).unwrap_or(0);
        if pointer != 0 {
            let wide = widen_struct(ctx, memory, &inner, pointer);
            let _ = memory.write_bytes(copy, &(wide as u64).to_le_bytes());
            inner_copy = Some((inner, pointer, wide));
        }
    }
    let result = memory.call_guest(proc_va, &[code, wparam, copy]);
    if let Some((inner, pointer, wide)) = inner_copy {
        copy_back(memory, &inner, wide, true, pointer);
        free(ctx, wide);
    }
    // CBT_CREATEWND's hwndInsertAfter may change; the pointer stays ours.
    if let Some(after) = memory.read_u32(copy + 8) {
        if kind == 5 {
            let _ = memory.write_u32(lparam + 4, after);
        }
    }
    free(ctx, copy);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_have_their_windows_sizes() {
        for (layout, x86, x64) in [
            (MSG, 28, 48),
            (WNDCLASS, 40, 72),
            (WNDCLASSEX, 48, 80),
            (CREATESTRUCT, 48, 80),
            (PAINTSTRUCT, 64, 72),
            (WINDOWPOS, 28, 40),
            (NMHDR, 12, 24),
            (MENUITEMINFO, 48, 80),
            (STARTUPINFO, 68, 104),
            (SYSTEM_INFO, 36, 48),
            (MEMORY_BASIC_INFORMATION, 28, 48),
            (PROCESS_INFORMATION, 16, 24),
            (DRAWITEMSTRUCT, 48, 64),
            (OPENFILENAME, 88, 152),
        ] {
            assert_eq!((layout.size(false), layout.size(true)), (x86, x64), "{layout:?}");
        }
    }

    #[test]
    fn converting_widens_pointers_and_keeps_values() {
        let mut msg32 = Vec::new();
        for value in [0x1234u32, 0x0201, 0xFFFF_FFFF, 0x0040_1000, 99, 5, 6] {
            msg32.extend_from_slice(&value.to_le_bytes());
        }
        let msg64 = convert(&MSG, &msg32, true);
        assert_eq!(&msg64[0..8], &0x1234u64.to_le_bytes());
        assert_eq!(&msg64[8..12], &0x0201u32.to_le_bytes());
        assert_eq!(&msg64[16..24], &u64::MAX.to_le_bytes());
        assert_eq!(&msg64[24..32], &0x0040_1000u64.to_le_bytes());
        assert_eq!(convert(&MSG, &msg64, false), msg32);
    }
}
