//! Structured exception handling: the kernel side.
//!
//! An exception — a CPU fault or a software `RaiseException` — is delivered
//! the way Windows NT does it: an `EXCEPTION_RECORD` and a `CONTEXT` are
//! written onto the thread's stack and the thread continues in the
//! user-mode [`dispatcher`] (NTDLL's `KiUserExceptionDispatcher`), which
//! walks the `FS:[0]` registration chain and calls each frame handler.
//! A handler takes over by unwinding ([`global_unwind`]) and asking the CPU
//! to resume elsewhere, or returns `ExceptionContinueExecution` to resume
//! the faulting context. The frame handlers themselves belong to the
//! language runtimes (see `win32::cxx` for MSVC C++ and `__try`).

use super::{
    cpu::{STACK_BASE, STACK_TOP, TIB_BASE},
    memory::{ApiOutcome, GuestMemory, Resume},
};

pub const EH_NONCONTINUABLE: u32 = 0x1;
pub const EH_UNWINDING: u32 = 0x2;
pub const EH_EXIT_UNWIND: u32 = 0x4;
pub const STATUS_NONCONTINUABLE_EXCEPTION: u32 = 0xC000_0025;
/// Exception codes the CPU raises for hardware faults.
pub const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const STATUS_INTEGER_DIVIDE_BY_ZERO: u32 = 0xC000_0094;

/// Frame handler results.
pub const CONTINUE_EXECUTION: u32 = 0;
pub const CONTINUE_SEARCH: u32 = 1;

pub const RECORD_SIZE: u32 = 80;
pub const CONTEXT_SIZE: u32 = 0xCC;
// CONTEXT (i386) register offsets.
pub const CTX_EDI: u32 = 0x9C;
pub const CTX_ESI: u32 = 0xA0;
pub const CTX_EBX: u32 = 0xA4;
pub const CTX_EDX: u32 = 0xA8;
pub const CTX_ECX: u32 = 0xAC;
pub const CTX_EAX: u32 = 0xB0;
pub const CTX_EBP: u32 = 0xB4;
pub const CTX_EIP: u32 = 0xB8;
pub const CTX_EFLAGS: u32 = 0xC0;
pub const CTX_ESP: u32 = 0xC4;

pub fn rd<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> u32 {
    memory.read_u32(va).unwrap_or(0)
}

pub fn rdi<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> i32 {
    rd(memory, va) as i32
}

pub fn wr<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, value: u32) {
    let _ = memory.write_u32(va, value);
}

pub fn trace() -> bool {
    std::env::var_os("OTW_TRACE_EH").is_some()
}

/// Whether an exception registration record lies on the running thread's
/// stack (bounds from its TIB).
fn on_stack<M: GuestMemory + ?Sized>(memory: &mut M, address: u32) -> bool {
    let tib = memory.current_tib();
    let (top, bottom) = if tib == TIB_BASE {
        (STACK_TOP, STACK_BASE)
    } else {
        (rd(memory, tib + 4), rd(memory, tib + 8))
    };
    (bottom..top.saturating_add(0x100)).contains(&address) && address & 3 == 0
}

/// Outcome of dispatching one exception.
/// Outcome of dispatching one exception.
enum Dispatch {
    /// A handler took over; execution resumes elsewhere.
    Handled,
    /// A handler asked to continue at the faulting context.
    ContinueExecution,
    Unhandled,
}
// ── Raising and dispatch ─────────────────────────────────────────────────

/// `RaiseException` from inside a system call: the thread continues in the
/// dispatcher with a record for `code` and a context that returns from the
/// call (popping `arg_count` stack arguments).
pub fn raise<M: GuestMemory + ?Sized>(
    memory: &mut M,
    code: u32,
    flags: u32,
    params: &[u32],
    arg_count: usize,
) -> ApiOutcome {
    let registers = memory.registers().unwrap_or_default();
    let return_address = rd(memory, registers.esp);
    let Some(dispatcher) = memory.exception_dispatcher() else {
        eprintln!("[eh] exception {code:#010x} with no dispatcher; terminating");
        return ApiOutcome {
            wide: None,
            value: 0,
            arg_count,
            exit_code: Some(code),
        };
    };
    if trace() {
        eprintln!("[eh] raise {code:#010x} flags={flags:#x} params={params:08x?} at {return_address:#010x}");
    }
    // Below the caller's frame: return address, &record, &context, record, context.
    let base = registers
        .esp
        .wrapping_sub(0x40 + 12 + RECORD_SIZE + CONTEXT_SIZE)
        & !0xF;
    let (record, context) = (base + 12, base + 12 + RECORD_SIZE);
    let _ = memory.write_bytes(base, &vec![0u8; (12 + RECORD_SIZE + CONTEXT_SIZE) as usize]);
    wr(memory, base, return_address);
    wr(memory, base + 4, record);
    wr(memory, base + 8, context);
    wr(memory, record, code);
    wr(memory, record + 4, flags);
    wr(memory, record + 12, return_address);
    let count = params.len().min(15);
    wr(memory, record + 16, count as u32);
    for (index, value) in params.iter().take(count).enumerate() {
        wr(memory, record + 20 + index as u32 * 4, *value);
    }
    wr(memory, context, 0x0001_0007); // CONTEXT_FULL
    for (offset, value) in [
        (CTX_EDI, registers.edi),
        (CTX_ESI, registers.esi),
        (CTX_EBX, registers.ebx),
        (CTX_EDX, registers.edx),
        (CTX_ECX, registers.ecx),
        (CTX_EAX, registers.eax),
        (CTX_EBP, registers.ebp),
        (CTX_EIP, return_address),
        (CTX_ESP, registers.esp + 4 + 4 * arg_count as u32),
    ] {
        wr(memory, context + offset, value);
    }
    memory.request_resume(Resume {
        eip: dispatcher,
        esp: base,
        ebp: registers.ebp,
        regs: None,
    });
    ApiOutcome::new(0, arg_count)
}

fn resume_from_context<M: GuestMemory + ?Sized>(memory: &mut M, context: u32) -> Resume {
    Resume {
        eip: rd(memory, context + CTX_EIP),
        esp: rd(memory, context + CTX_ESP),
        ebp: rd(memory, context + CTX_EBP),
        regs: Some([
            rd(memory, context + CTX_EAX),
            rd(memory, context + CTX_EBX),
            rd(memory, context + CTX_ECX),
            rd(memory, context + CTX_EDX),
            rd(memory, context + CTX_ESI),
            rd(memory, context + CTX_EDI),
        ]),
    }
}

/// The user-mode exception dispatcher, entered with a record and context
/// on the stack (by a CPU fault or [`raise`]). Either a handler takes over,
/// the context is resumed (possibly as changed by a filter), or the process
/// terminates with the exception code.
pub fn dispatcher<M: GuestMemory + ?Sized>(
    memory: &mut M,
    record: u32,
    context: u32,
) -> ApiOutcome {
    let code = rd(memory, record);
    let flags = rd(memory, record + 4);
    let mut outcome = dispatch(memory, record, context);
    // A handler that neither resumed nor ended the process (a callback
    // that could not run) would return to the faulting instruction.
    if matches!(outcome, Dispatch::Handled) && !memory.resume_pending() && !memory.stopping() {
        outcome = Dispatch::Unhandled;
    }
    let terminate = |code: u32| ApiOutcome {
        wide: None,
        value: 0,
        arg_count: 2,
        exit_code: Some(code),
    };
    match outcome {
        Dispatch::Handled => ApiOutcome::new(0, 2),
        Dispatch::ContinueExecution if flags & EH_NONCONTINUABLE != 0 => {
            eprintln!(
                "[eh] a handler continued a noncontinuable exception {code:#010x}; terminating"
            );
            terminate(STATUS_NONCONTINUABLE_EXCEPTION)
        }
        Dispatch::ContinueExecution => {
            let resume = resume_from_context(memory, context);
            if trace() {
                eprintln!("[eh] continue at {:#010x}", resume.eip);
            }
            memory.request_resume(resume);
            ApiOutcome::new(0, 2)
        }
        Dispatch::Unhandled => {
            eprintln!(
                "[eh] unhandled exception {code:#010x} at {:#010x}; this program has performed an illegal operation and will be shut down",
                rd(memory, record + 12)
            );
            terminate(code)
        }
    }
}

fn dispatch<M: GuestMemory + ?Sized>(memory: &mut M, record: u32, context: u32) -> Dispatch {
    let tib = memory.current_tib();
    let mut frame = rd(memory, tib);
    for _ in 0..4096 {
        if memory.resume_pending() || memory.stopping() {
            return Dispatch::Handled;
        }
        if frame == u32::MAX || !on_stack(memory, frame) {
            return Dispatch::Unhandled;
        }
        let handler = rd(memory, frame + 4);
        if trace() {
            eprintln!("[eh] frame {frame:#010x} handler {handler:#010x}");
        }
        let result = memory.call_guest(handler, &[record, frame, context, 0]);
        if memory.resume_pending() || memory.stopping() {
            return Dispatch::Handled;
        }
        match result {
            Some(CONTINUE_EXECUTION) => return Dispatch::ContinueExecution,
            Some(_) => {}
            None => return Dispatch::Handled,
        }
        frame = rd(memory, frame);
    }
    Dispatch::Unhandled
}

/// RtlUnwind: call every handler above `target` with EH_UNWINDING so they
/// run their cleanup, popping them from the chain.
pub fn global_unwind<M: GuestMemory + ?Sized>(
    memory: &mut M,
    target: u32,
    record: u32,
    context: u32,
) {
    let flags = rd(memory, record + 4);
    wr(memory, record + 4, flags | EH_UNWINDING);
    let tib = memory.current_tib();
    let mut frame = rd(memory, tib);
    for _ in 0..4096 {
        if frame == target || frame == u32::MAX || !on_stack(memory, frame) {
            break;
        }
        let next = rd(memory, frame);
        let handler = rd(memory, frame + 4);
        memory.call_guest(handler, &[record, frame, context, 0]);
        if memory.resume_pending() || memory.stopping() {
            return;
        }
        wr(memory, tib, next);
        frame = next;
    }
    wr(memory, record + 4, flags);
}
