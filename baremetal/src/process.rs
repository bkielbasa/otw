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
use alloc::collections::BTreeMap;
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
use otw_vm::win32::{kobj, SharedState};

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

/// The programs running (for the status report): their runners.
static RUNNERS: otw_vm::platform::sync::Mutex<Vec<usize>> = otw_vm::platform::sync::Mutex::new(Vec::new());

/// Log what each running program is doing (every ten seconds, from the
/// desktop; with OTW_TRACE_STATUS=1): calls by name since the last report,
/// each thread's last calls, and the latest.
pub fn report() {
    if otw_vm::platform::env::var("OTW_TRACE_STATUS").map_or(true, |v| v == "0") {
        return;
    }
    // (The list stays locked: a program that ends leaves it first, and
    // its Runner goes only after.)
    let runners = RUNNERS.lock().unwrap();
    for &runner in runners.iter() {
        let runner = unsafe { &*(runner as *const Runner) };
        let _hold = otw_vm::kernel::lock::Hold::new(&runner.lock);
        let mut recent = runner.recent_calls(6);
        recent.reverse();
        let mut counts: Vec<(String, u64)> = core::mem::take(&mut *runner.counts.borrow_mut()).into_iter().collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = counts.iter().map(|(n, c)| alloc::format!("{n} {c}")).collect();
        log!("OTW kernel: [calls] {}", top.join(", "));
        let now = crate::apic::now_us();
        let threads: Vec<String> = runner
            .last_call
            .0
            .borrow()
            .iter()
            .filter(|(_, (names, _, _))| names.back().is_some_and(|n| n != "ThreadReturn"))
            .map(|(task, (names, at, calls))| {
                let names: Vec<&str> = names.iter().map(|n| n.as_str()).collect();
                alloc::format!("{task}:{}@{}ms/{calls}", names.join(">"), (now - at) / 1000)
            })
            .collect();
        log!("OTW kernel: [threads] {}", threads.join(" "));
        log!(
            "OTW kernel: [status] {} calls, {} threads on CPUs; last: {}",
            runner.api_calls.get(),
            runner.workers.borrow().len(),
            recent.join(", ")
        );
    }
}

/// Where the CPUs' time went since the last report (OTW_PROFILE): idle,
/// program code by module and place, kernel work by call or task.
pub fn profile_report() {
    if !crate::profile::on() {
        return;
    }
    let samples = crate::profile::take();
    if samples.is_empty() {
        return;
    }
    let counts: Vec<u64> = otw_vm::win32::kobj::COUNTS.iter().map(|c| c.swap(0, core::sync::atomic::Ordering::Relaxed)).collect();
    log!(
        "OTW kernel: [profile] waits: {} notifications woke {} threads, {} woken by key, {} parks ({} woken)",
        counts[0], counts[1], counts[2], counts[3], counts[4]
    );
    for (what, counts) in [("pipe data", &otw_vm::win32::kobj::PIPE_WAITS), ("completion packets", &otw_vm::win32::kobj::PORT_WAITS)] {
        let c: Vec<u64> = counts.iter().map(|c| c.swap(0, core::sync::atomic::Ordering::Relaxed)).collect();
        if c[0] > 0 {
            log!(
                "OTW kernel: [profile] {what}: {} taken, waited {} us on average, {} us at most; over 1 ms {}, over 5 ms {}, over 20 ms {}",
                c[0], c[1] / c[0], c[2], c[3], c[4], c[5]
            );
        }
    }
    let runners = RUNNERS.lock().unwrap();
    // Each program's address space and modules (copied under its lock).
    let mut spaces: Vec<(u64, String, Vec<(String, u64, u64, Vec<(u32, String)>)>)> = Vec::new();
    for &runner in runners.iter() {
        let runner = unsafe { &*(runner as *const Runner) };
        let _hold = otw_vm::kernel::lock::Hold::new(&runner.lock);
        let modules = runner
            .modules
            .borrow()
            .iter()
            .map(|m| {
                let mut exports: Vec<(u32, String)> = m.exports_by_name.iter().map(|(n, &rva)| (rva, n.clone())).collect();
                exports.sort();
                (m.name.clone(), m.image_base as u64, m.size as u64, exports)
            })
            .collect();
        spaces.push((runner.space.root, runner.name.clone(), modules));
    }
    drop(runners);
    let total = samples.len() as u64;
    let mut idle = 0u64;
    let mut kernel: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_module: BTreeMap<String, u64> = BTreeMap::new();
    let mut places: BTreeMap<String, u64> = BTreeMap::new();
    let mut user = 0u64;
    for s in &samples {
        match s.kind {
            crate::profile::IDLE => idle += 1,
            crate::profile::USER => {
                user += 1;
                let space = spaces.iter().find(|(root, _, _)| *root == s.space);
                let module = space.and_then(|(_, _, modules)| modules.iter().find(|(_, base, size, _)| s.at >= *base && s.at < base + size));
                let (module_name, place) = match module {
                    Some((name, base, _, exports)) => {
                        let rva = (s.at - base) as u32;
                        let near = exports.iter().rev().find(|(e, _)| *e <= rva);
                        let place = match near {
                            Some((e, n)) if rva - e < 0x1000 => alloc::format!("{name}!{n}+{:#x}", rva - e),
                            _ => alloc::format!("{name}+{:#x}", rva & !0xFF),
                        };
                        (name.clone(), place)
                    }
                    None => (String::from("?"), alloc::format!("{:#x}", s.at & !0xFF)),
                };
                *by_module.entry(module_name).or_default() += 1;
                *places.entry(place).or_default() += 1;
            }
            _ => {
                let name = crate::profile::name(s.at as usize);
                let key = if name.is_empty() { alloc::format!("[task {}]", s.task) } else { String::from(name) };
                *kernel.entry(key).or_default() += 1;
            }
        }
    }
    let top = |map: BTreeMap<String, u64>, n: usize| -> String {
        let mut v: Vec<(String, u64)> = map.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.iter().take(n).map(|(k, c)| alloc::format!("{k} {:.1}%", *c as f64 * 100.0 / total as f64)).collect::<Vec<_>>().join(", ")
    };
    let kernel_total = total - idle - user;
    log!(
        "OTW kernel: [profile] {} samples: idle {:.1}%, programs {:.1}%, kernel {:.1}%",
        total,
        idle as f64 * 100.0 / total as f64,
        user as f64 * 100.0 / total as f64,
        kernel_total as f64 * 100.0 / total as f64
    );
    log!("OTW kernel: [profile] kernel: {}", top(kernel, 25));
    log!("OTW kernel: [profile] modules: {}", top(by_module, 12));
    log!("OTW kernel: [profile] places: {}", top(places, 25));
}

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

/// State each thread of a program has (kept by the task running it).
struct PerTask<T: Default>(RefCell<alloc::collections::BTreeMap<u64, T>>);

impl<T: Default> PerTask<T> {
    fn new() -> Self {
        Self(RefCell::new(alloc::collections::BTreeMap::new()))
    }

    fn borrow_mut(&self) -> core::cell::RefMut<'_, T> {
        let id = crate::sched::current_id();
        core::cell::RefMut::map(self.0.borrow_mut(), |m| m.entry(id).or_default())
    }

    fn borrow(&self) -> core::cell::Ref<'_, T> {
        let id = crate::sched::current_id();
        self.0.borrow_mut().entry(id).or_default();
        core::cell::Ref::map(self.0.borrow(), |m| &m[&id])
    }

    fn take(&self) -> T {
        core::mem::take(&mut *self.borrow_mut())
    }

    fn set(&self, value: T) {
        *self.borrow_mut() = value;
    }

    fn forget(&self) {
        self.0.borrow_mut().remove(&crate::sched::current_id());
    }
}

/// The program's memory, threads and state.
struct Runner {
    space: AddressSpace,
    /// The process's subsystem state (checked: never borrowed while its
    /// code runs, when other threads may want it).
    state: Rc<RefCell<SharedState>>,
    x64: bool,
    entries: Vec<IatEntry>,
    /// The program's file name.
    name: String,
    /// Each entry's profile tag (OTW_PROFILE), made when first called.
    tags: RefCell<Vec<usize>>,
    syscalls: Rc<dyn SystemCalls>,
    /// The loaded modules (to name code addresses when a program fails).
    modules: Rc<RefCell<Vec<otw_vm::win32::LoadedModule>>>,
    dispatcher: u64,
    thread_return: u64,
    /// Address ranges the program may use; their pages are made when first
    /// touched.
    reserved: RefCell<Vec<(u64, u64)>>,
    ended: RefCell<Option<Ended>>,
    api_calls: Cell<u64>,
    /// Collect the calls for the status report (OTW_TRACE_STATUS).
    trace_status: bool,
    /// Each thread's last call, when it was made, and its calls so far.
    last_call: PerTask<(alloc::collections::VecDeque<String>, u64, u64)>,
    /// Calls by name since the last status report.
    counts: RefCell<alloc::collections::BTreeMap<String, u64>>,
    /// Exception handling's request to continue elsewhere.
    resume: PerTask<Option<Resume>>,
    resume_full: PerTask<Option<FullRegisters>>,
    /// The stack pointers below which each running callback's frames lie
    /// (outermost first).
    levels: PerTask<Vec<u64>>,
    /// The running threads (id, TIB), outermost first: a thread runs the
    /// others from inside its waits.
    running: RefCell<Vec<(u32, u64)>>,
    waiting: RefCell<Vec<Thread>>,
    next_thread: Cell<u32>,
    signal: PerTask<Option<Signal>>,
    /// The subsystem's lock: one thread at a time in the subsystem; program
    /// code runs without it.
    lock: alloc::sync::Arc<otw_vm::kernel::lock::BigLock>,
    /// Threads get tasks of their own (and so other CPUs).
    parallel: bool,
    /// The threads running as tasks of their own: task id → (thread id,
    /// thread block).
    workers: RefCell<alloc::collections::BTreeMap<u64, (u32, u64)>>,
    /// Each module's TLS template, copied for every thread.
    tls: RefCell<Vec<Vec<u8>>>,
    /// Memory shared between mappings (sections, and 64-bit memory above
    /// 4 GiB with its low alias), by address: size, alias, the memory.
    shared: RefCell<alloc::collections::BTreeMap<u64, (u64, Option<u64>, alloc::sync::Arc<memory::SharedMemory>)>>,
    /// The last ranges unmapped (for when a program touches one).
    unmapped: RefCell<alloc::collections::VecDeque<(u64, u64)>>,
    /// The last calls made (for when a program ends badly).
    /// (Entries, by index: named only when shown.)
    recent: RefCell<alloc::collections::VecDeque<usize>>,
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

    /// A free range of `size` bytes: from `preferred` up (the first free
    /// 64 KiB-aligned place), else from the start of the separate window.
    fn find_free(&self, preferred: u64, size: u64) -> Option<u64> {
        let size = (size + PAGE - 1) & !(PAGE - 1);
        // Below the thunks in 32-bit processes; below NTDLL's image in
        // 64-bit ones (which reach the whole low 4 GiB).
        let limit = if self.x64 { memory::NTDLL_HANDLE as u64 } else { THUNK_BASE as u64 };
        let reserved = self.reserved.borrow();
        let search = |from: u64| -> Option<u64> {
            let mut start = from;
            loop {
                if start + size > limit {
                    return None;
                }
                match reserved.iter().filter(|&&(s, e)| start < e && start + size > s).map(|&(_, e)| e).max() {
                    None => return Some(start),
                    Some(end) => start = (end + 0xFFFF) & !0xFFFF,
                }
            }
        };
        if preferred != 0 {
            let exact = preferred & !(PAGE - 1);
            if search(exact) == Some(exact) {
                return Some(exact);
            }
        }
        let from = if preferred != 0 { (preferred + 0xFFFF) & !0xFFFF } else { memory::SEPARATE_BASE as u64 };
        search(from).or_else(|| search(memory::SEPARATE_BASE as u64))
    }

    fn end(&self, how: Ended) {
        let mut ended = self.ended.borrow_mut();
        if ended.is_none() {
            *ended = Some(how);
            drop(ended);
            self.kick_workers();
        }
    }

    /// Make threads running program code on other CPUs come back to the
    /// kernel (and see that the process ends).
    fn kick_workers(&self) {
        if self.workers.borrow().is_empty() {
            return;
        }
        let me = apic::cpu_index();
        for index in (0..apic::cpu_count()).filter(|&i| i != me) {
            if let Some(id) = apic::apic_id_of(index) {
                apic::send_ipi(id, arch::WAKE_VECTOR as u32);
            }
        }
    }

    fn is_worker(&self) -> bool {
        self.workers.borrow().contains_key(&crate::sched::current_id())
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
            // Program code runs without the subsystem's lock.
            let depth = self.lock.release_all();
            if depth == 0 {
                static SAID: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
                if !SAID.swap(true, core::sync::atomic::Ordering::Relaxed) {
                    log!("OTW kernel: [lock] program code entered without the lock, in {:?}", crate::profile::name(crate::sched::current().tag));
                }
            }
            user::run(regs);
            let vector = regs.vector as u8;
            if vector == arch::TIMER_VECTOR || vector == arch::WAKE_VECTOR {
                // Another task wanted this CPU (or the program is ending).
                if vector == arch::TIMER_VECTOR {
                    crate::sched::yield_now();
                }
                self.lock.reacquire(depth);
                continue;
            }
            {
                let _tag = crate::profile::on().then(|| crate::sched::tagged(crate::profile::tag("[lock]")));
                self.lock.reacquire(depth);
            }
            let _tag = (crate::profile::on() && regs.vector == 14).then(|| crate::sched::tagged(crate::profile::tag("[page fault]")));
            let stop = match regs.vector as u8 {
                arch::SYSTEM_CALL_VECTOR => self.system_call(regs),
                arch::CALLBACK_RETURN_VECTOR => Some(Stop::Returned(regs.rax)),
                // A page to make (or one another CPU made meanwhile, as this
                // one waited for the lock).
                14 if regs.error & 1 == 0 && (self.space.is_mapped(regs.fault_address) || self.fault_in(regs.fault_address)) => None,
                vector => self.raise_fault(regs, vector),
            };
            if let Some(stop) = stop {
                return stop;
            }
        }
    }

    /// A CPU fault: give it to the program's exception handlers.
    fn raise_fault(&self, regs: &mut UserRegs, vector: u8) -> Option<Stop> {
        // `int 0x29` (__fastfail): the program ends itself at once, no
        // handler asked (its gate is closed to user mode, so it arrives as a
        // protection fault naming it).
        if vector == 13 && regs.error == (0x29 << 3 | 2) {
            log!("OTW kernel: the program failed fast (reason {}) at {:#x}", regs.rcx, regs.rip);
            self.end(Ended::Exited(0xC000_0409));
            return Some(Stop::Ended);
        }
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
        {
            let address = regs.fault_address;
            let gone: Vec<(u64, u64)> = self.unmapped.borrow().iter().copied().filter(|&(s, e)| address >= s && address < e).collect();
            let shared: Vec<(u64, u64, Option<u64>)> = self.shared.borrow().iter().filter(|(&va, &(size, alias, _))| (address >= va && address < va + size) || alias.is_some_and(|l| address >= l && address < l + size)).map(|(&va, &(size, alias, _))| (va, size, alias)).collect();
            log!("[eh] {address:#x}: reserved {}, unmapped earlier {gone:x?}, shared {shared:x?}", self.reserved_at(address));
            // What the program was doing: return addresses on its stack,
            // strings its registers point at (a crash reason), its code.
            let stack: Vec<u64> = (0..24).filter_map(|i| self.read_u64(regs.rsp + i * 8)).filter(|&v| v >= 0x1_0000).collect();
            let modules = self.modules.borrow();
            let name = |v: u64| -> String {
                modules
                    .iter()
                    .find(|m| v >= m.image_base as u64 && v < m.image_base as u64 + m.size as u64)
                    .map(|m| alloc::format!("{}+{:#x}", m.name, v - m.image_base as u64))
                    .unwrap_or_else(|| alloc::format!("{v:#x}"))
            };
            let named: Vec<String> = stack.iter().map(|&v| name(v)).collect();
            log!("[eh] at {}; stack {}", name(at), named.join(" "));
            drop(modules);
            for (name, v) in [("rax", regs.rax), ("rcx", regs.rcx), ("rsi", regs.rsi), ("r8", regs.r8), ("r9", regs.r9)] {
                if let Some(bytes) = self.read(v, 96) {
                    let text: String = bytes.iter().take_while(|&&b| b != 0).map(|&b| if (32..127).contains(&b) { b as char } else { '.' }).collect();
                    log!("[eh] {name} -> {text:?}");
                }
            }
            log!("[eh] code {:02x?} rsp {:#x} rax {:#x} rcx {:#x} rdx {:#x} rbx {:#x} rsi {:#x} rdi {:#x} error {:#x}",
                self.read(regs.rip, 16).unwrap_or_default(), regs.rsp, regs.rax, regs.rcx, regs.rdx, regs.rbx, regs.rsi, regs.rdi, regs.error);
        }
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

    /// The last `n` calls made, by name, oldest first.
    fn recent_calls(&self, n: usize) -> Vec<String> {
        let recent = self.recent.borrow();
        let skip = recent.len().saturating_sub(n);
        recent.iter().skip(skip).map(|&i| self.syscalls.describe(&self.entries[i].dll, &self.entries[i].name)).collect()
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
        let index = (entry as *const IatEntry as usize - self.entries.as_ptr() as usize) / core::mem::size_of::<IatEntry>();
        let _tag = crate::profile::on().then(|| {
            let mut tags = self.tags.borrow_mut();
            if tags[index] == 0 {
                tags[index] = crate::profile::tag(&self.syscalls.describe(&entry.dll, &entry.name));
            }
            crate::sched::tagged(tags[index])
        });
        {
            let mut recent = self.recent.borrow_mut();
            if recent.len() >= 40 {
                recent.pop_front();
            }
            if self.trace_status {
                let described = self.syscalls.describe(&entry.dll, &entry.name);
                let short: String = described.rsplit('!').next().unwrap_or("").into();
                *self.counts.borrow_mut().entry(short.clone()).or_default() += 1;
                let mut last = self.last_call.borrow_mut();
                if last.0.back() != Some(&short) {
                    if last.0.len() >= 8 {
                        last.0.pop_front();
                    }
                    last.0.push_back(short);
                }
                last.1 = crate::apic::now_us();
                last.2 += 1;
            }
            recent.push_back(index);
        }
        let x64 = self.x64;
        let esp = regs.rsp;
        let return_address = if x64 { self.read_u64(esp) } else { self.read_u32(esp).map(u64::from) };
        let Some(return_address) = return_address else {
            return Some(self.fail(format!("no return address for {}", entry.dll)));
        };
        let kernel_entry = entry.dll == KERNEL_DLL;
        if kernel_entry && entry.name.as_str() == Some(THREAD_RETURN) {
            // A thread returned from its start routine.
            if self.running.borrow().len() > 1 || self.is_worker() {
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
            // OTW_STUB_MISSING: functions OTW lacks return 0 (64-bit
            // programs), named in the log, to see what comes next.
            if x64 && otw_vm::platform::env::var_os("OTW_STUB_MISSING").is_some() {
                let name = self.syscalls.describe(&entry.dll, &entry.name);
                if self.recent.borrow().iter().filter(|&&n| n == index).count() <= 1 {
                    log!("[stub] {name} returns 0");
                }
                regs.rax = 0;
                regs.rsp = esp + 8;
                regs.rip = return_address;
                return None;
            }
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
        if outcome.value == 0 && outcome.wide.unwrap_or(0) == 0 {
            if let Some(name) = entry.name.as_str() {
                if matches!(name, "VirtualAlloc" | "VirtualAllocEx" | "HeapAlloc" | "HeapReAlloc" | "malloc" | "calloc" | "realloc" | "_aligned_malloc" | "VirtualAlloc2" | "MapViewOfFile" | "MapViewOfFileEx" | "CreateFileMappingW") {
                    log!("[mem] {name} failed: {:x?}", [regs.rcx, regs.rdx, regs.r8, regs.r9]);
                }
            }
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
        let tls = self.tls.borrow();
        if tls.is_empty() {
            return;
        }
        let width = if self.x64 { 8 } else { 4 };
        let array = (tls.len() as u64 * width + 15) & !15;
        let total = array + tls.iter().map(|t| (t.len() as u64 + 15) & !15).sum::<u64>();
        let Some(block) = self.find_free(0x1000_0000, total.max(1)) else { return };
        self.reserve(block, total.max(1));
        let mut cursor = block + array;
        for (index, template) in tls.iter().enumerate() {
            self.write(cursor, template);
            self.write(block + index as u64 * width, &cursor.to_le_bytes()[..width as usize]);
            cursor += (template.len() as u64 + 15) & !15;
        }
        let slot = if self.x64 { teb + 0x58 } else { teb + 0x2C };
        self.write(slot, &block.to_le_bytes()[..width as usize]);
    }

    /// A module loaded later has TLS: every thread gets a longer array with
    /// a block for it. Returns its index.
    fn add_tls(&self, template: &[u8]) -> Option<u32> {
        let index = {
            let mut tls = self.tls.borrow_mut();
            tls.push(template.to_vec());
            tls.len() as u64 - 1
        };
        let width = if self.x64 { 8 } else { 4 };
        let mut tibs: Vec<u64> = self.running.borrow().iter().map(|&(_, tib)| tib).collect();
        tibs.extend(self.waiting.borrow().iter().map(|t| t.tib));
        for tib in tibs {
            let slot = if self.x64 { tib + 0x58 } else { tib + 0x2C };
            let old = self.read(slot, width as usize).map(|b| {
                let mut v = [0u8; 8];
                v[..width as usize].copy_from_slice(&b);
                u64::from_le_bytes(v)
            })?;
            let size = (index + 1) * width;
            let total = size + 16 + template.len() as u64;
            let array = self.find_free(0x1000_0000, total)?;
            self.reserve(array, total);
            if old != 0 && index > 0 {
                let previous = self.read(old, (index * width) as usize)?;
                self.write(array, &previous);
            }
            let block = (array + size + 15) & !15;
            self.write(block, template);
            self.write(array + index * width, &block.to_le_bytes()[..width as usize]);
            self.write(slot, &array.to_le_bytes()[..width as usize]);
        }
        Some(index as u32)
    }

    fn init_teb64(&self, teb: u64, top: u64, bottom: u64, thread: u32) {
        let peb = TIB_BASE as u64 + 0x2000;
        for (offset, value) in [(0x00u64, u64::MAX), (0x08, top), (0x10, bottom), (0x30, teb), (0x40, 1), (0x48, thread as u64), (0x60, peb)] {
            self.write(teb + offset, &value.to_le_bytes());
        }
    }

    /// The running task runs this program's code with thread block `tib`
    /// (GS in 64-bit code, FS in 32-bit code).
    fn set_thread_block(&self, tib: u64) {
        let task = crate::sched::current();
        task.cr3 = self.space.root;
        task.compat = !self.x64;
        if self.x64 {
            task.gs_base = tib;
            write_msr(0xC000_0101, tib);
        } else {
            task.fs_base = tib;
            write_msr(0xC000_0100, tib);
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
        if self.parallel {
            // A task of its own, on whichever CPU is free.
            let runner = self as *const Runner as usize;
            let task = crate::sched::spawn(move || worker_main(runner, id, regs, tib));
            self.workers.borrow_mut().insert(task, (id, tib));
            return Some(id);
        }
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

    /// Unmap [va, va + size): no longer the program's.
    fn unmap_range(&mut self, va: u64, size: u64) -> bool {
        let (start, end) = (va & !(PAGE - 1), (va + size + PAGE - 1) & !(PAGE - 1));
        {
            let mut unmapped = self.runner.unmapped.borrow_mut();
            if unmapped.len() >= 256 {
                unmapped.pop_front();
            }
            unmapped.push_back((start, end));
        }
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

    fn compare_exchange_u32(&mut self, va: u32, current: u32, new: u32) -> Option<Result<u32, u32>> {
        let va = va as u64;
        if va & 3 != 0 || !self.runner.touch(va, 4) {
            return None;
        }
        let pointer = self.runner.space.translate(va)? as *mut u32;
        // The program's other threads change it on other CPUs.
        let atomic = unsafe { core::sync::atomic::AtomicU32::from_ptr(pointer) };
        Some(atomic.compare_exchange(current, new, core::sync::atomic::Ordering::SeqCst, core::sync::atomic::Ordering::SeqCst))
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
        self.unmap_range(va as u64, size as u64)
    }

    fn map_region(&mut self, preferred: u32, size: u32) -> Option<u32> {
        let Some(start) = self.runner.find_free(preferred as u64, size as u64) else {
            log!("[mem] no room for {size:#x} bytes (asked {preferred:#x})");
            return None;
        };
        self.runner.reserve(start, size as u64);
        Some(start as u32)
    }


    fn map_shared(&mut self, va: u64, size: u64, alias: Option<u32>) -> bool {
        let size = (size + PAGE - 1) & !(PAGE - 1);
        let Some(section) = memory::SharedMemory::new(size as usize) else {
            log!("[mem] no memory for {size:#x} shared bytes at {va:#x}");
            return false;
        };
        let physical = crate::paging::to_physical(section.pointer(0) as u64);
        let runner = self.runner;
        if !runner.space.map_frames(va, physical, size) {
            return false;
        }
        runner.reserve(va, size);
        let alias = alias.map(|low| low as u64).filter(|&low| runner.space.map_frames(low, physical, size));
        if let Some(low) = alias {
            runner.reserve(low, size);
        }
        runner.shared.borrow_mut().insert(va, (size, alias, section));
        true
    }

    fn unmap_shared(&mut self, va: u64) -> bool {
        let Some((size, alias, _section)) = self.runner.shared.borrow_mut().remove(&va) else { return false };
        self.unmap_range(va, size);
        if let Some(low) = alias {
            self.unmap_range(low, size);
        }
        true
    }

    fn map_section(&mut self, section: alloc::sync::Arc<memory::SharedMemory>, offset: usize, size: u32, preferred: u32) -> Option<u32> {
        let size = (size as u64 + PAGE - 1) & !(PAGE - 1);
        if offset as u64 + size > section.len() as u64 {
            return None;
        }
        let runner = self.runner;
        let va = runner.find_free(if preferred != 0 { preferred as u64 } else { memory::SEPARATE_BASE as u64 }, size)?;
        let physical = crate::paging::to_physical(section.pointer(offset) as u64);
        if !runner.space.map_frames(va, physical, size) {
            return None;
        }
        runner.reserve(va, size);
        runner.shared.borrow_mut().insert(va, (size, None, section));
        Some(va as u32)
    }

    fn unmap_section(&mut self, va: u32) -> bool {
        let Some((size, _, _section)) = self.runner.shared.borrow_mut().remove(&(va as u64)) else { return false };
        self.unmap_range(va as u64, size);
        true
    }

    fn mapped(&mut self) -> Vec<(u64, u64)> {
        self.runner.reserved.borrow().iter().map(|&(s, e)| (s, e - s)).collect()
    }

    fn add_tls(&mut self, template: &[u8]) -> Option<u32> {
        self.runner.add_tls(template)
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
        if let Some(&(id, _)) = self.runner.workers.borrow().get(&crate::sched::current_id()) {
            return id;
        }
        self.runner.running.borrow().last().map(|&(id, _)| id).unwrap_or(1)
    }

    fn current_tib(&self) -> u32 {
        if let Some(&(_, tib)) = self.runner.workers.borrow().get(&crate::sched::current_id()) {
            return tib as u32;
        }
        self.runner.running.borrow().last().map(|&(_, tib)| tib as u32).unwrap_or(TIB_BASE)
    }

    fn is_worker(&self) -> bool {
        self.runner.is_worker()
    }

    fn others_running(&self) -> bool {
        !self.runner.workers.borrow().is_empty()
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
        if (runner.running.borrow().len() < 2 && !runner.is_worker()) || !runner.levels.borrow().is_empty() {
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

/// A thread of a program on a task of its own.
fn worker_main(runner: usize, id: u32, mut regs: Box<UserRegs>, tib: u64) {
    let runner = unsafe { &*(runner as *const Runner) };
    let hold = otw_vm::kernel::lock::Hold::new(&runner.lock);
    runner.set_thread_block(tib);
    // Created suspended: it waits to be resumed.
    while runner.syscalls.thread_suspended(id) && !runner.has_ended() {
        otw_vm::kernel::lock::unlocked(|| crate::sched::sleep_us(1000));
    }
    if otw_vm::platform::env::var_os("OTW_TRACE_THREADS").is_some() || id <= 6 {
        log!("OTW kernel: thread {id} starts on CPU {}", apic::cpu_index());
    }
    let stop = if runner.has_ended() { Stop::Ended } else { runner.execute(&mut regs) };
    if !runner.has_ended() {
        let code = if let Stop::ThreadExited(code) = stop { code } else { 0 };
        let mut memory = NativeMemory { runner, regs: &mut *regs };
        runner.syscalls.thread_exited(&mut memory, id, code);
    }
    runner.levels.forget();
    runner.resume.forget();
    runner.resume_full.forget();
    runner.signal.forget();
    runner.workers.borrow_mut().remove(&crate::sched::current_id());
    let task = crate::sched::current();
    task.cr3 = 0;
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) crate::paging::page_table_root()) };
    drop(hold);
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
/// end (a program the desktop starts).
pub fn run(path: &str, command_line: &str) -> Ended {
    let shared = Rc::new(RefCell::new(SharedState::default()));
    let pid = kobj::with(|k| k.new_pid(0, false));
    shared.borrow_mut().pid = pid;
    KernelLauncher::install(&shared);
    let ended = run_process(&shared, path, command_line, b"");
    finish_process(pid, &ended);
    ended
}

/// A process ended: its exit code for those who wait for it, its handles
/// closed.
fn finish_process(pid: u32, ended: &Ended) {
    let code = match ended {
        Ended::Exited(code) => *code,
        Ended::Failed(_) => 0xC000_0005,
    };
    kobj::with(|k| {
        if let Some(process) = k.processes.get_mut(&pid) {
            process.exit.get_or_insert(code);
        }
        k.close_all(pid);
    });
    kobj::notify();
}

/// Starts the programs a program creates (`CreateProcess`), as on Linux
/// (`vm::NestedLauncher`): a 64-bit program's 64-bit child that shares
/// handles with it (a browser's content process) runs alongside, as a
/// task of its own; others run to the end inside the call.
struct KernelLauncher;

impl KernelLauncher {
    fn install(shared: &Rc<RefCell<SharedState>>) {
        shared.borrow_mut().launcher = otw_vm::win32::LauncherSlot(Some(Rc::new(KernelLauncher)));
    }
}

impl otw_vm::win32::Launcher for KernelLauncher {
    fn start(&self, parent: &otw_vm::win32::ApiContext, path: &[u8], command_line: &[u8], directory: &[u8]) -> Result<u32, String> {
        let shared = Rc::new(RefCell::new(SharedState::default()));
        let parent_pid = parent.shared.borrow().pid;
        if let Some(environment) = parent.shared.borrow_mut().pending_environment.take() {
            otw_vm::win32::env::set_all(&mut shared.borrow_mut().env, &environment);
        }
        let pid = kobj::with(|k| k.new_pid(parent_pid, false));
        shared.borrow_mut().pid = pid;
        KernelLauncher::install(&shared);
        let path = String::from_utf8_lossy(path).into_owned();
        let command_line = String::from_utf8_lossy(command_line).into_owned();
        // In the parent's place: its address space and thread block come
        // back afterwards.
        let task = crate::sched::current();
        let saved = (task.cr3, task.fs_base, task.gs_base, task.compat);
        let ended = otw_vm::kernel::lock::unlocked(|| run_process(&shared, &path, &command_line, directory));
        let task = crate::sched::current();
        (task.cr3, task.fs_base, task.gs_base, task.compat) = saved;
        if task.cr3 != 0 {
            unsafe { core::arch::asm!("mov cr3, {}", in(reg) task.cr3) };
        }
        if let Ended::Failed(why) = &ended {
            log!("OTW kernel: {path} failed: {why}");
        }
        finish_process(pid, &ended);
        Ok(pid)
    }

    fn start_with(&self, parent: &otw_vm::win32::ApiContext, path: &[u8], command_line: &[u8], directory: &[u8], options: otw_vm::win32::LaunchOptions) -> Result<u32, String> {
        let child64 = otw_vm::win32::crt::host_path(path)
            .and_then(|host| otw_vm::platform::fs::read(host).ok())
            .and_then(|bytes| otw_vm::kernel::pe::PeImage::parse(bytes).ok())
            .is_some_and(|image| image.is64);
        let parent_pid = parent.shared.borrow().pid;
        let shares = options.inherit
            && match &options.handles {
                Some(handles) => handles.iter().any(|&h| kobj::with(|k| k.owns(parent_pid, h))),
                None => kobj::with(|k| !k.inheritable(parent_pid).is_empty()),
            };
        if !(child64 && parent.shared.borrow().x64 && shares) {
            if let Some(environment) = options.environment {
                parent.shared.borrow_mut().pending_environment = Some(environment);
            }
            return self.start(parent, path, command_line, directory);
        }
        // Alongside: a process of its own, on a task of its own.
        let pid = kobj::with(|k| {
            let pid = k.new_pid(parent_pid, options.suspended);
            if options.inherit {
                let handles = options.handles.clone().unwrap_or_else(|| k.inheritable(parent_pid));
                for handle in handles {
                    // Inherited handles keep their numbers.
                    if k.owns(parent_pid, handle) {
                        k.duplicate(parent_pid, handle, pid, Some(handle));
                    }
                }
            }
            pid
        });
        let screen = parent.shared.borrow().gui.screen;
        let environment = options.environment.clone();
        let path = String::from_utf8_lossy(path).into_owned();
        let command_line = String::from_utf8_lossy(command_line).into_owned();
        let directory = directory.to_vec();
        log!("OTW kernel: process {pid:#x} starts alongside {parent_pid:#x}: {command_line}");
        crate::sched::spawn(move || {
            let shared = Rc::new(RefCell::new(SharedState::default()));
            {
                let mut state = shared.borrow_mut();
                state.pid = pid;
                state.alongside = true;
                state.gui.screen = screen;
                state.gui.script = Some(Default::default());
                if let Some(block) = &environment {
                    otw_vm::win32::env::set_all(&mut state.env, block);
                }
            }
            KernelLauncher::install(&shared);
            // Created suspended: it waits to be resumed.
            while kobj::with(|k| k.processes.get(&pid).is_some_and(|p| p.suspended && p.exit.is_none())) {
                crate::sched::sleep_us(1000);
            }
            let ended = run_process(&shared, &path, &command_line, &directory);
            if let Ended::Failed(why) = &ended {
                log!("OTW kernel: process {pid:#x} failed: {why}");
            }
            finish_process(pid, &ended);
        });
        Ok(pid)
    }

    fn exit_code(&self, pid: u32) -> Option<u32> {
        kobj::exit_code(pid)
    }
}

/// Load the program at `path` into a process whose Win32 state is
/// `shared`, and run it to the end.
fn run_process(shared: &Rc<RefCell<SharedState>>, path: &str, command_line: &str, directory: &[u8]) -> Ended {
    let started = apic::now_us();
    let vm = match Vm::from_drive(path.as_bytes(), command_line.as_bytes(), shared.clone()) {
        Ok(vm) => vm,
        Err(error) => return Ended::Failed(format!("cannot load {path}: {error}")),
    };
    if !directory.is_empty() {
        shared.borrow_mut().current_dir = directory.to_vec();
    }
    let image = vm.process_image();
    let main = image.main();
    let x64 = main.is64;
    let runner = Runner {
        state: shared.clone(),
        space: AddressSpace::new(),
        x64,
        entries: vm.iat().entries.clone(),
        name: path.rsplit('\\').next().unwrap_or(path).into(),
        tags: RefCell::new(alloc::vec![0; vm.iat().entries.len()]),
        syscalls: Rc::new(vm.api_context().clone()),
        modules: vm.api_context().modules.clone(),
        dispatcher: image.entry_point(KERNEL_DLL, EXCEPTION_DISPATCHER).unwrap_or(0) as u64,
        thread_return: image.entry_point(KERNEL_DLL, THREAD_RETURN).unwrap_or(0) as u64,
        reserved: RefCell::new(Vec::new()),
        ended: RefCell::new(None),
        api_calls: Cell::new(0),
        counts: RefCell::new(alloc::collections::BTreeMap::new()),
        last_call: PerTask::new(),
        trace_status: otw_vm::platform::env::var("OTW_TRACE_STATUS").is_ok_and(|v| v != "0"),
        resume: PerTask::new(),
        resume_full: PerTask::new(),
        levels: PerTask::new(),
        running: RefCell::new(Vec::new()),
        waiting: RefCell::new(Vec::new()),
        next_thread: Cell::new(2),
        signal: PerTask::new(),
        lock: otw_vm::kernel::lock::BigLock::new(),
        parallel: x64 && apic::cpu_count() > 1 && otw_vm::platform::env::var("OTW_PARALLEL_THREADS").map_or(true, |v| v != "0"),
        workers: RefCell::new(alloc::collections::BTreeMap::new()),
        tls: RefCell::new(Vec::new()),
        recent: RefCell::new(alloc::collections::VecDeque::new()),
        unmapped: RefCell::new(alloc::collections::VecDeque::new()),
        shared: RefCell::new(alloc::collections::BTreeMap::new()),
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
    // Small C library functions the subsystem has machine code for (64-bit)
    // run as that code, after the thunks: called millions of times, they
    // are far faster without a system call.
    if x64 {
        let thunk_end = (THUNK_BASE as u64 + thunks * THUNK_STRIDE as u64 + PAGE - 1) & !(PAGE - 1);
        let mut leaves: Vec<u8> = Vec::new();
        let mut placed: Vec<(&'static [u8], u64)> = Vec::new();
        let mut bound = 0;
        for entry in runner.entries.iter().filter(|e| e.slot_va != 0) {
            let Some(native) = runner.syscalls.native_x64(&entry.dll, &entry.name) else { continue };
            // Code with a slow path is placed once for each import (it
            // jumps to that import's thunk).
            let shared = (!native.slow).then(|| placed.iter().find(|(code, _)| core::ptr::eq(*code, native.code))).flatten();
            let at = match shared {
                Some(&(_, at)) => at,
                None => {
                    let at = thunk_end + leaves.len() as u64;
                    leaves.extend_from_slice(&otw_vm::kernel::syscall::placed(native, at, entry.thunk_va as u64));
                    leaves.resize((leaves.len() + 15) & !15, 0xCC);
                    if !native.slow {
                        placed.push((native.code, at));
                    }
                    at
                }
            };
            runner.write(entry.slot_va as u64, &at.to_le_bytes());
            bound += 1;
        }
        if !leaves.is_empty() {
            runner.reserve(thunk_end, leaves.len() as u64);
            runner.write(thunk_end, &leaves);
            log!("OTW kernel: {bound} imports run as machine code");
        }
    }
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
        runner.write(memory::NTDLL_HANDLE as u64, &memory::ntdll_headers());
    }
    // The first thread's information block (FS in 32-bit code, GS in
    // 64-bit), and its TLS.
    let tib = TIB_BASE as u64;
    runner.reserve(tib, if x64 { TEB64_SIZE } else { TIB_SIZE });
    if x64 {
        runner.init_teb64(tib, STACK_TOP as u64, X64_STACK_LOW, 1);
        let peb = tib + 0x2000;
        runner.write(peb + 0x20, &(peb + 0x400).to_le_bytes());
        // The clock the program's own QueryPerformanceCounter reads
        // (native.rs): microseconds since the process's start, as the
        // subsystem counts them.
        let started = vm.api_context().shared.borrow().started.micros();
        let clock = peb + otw_vm::win32::native::PEB_CLOCK as u64;
        for (offset, value) in [(0u64, apic::boot_tsc()), (8, apic::tsc_mhz()), (16, started)] {
            runner.write(clock + offset, &value.to_le_bytes());
        }
    } else {
        for (offset, value) in [(0u64, u32::MAX), (4, STACK_TOP), (8, STACK_BASE), (0x18, TIB_BASE), (0x30, TIB_BASE + 0x800)] {
            runner.write(tib + offset, &value.to_le_bytes());
        }
    }
    *runner.tls.borrow_mut() = tls_templates(image, &runner);
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

    fn state_in_use(state: usize) -> bool {
        unsafe { &*(state as *const RefCell<SharedState>) }.try_borrow_mut().is_err()
    }
    runner.lock.set_in_use_check(state_in_use, Rc::as_ptr(&runner.state) as usize);
    RUNNERS.lock().unwrap().push(&runner as *const Runner as usize);
    let on_desktop = crate::desktop::program_started(shared, &runner.lock, path.rsplit('\\').next().unwrap_or(path));
    // In the subsystem (until program code runs).
    let hold = otw_vm::kernel::lock::Hold::new(&runner.lock);
    // Its memory, its thread block.
    runner.space.activate();
    runner.running.borrow_mut().push((1, tib));
    runner.set_thread_block(tib);
    let ran = apic::now_us();
    // The program's own DLLs initialise first: DllMain(module,
    // DLL_PROCESS_ATTACH, 0). Win95's system DLLs' entry points are the
    // loader's private hooks, not DllMains: skipped.
    const SYSTEM_DLLS: &[&str] = &["kernel32.dll", "user32.dll", "gdi32.dll", "advapi32.dll", "comctl32.dll", "shell32.dll", "comdlg32.dll"];
    let caller = UserRegs::new(x64, 0, (STACK_TOP as u64 - 0x100) & !0xF);
    for module in image.modules.iter().skip(1) {
        if module.image.entry_rva == 0 || SYSTEM_DLLS.iter().any(|n| module.name.eq_ignore_ascii_case(n)) {
            continue;
        }
        let base = module.image.image_base as u64;
        let result = runner.call(&caller, module.image.entry_va() as u64, &[base, 1, 0], None, None);
        if runner.has_ended() {
            break;
        }
        if result.is_some_and(|r| r as u32 == 0) {
            log!("OTW kernel: {}'s DllMain reported failure", module.name);
        }
    }
    let stop = runner.execute(&mut regs);
    if let Stop::Returned(_) | Stop::Suspended | Stop::ThreadExited(_) = stop {
        runner.end(Ended::Exited(0));
    }
    // Its other threads stop before its memory goes.
    let waited = apic::now_us();
    while !runner.workers.borrow().is_empty() {
        runner.kick_workers();
        otw_vm::kernel::lock::unlocked(|| crate::sched::sleep_us(1000));
        if apic::now_us() - waited > 2_000_000 && (apic::now_us() - waited) % 1_000_000 < 1000 {
            log!("OTW kernel: waiting for {} threads to stop", runner.workers.borrow().len());
        }
    }
    drop(hold);
    RUNNERS.lock().unwrap().retain(|&r| r != &runner as *const Runner as usize);
    crate::desktop::program_ended(on_desktop);
    // Back to the kernel's tables before the program's go.
    let task = crate::sched::current();
    task.cr3 = 0;
    task.fs_base = 0;
    task.gs_base = 0;
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
    if !matches!(ended, Ended::Exited(0)) {
        log!("OTW kernel: its last calls: {}", runner.recent_calls(40).join(", "));
    }
    otw_vm::win32::registry::flush();
    ended
}
