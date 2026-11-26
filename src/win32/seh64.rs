//! Structured exception handling for 64-bit programs: table-based
//! unwinding as Windows x64 does it.
//!
//! Each module's exception directory (`.pdata`) lists its functions with
//! unwind information describing their prologs; programs that generate
//! code add tables of their own (`RtlAddFunctionTable`). An exception — a
//! CPU fault (delivered by the kernel to `OTW_DispatchException64`) or
//! `RaiseException` — is dispatched by walking the stack frame by frame
//! ([`virtual_unwind`]) and calling each frame's language handler
//! (`__C_specific_handler`, `__CxxFrameHandler4`, ...), after the vectored
//! handlers. A handler that takes the exception calls `RtlUnwindEx`, which
//! walks again calling the termination handlers, then resumes at the
//! target with every register restored — for C++ `catch` after running the
//! runtime's consolidation callback (which runs the catch block).
//!
//! Where the subsystem called into guest code (a window procedure, a
//! handler), the walk goes on from the code that called the subsystem.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::collections::HashMap;
use std::rc::Rc;

use crate::{
    kernel::{
        context64,
        layout::SENTINEL_BASE,
        memory::FullRegisters,
    },
    win32::{ApiContext, ApiOutcome, GuestMemory},
};

const UNW_FLAG_EHANDLER: u32 = 1;
const UNW_FLAG_UHANDLER: u32 = 2;
const UNW_FLAG_CHAININFO: u32 = 4;

const EH_NONCONTINUABLE: u32 = 0x1;
const EH_UNWINDING: u32 = 0x2;
const EH_EXIT_UNWIND: u32 = 0x4;
const EH_TARGET_UNWIND: u32 = 0x20;

const STATUS_UNWIND: u32 = 0xC000_0027;
const STATUS_LONGJUMP: u32 = 0x8000_0026;
const STATUS_UNWIND_CONSOLIDATE: u32 = 0x8000_0029;
const STATUS_NONCONTINUABLE_EXCEPTION: u32 = 0xC000_0025;

const DISPATCHER_CONTEXT_SIZE: u32 = 0x50;

#[derive(Debug, Default)]
pub struct State {
    /// Each module's function table: (begin, end, unwind) RVAs, sorted.
    tables: HashMap<u32, Rc<Vec<[u32; 3]>>>,
    /// Where a stack walk continues past a call the subsystem made into
    /// guest code for exception handling: the stack pointer at the call,
    /// and the context beyond it — the exception's for handlers called
    /// while dispatching, the target frame's for a consolidation callback
    /// running a catch block (as a machine frame would).
    boundaries: Vec<(u32, FullRegisters)>,
}

fn trace() -> bool {
    std::env::var_os("OTW_TRACE_EH").is_some()
}

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "OTW_DispatchException64" | "RtlRestoreContext" => 2,
        "RtlCaptureContext" | "RtlRaiseException" => 1,
        "RtlLookupFunctionEntry" => 3,
        "RtlVirtualUnwind" => 8,
        "RtlUnwindEx" => 6,
        "RtlUnwind" | "RaiseException" => 4,
        _ => return None,
    })
}

fn rd32(memory: &mut dyn GuestMemory, va: u64) -> u32 {
    memory.read_u32(va as u32).unwrap_or(0)
}

fn rd64(memory: &mut dyn GuestMemory, va: u64) -> u64 {
    memory.read_u64(va as u32).unwrap_or(0)
}

fn rd8(memory: &mut dyn GuestMemory, va: u64) -> u8 {
    memory.read_bytes(va as u32, 1).and_then(|b| b.first().copied()).unwrap_or(0)
}

fn alloc(ctx: &ApiContext, size: u32) -> u32 {
    ctx.shared.borrow_mut().heap.alloc(size).unwrap_or(0)
}

fn release(ctx: &ApiContext, va: u32) {
    ctx.shared.borrow_mut().heap.free(va);
}

// ── Function tables ──────────────────────────────────────────────────────

/// The 64-bit module containing an address: its base and size.
fn module_of(ctx: &ApiContext, memory: &mut dyn GuestMemory, pc: u64) -> Option<(u32, u32)> {
    let mut bases = vec![ctx.image_base];
    bases.extend(ctx.modules.borrow().iter().map(|m| m.image_base));
    for base in bases {
        if base == 0 || pc < base as u64 {
            continue;
        }
        let pe = rd32(memory, base as u64 + 0x3C);
        if rd32(memory, (base + pe) as u64) != 0x4550 || memory.read_bytes(base + pe + 24, 2) != Some(vec![0x0B, 0x02]) {
            continue;
        }
        let size = rd32(memory, (base + pe + 24 + 56) as u64);
        if pc < base as u64 + size as u64 {
            return Some((base, size));
        }
    }
    None
}

fn table_of(ctx: &ApiContext, memory: &mut dyn GuestMemory, base: u32) -> Rc<Vec<[u32; 3]>> {
    if let Some(table) = ctx.shared.borrow().seh.tables.get(&base) {
        return table.clone();
    }
    let pe = rd32(memory, base as u64 + 0x3C);
    let directory = base + pe + 24 + 112 + 3 * 8;
    let (rva, size) = (rd32(memory, directory as u64), rd32(memory, directory as u64 + 4));
    let bytes = if rva != 0 { memory.read_bytes(base + rva, size as usize).unwrap_or_default() } else { Vec::new() };
    let mut entries: Vec<[u32; 3]> = bytes
        .chunks_exact(12)
        .map(|c| [0, 4, 8].map(|o| u32::from_le_bytes([c[o], c[o + 1], c[o + 2], c[o + 3]])))
        .collect();
    entries.sort_by_key(|e| e[0]);
    let table = Rc::new(entries);
    ctx.shared.borrow_mut().seh.tables.insert(base, table.clone());
    table
}

/// RtlLookupFunctionEntry: the image (or table) base and the address of
/// the RUNTIME_FUNCTION covering `pc`.
pub fn lookup(ctx: &ApiContext, memory: &mut dyn GuestMemory, pc: u64) -> Option<(u64, u64)> {
    if let Some((base, _)) = module_of(ctx, memory, pc) {
        let table = table_of(ctx, memory, base);
        let rva = (pc - base as u64) as u32;
        let index = table.partition_point(|e| e[0] <= rva);
        if index == 0 {
            return None;
        }
        let entry = table[index - 1];
        if rva >= entry[1] {
            return None;
        }
        // The address of that entry in the image.
        let pe = rd32(memory, base as u64 + 0x3C);
        let directory = rd32(memory, (base + pe + 24 + 112 + 3 * 8) as u64);
        let count = rd32(memory, (base + pe + 24 + 112 + 3 * 8 + 4) as u64) / 12;
        // Tables are sorted already in practice; find the entry itself.
        let (mut low, mut high) = (0u32, count);
        while low < high {
            let middle = (low + high) / 2;
            let at = (base + directory + middle * 12) as u64;
            let begin = rd32(memory, at);
            if begin <= rva && rva < rd32(memory, at + 4) {
                return Some((base as u64, at));
            }
            if begin > rva {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        return None;
    }
    // Tables added for generated code.
    let tables = ctx.shared.borrow().nt.function_tables.clone();
    for (base, table, count) in tables {
        if pc < base {
            continue;
        }
        let rva = pc - base;
        for index in 0..count as u64 {
            let at = table + index * 12;
            let (begin, end) = (rd32(memory, at) as u64, rd32(memory, at + 4) as u64);
            if begin <= rva && rva < end {
                return Some((base, at));
            }
        }
    }
    None
}

// ── Unwinding one frame ──────────────────────────────────────────────────

fn opcode_slots(op: u8, info: u8) -> u32 {
    match op {
        1 => if info == 0 { 2 } else { 3 },
        4 | 8 => 2,
        5 | 9 => 3,
        _ => 1,
    }
}

/// Undo an epilog the instruction pointer is in, if it is in one: `add
/// rsp`/`lea rsp`, pops of nonvolatile registers, then `ret` or a jump.
fn unwind_epilog(memory: &mut dyn GuestMemory, regs: &mut FullRegisters, begin: u64, end: u64) -> bool {
    let code = memory.read_bytes(regs.rip as u32, 64).unwrap_or_default();
    let at = |k: usize| code.get(k).copied().unwrap_or(0);
    let mut k = 0usize;
    let mut rsp = regs.gpr[4];
    // add rsp, imm8 / imm32
    if at(k) == 0x48 && at(k + 1) == 0x83 && at(k + 2) == 0xC4 {
        rsp = rsp.wrapping_add(at(k + 3) as i8 as i64 as u64);
        k += 4;
    } else if at(k) == 0x48 && at(k + 1) == 0x81 && at(k + 2) == 0xC4 {
        let imm = i32::from_le_bytes([at(k + 3), at(k + 4), at(k + 5), at(k + 6)]);
        rsp = rsp.wrapping_add(imm as i64 as u64);
        k += 7;
    } else if (at(k) & 0xFE) == 0x48 && at(k + 1) == 0x8D && (at(k + 2) & 0x38) == 0x20 && (at(k + 2) >> 6) != 3 {
        // lea rsp, [reg + disp]
        let modrm = at(k + 2);
        let reg = ((modrm & 7) | if at(k) & 1 != 0 { 8 } else { 0 }) as usize;
        if modrm & 7 == 4 {
            return false; // SIB form: not an epilog we know
        }
        let (disp, length) = match modrm >> 6 {
            1 => (at(k + 3) as i8 as i64, 4),
            2 => (i32::from_le_bytes([at(k + 3), at(k + 4), at(k + 5), at(k + 6)]) as i64, 7),
            _ => (0, 3),
        };
        rsp = regs.gpr[reg].wrapping_add(disp as u64);
        k += length;
    }
    let mut pops: Vec<usize> = Vec::new();
    loop {
        if (0x58..=0x5F).contains(&at(k)) {
            pops.push((at(k) - 0x58) as usize);
            k += 1;
        } else if at(k) == 0x41 && (0x58..=0x5F).contains(&at(k + 1)) {
            pops.push((at(k + 1) - 0x58 + 8) as usize);
            k += 2;
        } else {
            break;
        }
    }
    let ends = match at(k) {
        0xC3 => true,
        0xF3 if at(k + 1) == 0xC3 => true,
        0xC2 => true,
        0xE9 => {
            let target = (regs.rip + k as u64 + 5).wrapping_add(i32::from_le_bytes([at(k + 1), at(k + 2), at(k + 3), at(k + 4)]) as i64 as u64);
            target < begin || target >= end
        }
        0xEB => {
            let target = (regs.rip + k as u64 + 2).wrapping_add(at(k + 1) as i8 as i64 as u64);
            target < begin || target >= end
        }
        0xFF if at(k + 1) == 0x25 => true,
        0x48 if at(k + 1) == 0xFF && at(k + 2) == 0x25 => true,
        _ => false,
    };
    // A bare jump is not an epilog.
    if !ends || (k == 0 && !matches!(at(0), 0xC3 | 0xF3 | 0xC2)) {
        return false;
    }
    for reg in pops {
        regs.gpr[reg] = rd64(memory, rsp);
        rsp += 8;
    }
    regs.rip = rd64(memory, rsp);
    regs.gpr[4] = rsp + 8;
    true
}

/// RtlVirtualUnwind: undo one frame of `regs`. Returns the frame's
/// language handler of the wanted kind (0 when none), its data, and the
/// establisher frame.
pub fn virtual_unwind(
    memory: &mut dyn GuestMemory,
    kind: u32,
    base: u64,
    entry: u64,
    regs: &mut FullRegisters,
) -> (u64, u64, u64) {
    let pc = regs.rip;
    let mut function = entry;
    let mut frame = regs.gpr[4];
    let mut machine_frame = false;
    let mut flags;
    let mut handler_at;
    let mut prolog_offset;
    let mut first = true;
    loop {
        let begin = rd32(memory, function) as u64;
        let end = rd32(memory, function + 4) as u64;
        let mut unwind = rd32(memory, function + 8) as u64;
        if unwind & 1 != 0 {
            // An index to the primary entry.
            let primary = base + (unwind & !1);
            unwind = rd32(memory, primary + 8) as u64;
        }
        let info = base + unwind;
        let header = rd8(memory, info);
        flags = (header >> 3) as u32;
        let prolog = rd8(memory, info + 1) as u64;
        let count = rd8(memory, info + 2) as u32;
        let frame_byte = rd8(memory, info + 3);
        let (frame_reg, frame_offset) = ((frame_byte & 0xF) as usize, (frame_byte >> 4) as u64);
        if frame_reg != 0 {
            frame = regs.gpr[frame_reg].wrapping_sub(frame_offset * 16);
        }
        if first && pc >= base + begin && pc < base + begin + prolog {
            prolog_offset = pc - base - begin;
        } else {
            prolog_offset = u64::MAX;
            if first && count != 0 && pc >= base + begin && pc < base + end && unwind_epilog(memory, regs, base + begin, base + end) {
                return (0, 0, frame);
            }
        }
        first = false;
        let mut i = 0u32;
        while i < count {
            let code_offset = rd8(memory, info + 4 + i as u64 * 2) as u64;
            let op_byte = rd8(memory, info + 5 + i as u64 * 2);
            let (op, op_info) = (op_byte & 0xF, op_byte >> 4);
            let slots = opcode_slots(op, op_info);
            let slot = |memory: &mut dyn GuestMemory, n: u32| {
                memory.read_bytes((info + 4 + (i + n) as u64 * 2) as u32, 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0)
            };
            if prolog_offset >= code_offset {
                match op {
                    0 => {
                        regs.gpr[op_info as usize] = rd64(memory, regs.gpr[4]);
                        regs.gpr[4] += 8;
                    }
                    1 => {
                        let size = if op_info == 0 {
                            slot(memory, 1) as u64 * 8
                        } else {
                            slot(memory, 1) as u64 | (slot(memory, 2) as u64) << 16
                        };
                        regs.gpr[4] += size;
                    }
                    2 => regs.gpr[4] += op_info as u64 * 8 + 8,
                    3 => regs.gpr[4] = frame,
                    4 => {
                        let at = frame + slot(memory, 1) as u64 * 8;
                        regs.gpr[op_info as usize] = rd64(memory, at);
                    }
                    5 => {
                        let at = frame + (slot(memory, 1) as u64 | (slot(memory, 2) as u64) << 16);
                        regs.gpr[op_info as usize] = rd64(memory, at);
                    }
                    8 | 9 => {
                        let at = if op == 8 {
                            frame + slot(memory, 1) as u64 * 16
                        } else {
                            frame + (slot(memory, 1) as u64 | (slot(memory, 2) as u64) << 16)
                        };
                        let bytes = memory.read_bytes(at as u32, 16).unwrap_or_else(|| vec![0; 16]);
                        regs.xmm[op_info as usize].copy_from_slice(&bytes);
                    }
                    10 => {
                        if op_info != 0 {
                            regs.gpr[4] += 8;
                        }
                        regs.rip = rd64(memory, regs.gpr[4]);
                        regs.eflags = rd64(memory, regs.gpr[4] + 16);
                        regs.gpr[4] = rd64(memory, regs.gpr[4] + 24);
                        machine_frame = true;
                    }
                    _ => {}
                }
            }
            i += slots;
        }
        handler_at = info + 4 + ((count + 1) & !1) as u64 * 2;
        if flags & UNW_FLAG_CHAININFO == 0 {
            break;
        }
        function = handler_at;
    }
    if !machine_frame {
        regs.rip = rd64(memory, regs.gpr[4]);
        regs.gpr[4] += 8;
    }
    if flags & kind == 0 || prolog_offset != u64::MAX {
        return (0, 0, frame);
    }
    let handler = rd32(memory, handler_at) as u64;
    (base + handler, handler_at + 4, frame)
}

/// One step of a stack walk: unwind `regs` past its frame. Returns the
/// handler, its data, the establisher frame, the image base and the
/// function entry; None at the end of the stack.
struct Step {
    handler: u64,
    data: u64,
    frame: u64,
    base: u64,
    entry: u64,
    next: FullRegisters,
}

fn step(ctx: &ApiContext, memory: &mut dyn GuestMemory, kind: u32, regs: &FullRegisters, levels: &mut Vec<u32>) -> Option<Step> {
    let mut regs = regs.clone();
    // Through a callback from the subsystem: on from its caller.
    while regs.rip == SENTINEL_BASE as u64 {
        let level = levels.pop()?;
        let consolidation =
            ctx.shared.borrow().seh.boundaries.iter().rev().find(|(at, _)| *at == level).map(|(_, target)| target.clone());
        match consolidation {
            Some(target) => regs = target,
            None => {
                regs.rip = rd64(memory, level as u64);
                regs.gpr[4] = level as u64 + 8;
            }
        }
    }
    if regs.rip == 0 {
        return None;
    }
    let mut next = regs.clone();
    match lookup(ctx, memory, regs.rip) {
        Some((base, entry)) => {
            let (handler, data, frame) = virtual_unwind(memory, kind, base, entry, &mut next);
            Some(Step { handler, data, frame, base, entry, next })
        }
        None => {
            // A leaf function (inside a module), or the end of the stack.
            module_of(ctx, memory, regs.rip)?;
            let frame = regs.gpr[4];
            next.rip = rd64(memory, frame);
            next.gpr[4] = frame + 8;
            Some(Step { handler: 0, data: 0, frame, base: 0, entry: 0, next })
        }
    }
}

/// The current frame's registers, as `step` will see them: `regs` after
/// following callback boundaries.
fn fill_dispatcher_context(
    memory: &mut dyn GuestMemory,
    dc: u32,
    regs: &FullRegisters,
    step: &Step,
    target_ip: u64,
    context: u32,
) {
    let mut bytes = vec![0u8; DISPATCHER_CONTEXT_SIZE as usize];
    for (index, value) in [regs.rip, step.base, step.entry, step.frame, target_ip, context as u64, step.handler, step.data, 0]
        .into_iter()
        .enumerate()
    {
        bytes[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    let _ = memory.write_bytes(dc, &bytes);
}

/// Follow callback boundaries so `regs` is a real guest frame (the
/// handler sees that frame's instruction pointer as ControlPc).
fn settle(ctx: &ApiContext, memory: &mut dyn GuestMemory, regs: &mut FullRegisters, levels: &mut Vec<u32>) -> bool {
    while regs.rip == SENTINEL_BASE as u64 {
        let Some(level) = levels.pop() else { return false };
        let consolidation =
            ctx.shared.borrow().seh.boundaries.iter().rev().find(|(at, _)| *at == level).map(|(_, target)| target.clone());
        match consolidation {
            Some(target) => *regs = target,
            None => {
                regs.rip = rd64(memory, level as u64);
                regs.gpr[4] = level as u64 + 8;
            }
        }
    }
    true
}

// ── Dispatch ─────────────────────────────────────────────────────────────

enum Outcome {
    Handled,
    Continue,
    Unhandled,
}

fn dispatch(ctx: &ApiContext, memory: &mut dyn GuestMemory, record: u32, context: u32) -> Outcome {
    // Vectored handlers first: handler(&EXCEPTION_POINTERS).
    let vectored = ctx.shared.borrow().nt.vectored.clone();
    if !vectored.is_empty() {
        let pointers = alloc(ctx, 16);
        let _ = memory.write_u64(pointers, record as u64);
        let _ = memory.write_u64(pointers + 8, context as u64);
        for (_, handler) in vectored {
            let result = memory.call_guest_wide(handler as u32, &[pointers as u64]);
            if memory.resume_pending() || memory.stopping() {
                return Outcome::Handled;
            }
            if result.map(|r| r as u32 as i32) == Some(-1) {
                release(ctx, pointers);
                return Outcome::Continue;
            }
        }
        release(ctx, pointers);
    }
    let mut regs = context64::read(memory, context);
    let mut levels = memory.callback_levels();
    let (work, dc) = (alloc(ctx, context64::CONTEXT_SIZE), alloc(ctx, DISPATCHER_CONTEXT_SIZE));
    let mut outcome = Outcome::Unhandled;
    // Handlers that walk the stack (unwinding) continue past their own
    // call at the exception's context.
    let boundary = memory.full_registers().map(|r| r.gpr[4] as u32).unwrap_or(0);
    ctx.shared.borrow_mut().seh.boundaries.push((boundary, regs.clone()));
    for _ in 0..4096 {
        if !settle(ctx, memory, &mut regs, &mut levels) {
            break;
        }
        let Some(step) = step(ctx, memory, UNW_FLAG_EHANDLER, &regs, &mut levels) else { break };
        if step.handler != 0 {
            if trace() {
                eprintln!("[eh64] frame {:#x} at {:#x}: handler {:#x}", step.frame, regs.rip, step.handler);
            }
            context64::write(memory, work, &regs);
            fill_dispatcher_context(memory, dc, &regs, &step, 0, work);
            let result = memory.call_guest_wide(step.handler as u32, &[record as u64, step.frame, context as u64, dc as u64]);
            if memory.resume_pending() || memory.stopping() || result.is_none() {
                outcome = Outcome::Handled;
                break;
            }
            if result == Some(0) {
                outcome = Outcome::Continue;
                break;
            }
        }
        regs = step.next;
    }
    {
        let mut shared = ctx.shared.borrow_mut();
        if let Some(index) = shared.seh.boundaries.iter().rposition(|(at, _)| *at == boundary) {
            shared.seh.boundaries.remove(index);
        }
    }
    release(ctx, work);
    release(ctx, dc);
    outcome
}

/// Dispatch an exception whose record and context are in guest memory,
/// then resume or end the process.
fn dispatch_and_finish(ctx: &ApiContext, memory: &mut dyn GuestMemory, record: u32, context: u32, count: usize) -> ApiOutcome {
    let code = memory.read_u32(record).unwrap_or(0);
    let flags = memory.read_u32(record + 4).unwrap_or(0);
    if trace() {
        eprintln!("[eh64] dispatch {code:#010x} at {:#x}", memory.read_u64(record + 0x10).unwrap_or(0));
    }
    let terminate = |code: u32| ApiOutcome { value: 0, wide: None, arg_count: count, exit_code: Some(code) };
    match dispatch(ctx, memory, record, context) {
        Outcome::Handled if memory.resume_pending() || memory.stopping() => ApiOutcome::new(0, count),
        Outcome::Continue if flags & EH_NONCONTINUABLE != 0 => {
            eprintln!("[eh] a handler continued a noncontinuable exception {code:#010x}; terminating");
            terminate(STATUS_NONCONTINUABLE_EXCEPTION)
        }
        Outcome::Continue => {
            let regs = context64::read(memory, context);
            memory.request_resume_full(regs);
            ApiOutcome::new(0, count)
        }
        _ => {
            eprintln!(
                "[eh] unhandled exception {code:#010x} at {:#x}; this program has performed an illegal operation and will be shut down",
                memory.read_u64(record + 0x10).unwrap_or(0)
            );
            terminate(code)
        }
    }
}

/// The registers of the code that called the import being handled.
fn caller(memory: &mut dyn GuestMemory) -> FullRegisters {
    let mut regs = memory.full_registers().unwrap_or_default();
    let rsp = regs.gpr[4];
    regs.rip = rd64(memory, rsp);
    regs.gpr[4] = rsp + 8;
    regs
}

/// Put a record and context below the current stack and move the stack
/// pointer under them, so that calls into handlers leave them intact.
fn place(ctx: &ApiContext, memory: &mut dyn GuestMemory, record: &[u8], regs: &FullRegisters) -> (u32, u32) {
    let _ = ctx;
    let mut current = memory.full_registers().unwrap_or_default();
    let rsp = current.gpr[4] as u32;
    let base = rsp.wrapping_sub(0x200 + context64::RECORD_SIZE + context64::CONTEXT_SIZE) & !0xF;
    let record_va = base + 0x30;
    let context_va = (record_va + context64::RECORD_SIZE + 15) & !15;
    let _ = memory.write_bytes(record_va, record);
    context64::write(memory, context_va, regs);
    current.gpr[4] = base as u64;
    memory.set_full_registers(&current);
    (record_va, context_va)
}

/// Resume at `regs` after an unwind; a consolidation (C++ catch) or long
/// jump record changes where.
fn restore(ctx: &ApiContext, memory: &mut dyn GuestMemory, mut regs: FullRegisters, record: u32) {
    let code = memory.read_u32(record).unwrap_or(0);
    let count = memory.read_u32(record + 0x18).unwrap_or(0);
    let info = |memory: &mut dyn GuestMemory, i: u32| memory.read_u64(record + 0x20 + i * 8).unwrap_or(0);
    if code == STATUS_UNWIND_CONSOLIDATE && count >= 1 {
        // The callback (the C++ runtime's, which runs the catch block)
        // runs with the target frame's nonvolatile registers and returns
        // where to continue; a walk from inside it continues at the target.
        let callback = info(memory, 0);
        let saved = memory.full_registers().unwrap_or_default();
        let mut entry = saved.clone();
        for index in [3usize, 5, 6, 7, 12, 13, 14, 15] {
            entry.gpr[index] = regs.gpr[index];
        }
        entry.xmm[6..].copy_from_slice(&regs.xmm[6..]);
        memory.set_full_registers(&entry);
        let at = saved.gpr[4] as u32;
        ctx.shared.borrow_mut().seh.boundaries.push((at, regs.clone()));
        if trace() {
            eprintln!("[eh64] consolidate: callback {callback:#x}");
        }
        let result = memory.call_guest_wide(callback as u32, &[record as u64]);
        {
            let mut shared = ctx.shared.borrow_mut();
            if let Some(index) = shared.seh.boundaries.iter().rposition(|(a, _)| *a == at) {
                shared.seh.boundaries.remove(index);
            }
        }
        if memory.resume_pending() || memory.stopping() {
            return;
        }
        memory.set_full_registers(&saved);
        let Some(continuation) = result else { return };
        regs.rip = continuation;
    } else if code == STATUS_LONGJUMP && count >= 1 {
        // _JUMP_BUFFER: frame, rbx, rsp, rbp, rsi, rdi, r12-r15, rip,
        // mxcsr, fpcsr, spare, xmm6-15.
        let buffer = info(memory, 0) as u32;
        let read = |memory: &mut dyn GuestMemory, offset: u32| memory.read_u64(buffer + offset).unwrap_or(0);
        for (index, offset) in [(3usize, 0x08), (4, 0x10), (5, 0x18), (6, 0x20), (7, 0x28), (12, 0x30), (13, 0x38), (14, 0x40), (15, 0x48)] {
            regs.gpr[index] = read(memory, offset);
        }
        regs.rip = read(memory, 0x50);
        for index in 0..10 {
            let bytes = memory.read_bytes(buffer + 0x60 + index * 16, 16).unwrap_or_else(|| vec![0; 16]);
            regs.xmm[6 + index as usize].copy_from_slice(&bytes);
        }
    }
    if trace() {
        eprintln!("[eh64] resume at {:#x} rsp {:#x}", regs.rip, regs.gpr[4]);
    }
    memory.request_resume_full(regs);
}

/// RtlUnwindEx: call the termination handlers of every frame up to the
/// target, then resume at `target_ip` in it.
fn unwind(ctx: &ApiContext, memory: &mut dyn GuestMemory, target_frame: u64, target_ip: u64, record: u32, value: u64) {
    let mut regs = caller(memory);
    let record = if record != 0 {
        record
    } else {
        let block = alloc(ctx, context64::RECORD_SIZE);
        let _ = memory.write_bytes(block, &context64::record(STATUS_UNWIND, 0, regs.rip, &[]));
        block
    };
    let flags = memory.read_u32(record + 4).unwrap_or(0);
    let _ = memory.write_u32(record + 4, flags | EH_UNWINDING | if target_frame == 0 { EH_EXIT_UNWIND } else { 0 });
    let mut levels = memory.callback_levels();
    let (work, dc) = (alloc(ctx, context64::CONTEXT_SIZE), alloc(ctx, DISPATCHER_CONTEXT_SIZE));
    let mut reached = false;
    for _ in 0..4096 {
        if !settle(ctx, memory, &mut regs, &mut levels) {
            break;
        }
        let Some(step) = step(ctx, memory, UNW_FLAG_UHANDLER, &regs, &mut levels) else { break };
        if target_frame != 0 && step.frame > target_frame && step.base != 0 {
            eprintln!("[eh64] unwind passed its target frame {target_frame:#x} (at {:#x})", step.frame);
            break;
        }
        let at_target = step.frame == target_frame;
        if at_target {
            let flags = memory.read_u32(record + 4).unwrap_or(0);
            let _ = memory.write_u32(record + 4, flags | EH_TARGET_UNWIND);
        }
        if step.handler != 0 {
            if trace() {
                eprintln!("[eh64] unwind frame {:#x} at {:#x}: handler {:#x}", step.frame, regs.rip, step.handler);
            }
            context64::write(memory, work, &regs);
            fill_dispatcher_context(memory, dc, &regs, &step, target_ip, work);
            memory.call_guest_wide(step.handler as u32, &[record as u64, step.frame, work as u64, dc as u64]);
            if memory.resume_pending() || memory.stopping() {
                release(ctx, work);
                release(ctx, dc);
                return;
            }
        }
        if at_target {
            reached = true;
            break;
        }
        regs = step.next;
    }
    release(ctx, work);
    release(ctx, dc);
    if !reached && target_frame != 0 {
        eprintln!("[eh64] unwind did not reach its target frame {target_frame:#x}");
    }
    regs.gpr[0] = value;
    regs.rip = target_ip;
    restore(ctx, memory, regs, record);
}

pub fn call(ctx: &ApiContext, memory: &mut dyn GuestMemory, name: &str, a: &[u64]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    let arg = |i: usize| a.get(i).copied().unwrap_or(0);
    let ok = |value: u64| Some(ApiOutcome::wide(value, count));
    match name {
        "OTW_DispatchException64" => Some(dispatch_and_finish(ctx, memory, arg(0) as u32, arg(1) as u32, count)),
        "RaiseException" => {
            // (code, flags, count, arguments): ULONG_PTR arguments.
            let regs = caller(memory);
            let params: Vec<u64> = (0..arg(2).min(15)).map(|i| memory.read_u64(arg(3) as u32 + i as u32 * 8).unwrap_or(0)).collect();
            let record = context64::record(arg(0) as u32, arg(1) as u32 & EH_NONCONTINUABLE, regs.rip, &params);
            let (record, context) = place(ctx, memory, &record, &regs);
            if trace() {
                eprintln!("[eh64] RaiseException {:#010x} from {:#x}", arg(0), regs.rip);
            }
            Some(dispatch_and_finish(ctx, memory, record, context, count))
        }
        "RtlRaiseException" => {
            let regs = caller(memory);
            let bytes = memory.read_bytes(arg(0) as u32, context64::RECORD_SIZE as usize).unwrap_or_default();
            let (record, context) = place(ctx, memory, &bytes, &regs);
            Some(dispatch_and_finish(ctx, memory, record, context, count))
        }
        "RtlCaptureContext" => {
            let regs = caller(memory);
            context64::write(memory, arg(0) as u32, &regs);
            ok(0)
        }
        "RtlLookupFunctionEntry" => match lookup(ctx, memory, arg(0)) {
            Some((base, entry)) => {
                if arg(1) != 0 {
                    let _ = memory.write_u64(arg(1) as u32, base);
                }
                ok(entry)
            }
            None => {
                if arg(1) != 0 {
                    let _ = memory.write_u64(arg(1) as u32, 0);
                }
                ok(0)
            }
        },
        "RtlVirtualUnwind" => {
            // (type, base, pc, entry, context, &data, &frame, pointers)
            let mut regs = context64::read(memory, arg(4) as u32);
            regs.rip = arg(2);
            let (handler, data, frame) = virtual_unwind(memory, arg(0) as u32, arg(1), arg(3), &mut regs);
            context64::write(memory, arg(4) as u32, &regs);
            if arg(5) != 0 {
                let _ = memory.write_u64(arg(5) as u32, data);
            }
            if arg(6) != 0 {
                let _ = memory.write_u64(arg(6) as u32, frame);
            }
            ok(handler)
        }
        "RtlUnwindEx" | "RtlUnwind" => {
            unwind(ctx, memory, arg(0), arg(1), arg(2) as u32, arg(3));
            ok(0)
        }
        "RtlRestoreContext" => {
            let regs = context64::read(memory, arg(0) as u32);
            if arg(1) != 0 {
                restore(ctx, memory, regs, arg(1) as u32);
            } else {
                memory.request_resume_full(regs);
            }
            ok(0)
        }
        _ => None,
    }
}
