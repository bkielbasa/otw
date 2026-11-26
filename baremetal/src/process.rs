//! Programs run natively: their code on the CPU itself, in user mode, in an
//! address space of their own; their imports are thunks that make a system
//! call (`int 0x80`) into OTW's Win32 subsystem, the same code as on Linux
//! (`otw_vm::win32`), which reaches the program's memory and calls back
//! into it through `NativeMemory`.
//!
//! So far: one thread, no exception dispatch to the program (a fault ends
//! it), no DLL initialisation; programs whose windows need a display get
//! none yet.

use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use otw_vm::kernel::iat::{IatEntry, THUNK_BASE, THUNK_STRIDE};
use otw_vm::kernel::image::{KERNEL_DLL, THREAD_RETURN};
use otw_vm::kernel::layout::{SENTINEL_BASE, STACK_BASE, STACK_SIZE, STACK_TOP, TIB_BASE};
use otw_vm::kernel::memory::{self, GuestMemory};
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

/// The program's memory and what it is doing.
struct Runner {
    space: AddressSpace,
    x64: bool,
    entries: Vec<IatEntry>,
    syscalls: Rc<dyn SystemCalls>,
    /// Address ranges the program may use; their pages are made when first
    /// touched.
    reserved: RefCell<Vec<(u64, u64)>>,
    ended: RefCell<Option<Ended>>,
    depth: RefCell<usize>,
    api_calls: RefCell<u64>,
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

    fn end(&self, how: Ended) {
        let mut ended = self.ended.borrow_mut();
        if ended.is_none() {
            *ended = Some(how);
        }
    }

    fn has_ended(&self) -> bool {
        self.ended.borrow().is_some()
    }

    /// Run the program from `regs` until it ends, or (a callback) returns
    /// to `CALLBACK_RETURN`. Returns the callback's result.
    fn execute(&self, regs: &mut UserRegs) -> Option<u64> {
        loop {
            if self.has_ended() {
                return None;
            }
            user::run(regs);
            match regs.vector as u8 {
                arch::SYSTEM_CALL_VECTOR => self.system_call(regs),
                arch::CALLBACK_RETURN_VECTOR => return Some(regs.rax),
                14 if regs.error & 1 == 0 && self.fault_in(regs.fault_address) => {}
                vector => {
                    self.end(Ended::Failed(format!(
                        "{} at {:#x} (address {:#x}, error {:#x})",
                        exception_name(vector),
                        regs.rip,
                        regs.fault_address,
                        regs.error
                    )));
                    return None;
                }
            }
        }
    }

    /// The program called an import: run it in the subsystem.
    fn system_call(&self, regs: &mut UserRegs) {
        let thunk = regs.rip.wrapping_sub(2);
        let index = thunk.wrapping_sub(THUNK_BASE as u64) / THUNK_STRIDE as u64;
        let Some(entry) = self.entries.iter().find(|e| e.thunk_va as u64 == thunk).or(self.entries.get(index as usize)) else {
            self.end(Ended::Failed(format!("a system call from {:#x}, not an import", regs.rip)));
            return;
        };
        *self.api_calls.borrow_mut() += 1;
        let x64 = self.x64;
        let esp = regs.rsp;
        let return_address = if x64 { self.read_u64(esp) } else { self.read_u32(esp).map(u64::from) };
        let Some(return_address) = return_address else {
            self.end(Ended::Failed(format!("no return address for {}", entry.dll)));
            return;
        };
        if entry.dll == KERNEL_DLL && entry.name.as_str() == Some(THREAD_RETURN) {
            // The main thread returned from its entry point.
            self.end(Ended::Exited(regs.rax as u32));
            return;
        }
        let arg_count = if x64 {
            self.syscalls.arg_count64(&entry.dll, &entry.name)
        } else {
            self.syscalls.arg_count(&entry.dll, &entry.name)
        };
        let Some(arg_count) = arg_count else {
            self.end(Ended::Failed(format!("{} is not implemented", self.syscalls.describe(&entry.dll, &entry.name))));
            return;
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
                None => {
                    self.end(Ended::Failed(format!("cannot read argument {i} of {}", entry.dll)));
                    return;
                }
            }
        }
        let outcome = {
            let mut memory = NativeMemory { runner: self, regs };
            let args = Args::new(args, x64);
            self.syscalls.call(&mut memory, &entry.dll, &entry.name, &args, return_address as u32)
        };
        if self.has_ended() {
            return;
        }
        let Some(outcome) = outcome else {
            self.end(Ended::Failed(format!("{} failed", self.syscalls.describe(&entry.dll, &entry.name))));
            return;
        };
        if let Some(code) = outcome.exit_code {
            self.end(Ended::Exited(code));
            return;
        }
        if x64 {
            regs.rax = outcome.wide.unwrap_or_else(|| widen(outcome.value));
            regs.rsp = esp + 8;
        } else {
            regs.rax = outcome.value as u64;
            regs.rsp = esp + 4 + outcome.arg_count as u64 * 4;
        }
        regs.rip = return_address;
    }

    /// Call program code at `function` with `args` (from a system call
    /// made with `caller`), returning what it returns.
    fn call(&self, caller: &UserRegs, function: u64, args: &[u64]) -> Option<u64> {
        if *self.depth.borrow() >= MAX_DEPTH {
            return None;
        }
        // Below the caller's stack, aligned.
        let mut sp = (caller.rsp - 0x200) & !0xF;
        let mut regs = UserRegs::new(self.x64, function, 0);
        regs.cs = caller.cs;
        regs.fx = caller.fx;
        if self.x64 {
            let stack_args = args.len().saturating_sub(4);
            // Home area, stack arguments, return address; RSP ≡ 8 (mod 16)
            // at entry.
            sp -= ((32 + stack_args * 8 + 15) & !15) as u64;
            for (i, &value) in args.iter().enumerate().skip(4) {
                self.write(sp + 32 + (i as u64 - 4) * 8 + 8, &value.to_le_bytes());
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
        }
        regs.rsp = sp;
        *self.depth.borrow_mut() += 1;
        let result = self.execute(&mut regs);
        *self.depth.borrow_mut() -= 1;
        result
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
        let size = (size as u64 + PAGE - 1) & !(PAGE - 1);
        let free = |start: u64| {
            !self.runner.reserved.borrow().iter().any(|&(s, e)| start < e && start + size > s)
        };
        let mut start = if preferred != 0 { preferred as u64 & !(PAGE - 1) } else { memory::SEPARATE_BASE as u64 };
        if !free(start) {
            start = memory::SEPARATE_BASE as u64;
            while !free(start) && start + size < 0x7000_0000 {
                start += 0x1_0000;
            }
        }
        (start + size < 0x7000_0000 || preferred != 0).then(|| {
            self.runner.reserve(start, size);
            start as u32
        })
    }

    fn mapped(&mut self) -> Vec<(u64, u64)> {
        self.runner.reserved.borrow().iter().map(|&(s, e)| (s, e - s)).collect()
    }

    fn call_guest(&mut self, func: u32, args: &[u32]) -> Option<u32> {
        let args: Vec<u64> = args.iter().map(|&a| a as u64).collect();
        let caller = unsafe { &*self.regs };
        self.runner.call(caller, func as u64, &args).map(|v| v as u32)
    }

    fn call_guest_wide(&mut self, func: u32, args: &[u64]) -> Option<u64> {
        let caller = unsafe { &*self.regs };
        self.runner.call(caller, func as u64, args)
    }

    fn current_thread(&self) -> u32 {
        1
    }

    fn current_tib(&self) -> u32 {
        TIB_BASE
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

/// Load the program at `path` (`C:\\…`) from drive C: and run it to the
/// end.
pub fn run(path: &str, command_line: &str) -> Ended {
    let started = apic::now_us();
    let shared = Rc::new(RefCell::new(SharedState::default()));
    let vm = match Vm::from_drive(path.as_bytes(), command_line.as_bytes(), shared) {
        Ok(vm) => vm,
        Err(error) => return Ended::Failed(format!("cannot load {path}: {error}")),
    };
    let image = vm.process_image();
    let main = image.main();
    let x64 = main.is64;
    let runner = Runner {
        space: AddressSpace::new(),
        x64,
        entries: vm.iat().entries.clone(),
        syscalls: Rc::new(vm.api_context().clone()),
        reserved: RefCell::new(Vec::new()),
        ended: RefCell::new(None),
        depth: RefCell::new(0),
        api_calls: RefCell::new(0),
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
    // The thread's information block (FS in 32-bit code, GS in 64-bit).
    let tib = TIB_BASE as u64;
    runner.reserve(tib, if x64 { TEB64_SIZE } else { TIB_SIZE });
    if x64 {
        let peb = tib + 0x2000;
        for (offset, value) in [
            (0x00u64, u64::MAX),
            (0x08, STACK_TOP as u64),
            (0x10, X64_STACK_LOW),
            (0x30, tib),
            (0x40, 1),
            (0x48, 1),
            (0x60, peb),
            (0x2000 + 0x20, peb + 0x400),
        ] {
            runner.write(tib + offset, &value.to_le_bytes());
        }
    } else {
        for (offset, value) in [(0u64, u32::MAX), (4, STACK_TOP), (8, STACK_BASE), (0x18, TIB_BASE), (0x30, TIB_BASE + 0x800)] {
            runner.write(tib + offset, &value.to_le_bytes());
        }
    }

    // The entry point, returning to the thread's end.
    let entry = main.image_base as u64 + main.entry_rva as u64;
    let Some(thread_return) = image.entry_point(KERNEL_DLL, THREAD_RETURN) else {
        return Ended::Failed(String::from("no thread return"));
    };
    let mut sp = (STACK_TOP as u64) & !0xF;
    if x64 {
        sp -= 8;
        runner.write(sp, &(thread_return as u64).to_le_bytes());
    } else {
        sp -= 4;
        runner.write(sp, &thread_return.to_le_bytes());
    }
    let mut regs = UserRegs::new(x64, entry, sp);
    log!(
        "OTW kernel: running {path} ({}-bit, {} modules, {} imports) after {} ms of loading",
        if x64 { 64 } else { 32 },
        image.modules.len(),
        runner.entries.len(),
        (apic::now_us() - started) / 1000
    );

    // Its memory, its thread block.
    runner.space.activate();
    if x64 {
        write_msr(0xC000_0101, tib); // GS base
    } else {
        unsafe { core::arch::asm!("mov fs, {0:x}", in(reg) arch::USER_DATA as u64) };
        write_msr(0xC000_0100, tib); // FS base
    }
    let ran = apic::now_us();
    runner.execute(&mut regs);
    // Back to the kernel's tables before the program's go.
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) crate::paging::page_table_root()) };
    write_msr(0xC000_0101, 0);
    write_msr(0xC000_0100, 0);
    let ended = runner.ended.borrow_mut().take().unwrap_or(Ended::Failed(String::from("stopped")));
    log!(
        "OTW kernel: {path} ran {} ms, {} system calls",
        (apic::now_us() - ran) / 1000,
        runner.api_calls.borrow()
    );
    otw_vm::win32::registry::flush();
    ended
}
