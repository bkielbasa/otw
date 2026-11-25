//! The MSVC language runtime's exception handling (MSVCRT20): C++
//! `try`/`catch` (`__CxxFrameHandler`, MSVC 2.x–4.x `FuncInfo`, magic
//! `0x19930520`), `throw` (`_CxxThrowException`) and `__try`/`__except`/
//! `__finally` (`_except_handler3`). These are frame handlers called by the
//! kernel's exception dispatcher (`kernel::eh`).

use crate::{
    kernel::cpu::TIB_BASE,
    kernel::eh::{
        global_unwind, raise, rd, rdi, trace, wr, CONTINUE_EXECUTION, CONTINUE_SEARCH,
        EH_EXIT_UNWIND, EH_NONCONTINUABLE, EH_UNWINDING,
    },
    win32::{ApiContext, ApiOutcome, GuestMemory, Resume},
};

pub const CXX_EXCEPTION: u32 = 0xE06D_7363;
const CXX_MAGIC_MIN: u32 = 0x1993_0520;
const CXX_MAGIC_MAX: u32 = 0x1993_0522;

/// Name of the thrown C++ type, for diagnostics.
pub fn cxx_type_name<M: GuestMemory + ?Sized>(memory: &mut M, record: u32) -> Option<String> {
    if rd(memory, record) != CXX_EXCEPTION || rd(memory, record + 16) < 3 {
        return None;
    }
    let info = rd(memory, record + 28);
    let table = rd(memory, info + 12);
    if info == 0 || table == 0 || rd(memory, table) == 0 {
        return None;
    }
    let first = rd(memory, table + 4);
    let descriptor = rd(memory, first + 4);
    let name = crate::win32::read_cstr(memory, descriptor + 8);
    Some(String::from_utf8_lossy(&name).into_owned())
}

// ── C++ (__CxxFrameHandler) ──────────────────────────────────────────────

/// Unwind map walk: run destructors of a frame from its current state down
/// to `target`.
fn cxx_local_unwind<M: GuestMemory + ?Sized>(
    memory: &mut M,
    frame: u32,
    funcinfo: u32,
    target: i32,
) {
    let max_state = rdi(memory, funcinfo + 4);
    let table = rd(memory, funcinfo + 8);
    let mut state = rdi(memory, frame + 8);
    for _ in 0..1024 {
        if state == target || state < 0 || state >= max_state {
            break;
        }
        let entry = table + state as u32 * 8;
        let prev = rdi(memory, entry);
        let handler = rd(memory, entry + 4);
        // Leave the state updated first, so a destructor that throws does
        // not run again.
        wr(memory, frame + 8, prev as u32);
        if handler != 0 {
            if trace() {
                eprintln!("[eh] unwind state {state} -> {prev} via {handler:#010x}");
            }
            memory.call_guest_ex(handler, &[], Some(frame + 12), None);
            if memory.resume_pending() || memory.stopping() {
                return;
            }
        }
        state = prev;
    }
    wr(memory, frame + 8, target as u32);
}

fn type_name<M: GuestMemory + ?Sized>(memory: &mut M, descriptor: u32) -> Vec<u8> {
    crate::win32::read_cstr(memory, descriptor + 8)
}

/// Find the CatchableType of the thrown object that matches a catch block.
/// `Some(None)` is a catch-all (`catch (...)`).
fn match_catch<M: GuestMemory + ?Sized>(
    memory: &mut M,
    catch_type: u32,
    catch_flags: u32,
    info: u32,
) -> Option<Option<u32>> {
    if catch_type == 0 || rd(memory, catch_type + 8) & 0xFF == 0 {
        return Some(None);
    }
    if info == 0 {
        return None;
    }
    let info_flags = rd(memory, info);
    let table = rd(memory, info + 12);
    let count = rd(memory, table).min(64);
    let wanted = type_name(memory, catch_type);
    for index in 0..count {
        let catchable = rd(memory, table + 4 + index * 4);
        let descriptor = rd(memory, catchable + 4);
        if descriptor != catch_type && type_name(memory, descriptor) != wanted {
            continue;
        }
        // A const/volatile thrown object needs a const/volatile catch.
        if info_flags & 1 != 0 && catch_flags & 1 == 0 {
            continue;
        }
        if info_flags & 2 != 0 && catch_flags & 2 == 0 {
            continue;
        }
        return Some(Some(catchable));
    }
    None
}

/// Adjust the thrown object pointer to the base class a CatchableType names.
fn adjust_this<M: GuestMemory + ?Sized>(memory: &mut M, object: u32, catchable: u32) -> u32 {
    let mdisp = rdi(memory, catchable + 8);
    let pdisp = rdi(memory, catchable + 12);
    let vdisp = rdi(memory, catchable + 16);
    let mut ptr = object as i32;
    if pdisp >= 0 {
        ptr += pdisp;
        let vbtable = rd(memory, ptr as u32);
        ptr += rdi(memory, (vbtable as i32 + vdisp) as u32);
    }
    (ptr + mdisp) as u32
}

/// `__CxxFrameHandler(record, frame, context, dispatcher)` with the
/// function's FuncInfo in EAX.
pub fn cxx_frame_handler<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    record: u32,
    frame: u32,
    context: u32,
    funcinfo: u32,
) -> u32 {
    let magic = rd(memory, funcinfo);
    if !(CXX_MAGIC_MIN..=CXX_MAGIC_MAX).contains(&magic) {
        return CONTINUE_SEARCH;
    }
    let flags = rd(memory, record + 4);
    if flags & (EH_UNWINDING | EH_EXIT_UNWIND) != 0 {
        if rdi(memory, funcinfo + 4) > 0 {
            cxx_local_unwind(memory, frame, funcinfo, -1);
        }
        return CONTINUE_SEARCH;
    }
    let try_count = rd(memory, funcinfo + 12).min(256);
    if try_count == 0 {
        return CONTINUE_SEARCH;
    }
    let code = rd(memory, record);
    let is_cxx = code == CXX_EXCEPTION
        && rd(memory, record + 16) >= 3
        && (CXX_MAGIC_MIN..=CXX_MAGIC_MAX).contains(&rd(memory, record + 20));
    let (object, info) = if is_cxx {
        (rd(memory, record + 24), rd(memory, record + 28))
    } else {
        (0, 0)
    };
    let state = rdi(memory, frame + 8);
    let tries = rd(memory, funcinfo + 16);
    for index in 0..try_count {
        let block = tries + index * 20;
        let (start, end) = (rdi(memory, block), rdi(memory, block + 4));
        if state < start || state > end {
            continue;
        }
        let catch_count = rd(memory, block + 12).min(64);
        let catches = rd(memory, block + 16);
        for c in 0..catch_count {
            let entry = catches + c * 16;
            let (catch_flags, catch_type, catch_offset, handler) = (
                rd(memory, entry),
                rd(memory, entry + 4),
                rdi(memory, entry + 8),
                rd(memory, entry + 12),
            );
            let Some(catchable) = match_catch(memory, catch_type, catch_flags, info) else {
                continue;
            };
            if !is_cxx && catchable.is_some() {
                continue;
            }
            let frame_ebp = frame + 12;
            if trace() {
                let name = if catch_type != 0 {
                    String::from_utf8_lossy(&type_name(memory, catch_type)).into_owned()
                } else {
                    "...".into()
                };
                eprintln!("[eh] frame {frame:#010x}: catch ({name}) at {handler:#010x}, state {state} in try {start}..={end}");
            }
            // Copy the exception object into the catch parameter.
            if let Some(catchable) = catchable {
                if object != 0 && catch_offset != 0 {
                    let dest = (frame_ebp as i32 + catch_offset) as u32;
                    let source = adjust_this(memory, object, catchable);
                    let class_flags = rd(memory, catchable);
                    let size = rd(memory, catchable + 20) as usize;
                    let copy_ctor = rd(memory, catchable + 24);
                    if catch_flags & 8 != 0 {
                        wr(memory, dest, source);
                    } else if class_flags & 1 != 0 || copy_ctor == 0 {
                        if let Some(bytes) = memory.read_bytes(source, size.min(4096)) {
                            let _ = memory.write_bytes(dest, &bytes);
                        }
                    } else {
                        let ctor_args: &[u32] = if class_flags & 4 != 0 {
                            &[source, 1]
                        } else {
                            &[source]
                        };
                        let ctor_args = ctor_args.to_vec();
                        memory.call_guest_ex(copy_ctor, &ctor_args, None, Some(dest));
                    }
                }
            }
            global_unwind(memory, frame, record, context);
            if memory.resume_pending() || memory.stopping() {
                return CONTINUE_SEARCH;
            }
            cxx_local_unwind(memory, frame, funcinfo, start);
            if memory.resume_pending() || memory.stopping() {
                return CONTINUE_SEARCH;
            }
            wr(memory, frame + 8, (end + 1) as u32);
            // Run the catch block; it returns the continuation address.
            ctx.shared.borrow_mut().eh_current.push((object, info));
            let continuation = memory.call_guest_ex(handler, &[], Some(frame_ebp), None);
            ctx.shared.borrow_mut().eh_current.pop();
            if memory.resume_pending() || memory.stopping() {
                // The catch block threw (or rethrew) and someone else handled it.
                return CONTINUE_SEARCH;
            }
            let Some(continuation) = continuation else {
                return CONTINUE_SEARCH;
            };
            // The exception object dies when its catch block exits.
            if object != 0 && info != 0 {
                let destructor = rd(memory, info + 4);
                if destructor != 0 {
                    memory.call_guest_ex(destructor, &[], None, Some(object));
                }
            }
            wr(memory, TIB_BASE, frame);
            let resume = Resume {
                eip: continuation,
                esp: rd(memory, frame - 4),
                ebp: frame_ebp,
                regs: None,
            };
            if trace() {
                eprintln!(
                    "[eh] resume at {:#010x} esp={:#010x} ebp={:#010x}",
                    resume.eip, resume.esp, resume.ebp
                );
            }
            memory.request_resume(resume);
            return CONTINUE_SEARCH;
        }
    }
    CONTINUE_SEARCH
}

/// `_CxxThrowException(object, throw_info)`; a null `throw_info` rethrows
/// the exception currently being handled.
pub fn throw<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    object: u32,
    info: u32,
) -> ApiOutcome {
    let (object, info) = if info == 0 {
        match ctx.shared.borrow().eh_current.last().copied() {
            Some(current) => current,
            None => {
                eprintln!("[eh] rethrow with no active exception; terminating");
                return ApiOutcome {
                    wide: None,
                    value: 0,
                    arg_count: 2,
                    exit_code: Some(3),
                };
            }
        }
    } else {
        (object, info)
    };
    let _ = ctx;
    raise(
        memory,
        CXX_EXCEPTION,
        EH_NONCONTINUABLE,
        &[CXX_MAGIC_MIN, object, info],
        2,
    )
}

// ── SEH (_except_handler3) ───────────────────────────────────────────────

/// Run `__finally` blocks from the current try level down to `stop`.
fn seh_local_unwind<M: GuestMemory + ?Sized>(memory: &mut M, frame: u32, stop: i32) {
    let scope_table = rd(memory, frame + 8);
    let ebp = frame + 16;
    let mut level = rdi(memory, frame + 12);
    for _ in 0..1024 {
        if level == -1 || level == stop || level < 0 {
            break;
        }
        let entry = scope_table + level as u32 * 12;
        let prev = rdi(memory, entry);
        let filter = rd(memory, entry + 4);
        let handler = rd(memory, entry + 8);
        wr(memory, frame + 12, prev as u32);
        if filter == 0 && handler != 0 {
            memory.call_guest_ex(handler, &[], Some(ebp), None);
            if memory.resume_pending() || memory.stopping() {
                return;
            }
        }
        level = prev;
    }
}

/// `_except_handler3(record, frame, context, dispatcher)`.
pub fn except_handler3<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    record: u32,
    frame: u32,
    context: u32,
) -> u32 {
    let flags = rd(memory, record + 4);
    if flags & (EH_UNWINDING | EH_EXIT_UNWIND) != 0 {
        seh_local_unwind(memory, frame, -1);
        return CONTINUE_SEARCH;
    }
    let scope_table = rd(memory, frame + 8);
    let ebp = frame + 16;
    // GetExceptionInformation() reads EXCEPTION_POINTERS from [ebp-0x14].
    let pointers = ctx.shared.borrow_mut().heap.alloc(8).unwrap_or(0);
    wr(memory, pointers, record);
    wr(memory, pointers + 4, context);
    wr(memory, frame - 4, pointers);
    let mut level = rdi(memory, frame + 12);
    let mut result = CONTINUE_SEARCH;
    for _ in 0..1024 {
        if level < 0 {
            break;
        }
        let entry = scope_table + level as u32 * 12;
        let prev = rdi(memory, entry);
        let filter = rd(memory, entry + 4);
        let handler = rd(memory, entry + 8);
        if filter != 0 {
            let verdict = memory
                .call_guest_ex(filter, &[], Some(ebp), None)
                .unwrap_or(0) as i32;
            if memory.resume_pending() || memory.stopping() {
                break;
            }
            if trace() {
                eprintln!("[eh] __except filter {filter:#010x} at level {level} -> {verdict}");
            }
            if verdict < 0 {
                result = CONTINUE_EXECUTION;
                break;
            }
            if verdict > 0 {
                global_unwind(memory, frame, record, context);
                if memory.resume_pending() || memory.stopping() {
                    break;
                }
                seh_local_unwind(memory, frame, level);
                wr(memory, frame + 12, prev as u32);
                wr(memory, TIB_BASE, frame);
                let esp = rd(memory, frame - 8);
                memory.request_resume(Resume {
                    eip: handler,
                    esp,
                    ebp,
                    regs: None,
                });
                break;
            }
        }
        level = prev;
    }
    ctx.shared.borrow_mut().heap.free(pointers);
    result
}

/// `_XcptFilter(code, pointers)`: the CRT's last-chance filter. Report the
/// exception and let the startup code's handler terminate the process.
pub fn xcpt_filter<M: GuestMemory + ?Sized>(memory: &mut M, code: u32, pointers: u32) -> u32 {
    let record = rd(memory, pointers);
    match cxx_type_name(memory, record) {
        Some(name) => eprintln!("[eh] unhandled C++ exception of type {name}; terminating"),
        None => eprintln!("[eh] unhandled exception {code:#010x}; terminating"),
    }
    1 // EXCEPTION_EXECUTE_HANDLER
}
