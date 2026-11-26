//! Programs run natively: their code on the CPU itself, in user mode, in an
//! address space of their own; their imports are thunks that make a system
//! call (`int 0x80`) into OTW's Win32 subsystem, the same code as on Linux
//! (`otw_vm::win32`), which reaches the program's memory and calls back
//! into it through `NativeMemory`.
//!
//! Exceptions: CPU faults and `RaiseException` go to the program's own
//! handlers (structured exception handling, through the subsystem's
//! dispatcher, as on Linux). Threads take turns on one CPU, the way the
//! subsystem expects: a thread that waits lets the others run until they
//! wait or end (`run_thread`), and is run again later from the same call.

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use otw_vm::kernel::context64;
use otw_vm::kernel::eh;
use otw_vm::kernel::iat::{IatEntry, THUNK_BASE, THUNK_STRIDE};
use otw_vm::kernel::image::{Image, EXCEPTION_DISPATCHER, KERNEL_DLL, THREAD_RETURN};
use otw_vm::kernel::layout::{SENTINEL_BASE, STACK_BASE, STACK_SIZE, STACK_TOP, TIB_BASE};
use otw_vm::kernel::memory::{self, FullRegisters, GuestMemory, Registers, Resume, ThreadRun};
use otw_vm::kernel::pe::ImportName;
use otw_vm::kernel::syscall::{widen, Args, SystemCalls};
use otw_vm::vm::Vm;
use otw_vm::win32::SharedState;

use crate::arch::{self, write_msr};
use crate::paging::AddressSpace;
use crate::user::{self, UserRegs};
use crate::{apic, log};

const PAGE: u64 = 4096;
/// A 64-bit program's first thread gets nearly 2 MiB of stack.
const X64_STACK_LOW: u64 = 0x0001_0000;
const TEB64_SIZE: u64 = 0x3000;
const TIB_SIZE: u64 = 0x1000;
/// Where callbacks return: `int 0x81`.
const CALLBACK_RETURN: u64 = SENTINEL_BASE as u64;
/// Callbacks within callbacks, at most.
const MAX_DEPTH: usize = 48;

/// How a program run ended.
pub enum Ended {
    Exited(u32),
    Failed(String),
}

/// Why a run of program code stopped.
enum Stop {
    /// The process ended (or failed).
    Ended,
    /// A callback returned (`int 0x81`), or was left to unwind to an outer
    /// level (exception handling).
    Returned(u64),
    /// The thread waits (its call is made again when it runs next).
    Suspended,
    /// The thread ended.
    ThreadExited(u32),
}

/// What a thread asked for in its last system call.
#[derive(Clone, Copy)]
enum Signal {
    Suspend,
    Exit(u32),
}

/// A thread that is not running.
struct Thread {
    id: u32,
    regs: Box<UserRegs>,
    tib: u64,
    levels: Vec<u64>,
}

/// The program's memory, threads and state.
struct Runner {
    space: AddressSpace,
    x64: bool,
    entries: Vec<IatEntry>,
    syscalls: Rc<dyn SystemCalls>,
    dispatcher: u64,
    thread_return: u64,
    /// Address ranges the program may use; their pages are made when first
    /// touched.
    reserved: RefCell<Vec<(u64, u64)>>,
    ended: RefCell<Option<Ended>>,
    api_calls: Cell<u64>,
    /// Exception handling's request to continue elsewhere.
    resume: RefCell<Option<Resume>>,
    resume_full: RefCell<Option<FullRegisters>>,
    /// The stack pointers below which each running callback's frames lie
    /// (outermost first).
    levels: RefCell<Vec<u64>>,
    /// The running threads (id, TIB), outermost first: a thread runs the
    /// others from inside its waits.
    running: RefCell<Vec<(u32, u64)>>,
    waiting: RefCell<Vec<Thread>>,
    next_thread: Cell<u32>,
    signal: Cell<Option<Signal>>,
    /// Each module's TLS template, copied for every thread.
    tls: Vec<Vec<u8>>,
}

/// What the subsystem sees of the program (`GuestMemory`), while it runs a
/// system call made with `regs`.
struct NativeMemory<'a> {
    runner: &'a Runner,
    regs: *mut UserRegs,
}

impl Runner {
    fn reserve(&self, start: u64, size: u64) {
        let start = start & !(PAGE - 1);
        let end = (start + size + PAGE - 1) & !(PAGE - 1);
        self.reserved.borrow_mut().push((start, end));
    }

    fn reserved_at(&self, address: u64) -> bool {
        self.reserved.borrow().iter().any(|&(s, e)| address >= s && address < e)
    }

    /// Make the page at `address` if the program may use it.
    fn fault_in(&self, address: u64) -> bool {
        self.reserved_at(address) && !self.space.is_mapped(address) && self.space.map(address & !(PAGE - 1), PAGE)
    }

    /// Make every page of a range (before the kernel writes there).
    fn touch(&self, address: u64, size: usize) -> bool {
        let mut page = address & !(PAGE - 1);
        while page < address + size as u64 {
            if !self.space.is_mapped(page) && !self.fault_in(page) {
                return false;
            }
            page += PAGE;
        }
        true
    }

    fn read(&self, address: u64, len: usize) -> Option<Vec<u8>> {
        let mut bytes = vec![0u8; len];
        (self.touch(address, len) && self.space.read(address, &mut bytes)).then_some(bytes)
    }

    fn write(&self, address: u64, bytes: &[u8]) -> bool {
        self.touch(address, bytes.len()) && self.space.write(address, bytes)
    }

    fn read_u64(&self, address: u64) -> Option<u64> {
        self.read(address, 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn read_u32(&self, address: u64) -> Option<u32> {
        self.read(address, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    /// A free range of `size` bytes (from `preferred` if it is free).
    fn find_free(&self, preferred: u64, size: u64) -> Option<u64> {
        let size = (size + PAGE - 1) & !(PAGE - 1);
        let free = |start: u64| !self.reserved.borrow().iter().any(|&(s, e)| start < e && start + size > s);
        if preferred != 0 && free(preferred & !(PAGE - 1)) {
            return Some(preferred & !(PAGE - 1));
        }
        let mut start = memory::SEPARATE_BASE as u64;
        while start + size < 0x7000_0000 {
            if free(start) {
                return Some(start);
            }
            start += 0x1_0000;
        }
        None
    }

    fn end(&self, how: Ended) {
        let mut ended = self.ended.borrow_mut();
        if ended.is_none() {
            *ended = Some(how);
        }
    }

    fn has_ended(&self) -> bool {
        self.ended.borrow().is_some()
    }

    fn fail(&self, why: String) -> Stop {
        self.end(Ended::Failed(why));
        Stop::Ended
    }

    /// Run program code from `regs` until the process ends, a callback
    /// returns, or the thread waits or ends.
    fn execute(&self, regs: &mut UserRegs) -> Stop {
        loop {
            if self.has_ended() {
                return Stop::Ended;
            }
            user::run(regs);
            let stop = match regs.vector as u8 {
                arch::SYSTEM_CALL_VECTOR => self.system_call(regs),
                arch::CALLBACK_RETURN_VECTOR => Some(Stop::Returned(regs.rax)),
                14 if regs.error & 1 == 0 && self.fault_in(regs.fault_address) => None,
                vector => self.raise_fault(regs, vector),
            };
            if let Some(stop) = stop {
                return stop;
            }
        }
    }

    /// A CPU fault: give it to the program's exception handlers.
    fn raise_fault(&self, regs: &mut UserRegs, vector: u8) -> Option<Stop> {
        let (code, params): (u32, Vec<u64>) = match vector {
            0 => (eh::STATUS_INTEGER_DIVIDE_BY_ZERO, Vec::new()),
            3 => (0x8000_0003, Vec::new()),
            4 => (0xC000_0095, Vec::new()),
            6 => (0xC000_001D, Vec::new()),
            13 => (eh::STATUS_ACCESS_VIOLATION, vec![0, u64::MAX]),
            14 => (eh::STATUS_ACCESS_VIOLATION, vec![(regs.error >> 1) & 1, regs.fault_address]),
            16 | 19 => (0xC000_0090, Vec::new()),
            _ => (eh::STATUS_ACCESS_VIOLATION, vec![0, 0]),
        };
        // A breakpoint reports the int3 itself.
        let at = if vector == 3 { regs.rip - 1 } else { regs.rip };
        log!("OTW kernel: {} at {at:#x} ({:#x})", exception_name(vector), regs.fault_address);
        if self.dispatcher == 0 {
            return Some(self.fail(format!("{} at {at:#x}", exception_name(vector))));
        }
        if self.x64 {
            let mut full = full_registers(regs);
            full.rip = at;
            let rsp = full.gpr[4];
            let base = rsp.wrapping_sub(0x200 + (context64::RECORD_SIZE + context64::CONTEXT_SIZE) as u64) & !0xF;
            let record = base + 0x30;
            let context = (record + context64::RECORD_SIZE as u64 + 15) & !15;
            let written = self.write(record, &context64::record(code, 0, at, &params))
                && self.write(context, &context64::encode(&full))
                && self.write(base, &at.to_le_bytes());
            if !written {
                return Some(self.fail(format!("{} at {at:#x} (cannot raise it)", exception_name(vector))));
            }
            regs.rsp = base;
            regs.rcx = record;
            regs.rdx = context;
        } else {
            let esp = regs.rsp as u32;
            let base = esp.wrapping_sub(0x40 + 12 + eh::RECORD_SIZE + eh::CONTEXT_SIZE) & !0xF;
            let (record, context) = (base + 12, base + 12 + eh::RECORD_SIZE);
            let mut frame = vec![0u8; (12 + eh::RECORD_SIZE + eh::CONTEXT_SIZE) as usize];
            let mut put = |offset: u32, value: u32| frame[offset as usize..offset as usize + 4].copy_from_slice(&value.to_le_bytes());
            put(0, at as u32);
            put(4, record);
            put(8, context);
            put(12, code);
            put(12 + 12, at as u32);
            put(12 + 16, params.len() as u32);
            for (i, p) in params.iter().enumerate() {
                put(12 + 20 + i as u32 * 4, *p as u32);
            }
            let c = 12 + eh::RECORD_SIZE;
            put(c, 0x0001_0007); // CONTEXT_FULL
            for (offset, value) in [
                (eh::CTX_EDI, regs.rdi),
                (eh::CTX_ESI, regs.rsi),
                (eh::CTX_EBX, regs.rbx),
                (eh::CTX_EDX, regs.rdx),
                (eh::CTX_ECX, regs.rcx),
                (eh::CTX_EAX, regs.rax),
                (eh::CTX_EBP, regs.rbp),
                (eh::CTX_EFLAGS, regs.rflags),
                (eh::CTX_ESP, regs.rsp),
                (eh::CTX_EIP, at),
            ] {
                put(c + offset, value as u32);
            }
            put(c + 0xBC, arch::USER_CODE32 as u32);
            put(c + 0xC8, arch::USER_DATA as u32);
            if !self.write(base as u64, &frame) {
                return Some(self.fail(format!("{} at {at:#x} (cannot raise it)", exception_name(vector))));
            }
            regs.rsp = base as u64;
        }
        regs.rip = self.dispatcher;
        None
    }

    /// The program called an import: run it in the subsystem.
    fn system_call(&self, regs: &mut UserRegs) -> Option<Stop> {
        let thunk = regs.rip.wrapping_sub(2);
        let index = thunk.wrapping_sub(THUNK_BASE as u64) / THUNK_STRIDE as u64;
        let Some(entry) = self
            .entries
            .get(index as usize)
            .filter(|e| e.thunk_va as u64 == thunk)
            .or_else(|| self.entries.iter().find(|e| e.thunk_va as u64 == thunk))
        else {
            return Some(self.fail(format!("a system call from {:#x}, not an import", regs.rip)));
        };
        self.api_calls.set(self.api_calls.get() + 1);
        let x64 = self.x64;
        let esp = regs.rsp;
        let return_address = if x64 { self.read_u64(esp) } else { self.read_u32(esp).map(u64::from) };
        let Some(return_address) = return_address else {
            return Some(self.fail(format!("no return address for {}", entry.dll)));
        };
        let kernel_entry = entry.dll == KERNEL_DLL;
        if kernel_entry && entry.name.as_str() == Some(THREAD_RETURN) {
            // A thread returned from its start routine.
            if self.running.borrow().len() > 1 {
                return Some(Stop::ThreadExited(regs.rax as u32));
            }
            self.end(Ended::Exited(regs.rax as u32));
            return Some(Stop::Ended);
        }
        let arg_count = if kernel_entry {
            Some(2)
        } else if x64 {
            self.syscalls.arg_count64(&entry.dll, &entry.name)
        } else {
            self.syscalls.arg_count(&entry.dll, &entry.name)
        };
        let Some(arg_count) = arg_count else {
            return Some(self.fail(format!("{} is not implemented", self.syscalls.describe(&entry.dll, &entry.name))));
        };
        // x64: the first four in RCX, RDX, R8, R9, the rest above the return
        // address and the 32-byte home area. 32-bit: all on the stack.
        let mut args = Vec::with_capacity(arg_count);
        for i in 0..arg_count {
            let value = if x64 && i < 4 {
                Some([regs.rcx, regs.rdx, regs.r8, regs.r9][i])
            } else if x64 {
                self.read_u64(esp + 8 + i as u64 * 8)
            } else {
                self.read_u32(esp + 4 + i as u64 * 4).map(u64::from)
            };
            match value {
                Some(v) => args.push(v),
                None => return Some(self.fail(format!("cannot read argument {i} of {}", entry.dll))),
            }
        }
        let outcome = {
            let mut memory = NativeMemory { runner: self, regs };
            if kernel_entry && x64 {
                // The 64-bit dispatcher is the subsystem's (it unwinds
                // through the modules' function tables).
                let name = ImportName::ByName(String::from("OTW_DispatchException64"));
                self.syscalls.call(&mut memory, "KERNEL32.dll", &name, &Args::new(args, true), return_address as u32)
            } else if kernel_entry {
                Some(eh::dispatcher(&mut memory, args[0] as u32, args[1] as u32))
            } else {
                self.syscalls.call(&mut memory, &entry.dll, &entry.name, &Args::new(args, x64), return_address as u32)
            }
        };
        if self.has_ended() {
            return Some(Stop::Ended);
        }
        // The thread waits, or ends.
        match self.signal.take() {
            Some(Signal::Suspend) => {
                // Make the same call again when it next runs.
                regs.rip = thunk;
                return Some(Stop::Suspended);
            }
            Some(Signal::Exit(code)) => return Some(Stop::ThreadExited(code)),
            None => {}
        }
        // Exception handling continues elsewhere: here, or in an outer
        // level (this callback ends).
        let pending = *self.resume.borrow();
        if let Some(resume) = pending {
            let here = self.levels.borrow().last().is_none_or(|&boundary| (resume.esp as u64) < boundary);
            if !here {
                return Some(Stop::Returned(0));
            }
            self.resume.borrow_mut().take();
            let full = self.resume_full.borrow_mut().take();
            match full {
                Some(full) => set_full_registers(regs, &full, true),
                None => {
                    if let Some([eax, ebx, ecx, edx, esi, edi]) = resume.regs {
                        regs.rax = eax as u64;
                        regs.rbx = ebx as u64;
                        regs.rcx = ecx as u64;
                        regs.rdx = edx as u64;
                        regs.rsi = esi as u64;
                        regs.rdi = edi as u64;
                    }
                    regs.rsp = resume.esp as u64;
                    regs.rbp = resume.ebp as u64;
                    regs.rip = resume.eip as u64;
                }
            }
            return None;
        }
        let Some(outcome) = outcome else {
            return Some(self.fail(format!("{} failed", self.syscalls.describe(&entry.dll, &entry.name))));
        };
        if let Some(code) = outcome.exit_code {
            self.end(Ended::Exited(code));
            return Some(Stop::Ended);
        }
        if x64 {
            regs.rax = outcome.wide.unwrap_or_else(|| widen(outcome.value));
            regs.rsp = esp + 8;
        } else {
            regs.rax = outcome.value as u64;
            regs.rsp = esp + 4 + outcome.arg_count as u64 * 4;
        }
        regs.rip = return_address;
        None
    }

    /// Call program code at `function` with `args` (from a system call
    /// made with `caller`), returning what it returns.
    fn call(&self, caller: &UserRegs, function: u64, args: &[u64], ebp: Option<u64>, ecx: Option<u64>) -> Option<u64> {
        if self.levels.borrow().len() >= MAX_DEPTH {
            return None;
        }
        // The level is the caller's stack pointer (its return address, for
        // unwinding through the callback); the callback runs below it.
        let boundary = caller.rsp;
        let mut sp = (caller.rsp - 0x200) & !0xF;
        let mut regs = Box::new(UserRegs::new(self.x64, function, 0));
        regs.cs = caller.cs;
        regs.fx = caller.fx;
        if self.x64 {
            let stack_args = args.len().saturating_sub(4);
            // Home area, stack arguments, return address; RSP = 8 (mod 16)
            // at entry.
            sp -= ((32 + stack_args * 8 + 15) & !15) as u64;
            for (i, &value) in args.iter().enumerate().skip(4) {
                self.write(sp + 32 + (i as u64 - 4) * 8, &value.to_le_bytes());
            }
            sp -= 8;
            self.write(sp, &CALLBACK_RETURN.to_le_bytes());
            let get = |i: usize| args.get(i).copied().unwrap_or(0);
            regs.rcx = get(0);
            regs.rdx = get(1);
            regs.r8 = get(2);
            regs.r9 = get(3);
        } else {
            for &value in args.iter().rev() {
                sp -= 4;
                self.write(sp, &(value as u32).to_le_bytes());
            }
            sp -= 4;
            self.write(sp, &(CALLBACK_RETURN as u32).to_le_bytes());
            regs.rbp = ebp.unwrap_or(caller.rbp);
            regs.rcx = ecx.unwrap_or(0);
        }
        regs.rsp = sp;
        self.levels.borrow_mut().push(boundary);
        let stop = self.execute(&mut regs);
        self.levels.borrow_mut().pop();
        match stop {
            Stop::Returned(value) => Some(value),
            _ => None,
        }
    }

    /// Give a thread its copies of the TLS templates.
    fn install_tls(&self, teb: u64) {
        if self.tls.is_empty() {
            return;
        }
        let width = if self.x64 { 8 } else { 4 };
        let array = (self.tls.len() as u64 * width + 15) & !15;
        let total = array + self.tls.iter().map(|t| (t.len() as u64 + 15) & !15).sum::<u64>();
        let Some(block) = self.find_free(0x1000_0000, total.max(1)) else { return };
        self.reserve(block, total.max(1));
        let mut cursor = block + array;
        for (index, template) in self.tls.iter().enumerate() {
            self.write(cursor, template);
            self.write(block + index as u64 * width, &cursor.to_le_bytes()[..width as usize]);
            cursor += (template.len() as u64 + 15) & !15;
        }
        let slot = if self.x64 { teb + 0x58 } else { teb + 0x2C };
        self.write(slot, &block.to_le_bytes()[..width as usize]);
    }

    fn init_teb64(&self, teb: u64, top: u64, bottom: u64, thread: u32) {
        let peb = TIB_BASE as u64 + 0x2000;
        for (offset, value) in [(0x00u64, u64::MAX), (0x08, top), (0x10, bottom), (0x30, teb), (0x40, 1), (0x48, thread as u64), (0x60, peb)] {
            self.write(teb + offset, &value.to_le_bytes());
        }
    }

    fn set_thread_block(&self, tib: u64) {
        if self.x64 {
            write_msr(0xC000_0101, tib); // GS base
        } else {
            write_msr(0xC000_0100, tib); // FS base
        }
    }

    fn create_thread(&self, entry: u64, param: u64, stack_size: u64) -> Option<u32> {
        let stack_size = (stack_size.clamp(0x1_0000, 0x100_0000) + PAGE - 1) & !(PAGE - 1);
        let stack = self.find_free(0x1000_0000, stack_size)?;
        self.reserve(stack, stack_size);
        let tib_size = if self.x64 { TEB64_SIZE } else { PAGE };
        let tib = self.find_free(0x1000_0000, tib_size)?;
        self.reserve(tib, tib_size);
        let top = stack + stack_size;
        let id = self.next_thread.get();
        self.next_thread.set(id + 1);
        let mut regs = Box::new(UserRegs::new(self.x64, entry, 0));
        if self.x64 {
            self.init_teb64(tib, top, stack, id);
            let rsp = ((top - 0x40) & !0xF) - 8;
            self.write(rsp, &self.thread_return.to_le_bytes());
            regs.rsp = rsp;
            regs.rcx = param;
        } else {
            // A copy of the first thread's TIB with its own exception
            // chain, stack bounds and self pointer.
            let mut page = self.read(TIB_BASE as u64, PAGE as usize)?;
            for (offset, value) in [(0usize, u32::MAX), (4, top as u32), (8, stack as u32), (0x18, tib as u32)] {
                page[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            }
            self.write(tib, &page);
            let esp = top - 0x10;
            self.write(esp, &(self.thread_return as u32).to_le_bytes());
            self.write(esp + 4, &(param as u32).to_le_bytes());
            regs.rsp = esp;
        }
        self.install_tls(tib);
        self.waiting.borrow_mut().push(Thread { id, regs, tib, levels: Vec::new() });
        Some(id)
    }

    /// Run a waiting thread until it waits again or ends.
    fn run_thread(&self, id: u32) -> Option<ThreadRun> {
        if self.has_ended() || self.resume.borrow().is_some() {
            return None;
        }
        let mut thread = {
            let mut waiting = self.waiting.borrow_mut();
            let index = waiting.iter().position(|t| t.id == id)?;
            waiting.remove(index)
        };
        let outer_levels = core::mem::replace(&mut *self.levels.borrow_mut(), core::mem::take(&mut thread.levels));
        let outer_tib = self.running.borrow().last().map(|&(_, tib)| tib).unwrap_or(TIB_BASE as u64);
        self.running.borrow_mut().push((id, thread.tib));
        self.set_thread_block(thread.tib);
        let stop = self.execute(&mut thread.regs);
        self.set_thread_block(outer_tib);
        self.running.borrow_mut().pop();
        let levels = core::mem::replace(&mut *self.levels.borrow_mut(), outer_levels);
        match stop {
            Stop::Suspended => {
                thread.levels = levels;
                self.waiting.borrow_mut().push(thread);
                Some(ThreadRun::Waiting)
            }
            Stop::ThreadExited(code) => Some(ThreadRun::Exited(code)),
            Stop::Ended | Stop::Returned(_) => Some(ThreadRun::Exited(0)),
        }
    }
}

fn full_registers(regs: &UserRegs) -> FullRegisters {
    let mut xmm = [[0u8; 16]; 16];
    for (i, x) in xmm.iter_mut().enumerate() {
        x.copy_from_slice(&regs.fx[160 + i * 16..176 + i * 16]);
    }
    FullRegisters {
        gpr: [
            regs.rax, regs.rcx, regs.rdx, regs.rbx, regs.rsp, regs.rbp, regs.rsi, regs.rdi, regs.r8, regs.r9, regs.r10,
            regs.r11, regs.r12, regs.r13, regs.r14, regs.r15,
        ],
        rip: regs.rip,
        eflags: regs.rflags,
        mxcsr: u32::from_le_bytes(regs.fx[24..28].try_into().unwrap()),
        xmm,
    }
}

fn set_full_registers(regs: &mut UserRegs, full: &FullRegisters, with_rip: bool) {
    let g = &full.gpr;
    regs.rax = g[0];
    regs.rcx = g[1];
    regs.rdx = g[2];
    regs.rbx = g[3];
    regs.rsp = g[4];
    regs.rbp = g[5];
    regs.rsi = g[6];
    regs.rdi = g[7];
    regs.r8 = g[8];
    regs.r9 = g[9];
    regs.r10 = g[10];
    regs.r11 = g[11];
    regs.r12 = g[12];
    regs.r13 = g[13];
    regs.r14 = g[14];
    regs.r15 = g[15];
    if with_rip {
        regs.rip = full.rip;
        // Arithmetic flags from the context; interrupts stay on.
        regs.rflags = (full.eflags & 0xCD5) | 0x202;
    }
    regs.fx[24..28].copy_from_slice(&full.mxcsr.to_le_bytes());
    for (i, x) in full.xmm.iter().enumerate() {
        regs.fx[160 + i * 16..176 + i * 16].copy_from_slice(x);
    }
}

impl NativeMemory<'_> {
    fn regs(&self) -> &UserRegs {
        unsafe { &*self.regs }
    }
}

impl GuestMemory for NativeMemory<'_> {
    fn read_bytes(&mut self, va: u32, len: usize) -> Option<Vec<u8>> {
        self.runner.read(va as u64, len)
    }

    fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Option<()> {
        self.runner.write(va as u64, bytes).then_some(())
    }

    fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
        self.write_bytes(va, &value.to_le_bytes())
    }

    fn read_bytes_at(&mut self, va: u64, len: usize) -> Option<Vec<u8>> {
        self.runner.read(va, len)
    }

    fn write_bytes_at(&mut self, va: u64, bytes: &[u8]) -> Option<()> {
        self.runner.write(va, bytes).then_some(())
    }

    fn ptr_size(&self) -> u32 {
        if self.runner.x64 {
            8
        } else {
            4
        }
    }

    fn map_exact(&mut self, va: u32, size: u32) -> bool {
        self.runner.reserve(va as u64, size as u64);
        true
    }

    fn unmap(&mut self, va: u32, size: u32) -> bool {
        let (start, end) = (va as u64 & !(PAGE - 1), (va as u64 + size as u64 + PAGE - 1) & !(PAGE - 1));
        let mut reserved = self.runner.reserved.borrow_mut();
        let mut kept = Vec::new();
        for &(s, e) in reserved.iter() {
            if e <= start || s >= end {
                kept.push((s, e));
                continue;
            }
            if s < start {
                kept.push((s, start));
            }
            if e > end {
                kept.push((end, e));
            }
        }
        *reserved = kept;
        drop(reserved);
        self.runner.space.unmap(start, end - start);
        true
    }

    fn map_region(&mut self, preferred: u32, size: u32) -> Option<u32> {
        let start = self.runner.find_free(preferred as u64, size as u64)?;
        self.runner.reserve(start, size as u64);
        Some(start as u32)
    }

    fn mapped(&mut self) -> Vec<(u64, u64)> {
        self.runner.reserved.borrow().iter().map(|&(s, e)| (s, e - s)).collect()
    }

    fn xmm(&mut self, index: usize) -> u64 {
        let fx = &self.regs().fx;
        u64::from_le_bytes(fx[160 + index * 16..168 + index * 16].try_into().unwrap())
    }

    fn set_xmm0(&mut self, bits: u64) {
        let regs = unsafe { &mut *self.regs };
        regs.fx[160..168].copy_from_slice(&bits.to_le_bytes());
        regs.fx[168..176].fill(0);
    }

    fn call_guest(&mut self, func: u32, args: &[u32]) -> Option<u32> {
        let args: Vec<u64> = args.iter().map(|&a| a as u64).collect();
        self.runner.call(self.regs(), func as u64, &args, None, None).map(|v| v as u32)
    }

    fn call_guest_wide(&mut self, func: u32, args: &[u64]) -> Option<u64> {
        self.runner.call(self.regs(), func as u64, args, None, None)
    }

    fn call_guest_ex(&mut self, func: u32, args: &[u32], ebp: Option<u32>, ecx: Option<u32>) -> Option<u32> {
        let args: Vec<u64> = args.iter().map(|&a| a as u64).collect();
        self.runner.call(self.regs(), func as u64, &args, ebp.map(u64::from), ecx.map(u64::from)).map(|v| v as u32)
    }

    fn create_thread(&mut self, entry: u32, param: u32, stack_size: u32) -> Option<u32> {
        self.runner.create_thread(entry as u64, param as u64, stack_size as u64)
    }

    fn current_thread(&self) -> u32 {
        self.runner.running.borrow().last().map(|&(id, _)| id).unwrap_or(1)
    }

    fn current_tib(&self) -> u32 {
        self.runner.running.borrow().last().map(|&(_, tib)| tib as u32).unwrap_or(TIB_BASE)
    }

    fn waiting_threads(&self) -> Vec<u32> {
        self.runner.waiting.borrow().iter().map(|t| t.id).collect()
    }

    fn run_thread(&mut self, id: u32) -> Option<ThreadRun> {
        self.runner.run_thread(id)
    }

    fn suspend_thread(&mut self) -> bool {
        let runner = self.runner;
        if runner.running.borrow().len() < 2 || !runner.levels.borrow().is_empty() || runner.resume.borrow().is_some() {
            return false;
        }
        runner.signal.set(Some(Signal::Suspend));
        true
    }

    fn end_thread(&mut self, code: u32) -> bool {
        let runner = self.runner;
        if runner.running.borrow().len() < 2 || !runner.levels.borrow().is_empty() {
            return false;
        }
        runner.signal.set(Some(Signal::Exit(code)));
        true
    }

    fn registers(&mut self) -> Option<Registers> {
        let r = self.regs();
        Some(Registers {
            eax: r.rax as u32,
            ebx: r.rbx as u32,
            ecx: r.rcx as u32,
            edx: r.rdx as u32,
            esi: r.rsi as u32,
            edi: r.rdi as u32,
            ebp: r.rbp as u32,
            esp: r.rsp as u32,
        })
    }

    fn request_resume(&mut self, resume: Resume) {
        *self.runner.resume.borrow_mut() = Some(resume);
    }

    fn request_resume_full(&mut self, registers: FullRegisters) {
        *self.runner.resume.borrow_mut() = Some(Resume {
            eip: registers.rip as u32,
            esp: registers.gpr[4] as u32,
            ebp: registers.gpr[5] as u32,
            regs: None,
        });
        *self.runner.resume_full.borrow_mut() = Some(registers);
    }

    fn set_full_registers(&mut self, registers: &FullRegisters) {
        let regs = unsafe { &mut *self.regs };
        set_full_registers(regs, registers, false);
    }

    fn full_registers(&mut self) -> Option<FullRegisters> {
        Some(full_registers(self.regs()))
    }

    fn callback_levels(&self) -> Vec<u32> {
        self.runner.levels.borrow().iter().map(|&l| l as u32).collect()
    }

    fn resume_pending(&self) -> bool {
        self.runner.resume.borrow().is_some()
    }

    fn exception_dispatcher(&self) -> Option<u32> {
        (self.runner.dispatcher != 0).then_some(self.runner.dispatcher as u32)
    }

    fn stopping(&self) -> bool {
        self.runner.has_ended()
    }

    /// The desktop is there: programs show windows and wait for input.
    fn has_host(&self) -> bool {
        true
    }

    fn pump(&mut self, block: bool) {
        crate::desktop::pump(block);
    }
}

fn exception_name(vector: u8) -> &'static str {
    match vector {
        0 => "division by zero",
        3 => "breakpoint",
        6 => "invalid instruction",
        13 => "protection fault",
        14 => "access violation",
        _ => "exception",
    }
}

/// Each module's static TLS template (its TLS directory's data and zero
/// fill); writes the module's index variable.
fn tls_templates(image: &Image, runner: &Runner) -> Vec<Vec<u8>> {
    let mut templates = Vec::new();
    for module in &image.modules {
        let memory = &module.memory;
        let read32 = |at: usize| memory.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        let Some(pe) = read32(0x3C).map(|v| v as usize) else { continue };
        let x64 = module.image.is64;
        let directories = pe + 24 + if x64 { 112 } else { 96 };
        let (Some(rva), Some(size)) = (read32(directories + 9 * 8), read32(directories + 9 * 8 + 4)) else { continue };
        if rva == 0 || size == 0 {
            continue;
        }
        let at = rva as usize;
        let width = if x64 { 8 } else { 4 };
        let field = |index: usize| read32(at + index * width).unwrap_or(0);
        let (start, end, index_va) = (field(0), field(1), field(2));
        let zero_fill = read32(at + 4 * width).unwrap_or(0);
        let base = module.image.image_base;
        let mut template = memory
            .get(start.wrapping_sub(base) as usize..end.wrapping_sub(base) as usize)
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        template.extend(core::iter::repeat_n(0, zero_fill as usize));
        runner.write(index_va as u64, &(templates.len() as u32).to_le_bytes());
        templates.push(template);
    }
    templates
}

/// Load the program at `path` (`C:\\…`) from drive C: and run it to the
/// end.
pub fn run(path: &str, command_line: &str) -> Ended {
    let started = apic::now_us();
    let shared = Rc::new(RefCell::new(SharedState::default()));
    let vm = match Vm::from_drive(path.as_bytes(), command_line.as_bytes(), shared.clone()) {
        Ok(vm) => vm,
        Err(error) => return Ended::Failed(format!("cannot load {path}: {error}")),
    };
    let image = vm.process_image();
    let main = image.main();
    let x64 = main.is64;
    let mut runner = Runner {
        space: AddressSpace::new(),
        x64,
        entries: vm.iat().entries.clone(),
        syscalls: Rc::new(vm.api_context().clone()),
        dispatcher: image.entry_point(KERNEL_DLL, EXCEPTION_DISPATCHER).unwrap_or(0) as u64,
        thread_return: image.entry_point(KERNEL_DLL, THREAD_RETURN).unwrap_or(0) as u64,
        reserved: RefCell::new(Vec::new()),
        ended: RefCell::new(None),
        api_calls: Cell::new(0),
        resume: RefCell::new(None),
        resume_full: RefCell::new(None),
        levels: RefCell::new(Vec::new()),
        running: RefCell::new(Vec::new()),
        waiting: RefCell::new(Vec::new()),
        next_thread: Cell::new(2),
        signal: Cell::new(None),
        tls: Vec::new(),
    };

    // The modules, where the loader placed them.
    for module in &image.modules {
        let base = module.image.image_base as u64;
        runner.reserve(base, module.memory.len() as u64);
        if !runner.write(base, &module.memory) {
            return Ended::Failed(format!("cannot map {path}"));
        }
    }
    // Imports: each thunk makes a system call (`int 0x80; ret`).
    let thunks = runner.entries.len() as u64 + 1;
    runner.reserve(THUNK_BASE as u64, thunks * THUNK_STRIDE as u64);
    let mut code = Vec::with_capacity((thunks * THUNK_STRIDE as u64) as usize);
    for _ in 0..thunks {
        code.extend_from_slice(&[0xCD, 0x80, 0xC3, 0x90]);
    }
    runner.write(THUNK_BASE as u64, &code);
    // Where callbacks return.
    runner.reserve(CALLBACK_RETURN, PAGE);
    runner.write(CALLBACK_RETURN, &[0xCD, 0x81]);
    // Stack, heap, and (64-bit) the larger heap and NTDLL's headers.
    let stack_low = if x64 { X64_STACK_LOW } else { STACK_BASE as u64 };
    runner.reserve(stack_low, (STACK_BASE + STACK_SIZE) as u64 - stack_low);
    runner.reserve(memory::HEAP_BASE as u64, memory::HEAP_SIZE as u64);
    if x64 {
        runner.reserve(memory::HEAP64_EXTRA_BASE as u64, memory::HEAP64_EXTRA_SIZE as u64);
        runner.reserve(memory::NTDLL_HANDLE as u64, memory::NTDLL_SIZE as u64);
    }
    // The first thread's information block (FS in 32-bit code, GS in
    // 64-bit), and its TLS.
    let tib = TIB_BASE as u64;
    runner.reserve(tib, if x64 { TEB64_SIZE } else { TIB_SIZE });
    if x64 {
        runner.init_teb64(tib, STACK_TOP as u64, X64_STACK_LOW, 1);
        let peb = tib + 0x2000;
        runner.write(peb + 0x20, &(peb + 0x400).to_le_bytes());
    } else {
        for (offset, value) in [(0u64, u32::MAX), (4, STACK_TOP), (8, STACK_BASE), (0x18, TIB_BASE), (0x30, TIB_BASE + 0x800)] {
            runner.write(tib + offset, &value.to_le_bytes());
        }
    }
    runner.tls = tls_templates(image, &runner);
    runner.install_tls(tib);

    // The entry point, returning to the thread's end.
    let entry = main.image_base as u64 + main.entry_rva as u64;
    let mut sp = (STACK_TOP as u64) & !0xF;
    if x64 {
        sp -= 8;
        runner.write(sp, &runner.thread_return.to_le_bytes());
    } else {
        sp -= 4;
        runner.write(sp, &(runner.thread_return as u32).to_le_bytes());
    }
    let mut regs = Box::new(UserRegs::new(x64, entry, sp));
    log!(
        "OTW kernel: running {path} ({}-bit, {} modules, {} imports) after {} ms of loading",
        if x64 { 64 } else { 32 },
        image.modules.len(),
        runner.entries.len(),
        (apic::now_us() - started) / 1000
    );

    crate::desktop::program_started(&shared, path.rsplit('\\').next().unwrap_or(path));
    // Its memory, its thread block.
    runner.space.activate();
    if !x64 {
        unsafe { core::arch::asm!("mov fs, {0:x}", in(reg) arch::USER_DATA as u64) };
    }
    runner.running.borrow_mut().push((1, tib));
    runner.set_thread_block(tib);
    let ran = apic::now_us();
    let stop = runner.execute(&mut regs);
    if let Stop::Returned(_) | Stop::Suspended | Stop::ThreadExited(_) = stop {
        runner.end(Ended::Exited(0));
    }
    // Back to the kernel's tables before the program's go.
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) crate::paging::page_table_root()) };
    write_msr(0xC000_0101, 0);
    write_msr(0xC000_0100, 0);
    let ended = runner.ended.borrow_mut().take().unwrap_or(Ended::Failed(String::from("stopped")));
    log!(
        "OTW kernel: {path} ran {} ms, {} system calls, {} threads",
        (apic::now_us() - ran) / 1000,
        runner.api_calls.get(),
        runner.next_thread.get() - 1
    );
    otw_vm::win32::registry::flush();
    ended
}
