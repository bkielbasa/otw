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
    pub const fn new(fields: &'static [F]) -> Self {
        Self { fields, sized: false }
    }

    pub const fn sized(fields: &'static [F]) -> Self {
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

    /// Set a pointer-sized field to a full 64-bit value (in a 32-bit
    /// process, its low half).
    pub fn set_wide<M: GuestMemory + ?Sized>(&self, memory: &mut M, index: usize, value: u64) {
        let at = self.va + self.offsets[index];
        match self.fields[index] {
            F::Q => {
                let _ = memory.write_u64(at, value);
            }
            F::P if self.x64 => {
                let _ = memory.write_u64(at, value);
            }
            _ => self.set(memory, index, value as u32),
        }
    }
}

/// Fill a structure in the process's layout: zeroed, then its first fields
/// set to `values` in order; a sized structure gets its size first.
pub fn write_struct<M: GuestMemory + ?Sized>(memory: &mut M, layout: &Layout, va: u32, values: &[u32]) {
    if va == 0 {
        return;
    }
    let x64 = memory.ptr_size() == 8;
    let _ = memory.write_bytes(va, &vec![0u8; layout.size(x64) as usize]);
    let view = View::new(memory, layout, va);
    let first = usize::from(layout.sized);
    if layout.sized {
        view.set(memory, 0, layout.size(x64));
    }
    for (index, &value) in values.iter().enumerate() {
        view.set(memory, first + index, value);
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
pub const MSGBOXPARAMS: Layout = Layout::sized(&[D, P, P, P, P, D, P, P, P, D]);
pub const SHELLEXECUTEINFO: Layout = Layout::sized(&[D, D, P, P, P, P, P, D, P, P, P, P, D, P, P]);
pub const OPENFILENAME: Layout = Layout::sized(&[
    D, P, P, P, P, D, D, P, D, P, D, P, P, D, W, W, P, P, P, P, P, D, D,
]);
pub const SHFILEOPSTRUCT: Layout = Layout::new(&[P, D, P, P, W, D, P, P]);
pub const LVITEM: Layout = Layout::new(&[D, D, D, D, D, P, D, D, P, D, D, D, P, D, D]);
pub const LVCOLUMN: Layout = Layout::new(&[D, D, D, P, D, D, D, D, D, D, D]);
pub const NMLISTVIEW: Layout = Layout::new(&[P, P, D, D, D, D, D, D, D, D, P]);
pub const NMTOOLBAR: Layout = Layout::new(&[P, P, D, D, AlignP, D, D, B, B, P, P, D, P, A(4)]);
pub const TVITEM: Layout = Layout::new(&[D, P, D, D, P, D, D, D, D, P]);
pub const PSHNOTIFY: Layout = Layout::new(&[P, P, D, P]);
pub const CBT_CREATEWND: Layout = Layout::new(&[P, P]);

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
pub(crate) fn x64_only(name: &str) -> Option<usize> {
    Some(match name {
        "InitializeCriticalSectionEx" => 3,
        "VerSetConditionMask" => 3,
        "RtlPcToFileHeader" => 2,
        _ => return None,
    })
}

fn by_name(name: &ImportName) -> Option<&str> {
    name.as_str()
}

/// The x64 argument count of an import.
pub fn arg_count(ctx: &ApiContext, dll: &str, name: &ImportName) -> Option<usize> {
    if crate::win32::is_ucrt(crate::win32::api_set_host(dll).unwrap_or(dll)) {
        return crate::win32::ucrt::arg_count(by_name(name)?);
    }
    if dll.eq_ignore_ascii_case("dwrite.dll") {
        return crate::win32::dwrite::arg_count(by_name(name)?);
    }
    if let Some(count) = by_name(name).and_then(crate::win32::ipc::arg_count) {
        return Some(count);
    }
    if let Some(count) = by_name(name).and_then(crate::win32::seh64::arg_count) {
        return Some(count);
    }
    if let Some(count) = by_name(name).and_then(crate::win32::nt::arg_count) {
        return Some(count);
    }
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
    if std::env::var_os("OTW_TRACE_HIGH").is_some() {
        if let Some(high) = args.iter().take(count).find(|&&a| a > u32::MAX as u64 && crate::win32::vmem::owns_high(ctx, a)) {
            eprintln!("[high] {dll}!{name:?} gets {high:#x} (args {:x?})", &args[..count.min(args.len())]);
        }
    }
    if crate::win32::is_ucrt(crate::win32::api_set_host(dll).unwrap_or(dll)) {
        return crate::win32::ucrt::call(ctx, memory, by_name(name)?, args);
    }
    if dll.eq_ignore_ascii_case("dwrite.dll") {
        return crate::win32::dwrite::call(ctx, memory, by_name(name)?, args);
    }
    if let Some(outcome) = by_name(name).and_then(|text| crate::win32::ipc::call(ctx, memory, text, args)) {
        return Some(outcome);
    }
    if let Some(outcome) = by_name(name).and_then(|text| crate::win32::seh64::call(ctx, memory, text, args)) {
        return Some(outcome);
    }
    if let Some(outcome) = by_name(name).and_then(|text| crate::win32::nt::call(ctx, memory, text, args)) {
        return Some(outcome);
    }
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
    if trace() {
        eprintln!("[x64] {dll}!{} {:x?} -> {:x?}", name.display(), args, narrow_args);
    }
    let outcome: Option<ApiOutcome> = crate::win32::dispatch_with_memory(ctx, memory, dll, &name, &narrow_args, caller);
    let outcome = outcome?;
    let value = outcome.wide.unwrap_or_else(|| widen(outcome.value));
    Some(ApiOutcome { value: value as u32, wide: Some(value), arg_count: count, exit_code: outcome.exit_code })
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
        "RtlPcToFileHeader" => {
            let base = module_base(ctx, args[0] as u32);
            let _ = memory.write_bytes(args[1] as u32, &(base as u64).to_le_bytes());
            base as u64
        }
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

/// Call a 64-bit window procedure with a message from the subsystem (its
/// structures are already in the process's layout).
pub fn call_proc<M: GuestMemory + ?Sized>(
    _ctx: &ApiContext,
    memory: &mut M,
    proc_va: u32,
    hwnd: u32,
    msg: u32,
    wparam: u32,
    lparam: u32,
) -> Option<u32> {
    memory.call_guest(proc_va, &[hwnd, msg, wparam, lparam])
}

/// Call a 64-bit hook procedure: CBT window creation passes a structure
/// pointing to a CREATESTRUCT; window-procedure hooks pass the message.
pub fn call_hook<M: GuestMemory + ?Sized>(
    _ctx: &ApiContext,
    memory: &mut M,
    proc_va: u32,
    _kind: i32,
    code: u32,
    wparam: u32,
    lparam: u32,
) -> Option<u32> {
    memory.call_guest(proc_va, &[code, wparam, lparam])
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
    fn views_use_the_process_layout() {
        struct Flat(Vec<u8>, u32);
        impl GuestMemory for Flat {
            fn read_bytes(&mut self, va: u32, len: usize) -> Option<Vec<u8>> {
                self.0.get(va as usize..va as usize + len).map(<[u8]>::to_vec)
            }
            fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Option<()> {
                self.0.get_mut(va as usize..va as usize + bytes.len())?.copy_from_slice(bytes);
                Some(())
            }
            fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
                self.write_bytes(va, &value.to_le_bytes())
            }
            fn ptr_size(&self) -> u32 {
                self.1
            }
        }
        for size in [4, 8] {
            let mut memory = Flat(vec![0xAA; 80], size);
            write_struct(&mut memory, &MSG, 16, &[0x1234, 0x0201, 0xFFFF_FFFF, 0x0040_1000, 99, 5, 6]);
            let view = View::new(&memory, &MSG, 16);
            assert_eq!(view.get32(&mut memory, 1), 0x0201);
            assert_eq!(view.get32(&mut memory, 3), 0x0040_1000);
            if size == 8 {
                // Pointer-sized fields are written in full: -1 stays -1.
                assert_eq!(view.get(&mut memory, 2), u64::MAX);
                assert_eq!(&memory.0[24..28], &0x0201u32.to_le_bytes());
            } else {
                assert_eq!(&memory.0[20..24], &0x0201u32.to_le_bytes());
            }
        }
    }
}
