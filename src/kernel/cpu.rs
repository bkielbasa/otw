//! x86 CPU backend backed by Unicorn.
//!
//! The loader maps the PE into flat guest memory and patches every IAT slot
//! with a synthetic thunk. Each thunk is executable guest memory containing a
//! `ret`, but a code hook intercepts it before the instruction runs. The hook
//! reads `stdcall` arguments from the guest stack, invokes the Rust API layer,
//! writes EAX, pops the return address and arguments, and resumes at the
//! original return address.

use std::{cell::RefCell, collections::VecDeque, fmt, rc::Rc};

use unicorn_engine::{
    uc_error,
    unicorn_const::{Arch, HookType, MemType, Mode, Prot, SECOND_SCALE},
    RegisterX86 as Reg, Unicorn,
};

use super::{
    iat::THUNK_BASE,
    image::{Image, EXCEPTION_DISPATCHER, KERNEL_DLL, THREAD_RETURN},
    memory::{self, GuestMemory, ThreadRun, HEAP_BASE, HEAP_SIZE},
    syscall::SystemCalls,
};

pub const STACK_BASE: u32 = 0x0010_0000;
pub const STACK_SIZE: u32 = 0x0010_0000;
pub const STACK_TOP: u32 = STACK_BASE + STACK_SIZE - 0x100;
/// The bottom of a 64-bit program's first stack.
const X64_STACK_LOW: u32 = 0x0001_0000;

/// Synthetic Win95 pages sit above genuine high-based DLLs. OLE32 loads at
/// `0x7ff60000` with a `0x86000` image, ending at `0x7ffe6000`, so the TIB,
/// loader state and GDT must stay above that range.
///
/// Minimal Win95-style thread information block. MSVC CRT startup code reads
/// and writes `FS:[0]` to install its structured exception handler chain.
pub const TIB_BASE: u32 = 0x7FFF_0000;
pub const TIB_SIZE: u32 = 0x1000;
/// A 64-bit thread's TEB (0x1838 bytes) and its process's PEB after it.
pub const TEB64_SIZE: u32 = 0x3000;

/// Minimal global descriptor table. Selector `0x08` points at the TIB page.
const GDT_BASE: u32 = 0x7FFE_C000;
const GDT_SIZE: u32 = 0x1000;
const CODE_SELECTOR: u32 = 0x08;
const DATA_SELECTOR: u32 = 0x10;
const FS_SELECTOR: u32 = 0x18;
const GS_SELECTOR: u32 = 0x20;

/// KERNEL32's `cs: call far [bffbc004]` VMM gates. Unicorn cannot transfer into
/// a real Windows 95 VxD, so these are emulated as synthetic VxD services.
const VXD_GATES: [u32; 5] = [
    0xbff7_10c3,
    0xbff7_13db,
    0xbff7_13e7,
    0xbff7_13f3,
    0xbff9_931f,
];

const PAGE_SIZE: u32 = 0x1000;
/// A single `ret` instruction used as the return address for synthetic
/// DllMain calls. Unicorn stops before executing it.
pub const SENTINEL_BASE: u32 = 0x6FFF_0000;
/// Top-level emulation runs until EIP reaches this address; a program that
/// yields jumps here, which ends the run like the sentinel ends callbacks.
const YIELD_ADDRESS: u32 = SENTINEL_BASE + 0x100;
/// A thread given the CPU by another runs until EIP reaches this address:
/// it waits again or has ended.
const THREAD_STOP: u32 = SENTINEL_BASE + 0x200;
/// Synthetic Win95 loader state. Real KERNEL32 receives these pointers in a
/// private initialisation block from the Win95 loader; the PE entry point is
/// not a normal DllMain. Providing the same globals lets genuine KERNEL32 code
/// (called through genuine DLL IATs) run without inventing substitute APIs.
const WIN95_STATE_BASE: u32 = 0x7FFE_7000;
const WIN95_STATE_SIZE: u32 = 0x4000;
/// KERNEL32's private 16-bit thunk allocator keeps its free list at FS:[0x10].
/// Each free block is eight bytes: the first dword links to the next block and
/// both dwords are overwritten while the block is allocated.
const LOCAL_HANDLE_LIST_BASE: u32 = WIN95_STATE_BASE + 0x1000;
const LOCAL_HANDLE_COUNT: usize = 512;
/// Zero means "no limit": an interactive guest stays in its message loop until
/// it calls ExitProcess (or a host event stops emulation).
const MAX_INSTRUCTIONS: usize = 0;
/// Zero means "no timeout" for the same reason.
const TIMEOUT_SECONDS: u64 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunResult {
    pub exit_code: u32,
    pub api_calls: usize,
}

#[derive(Debug)]
pub enum RunError {
    Unicorn(&'static str, uc_error),
    Guest(String),
    NoExit,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Unicorn(operation, error) => {
                write!(f, "x86 backend failed while {operation}: {error:?}")
            }
            RunError::Guest(message) => write!(f, "guest execution error: {message}"),
            RunError::NoExit => write!(f, "guest stopped without calling ExitProcess"),
        }
    }
}

impl std::error::Error for RunError {}

#[derive(Default)]
struct RunState<'a> {
    exit_code: Option<u32>,
    api_calls: usize,
    error: Option<String>,
    /// Called by message APIs so the host window system can pump its event
    /// queue into the guest message queue. The flag allows a short wait.
    pump: Option<Box<dyn FnMut(bool) + 'a>>,
    /// Depth of synchronous Rust-to-guest callbacks currently running.
    callback_depth: usize,
    /// ESP of the code that started each nested callback level, innermost
    /// last. Code running at a level has its stack below that value.
    levels: Vec<u32>,
    /// A pending exception-handling transfer of control.
    resume: Option<memory::Resume>,
    /// All registers to set with `resume` (64-bit exception handling).
    resume_full: Option<memory::FullRegisters>,
    /// A CPU fault that stopped emulation and has yet to be raised as an
    /// exception in the guest.
    fault: Option<Fault>,
    /// Guest address of the Rust-backed user-mode exception dispatcher.
    dispatcher: u32,
    /// The process may give up the CPU while it waits for messages
    /// (several programs share the desktop).
    can_yield: bool,
    /// A message wait asked to yield; the thunk hook rewinds the call.
    yielding: bool,
    /// Emulation stopped at a message wait and resumes there.
    yielded: bool,
    /// The API thunk to call again when a yielded program resumes.
    yield_eip: u32,
    /// Threads that are off the CPU.
    threads: Vec<ThreadSlot>,
    /// Threads on the CPU, the first thread first: each runs nested inside
    /// a wait of the one before it.
    running: Vec<u32>,
    /// Each thread's TIB.
    tibs: Vec<(u32, u32)>,
    next_thread: u32,
    /// The running thread is to come off the CPU after its current call.
    thread_signal: Option<ThreadSignal>,
    /// The thunk start routines return to.
    thread_return: u32,
    /// A 64-bit process: the x64 calling convention, GS-based TEBs.
    x64: bool,
    /// Static thread-local storage templates (`__declspec(thread)`), one
    /// per module with a TLS directory, in index order.
    tls_templates: Vec<Vec<u8>>,
}

/// A thread off the CPU.
struct ThreadSlot {
    id: u32,
    context: unicorn_engine::Context,
    /// Its callback levels (empty: it stopped at its own top level).
    levels: Vec<u32>,
    callback_depth: usize,
    /// The API thunk to call again when it next runs.
    retry_eip: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
enum ThreadSignal {
    Suspend,
    Exit(u32),
}

/// The FS descriptor: a data segment based at a TIB.
fn fs_descriptor(base: u32) -> [u8; 8] {
    let limit = TIB_SIZE - 1;
    [
        limit as u8,
        (limit >> 8) as u8,
        base as u8,
        (base >> 8) as u8,
        (base >> 16) as u8,
        0x92, // present, DPL 0, data segment, read/write
        0x40, // 32-bit segment, byte granularity
        (base >> 24) as u8,
    ]
}

/// Point FS at another TIB.
fn set_fs_base(uc: &mut Unicorn<'_, ()>, base: u32) -> Result<(), uc_error> {
    uc.mem_write((GDT_BASE + FS_SELECTOR) as u64, &fs_descriptor(base))?;
    uc.reg_write(Reg::FS, FS_SELECTOR as u64)
}

/// A hardware fault to deliver as a Win32 exception.
#[derive(Clone, Copy, Debug)]
struct Fault {
    code: u32,
    /// The faulting instruction.
    eip: u32,
    /// ExceptionInformation: for access violations, 0 = read / 1 = write
    /// and the inaccessible address.
    params: [u32; 2],
    param_count: u32,
}

impl Fault {
    fn describe(&self) -> String {
        match self.code {
            crate::kernel::eh::STATUS_ACCESS_VIOLATION => format!(
                "access violation {} {:#010x} at {:#010x}",
                if self.params[0] == 1 {
                    "writing"
                } else {
                    "reading"
                },
                self.params[1],
                self.eip
            ),
            crate::kernel::eh::STATUS_INTEGER_DIVIDE_BY_ZERO => {
                format!("integer divide by zero at {:#010x}", self.eip)
            }
            code => format!("exception {code:#010x} at {:#010x}", self.eip),
        }
    }
}

/// Raise the pending fault in the guest, as Windows does: push an
/// A 64-bit thread environment block at `teb`, made current through the GS
/// base: stack bounds, the self pointer, the client id and an (empty) PEB.
fn init_teb64(uc: &mut Unicorn<'_, ()>, teb: u32, top: u32, bottom: u32, thread: u32) -> Result<(), RunError> {
    let peb = teb + 0x2000;
    for (offset, value) in [
        (0x00u32, u64::MAX),       // ExceptionList (unused on x64)
        (0x08, top as u64),        // StackBase
        (0x10, bottom as u64),     // StackLimit
        (0x30, teb as u64),        // Self
        (0x40, 1),                 // ClientId.UniqueProcess
        (0x48, thread as u64),     // ClientId.UniqueThread
        (0x60, peb as u64),        // ProcessEnvironmentBlock
        // PEB.ProcessParameters: an all-zero RTL_USER_PROCESS_PARAMETERS in
        // the PEB's unused tail (runtime libraries read its flags).
        (0x2000 + 0x20, (peb + 0x400) as u64),
    ] {
        uc.mem_write((teb + offset) as u64, &value.to_le_bytes())
            .map_err(|error| RunError::Unicorn("initialising the TEB", error))?;
    }
    uc.reg_write(Reg::GS_BASE, teb as u64)
        .map_err(|error| RunError::Unicorn("setting the GS base", error))
}

/// The static TLS template of each module with a TLS directory, and its
/// index written to the module's index variable.
fn tls_templates(process: &Image, uc: &mut Unicorn<'_, ()>) -> Vec<Vec<u8>> {
    let mut templates = Vec::new();
    for module in &process.modules {
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
        // IMAGE_TLS_DIRECTORY: start, end, index address, callbacks (VAs,
        // pointer-sized), zero fill.
        let width = if x64 { 8 } else { 4 };
        let field = |index: usize| read32(at + index * width).unwrap_or(0);
        let (start, end, index_va) = (field(0), field(1), field(2));
        let zero_fill = read32(at + 4 * width).unwrap_or(0);
        let base = module.image.image_base;
        let mut template = memory
            .get((start.wrapping_sub(base)) as usize..(end.wrapping_sub(base)) as usize)
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        template.extend(std::iter::repeat_n(0, zero_fill as usize));
        let _ = uc.mem_write(index_va as u64, &(templates.len() as u32).to_le_bytes());
        templates.push(template);
    }
    templates
}

/// Give a thread its copies of the TLS templates: the TEB's
/// ThreadLocalStoragePointer (x64 TEB+0x58, x86 TIB+0x2C) points to an
/// array of one block per module.
fn install_tls(uc: &mut Unicorn<'_, ()>, templates: &[Vec<u8>], teb: u32, x64: bool) -> Result<(), uc_error> {
    if templates.is_empty() {
        return Ok(());
    }
    let width = if x64 { 8 } else { 4 };
    let array_size = (templates.len() as u32 * width + 15) & !15;
    let total = array_size + templates.iter().map(|t| (t.len() as u32 + 15) & !15).sum::<u32>();
    let mut memory = UnicornGuestMemory { uc, run: None };
    let block = memory.map_region(0x1000_0000, total.max(1)).ok_or(uc_error::NOMEM)?;
    let mut cursor = block + array_size;
    for (index, template) in templates.iter().enumerate() {
        uc.mem_write(cursor as u64, template)?;
        uc.mem_write((block + index as u32 * width) as u64, &(cursor as u64).to_le_bytes()[..width as usize])?;
        cursor += (template.len() as u32 + 15) & !15;
    }
    let slot = if x64 { teb + 0x58 } else { teb + 0x2C };
    uc.mem_write(slot as u64, &(block as u64).to_le_bytes()[..width as usize])
}

/// CPUID as the emulated processor answers it, without SSE4.2. The
/// emulator's PCMPESTRI gets "equal ordered" searches wrong, and runtime
/// libraries (the C++ library's string search among them) use it only when
/// CPUID reports SSE4.2. Answers come from a scratch engine of the same
/// model, cached.
fn hide_sse42(uc: &mut Unicorn<'_, ()>) -> Result<(), uc_error> {
    use std::{collections::HashMap, ffi::c_void};
    use unicorn_engine::RegisterX86;

    type Engine = *mut c_void;
    unsafe extern "C" {
        fn uc_hook_add(uc: Engine, hook: *mut usize, kind: i32, callback: *mut c_void, data: *mut c_void, begin: u64, end: u64, ...) -> i32;
        fn uc_reg_read(uc: Engine, reg: i32, value: *mut c_void) -> i32;
        fn uc_reg_write(uc: Engine, reg: i32, value: *const c_void) -> i32;
    }
    thread_local! {
        static ANSWERS: RefCell<HashMap<(u32, u32), [u32; 4]>> = RefCell::new(HashMap::new());
    }

    fn native(leaf: u32, subleaf: u32) -> [u32; 4] {
        let run = || -> Result<[u32; 4], uc_error> {
            let mut scratch = Unicorn::new(Arch::X86, Mode::MODE_64)?;
            scratch.mem_map(0x1000, 0x1000, unicorn_engine::Prot::ALL)?;
            scratch.mem_write(0x1000, &[0x0F, 0xA2])?;
            scratch.reg_write(RegisterX86::RAX, leaf as u64)?;
            scratch.reg_write(RegisterX86::RCX, subleaf as u64)?;
            scratch.emu_start(0x1000, 0x1002, 0, 1)?;
            Ok([RegisterX86::RAX, RegisterX86::RBX, RegisterX86::RCX, RegisterX86::RDX]
                .map(|reg| scratch.reg_read(reg).unwrap_or(0) as u32))
        };
        run().unwrap_or([0; 4])
    }

    extern "C" fn on_cpuid(uc: Engine, _data: *mut c_void) -> i32 {
        let read = |reg: RegisterX86| {
            let mut value = 0u64;
            unsafe { uc_reg_read(uc, reg as i32, &mut value as *mut u64 as *mut c_void) };
            value as u32
        };
        let (leaf, subleaf) = (read(RegisterX86::EAX), read(RegisterX86::ECX));
        let mut answer = ANSWERS.with(|answers| *answers.borrow_mut().entry((leaf, subleaf)).or_insert_with(|| native(leaf, subleaf)));
        if leaf == 1 {
            answer[2] &= !(1 << 20); // SSE4.2
        }
        for (reg, value) in [RegisterX86::RAX, RegisterX86::RBX, RegisterX86::RCX, RegisterX86::RDX].into_iter().zip(answer) {
            let value = value as u64;
            unsafe { uc_reg_write(uc, reg as i32, &value as *const u64 as *const c_void) };
        }
        1 // the hook answered; skip the instruction
    }

    const UC_HOOK_INSN: i32 = 1 << 1;
    const UC_X86_INS_CPUID: i32 = 113;
    let mut hook = 0usize;
    let status = unsafe {
        uc_hook_add(uc.get_handle() as Engine, &mut hook, UC_HOOK_INSN, on_cpuid as *mut c_void, std::ptr::null_mut(), 1, 0, UC_X86_INS_CPUID)
    };
    if status == 0 { Ok(()) } else { Err(uc_error::HOOK) }
}

/// The running thread's TIB.
fn current_tib(state: &RunState<'_>) -> u32 {
    let id = state.running.last().copied().unwrap_or(1);
    state.tibs.iter().find(|(t, _)| *t == id).map(|(_, b)| *b).unwrap_or(TIB_BASE)
}

/// EXCEPTION_RECORD and CONTEXT for the faulting instruction onto the
/// thread's stack and continue in the exception dispatcher, which walks the
/// SEH chain. Returns false when there is nothing to deliver.
fn deliver_fault(uc: &mut Unicorn<'_, ()>, state: &Rc<RefCell<RunState<'_>>>) -> bool {
    use crate::kernel::eh::*;
    let (fault, dispatcher) = {
        let mut run = state.borrow_mut();
        if run.error.is_some() || run.exit_code.is_some() {
            return false;
        }
        match run.fault.take() {
            Some(fault) => (fault, run.dispatcher),
            None => return false,
        }
    };
    eprintln!("[eh] {}", fault.describe());
    if state.borrow().x64 {
        // x64: the record and context below the faulting frame; the
        // dispatcher gets them in RCX and RDX.
        use crate::kernel::context64;
        let mut full = read_full(uc);
        full.rip = fault.eip as u64;
        let rsp = full.gpr[4] as u32;
        let base = rsp.wrapping_sub(0x200 + context64::RECORD_SIZE + context64::CONTEXT_SIZE) & !0xF;
        let record = base + 0x30;
        let context = (record + context64::RECORD_SIZE + 15) & !15;
        let params: Vec<u64> = fault.params.iter().take(fault.param_count as usize).map(|&p| p as u64).collect();
        let delivered = uc
            .mem_write(record as u64, &context64::record(fault.code, 0, fault.eip as u64, &params))
            .and_then(|()| uc.mem_write(context as u64, &context64::encode(&full)))
            .and_then(|()| uc.mem_write(base as u64, &(fault.eip as u64).to_le_bytes()))
            .and_then(|()| uc.reg_write(Reg::RSP, base as u64))
            .and_then(|()| uc.reg_write(Reg::RCX, record as u64))
            .and_then(|()| uc.reg_write(Reg::RDX, context as u64))
            .and_then(|()| uc.reg_write(Reg::RIP, dispatcher as u64));
        if dispatcher == 0 || delivered.is_err() {
            state.borrow_mut().error = Some(format!("{} (cannot raise it)", fault.describe()));
            return false;
        }
        return true;
    }
    let read = |uc: &Unicorn<'_, ()>, register| uc.reg_read(register).unwrap_or(0) as u32;
    let esp = read(uc, Reg::ESP);
    if std::env::var_os("OTW_TRACE_FAULTS").is_some() {
        let mut words = [0u8; 32];
        let _ = uc.mem_read(esp as u64, &mut words);
        let stack: Vec<String> = words.chunks(4).map(|w| format!("{:08x}", u32::from_le_bytes([w[0], w[1], w[2], w[3]]))).collect();
        eprintln!(
            "[eh] eax={:08x} ebx={:08x} ecx={:08x} edx={:08x} esi={:08x} edi={:08x} ebp={:08x} esp={esp:08x} stack {}",
            read(uc, Reg::EAX), read(uc, Reg::EBX), read(uc, Reg::ECX), read(uc, Reg::EDX),
            read(uc, Reg::ESI), read(uc, Reg::EDI), read(uc, Reg::EBP), stack.join(" ")
        );
    }
    // Below the faulting frame: return address, &record, &context, record, context.
    let base = esp.wrapping_sub(0x40 + 12 + RECORD_SIZE + CONTEXT_SIZE) & !0xF;
    // The faulting thread's own stack (its TIB holds the bounds).
    let (stack_base, stack_top) = {
        let tib = current_tib(&state.borrow());
        let mut bounds = [0u8; 8];
        let _ = uc.mem_read(tib as u64 + 4, &mut bounds);
        let top = u32::from_le_bytes([bounds[0], bounds[1], bounds[2], bounds[3]]);
        let bottom = u32::from_le_bytes([bounds[4], bounds[5], bounds[6], bounds[7]]);
        (bottom, top)
    };
    if dispatcher == 0 || !(stack_base + 0x1000..stack_top).contains(&base) {
        state.borrow_mut().error = Some(format!(
            "{} (cannot raise it: stack overflow)",
            fault.describe()
        ));
        return false;
    }
    let (record, context) = (base + 12, base + 12 + RECORD_SIZE);
    let mut frame = vec![0u8; (12 + RECORD_SIZE + CONTEXT_SIZE) as usize];
    let mut put = |offset: u32, value: u32| {
        frame[offset as usize..offset as usize + 4].copy_from_slice(&value.to_le_bytes());
    };
    put(0, fault.eip);
    put(4, record);
    put(8, context);
    put(12, fault.code);
    put(12 + 12, fault.eip);
    put(12 + 16, fault.param_count);
    put(12 + 20, fault.params[0]);
    put(12 + 24, fault.params[1]);
    let c = 12 + RECORD_SIZE;
    put(c, 0x0001_0007); // CONTEXT_FULL
    for (offset, register) in [
        (CTX_EDI, Reg::EDI),
        (CTX_ESI, Reg::ESI),
        (CTX_EBX, Reg::EBX),
        (CTX_EDX, Reg::EDX),
        (CTX_ECX, Reg::ECX),
        (CTX_EAX, Reg::EAX),
        (CTX_EBP, Reg::EBP),
        (CTX_EFLAGS, Reg::EFLAGS),
        (CTX_ESP, Reg::ESP),
    ] {
        put(c + offset, read(uc, register));
    }
    put(c + CTX_EIP, fault.eip);
    put(c + 0xBC, CODE_SELECTOR);
    put(c + 0xC8, DATA_SELECTOR);
    let delivered = uc
        .mem_write(base as u64, &frame)
        .and_then(|()| uc.reg_write(Reg::ESP, base as u64))
        .and_then(|()| uc.reg_write(Reg::EIP, dispatcher as u64));
    if delivered.is_err() {
        state.borrow_mut().error = Some(format!("{} (cannot raise it)", fault.describe()));
        return false;
    }
    true
}

/// Run one emulation level, raising CPU faults as guest exceptions and
/// continuing in the dispatcher until the level ends.
fn emulate(
    uc: &mut Unicorn<'_, ()>,
    state: &Rc<RefCell<RunState<'_>>>,
    begin: u32,
    until: u32,
    timeout: u64,
    count: usize,
) -> Result<(), uc_error> {
    let mut result = uc.emu_start(begin as u64, until as u64, timeout, count);
    while result.is_err() && deliver_fault(uc, state) {
        let eip = uc.reg_read(Reg::EIP).unwrap_or(0);
        result = uc.emu_start(eip, until as u64, timeout, count);
    }
    if result.is_err() {
        let mut run = state.borrow_mut();
        if let Some(fault) = run.fault.take() {
            if run.error.is_none() && run.exit_code.is_none() {
                run.error = Some(format!("invalid memory access: {}", fault.describe()));
            }
        }
    }
    result
}

/// Write a resume target into the CPU registers.
const FULL_GPRS: [Reg; 16] = [
    Reg::RAX, Reg::RCX, Reg::RDX, Reg::RBX, Reg::RSP, Reg::RBP, Reg::RSI, Reg::RDI,
    Reg::R8, Reg::R9, Reg::R10, Reg::R11, Reg::R12, Reg::R13, Reg::R14, Reg::R15,
];
const XMMS: [Reg; 16] = [
    Reg::XMM0, Reg::XMM1, Reg::XMM2, Reg::XMM3, Reg::XMM4, Reg::XMM5, Reg::XMM6, Reg::XMM7,
    Reg::XMM8, Reg::XMM9, Reg::XMM10, Reg::XMM11, Reg::XMM12, Reg::XMM13, Reg::XMM14, Reg::XMM15,
];

fn read_full(uc: &Unicorn<'_, ()>) -> memory::FullRegisters {
    let mut full = memory::FullRegisters::default();
    for (slot, register) in FULL_GPRS.iter().enumerate() {
        full.gpr[slot] = uc.reg_read(*register).unwrap_or(0);
    }
    full.rip = uc.reg_read(Reg::RIP).unwrap_or(0);
    full.eflags = uc.reg_read(Reg::EFLAGS).unwrap_or(0x202);
    full.mxcsr = uc.reg_read(Reg::MXCSR).unwrap_or(0x1F80) as u32;
    for (slot, register) in XMMS.iter().enumerate() {
        if let Ok(bytes) = uc.reg_read_long(*register) {
            let n = bytes.len().min(16);
            full.xmm[slot][..n].copy_from_slice(&bytes[..n]);
        }
    }
    full
}

fn write_full(uc: &mut Unicorn<'_, ()>, full: &memory::FullRegisters) -> Result<(), uc_error> {
    for (slot, register) in FULL_GPRS.iter().enumerate() {
        uc.reg_write(*register, full.gpr[slot])?;
    }
    uc.reg_write(Reg::EFLAGS, full.eflags | 0x2)?;
    let _ = uc.reg_write(Reg::MXCSR, full.mxcsr as u64);
    for (slot, register) in XMMS.iter().enumerate() {
        uc.reg_write_long(*register, &full.xmm[slot])?;
    }
    uc.reg_write(Reg::RIP, full.rip)
}

fn apply_resume_full(uc: &mut Unicorn<'_, ()>, state: &Rc<RefCell<RunState<'_>>>, resume: &memory::Resume) -> Result<(), uc_error> {
    let full = state.borrow_mut().resume_full.take();
    match full {
        Some(full) => write_full(uc, &full),
        None => apply_resume(uc, resume),
    }
}

fn apply_resume(uc: &mut Unicorn<'_, ()>, resume: &memory::Resume) -> Result<(), uc_error> {
    if let Some([eax, ebx, ecx, edx, esi, edi]) = resume.regs {
        for (register, value) in [
            (Reg::EAX, eax),
            (Reg::EBX, ebx),
            (Reg::ECX, ecx),
            (Reg::EDX, edx),
            (Reg::ESI, esi),
            (Reg::EDI, edi),
        ] {
            uc.reg_write(register, value as u64)?;
        }
    }
    uc.reg_write(Reg::ESP, resume.esp as u64)?;
    uc.reg_write(Reg::EBP, resume.ebp as u64)?;
    uc.reg_write(Reg::EIP, resume.eip as u64)
}

/// Whether a resume target lies in the stack region of the innermost
/// running level (the main program when no callback is active).
fn resume_belongs_here(state: &RunState<'_>, resume: &memory::Resume) -> bool {
    match state.levels.last() {
        Some(boundary) => resume.esp < *boundary,
        None => true,
    }
}

struct UnicornGuestMemory<'u, 'uc, 's, 'a> {
    uc: &'u mut Unicorn<'uc, ()>,
    run: Option<&'s Rc<RefCell<RunState<'a>>>>,
}

/// Registers preserved around a synchronous guest callback.
const SAVED_REGISTERS: [Reg; 10] = [
    Reg::EAX,
    Reg::EBX,
    Reg::ECX,
    Reg::EDX,
    Reg::ESI,
    Reg::EDI,
    Reg::EBP,
    Reg::ESP,
    Reg::EIP,
    Reg::EFLAGS,
];

/// Registers preserved around a synchronous guest callback in a 64-bit
/// process.
const SAVED_REGISTERS_64: [Reg; 18] = [
    Reg::RAX,
    Reg::RBX,
    Reg::RCX,
    Reg::RDX,
    Reg::RSI,
    Reg::RDI,
    Reg::RBP,
    Reg::RSP,
    Reg::RIP,
    Reg::EFLAGS,
    Reg::R8,
    Reg::R9,
    Reg::R10,
    Reg::R11,
    Reg::R12,
    Reg::R13,
    Reg::R14,
    Reg::R15,
];

/// Deepest Rust-to-guest callback nesting. Unicorn itself allows 64 levels.
const MAX_CALLBACK_DEPTH: usize = 48;

impl UnicornGuestMemory<'_, '_, '_, '_> {
    /// Run guest code synchronously: arguments as the process's calling
    /// convention passes them, a sentinel return address, every register
    /// restored afterwards. Returns RAX (EAX in a 32-bit process).
    fn call_guest_full(&mut self, func: u32, args: &[u64], ebp: Option<u32>, ecx: Option<u32>) -> Option<u64> {
        let run = self.run?;

        if func == 0 {
            return None;
        }
        {
            let state = run.borrow();
            if state.exit_code.is_some()
                || state.error.is_some()
                || state.resume.is_some()
                || state.callback_depth >= MAX_CALLBACK_DEPTH
            {
                return None;
            }
        }
        let x64 = run.borrow().x64;
        let registers: &[Reg] = if x64 { &SAVED_REGISTERS_64 } else { &SAVED_REGISTERS };
        let mut saved = vec![0u64; registers.len()];
        for (slot, register) in registers.iter().enumerate() {
            saved[slot] = self.uc.reg_read(*register).ok()?;
        }
        let esp = saved[7] as u32;
        if x64 {
            // The x64 convention: RCX, RDX, R8, R9, then the stack above a
            // 32-byte home area; RSP is 8 off 16-byte alignment at entry.
            let slots = args.len().max(4) as u32;
            let frame = ((esp.wrapping_sub(0x80 + 8 * (slots + 1))) & !0xF).wrapping_sub(8);
            let mut bytes = vec![0u8; 8 * (slots as usize + 1)];
            bytes[..8].copy_from_slice(&(SENTINEL_BASE as u64).to_le_bytes());
            for (index, arg) in args.iter().enumerate().skip(4) {
                bytes[8 + index * 8..16 + index * 8].copy_from_slice(&arg.to_le_bytes());
            }
            self.uc.mem_write(frame as u64, &bytes).ok()?;
            self.uc.reg_write(Reg::RSP, frame as u64).ok()?;
            for (register, arg) in [Reg::RCX, Reg::RDX, Reg::R8, Reg::R9].iter().zip(args) {
                self.uc.reg_write(*register, *arg).ok()?;
            }
        } else {
            // Leave a gap so the interrupted frame's locals are never touched.
            let frame = (esp.wrapping_sub(0x80 + 4 * (args.len() as u32 + 1))) & !0xF;
            let mut bytes = Vec::with_capacity(4 * (args.len() + 1));
            bytes.extend_from_slice(&SENTINEL_BASE.to_le_bytes());
            for arg in args {
                bytes.extend_from_slice(&(*arg as u32).to_le_bytes());
            }
            self.uc.mem_write(frame as u64, &bytes).ok()?;
            self.uc.reg_write(Reg::ESP, frame as u64).ok()?;
        }
        if let Some(ebp) = ebp {
            self.uc.reg_write(Reg::EBP, ebp as u64).ok()?;
        }
        if let Some(ecx) = ecx {
            self.uc.reg_write(Reg::ECX, ecx as u64).ok()?;
        }
        {
            let mut state = run.borrow_mut();
            state.callback_depth += 1;
            state.levels.push(esp);
        }
        let result = emulate(self.uc, run, func, SENTINEL_BASE, 0, 0);
        {
            let mut state = run.borrow_mut();
            state.callback_depth -= 1;
            state.levels.pop();
        }
        let eax = if x64 { self.uc.reg_read(Reg::RAX).unwrap_or(0) } else { self.uc.reg_read(Reg::EAX).unwrap_or(0) & 0xFFFF_FFFF };
        for (slot, register) in registers.iter().enumerate() {
            let _ = self.uc.reg_write(*register, saved[slot]);
        }
        if let Err(error) = result {
            let mut state = run.borrow_mut();
            if state.error.is_none() && state.exit_code.is_none() {
                state.error = Some(format!("guest callback {func:#010x} failed: {error:?}"));
            }
            return None;
        }
        let state = run.borrow();
        if state.exit_code.is_some() || state.error.is_some() || state.resume.is_some() {
            // A pending resume belongs to an outer level: unwind out.
            return None;
        }
        Some(eax)
    }

    /// A thread of a 64-bit process: its TEB through the GS base, the
    /// parameter in RCX, returning to the thread-end thunk.
    fn create_thread64(&mut self, entry: u32, param: u32, stack: u32, top: u32, teb: u32) -> Option<u32> {
        let run = self.run?;
        let id = run.borrow().next_thread;
        let thread_return = run.borrow().thread_return;
        let mut current = self.uc.context_alloc().ok()?;
        self.uc.context_save(&mut current).ok()?;
        init_teb64(self.uc, teb, top, stack, id).ok()?;
        let templates = run.borrow().tls_templates.clone();
        install_tls(self.uc, &templates, teb, true).ok()?;
        let rsp = ((top - 0x40) & !0xF) - 8;
        self.uc.mem_write(rsp as u64, &(thread_return as u64).to_le_bytes()).ok()?;
        for register in SAVED_REGISTERS_64 {
            self.uc.reg_write(register, 0).ok()?;
        }
        for (register, value) in [(Reg::RSP, rsp as u64), (Reg::RIP, entry as u64), (Reg::RCX, param as u64), (Reg::EFLAGS, 0x202)] {
            self.uc.reg_write(register, value).ok()?;
        }
        let mut context = self.uc.context_alloc().ok()?;
        self.uc.context_save(&mut context).ok()?;
        self.uc.context_restore(&current).ok()?;
        let mut state = run.borrow_mut();
        state.next_thread += 1;
        state.tibs.push((id, teb));
        state.threads.push(ThreadSlot {
            id,
            context,
            levels: Vec::new(),
            callback_depth: 0,
            retry_eip: None,
        });
        Some(id)
    }
}

impl GuestMemory for UnicornGuestMemory<'_, '_, '_, '_> {
    fn read_bytes(&mut self, va: u32, len: usize) -> Option<Vec<u8>> {
        self.uc.mem_read_as_vec(va as u64, len).ok()
    }

    fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Option<()> {
        self.uc.mem_write(va as u64, bytes).ok()
    }

    fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
        self.uc.mem_write(va as u64, &value.to_le_bytes()).ok()
    }

    /// Run guest code synchronously: push `args` and a sentinel return
    /// address below the current stack, emulate until the callee returns to
    /// the sentinel, then restore every register. This is how SendMessageA,
    /// CreateWindowExA and hooks call guest window procedures.
    fn call_guest_ex(
        &mut self,
        func: u32,
        args: &[u32],
        ebp: Option<u32>,
        ecx: Option<u32>,
    ) -> Option<u32> {
        let wide: Vec<u64> = args.iter().map(|&value| value as u64).collect();
        self.call_guest_full(func, &wide, ebp, ecx).map(|value| value as u32)
    }

    fn call_guest_wide(&mut self, func: u32, args: &[u64]) -> Option<u64> {
        self.call_guest_full(func, args, None, None)
    }


    fn map_region(&mut self, preferred: u32, size: u32) -> Option<u32> {
        const GRANULE: u64 = 0x1_0000;
        // A 64-bit process may use the whole 4 GiB below which OTW keeps it.
        let limit: u64 = if self.run.is_some_and(|run| run.borrow().x64) { 0xFFFF_0000 } else { 0xC000_0000 };
        let size = (size as u64 + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
        let regions: Vec<(u64, u64)> = self
            .uc
            .mem_regions()
            .ok()?
            .iter()
            .map(|region| (region.begin, region.end))
            .collect();
        let free = |start: u64| {
            start >= GRANULE
                && start + size <= limit
                && regions.iter().all(|&(begin, end)| start + size <= begin || start > end)
        };
        let mut start = (preferred as u64) & !(PAGE_SIZE as u64 - 1);
        if !free(start) {
            start = ((preferred as u64).max(0x1000_0000) + GRANULE - 1) & !(GRANULE - 1);
            while start + size <= limit && !free(start) {
                start += GRANULE;
            }
            if !free(start) {
                return None;
            }
        }
        self.uc.mem_map(start, size, Prot::ALL).ok()?;
        Some(start as u32)
    }

    fn add_tls(&mut self, template: &[u8]) -> Option<u32> {
        let run = self.run?;
        let (index, tibs, x64) = {
            let mut state = run.borrow_mut();
            state.tls_templates.push(template.to_vec());
            (state.tls_templates.len() as u32 - 1, state.tibs.clone(), state.x64)
        };
        let width = if x64 { 8 } else { 4 };
        for (_, tib) in tibs {
            // A longer array: the old blocks, then the new one.
            let slot = if x64 { tib + 0x58 } else { tib + 0x2C };
            let old = self.read_ptr(slot).unwrap_or(0) as u32;
            let size = (index + 1) * width;
            let array = self.map_region(0x1000_0000, size + template.len() as u32 + 16)?;
            if old != 0 && index > 0 {
                let previous = self.read_bytes(old, (index * width) as usize).unwrap_or_default();
                self.write_bytes(array, &previous)?;
            }
            let block = (array + size + 15) & !15;
            self.write_bytes(block, template)?;
            self.write_ptr(array + index * width, block as u64)?;
            self.write_ptr(slot, array as u64)?;
        }
        Some(index)
    }

    fn map_exact(&mut self, va: u32, size: u32) -> bool {
        let size = (size as u64 + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
        let start = va as u64 & !(PAGE_SIZE as u64 - 1);
        let limit: u64 = if self.run.is_some_and(|run| run.borrow().x64) { 0xFFFF_0000 } else { 0xC000_0000 };
        if start == 0 || start + size > limit {
            return false;
        }
        let overlaps = self
            .uc
            .mem_regions()
            .map(|regions| regions.iter().any(|r| start <= r.end && start + size > r.begin))
            .unwrap_or(true);
        !overlaps && self.uc.mem_map(start, size, Prot::ALL).is_ok()
    }

    fn unmap(&mut self, va: u32, size: u32) -> bool {
        let size = (size as u64 + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
        self.uc.mem_unmap(va as u64, size).is_ok()
    }

    fn mapped(&mut self) -> Vec<(u64, u64)> {
        self.uc.mem_regions().map(|regions| regions.iter().map(|r| (r.begin, r.end)).collect()).unwrap_or_default()
    }

    fn create_thread(&mut self, entry: u32, param: u32, stack_size: u32) -> Option<u32> {
        let run = self.run?;
        let stack_size = align_up(stack_size.clamp(0x1_0000, 0x100_0000), PAGE_SIZE);
        let stack = self.map_region(0x1000_0000, stack_size)?;
        let tib = self.map_region(0x1000_0000, if run.borrow().x64 { TEB64_SIZE } else { PAGE_SIZE })?;
        let top = stack + stack_size;
        if run.borrow().x64 {
            return self.create_thread64(entry, param, stack, top, tib);
        }
        // A copy of the first thread's TIB with its own exception chain,
        // stack bounds and self pointer.
        let mut page = self.uc.mem_read_as_vec(TIB_BASE as u64, TIB_SIZE as usize).ok()?;
        for (offset, value) in [(0usize, u32::MAX), (4, top), (8, stack), (0x18, tib)] {
            page[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        self.uc.mem_write(tib as u64, &page).ok()?;
        let templates = run.borrow().tls_templates.clone();
        install_tls(self.uc, &templates, tib, false).ok()?;
        // The start routine returns to the thread-end thunk.
        let esp = top - 0x10;
        let thread_return = run.borrow().thread_return;
        let mut frame = thread_return.to_le_bytes().to_vec();
        frame.extend_from_slice(&param.to_le_bytes());
        self.uc.mem_write(esp as u64, &frame).ok()?;
        // Its registers: the running thread's, with the new stack, TIB and
        // start address.
        let mut current = self.uc.context_alloc().ok()?;
        self.uc.context_save(&mut current).ok()?;
        set_fs_base(self.uc, tib).ok()?;
        for (register, value) in [
            (Reg::ESP, esp),
            (Reg::EBP, 0),
            (Reg::EIP, entry),
            (Reg::EAX, 0),
            (Reg::EBX, 0),
            (Reg::ECX, 0),
            (Reg::EDX, 0),
            (Reg::ESI, 0),
            (Reg::EDI, 0),
            (Reg::EFLAGS, 0x202),
        ] {
            self.uc.reg_write(register, value as u64).ok()?;
        }
        let mut context = self.uc.context_alloc().ok()?;
        self.uc.context_save(&mut context).ok()?;
        let current_tib = {
            let state = run.borrow();
            let id = *state.running.last()?;
            state.tibs.iter().find(|(t, _)| *t == id).map(|(_, b)| *b)?
        };
        let _ = self.uc.mem_write((GDT_BASE + FS_SELECTOR) as u64, &fs_descriptor(current_tib));
        self.uc.context_restore(&current).ok()?;
        let mut state = run.borrow_mut();
        let id = state.next_thread;
        state.next_thread += 1;
        state.tibs.push((id, tib));
        state.threads.push(ThreadSlot {
            id,
            context,
            levels: Vec::new(),
            callback_depth: 0,
            retry_eip: None,
        });
        Some(id)
    }

    fn current_thread(&self) -> u32 {
        self.run
            .and_then(|run| run.borrow().running.last().copied())
            .unwrap_or(1)
    }

    fn ptr_size(&self) -> u32 {
        if self.run.is_some_and(|run| run.borrow().x64) { 8 } else { 4 }
    }

    fn current_tib(&self) -> u32 {
        self.run.map(|run| current_tib(&run.borrow())).unwrap_or(TIB_BASE)
    }

    fn waiting_threads(&self) -> Vec<u32> {
        self.run
            .map(|run| run.borrow().threads.iter().map(|t| t.id).collect())
            .unwrap_or_default()
    }

    fn run_thread(&mut self, id: u32) -> Option<ThreadRun> {
        let run = self.run?;
        let slot = {
            let mut state = run.borrow_mut();
            if state.exit_code.is_some() || state.error.is_some() || state.resume.is_some() {
                return None;
            }
            let index = state.threads.iter().position(|t| t.id == id)?;
            state.threads.remove(index)
        };
        let mut current = self.uc.context_alloc().ok()?;
        self.uc.context_save(&mut current).ok()?;
        let (outer_levels, outer_depth, outer_tib) = {
            let mut state = run.borrow_mut();
            let outer_id = *state.running.last().unwrap_or(&1);
            let outer_tib = state.tibs.iter().find(|(t, _)| *t == outer_id).map(|(_, b)| *b).unwrap_or(TIB_BASE);
            let levels = std::mem::replace(&mut state.levels, slot.levels);
            let depth = std::mem::replace(&mut state.callback_depth, slot.callback_depth);
            state.running.push(id);
            (levels, depth, outer_tib)
        };
        let tib = run.borrow().tibs.iter().find(|(t, _)| *t == id).map(|(_, b)| *b).unwrap_or(TIB_BASE);
        let _ = self.uc.mem_write((GDT_BASE + FS_SELECTOR) as u64, &fs_descriptor(tib));
        let _ = self.uc.context_restore(&slot.context);
        let begin = slot
            .retry_eip
            .unwrap_or_else(|| self.uc.reg_read(Reg::EIP).unwrap_or(0) as u32);
        let result = emulate(self.uc, run, begin, THREAD_STOP, 0, 0);
        let signal = run.borrow_mut().thread_signal.take();
        let outcome = match signal {
            Some(ThreadSignal::Suspend) => {
                let retry = run.borrow().yield_eip;
                let mut context = self.uc.context_alloc().ok()?;
                let _ = self.uc.context_save(&mut context);
                let mut state = run.borrow_mut();
                let levels = std::mem::take(&mut state.levels);
                let depth = state.callback_depth;
                state.threads.push(ThreadSlot {
                    id,
                    context,
                    levels,
                    callback_depth: depth,
                    retry_eip: Some(retry),
                });
                ThreadRun::Waiting
            }
            Some(ThreadSignal::Exit(code)) => ThreadRun::Exited(code),
            None => {
                let mut state = run.borrow_mut();
                if let Err(error) = result {
                    if state.error.is_none() && state.exit_code.is_none() {
                        state.error = Some(format!("thread {id} failed: {error:?}"));
                    }
                }
                ThreadRun::Exited(0)
            }
        };
        {
            let mut state = run.borrow_mut();
            state.levels = outer_levels;
            state.callback_depth = outer_depth;
            state.running.pop();
            if matches!(outcome, ThreadRun::Exited(_)) {
                state.tibs.retain(|(t, _)| *t != id);
            }
        }
        let _ = self.uc.mem_write((GDT_BASE + FS_SELECTOR) as u64, &fs_descriptor(outer_tib));
        let _ = self.uc.context_restore(&current);
        Some(outcome)
    }

    fn suspend_thread(&mut self) -> bool {
        let Some(run) = self.run else {
            return false;
        };
        let mut state = run.borrow_mut();
        if state.running.len() < 2 || !state.levels.is_empty() || state.resume.is_some() {
            return false;
        }
        state.thread_signal = Some(ThreadSignal::Suspend);
        true
    }

    fn end_thread(&mut self, code: u32) -> bool {
        let Some(run) = self.run else {
            return false;
        };
        let mut state = run.borrow_mut();
        if state.running.len() < 2 || !state.levels.is_empty() {
            return false;
        }
        state.thread_signal = Some(ThreadSignal::Exit(code));
        true
    }

    fn xmm(&mut self, index: usize) -> u64 {
        let register = [Reg::XMM0, Reg::XMM1, Reg::XMM2, Reg::XMM3][index.min(3)];
        self.uc
            .reg_read_long(register)
            .ok()
            .and_then(|bytes| bytes.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8]))))
            .unwrap_or(0)
    }

    fn set_xmm0(&mut self, bits: u64) {
        let mut value = self.uc.reg_read_long(Reg::XMM0).map(|b| b.to_vec()).unwrap_or_else(|_| vec![0; 16]);
        value.resize(16, 0);
        value[..8].copy_from_slice(&bits.to_le_bytes());
        value[8..].fill(0);
        let _ = self.uc.reg_write_long(Reg::XMM0, &value);
    }

    fn registers(&mut self) -> Option<memory::Registers> {
        let read = |uc: &Unicorn<'_, ()>, register| uc.reg_read(register).map(|v| v as u32).ok();
        Some(memory::Registers {
            eax: read(self.uc, Reg::EAX)?,
            ebx: read(self.uc, Reg::EBX)?,
            ecx: read(self.uc, Reg::ECX)?,
            edx: read(self.uc, Reg::EDX)?,
            esi: read(self.uc, Reg::ESI)?,
            edi: read(self.uc, Reg::EDI)?,
            ebp: read(self.uc, Reg::EBP)?,
            esp: read(self.uc, Reg::ESP)?,
        })
    }

    fn request_resume(&mut self, resume: memory::Resume) {
        if let Some(run) = self.run {
            run.borrow_mut().resume = Some(resume);
        }
    }

    fn request_resume_full(&mut self, registers: memory::FullRegisters) {
        if let Some(run) = self.run {
            let mut state = run.borrow_mut();
            state.resume = Some(memory::Resume {
                eip: registers.rip as u32,
                esp: registers.gpr[4] as u32,
                ebp: registers.gpr[5] as u32,
                regs: None,
            });
            state.resume_full = Some(registers);
        }
    }

    fn set_full_registers(&mut self, registers: &memory::FullRegisters) {
        let rip = self.uc.reg_read(Reg::RIP).unwrap_or(0);
        let mut registers = registers.clone();
        registers.rip = rip;
        let _ = write_full(self.uc, &registers);
    }

    fn full_registers(&mut self) -> Option<memory::FullRegisters> {
        Some(read_full(self.uc))
    }

    fn callback_levels(&self) -> Vec<u32> {
        self.run.map(|run| run.borrow().levels.clone()).unwrap_or_default()
    }

    fn resume_pending(&self) -> bool {
        self.run
            .map(|run| run.borrow().resume.is_some())
            .unwrap_or(false)
    }

    fn exception_dispatcher(&self) -> Option<u32> {
        self.run
            .map(|run| run.borrow().dispatcher)
            .filter(|address| *address != 0)
    }

    fn can_yield(&self) -> bool {
        self.run
            .map(|run| {
                let state = run.borrow();
                state.can_yield
                    && state.levels.is_empty()
                    && state.running.len() == 1
                    && state.resume.is_none()
                    && state.exit_code.is_none()
                    && state.error.is_none()
            })
            .unwrap_or(false)
    }

    fn yield_now(&mut self) -> bool {
        if !self.can_yield() {
            return false;
        }
        if let Some(run) = self.run {
            run.borrow_mut().yielding = true;
        }
        true
    }

    fn pump(&mut self, block: bool) {
        let Some(run) = self.run else {
            return;
        };
        // Take the pump out while it runs so a re-entrant call cannot alias it.
        let pump = run.borrow_mut().pump.take();
        if let Some(mut pump) = pump {
            pump(block);
            run.borrow_mut().pump = Some(pump);
        } else if block {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn has_host(&self) -> bool {
        self.run
            .map(|run| run.borrow().pump.is_some())
            .unwrap_or(false)
    }

    fn stopping(&self) -> bool {
        self.run
            .map(|run| {
                let state = run.borrow();
                state.exit_code.is_some() || state.error.is_some() || state.resume.is_some()
            })
            .unwrap_or(false)
    }
}

fn align_down(value: u32, alignment: u32) -> u32 {
    value & !(alignment - 1)
}

fn align_up(value: u32, alignment: u32) -> u32 {
    align_down(value.wrapping_add(alignment - 1), alignment)
}

fn read_guest_u32(uc: &Unicorn<'_, ()>, va: u32) -> Result<u32, uc_error> {
    let mut bytes = [0u8; 4];
    uc.mem_read(va as u64, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn abort(state: &Rc<RefCell<RunState<'_>>>, uc: &mut Unicorn<'_, ()>, message: String) {
    state.borrow_mut().error = Some(message);
    let _ = uc.emu_stop();
}

/// Initialise the Win95 globals that the real loader passes to KERNEL32's
/// private boot entry point. The values are synthetic, but every API called
/// by the guest remains genuine KERNEL32 machine code.
fn init_win95_state(process: &Image, uc: &mut Unicorn<'_, ()>) -> Result<(), RunError> {
    uc.mem_map(WIN95_STATE_BASE as u64, WIN95_STATE_SIZE as u64, Prot::ALL)
        .map_err(|error| RunError::Unicorn("mapping Win95 loader state", error))?;
    let state = vec![0u8; WIN95_STATE_SIZE as usize];
    uc.mem_write(WIN95_STATE_BASE as u64, &state)
        .map_err(|error| RunError::Unicorn("clearing Win95 loader state", error))?;

    // Build a cyclic free list for KERNEL32's private thunk allocator. Genuine
    // code allocates four blocks, writes encoded handles into them and frees
    // them again; a real Win95 thread receives this list from the loader.
    for index in 0..LOCAL_HANDLE_COUNT {
        let block = LOCAL_HANDLE_LIST_BASE + (index * 8) as u32;
        let next = if index + 1 == LOCAL_HANDLE_COUNT {
            LOCAL_HANDLE_LIST_BASE
        } else {
            block + 8
        };
        uc.mem_write(block as u64, &next.to_le_bytes())
            .map_err(|error| RunError::Unicorn("writing synthetic thunk free list", error))?;
    }

    // bffbc27c points at a holder whose first dword is the process database
    // pointer; bff7a84f (used by GetModuleHandle-style paths) dereferences it
    // and walks the module list. A zeroed PDB makes the walk return "not
    // found" instead of faulting.
    let pdb = WIN95_STATE_BASE;
    let pdb_holder = WIN95_STATE_BASE + 0x100;
    let module_holder = WIN95_STATE_BASE + 0x200;
    let thunk_table = WIN95_STATE_BASE + 0x300;
    let command_line_fallback = WIN95_STATE_BASE + 0x400;
    let heap_holder = WIN95_STATE_BASE + 0x500;
    let system_directory = WIN95_STATE_BASE + 0x600;
    let module_database = WIN95_STATE_BASE + 0x800;
    let resource_table = WIN95_STATE_BASE + 0xa00;
    let vmm_list = WIN95_STATE_BASE + 0xc00;
    let command_line = process.command_line_va;
    // Private KERNEL32 module helpers dereference the PDB's first field before
    // decrementing a refcount at PDB+0x10. A self-reference keeps that walk in
    // synthetic state instead of faulting at address zero.
    uc.mem_write(pdb as u64, &pdb.to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic PDB self-reference", error))?;
    // Private process-snapshot helpers copy PDB+0x60 into their output record
    // and then dereference it at +0x28. Self-reference keeps that walk inside
    // synthetic state.
    uc.mem_write((pdb + 0x60) as u64, &pdb.to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic PDB snapshot pointer", error))?;
    // GetCommandLineA reads PDB+0xbc, falling back to *(PDB+0x40)+8.
    uc.mem_write((pdb + 0xbc) as u64, &command_line.to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic command line pointer", error))?;
    uc.mem_write((pdb + 0x40) as u64, &command_line_fallback.to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic command line holder", error))?;
    uc.mem_write(
        (command_line_fallback + 8) as u64,
        &command_line.to_le_bytes(),
    )
    .map_err(|error| RunError::Unicorn("writing synthetic command line fallback", error))?;
    // GlobalAlloc/LocalAlloc read the process heap descriptor from PDB+0x18.
    uc.mem_write((pdb + 0x18) as u64, &heap_holder.to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic heap pointer", error))?;
    uc.mem_write(pdb_holder as u64, &pdb.to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic PDB pointer", error))?;
    uc.mem_write(module_holder as u64, &pdb.to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic module pointer", error))?;
    // bffbc294 is the synthetic module database consulted by private KERNEL32
    // query helpers. The real one carries the 16-bit module signature 'HI' at
    // offset 0x72; without it KERNEL32 treats the pointer as invalid.
    uc.mem_write((module_database + 0x72) as u64, b"HI".as_slice())
        .map_err(|error| RunError::Unicorn("writing synthetic module signature", error))?;
    // bff78120 passes module_database+0x4c as a refcounted handle to
    // bff74340, which decrements [handle+0x10].
    uc.mem_write(
        (module_database + 0x4c) as u64,
        &module_database.to_le_bytes(),
    )
    .map_err(|error| RunError::Unicorn("writing synthetic module handle", error))?;
    // Make bff7435b accept the synthetic module immediately, avoiding its
    // private VMM service 0x2a001d path.
    uc.mem_write((module_database + 0x08) as u64, &pdb.to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic module owner", error))?;
    // The private allocator treats +0x08 as a list head and +0x10 as the next
    // block pointer. A self-terminating list makes the walk return failure
    // instead of dereferencing an uninitialised pointer.
    uc.mem_write(
        (module_database + 0x10) as u64,
        &(module_database + 0x0c).to_le_bytes(),
    )
    .map_err(|error| RunError::Unicorn("writing synthetic module list end", error))?;
    // Setting flag bit 0 skips the refcounted handle release at bff7811b;
    // leaving bit 0x40 clear avoids the private VMM allocation service.
    uc.mem_write((module_database + 0x70) as u64, &0x49u32.to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic module flags", error))?;

    // bffbc1e0 is the resource table consulted when KERNEL32 resolves a
    // resource in its own module. A synthetic empty table makes the lookup
    // return "not found" instead of dereferencing the uninitialised pointer.
    uc.mem_write(
        resource_table as u64,
        &(resource_table + 0x10).to_le_bytes(),
    )
    .map_err(|error| RunError::Unicorn("writing synthetic resource table", error))?;
    uc.mem_write(
        (resource_table + 0x14) as u64,
        &(resource_table + 0x100).to_le_bytes(),
    )
    .map_err(|error| RunError::Unicorn("writing synthetic resource base", error))?;

    // bffbf77c heads a VMM-maintained list of system objects. An empty list
    // makes private KERNEL32 state snapshots skip the optional object walk.
    uc.mem_write(vmm_list as u64, &(vmm_list + 0x04).to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic VMM list", error))?;
    uc.mem_write((vmm_list + 0x04) as u64, &(vmm_list + 0x08).to_le_bytes())
        .map_err(|error| RunError::Unicorn("writing synthetic VMM list node", error))?;

    // Private KERNEL32 registry helpers read the system directory string from
    // bffbfb10 and call lstrlenA on it; the real loader fills this during boot.
    uc.mem_write(system_directory as u64, b"C:\\WINDOWS\\SYSTEM\0".as_slice())
        .map_err(|error| RunError::Unicorn("writing synthetic system directory", error))?;

    // KERNEL32's generated 16/32-bit thunks index this table with a small
    // selector number (typically 0x38). Keep every entry mapped and valid.
    let table = vec![WIN95_STATE_BASE; 0x100];
    uc.mem_write(
        thunk_table as u64,
        &table
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    )
    .map_err(|error| RunError::Unicorn("writing synthetic thunk table", error))?;

    let modules = &process.modules;
    let module = modules
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("kernel32.dll"));
    let Some(module) = module else {
        return Ok(());
    };
    for (rva, value) in [
        // KERNEL32's own module handle and its synthetic resource table.
        (0x4c016, module.image.image_base),
        (0x4c1e0, resource_table),
        (0x4f77c, vmm_list),
        (0x4c278, pdb_holder),
        (0x4c27c, pdb_holder),
        (0x4c280, module_holder),
        (0x4c44c, thunk_table),
        (0x4fb10, system_directory),
        (0x4c294, module_database),
    ] {
        let va = module.image.image_base.wrapping_add(rva);
        uc.mem_write(va as u64, &value.to_le_bytes())
            .map_err(|error| RunError::Unicorn("writing Win95 KERNEL32 globals", error))?;
    }

    // USER32's private flat-thunk patcher copies a template into its .data
    // section and reads the selector table pointer from [thunk - 0x3a]. The
    // real loader fills that slot; point it at the synthetic table so the
    // generated thunk can index it without touching unmapped low memory.
    if let Some(user32) = modules
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("user32.dll"))
    {
        for rva in [0x701c, 0x7090] {
            let slot = user32.image.image_base.wrapping_add(rva);
            uc.mem_write(slot as u64, &thunk_table.to_le_bytes())
                .map_err(|error| RunError::Unicorn("writing USER32 thunk table pointer", error))?;
        }
    }
    if let Some(gdi32) = modules
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("gdi32.dll"))
    {
        let slot = gdi32.image.image_base.wrapping_add(0x1b01c);
        uc.mem_write(slot as u64, &thunk_table.to_le_bytes())
            .map_err(|error| RunError::Unicorn("writing GDI32 thunk table pointer", error))?;
    }
    Ok(())
}

/// Run a program to completion.
pub fn run<'a>(
    image: &Image,
    syscalls: Rc<dyn SystemCalls>,
    pump: Option<Box<dyn FnMut(bool) + 'a>>,
) -> Result<RunResult, RunError> {
    let mut process = Process::new(image, syscalls, pump, false)?;
    loop {
        if let Some(result) = process.resume()? {
            return Ok(result);
        }
    }
}

/// A loaded program with its own CPU. [`Process::resume`] runs it until it
/// exits or, when yielding is enabled, until it waits for messages with
/// nothing to do; the next `resume` continues where it stopped.
pub struct Process<'a> {
    uc: Unicorn<'a, ()>,
    state: Rc<RefCell<RunState<'a>>>,
    code_trace: Rc<RefCell<VecDeque<String>>>,
    trace_code: bool,
    entry: u32,
    started: bool,
}

impl<'a> Process<'a> {
    /// Map the program, install the API hooks and run DLL initialisation.
    pub fn new(
        image: &Image,
        syscalls: Rc<dyn SystemCalls>,
        pump: Option<Box<dyn FnMut(bool) + 'a>>,
        can_yield: bool,
    ) -> Result<Self, RunError> {
        let process = build(image, syscalls, pump)?;
        process.state.borrow_mut().can_yield = can_yield;
        Ok(process)
    }

    /// Run until the program exits (`Some`) or yields (`None`).
    pub fn resume(&mut self) -> Result<Option<RunResult>, RunError> {
        let begin = if self.started {
            let eip = self.uc.reg_read(Reg::EIP).unwrap_or(0) as u32;
            if eip == YIELD_ADDRESS {
                self.state.borrow().yield_eip
            } else {
                eip
            }
        } else {
            self.started = true;
            self.entry
        };
        self.state.borrow_mut().yielded = false;
        let state = self.state.clone();
        let uc = &mut self.uc;
        let mut emulation = emulate(
            uc,
            &state,
            begin,
            YIELD_ADDRESS,
            TIMEOUT_SECONDS * SECOND_SCALE,
            MAX_INSTRUCTIONS,
        );
        // An exception handled in the main program's frames stops emulation
        // from inside a callback; continue at the handler's target.
        loop {
            let resume = {
                let mut run = state.borrow_mut();
                if run.error.is_some() || run.exit_code.is_some() || run.yielded {
                    None
                } else {
                    run.resume.take()
                }
            };
            let Some(resume) = resume else {
                break;
            };
            if let Err(error) = apply_resume_full(uc, &state, &resume) {
                return Err(RunError::Unicorn("resuming after an exception", error));
            }
            emulation = emulate(
                uc,
                &state,
                resume.eip,
                YIELD_ADDRESS,
                TIMEOUT_SECONDS * SECOND_SCALE,
                MAX_INSTRUCTIONS,
            );
        }

        let state = state.borrow();
        if let Some(error) = &state.error {
            return Err(RunError::Guest(error.clone()));
        }
        if state.yielded && state.exit_code.is_none() {
            return Ok(None);
        }
        if let Err(error) = emulation {
            if self.trace_code {
                eprintln!("last instructions before guest emulation error:");
                for line in self.code_trace.borrow().iter() {
                    eprintln!("{line}");
                }
            }
            if std::env::var_os("OTW_TRACE_FAULTS").is_some() {
                let rip = self.uc.reg_read(unicorn_engine::RegisterX86::RIP).unwrap_or(0);
                let mut bytes = [0u8; 16];
                let _ = self.uc.mem_read(rip, &mut bytes);
                eprintln!("[eh] {error:?} at {rip:#x}: {bytes:02x?}");
            }
            return Err(RunError::Unicorn("emulating guest code", error));
        }

        let exit_code = if let Some(exit_code) = state.exit_code {
            exit_code
        } else {
            let eip = uc.reg_read(Reg::EIP).unwrap_or(0);
            if self.trace_code {
                eprintln!("last instructions before guest returned without ExitProcess (eip={eip:#010x}):");
                for line in self.code_trace.borrow().iter() {
                    eprintln!("{line}");
                }
            }
            eprintln!("guest stopped without calling ExitProcess (eip={eip:#010x})");
            return Err(RunError::NoExit);
        };
        Ok(Some(RunResult {
            exit_code,
            api_calls: state.api_calls,
        }))
    }
}

fn build<'a>(
    process: &Image,
    syscalls: Rc<dyn SystemCalls>,
    pump: Option<Box<dyn FnMut(bool) + 'a>>,
) -> Result<Process<'a>, RunError> {
    let image = process.main();
    let iat = &process.iat;

    let x64 = image.is64;
    let mut uc = Unicorn::new(Arch::X86, if x64 { Mode::MODE_64 } else { Mode::MODE_32 })
        .map_err(|error| RunError::Unicorn("creating the CPU", error))?;
    hide_sse42(&mut uc).map_err(|error| RunError::Unicorn("hooking CPUID", error))?;

    for module in &process.modules {
        let image_start = align_down(module.image.image_base, PAGE_SIZE);
        let image_end = align_up(
            module
                .image
                .image_base
                .wrapping_add(module.memory.len() as u32),
            PAGE_SIZE,
        );
        uc.mem_map(
            image_start as u64,
            (image_end - image_start) as u64,
            Prot::ALL,
        )
        .map_err(|error| RunError::Unicorn("mapping guest image", error))?;
        uc.mem_write(module.image.image_base as u64, &module.memory)
            .map_err(|error| RunError::Unicorn("writing guest image", error))?;
    }

    uc.mem_map(SENTINEL_BASE as u64, PAGE_SIZE as u64, Prot::ALL)
        .map_err(|error| RunError::Unicorn("mapping DllMain sentinel", error))?;
    uc.mem_write(SENTINEL_BASE as u64, &[0xC3])
        .map_err(|error| RunError::Unicorn("writing DllMain sentinel", error))?;
    uc.mem_write(YIELD_ADDRESS as u64, &[0xC3])
        .map_err(|error| RunError::Unicorn("writing yield address", error))?;
    uc.mem_write(THREAD_STOP as u64, &[0xC3])
        .map_err(|error| RunError::Unicorn("writing thread stop address", error))?;

    // A 64-bit program's first thread gets nearly 2 MiB of stack (current
    // programs check for at least 1 MiB), from 64 KiB up.
    let stack_low = if x64 { X64_STACK_LOW } else { STACK_BASE };
    uc.mem_map(stack_low as u64, (STACK_BASE + STACK_SIZE - stack_low) as u64, Prot::ALL)
        .map_err(|error| RunError::Unicorn("mapping guest stack", error))?;

    // The heap's range, except where a program image already lies in it.
    let mut images: Vec<(u32, u32)> = process
        .modules
        .iter()
        .map(|module| {
            let start = align_down(module.image.image_base, PAGE_SIZE);
            (start, align_up(module.image.image_base + module.memory.len() as u32, PAGE_SIZE))
        })
        .filter(|&(start, end)| start < HEAP_BASE + HEAP_SIZE && end > HEAP_BASE)
        .collect();
    images.sort();
    let mut cursor = HEAP_BASE;
    for (start, end) in images.into_iter().chain([(HEAP_BASE + HEAP_SIZE, HEAP_BASE + HEAP_SIZE)]) {
        if start > cursor {
            uc.mem_map(cursor as u64, (start - cursor) as u64, Prot::ALL)
                .map_err(|error| RunError::Unicorn("mapping guest heap", error))?;
        }
        cursor = cursor.max(end);
    }

    uc.mem_map(TIB_BASE as u64, if x64 { TEB64_SIZE } else { TIB_SIZE } as u64, Prot::ALL)
        .map_err(|error| RunError::Unicorn("mapping guest TIB", error))?;
    if x64 {
        init_teb64(&mut uc, TIB_BASE, STACK_TOP, X64_STACK_LOW, 1)?;
    } else {
        // Win95 TIB: a terminated SEH chain, stack bounds and a pointer to the TIB
        // itself. MFC's CRT and SEH prologues consult all four during startup.
        // KERNEL32's private state helpers compare FS:[0] with -1 and skip the
        // registration-record walk only when the chain head is the real terminator.
        let tib_init = [
            (0u32, u32::MAX),
            (4, STACK_TOP),
            (8, STACK_BASE),
            // Win95 KERNEL32's private 16-bit thunk allocator pops blocks from
            // FS:[0x10]. It expects a linked list of mapped free blocks, not zero.
            (0x10, LOCAL_HANDLE_LIST_BASE),
            (0x18, TIB_BASE),
            // FS:[0x30] (NT's PEB): a zeroed block. MSHTML's allocator reads
            // flags from it; KERNEL32's own code must not find the PDB there.
            (0x30, WIN95_STATE_BASE + 0x3000),
            // Win95 heap helpers load GS from FS:[0x0c] and test flag bits at
            // GS:[0x16]. Point GS at the synthetic state page, whose flags stay
            // zero so the simple (non-thunking) allocation path is used.
            (0x0c, GS_SELECTOR),
        ];
        for (offset, value) in tib_init {
            uc.mem_write((TIB_BASE + offset) as u64, &value.to_le_bytes())
                .map_err(|error| RunError::Unicorn("initialising guest TIB", error))?;
        }

        // Unicorn 2.1.5 ignores FS_BASE writes in 32-bit mode, so install a small
        // GDT and load FS with a selector whose descriptor bases it at TIB_BASE.
        uc.mem_map(GDT_BASE as u64, GDT_SIZE as u64, Prot::ALL)
            .map_err(|error| RunError::Unicorn("mapping guest GDT", error))?;
        let mut gdt = vec![0u8; 40];
        // Flat 32-bit code and data selectors, the TIB selector, and a selector
        // based at the synthetic Win95 state page for GS.
        gdt[8..16].copy_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0x9A, 0xCF, 0]);
        gdt[16..24].copy_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0x92, 0xCF, 0]);
        gdt[24..32].copy_from_slice(&fs_descriptor(TIB_BASE));
        let gs_base = WIN95_STATE_BASE;
        let gs_limit = WIN95_STATE_SIZE - 1;
        gdt[32..40].copy_from_slice(&[
            gs_limit as u8,
            (gs_limit >> 8) as u8,
            gs_base as u8,
            (gs_base >> 8) as u8,
            (gs_base >> 16) as u8,
            0x92,
            0xC0, // 32-bit segment, 4 KiB granularity
            (gs_base >> 24) as u8,
        ]);
        uc.mem_write(GDT_BASE as u64, &gdt)
            .map_err(|error| RunError::Unicorn("writing guest GDT", error))?;
        let mut gdtr = [0u8; 24];
        gdtr[8..16].copy_from_slice(&(GDT_BASE as u64).to_le_bytes());
        gdtr[16..20].copy_from_slice(&39u32.to_le_bytes());
        uc.reg_write_long(Reg::GDTR, &gdtr)
            .map_err(|error| RunError::Unicorn("initialising GDTR", error))?;
        for (register, selector) in [
            (Reg::CS, CODE_SELECTOR),
            (Reg::DS, DATA_SELECTOR),
            (Reg::ES, DATA_SELECTOR),
            (Reg::SS, DATA_SELECTOR),
            (Reg::FS, FS_SELECTOR),
            (Reg::GS, DATA_SELECTOR),
        ] {
            uc.reg_write(register, selector as u64)
                .map_err(|error| RunError::Unicorn("initialising guest segment selectors", error))?;
        }
    }

    let thunk_end = align_up(
        THUNK_BASE.wrapping_add((iat.thunk_count() as u32 + 1) * 4),
        PAGE_SIZE,
    );
    let thunk_size = thunk_end - THUNK_BASE;
    uc.mem_map(THUNK_BASE as u64, thunk_size as u64, Prot::ALL)
        .map_err(|error| RunError::Unicorn("mapping IAT thunks", error))?;
    uc.mem_write(THUNK_BASE as u64, &vec![0xC3; thunk_size as usize])
        .map_err(|error| RunError::Unicorn("writing IAT thunks", error))?;

    if !x64 {
        init_win95_state(process, &mut uc)?;
    }

    let thread_return = process.entry_point(KERNEL_DLL, THREAD_RETURN).unwrap_or(0);
    let initial_sp = if x64 {
        // As if called: the entry point returns to the thread-end thunk,
        // with the stack 16-byte aligned before the call.
        let sp = (STACK_TOP & !0xF) - 8;
        uc.mem_write(sp as u64, &(thread_return as u64).to_le_bytes())
            .map_err(|error| RunError::Unicorn("initialising the stack", error))?;
        sp
    } else {
        STACK_TOP
    };
    uc.reg_write(Reg::ESP, initial_sp as u64)
        .map_err(|error| RunError::Unicorn("initialising ESP", error))?;
    uc.reg_write(Reg::EBP, 0)
        .map_err(|error| RunError::Unicorn("initialising EBP", error))?;
    // The x87 state a Windows process starts with: every exception masked,
    // 53-bit precision, round to nearest. C runtimes check these masks and
    // raise floating-point exceptions for any that are clear.
    uc.reg_write(Reg::FPCW, 0x027F)
        .map_err(|error| RunError::Unicorn("initialising the FPU control word", error))?;

    let mut initial_state = RunState::default();
    initial_state.pump = pump;
    initial_state.dispatcher = process
        .entry_point(KERNEL_DLL, EXCEPTION_DISPATCHER)
        .unwrap_or(0);
    initial_state.thread_return = thread_return;
    initial_state.x64 = x64;
    initial_state.tls_templates = tls_templates(process, &mut uc);
    {
        let teb = TIB_BASE;
        install_tls(&mut uc, &initial_state.tls_templates, teb, x64)
            .map_err(|error| RunError::Unicorn("setting up thread-local storage", error))?;
    }
    initial_state.running = vec![1];
    initial_state.tibs = vec![(1, TIB_BASE)];
    initial_state.next_thread = 2;
    let state = Rc::new(RefCell::new(initial_state));

    let thunk_state = state.clone();
    let thunk_iat = iat.clone();
    let thunk_calls = syscalls;
    let trace_api = std::env::var_os("OTW_TRACE_API").is_some();
    let trace_results = std::env::var_os("OTW_TRACE_RESULTS").is_some();
    let trace_code = std::env::var_os("OTW_TRACE_CODE").is_some();
    let code_trace = Rc::new(RefCell::new(VecDeque::<String>::new()));
    if trace_code {
        let trace = code_trace.clone();
        uc.add_code_hook(
            0,
            u32::MAX as u64,
            move |uc, address, size| {
                let registers = [
                    Reg::EAX,
                    Reg::EBX,
                    Reg::ECX,
                    Reg::EDX,
                    Reg::ESI,
                    Reg::EDI,
                    Reg::EBP,
                    Reg::ESP,
                ];
                let mut values = [0u64; 8];
                for (slot, register) in registers.iter().enumerate() {
                    values[slot] = uc.reg_read(*register).unwrap_or(0);
                }
                let bytes = uc
                    .mem_read_as_vec(address, size as usize)
                    .ok()
                    .map(|bytes| {
                        bytes
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let mut trace = trace.borrow_mut();
                // A repeated string instruction keeps one line.
                let prefix = format!("eip={address:#010x} ");
                if trace.back().is_some_and(|last| last.starts_with(&prefix)) {
                    trace.pop_back();
                }
                if trace.len() >= 4096 {
                    trace.pop_front();
                }
                trace.push_back(format!(
                    "eip={address:#010x} bytes={bytes:<24} eax={:08x} ebx={:08x} ecx={:08x} edx={:08x} esi={:08x} edi={:08x} ebp={:08x} esp={:08x}",
                    values[0],
                    values[1],
                    values[2],
                    values[3],
                    values[4],
                    values[5],
                    values[6],
                    values[7],
                ));
            },
        )
        .map_err(|error| RunError::Unicorn("installing instruction trace hook", error))?;
    }
    let mut last_pump = std::time::Instant::now();
    uc.add_code_hook(
        THUNK_BASE as u64,
        (thunk_end - 1) as u64,
        move |uc, address, _size| {
            let Some(entry) = thunk_iat.entry_for_thunk(address as u32) else {
                return;
            };
            // A program busy for a while (an installer extracting files on a
            // worker thread, a long wait on other threads) still lets the
            // host window redraw and take input every few frames.
            if last_pump.elapsed() >= std::time::Duration::from_millis(30) {
                last_pump = std::time::Instant::now();
                let pump = thunk_state.borrow_mut().pump.take();
                if let Some(mut pump) = pump {
                    pump(false);
                    thunk_state.borrow_mut().pump = Some(pump);
                }
            }
            let esp = match uc.reg_read(Reg::ESP) {
                Ok(value) => value as u32,
                Err(error) => {
                    abort(&thunk_state, uc, format!("could not read ESP: {error:?}"));
                    return;
                }
            };
            let return_address = match read_guest_u32(uc, esp) {
                Ok(value) => value,
                Err(error) => {
                    abort(
                        &thunk_state,
                        uc,
                        format!("could not read return address at {esp:#010x}: {error:?}"),
                    );
                    return;
                }
            };

            let kernel_entry = entry.dll == KERNEL_DLL;
            if kernel_entry && entry.name.as_str() == Some(THREAD_RETURN) {
                // A thread's start routine returned: the thread ends.
                let code = uc.reg_read(Reg::EAX).unwrap_or(0) as u32;
                let mut state = thunk_state.borrow_mut();
                if state.running.len() > 1 {
                    state.thread_signal = Some(ThreadSignal::Exit(code));
                    drop(state);
                    let _ = uc.reg_write(Reg::EIP, THREAD_STOP as u64);
                } else {
                    state.exit_code = Some(code);
                    drop(state);
                    let _ = uc.emu_stop();
                }
                return;
            }
            let x64 = thunk_state.borrow().x64;
            let arg_count = if kernel_entry {
                Some(2)
            } else if x64 {
                thunk_calls.arg_count64(&entry.dll, &entry.name)
            } else {
                thunk_calls.arg_count(&entry.dll, &entry.name)
            };
            let Some(arg_count) = arg_count else {
                // Exploring a program: OTW_STUB_MISSING lets unimplemented
                // functions return 0 (and says which), to see what comes next.
                if x64 && std::env::var_os("OTW_STUB_MISSING").is_some() {
                    eprintln!("[stub] {} returns 0", thunk_calls.describe(&entry.dll, &entry.name));
                    let mut bytes = [0u8; 8];
                    let _ = uc.mem_read(esp as u64, &mut bytes);
                    let _ = uc.reg_write(Reg::RAX, 0);
                    let _ = uc.reg_write(Reg::RSP, esp as u64 + 8);
                    let _ = uc.reg_write(Reg::RIP, u64::from_le_bytes(bytes));
                    return;
                }
                abort(
                    &thunk_state,
                    uc,
                    format!(
                        "unimplemented import {}",
                        thunk_calls.describe(&entry.dll, &entry.name)
                    ),
                );
                return;
            };

            // x64: the first four arguments in RCX, RDX, R8 and R9, the rest
            // on the stack above the return address and the 32-byte home
            // area. 32-bit: all on the stack.
            let mut wide_args: Vec<u64> = Vec::with_capacity(arg_count);
            for index in 0..arg_count {
                let value = if x64 && index < 4 {
                    let register = [Reg::RCX, Reg::RDX, Reg::R8, Reg::R9][index];
                    uc.reg_read(register).map_err(|error| (esp, error))
                } else if x64 {
                    let arg_va = esp.wrapping_add(8 + index as u32 * 8);
                    let mut bytes = [0u8; 8];
                    uc.mem_read(arg_va as u64, &mut bytes).map(|()| u64::from_le_bytes(bytes)).map_err(|error| (arg_va, error))
                } else {
                    let arg_va = esp.wrapping_add(4 + index as u32 * 4);
                    read_guest_u32(uc, arg_va).map(u64::from).map_err(|error| (arg_va, error))
                };
                match value {
                    Ok(value) => wide_args.push(value),
                    Err((arg_va, error)) => {
                        abort(
                            &thunk_state,
                            uc,
                            format!("could not read argument {index} at {arg_va:#010x}: {error:?}"),
                        );
                        return;
                    }
                }
            }
            let args: Vec<u32> = wide_args.iter().map(|&value| value as u32).collect();

            if trace_api {
                eprintln!(
                    "api {} esp={esp:#010x} ret={return_address:#010x} args={:08x?}",
                    thunk_calls.describe(&entry.dll, &entry.name),
                    args
                );
            }

            let outcome = {
                let mut memory = UnicornGuestMemory {
                    uc,
                    run: Some(&thunk_state),
                };
                if kernel_entry && x64 {
                    // The 64-bit exception dispatcher is the subsystem's
                    // (it unwinds through the modules' function tables).
                    let call_args = super::syscall::Args::new(wide_args.clone(), true);
                    let name = super::pe::ImportName::ByName("OTW_DispatchException64".into());
                    thunk_calls.call(&mut memory, "KERNEL32.dll", &name, &call_args, return_address)
                } else if kernel_entry {
                    // The exception dispatcher: (record, context) on the stack.
                    Some(super::eh::dispatcher(&mut memory, args[0], args[1]))
                } else {
                    let call_args = super::syscall::Args::new(wide_args.clone(), x64);
                    thunk_calls.call(&mut memory, &entry.dll, &entry.name, &call_args, return_address)
                }
            };
            // A nested guest callback may have exited the process or faulted.
            {
                let state = thunk_state.borrow();
                if state.exit_code.is_some() || state.error.is_some() {
                    drop(state);
                    let _ = uc.emu_stop();
                    return;
                }
            }
            // The thread comes off the CPU: stop its run here (to make the
            // same call again when it next runs, or for good).
            {
                let state = thunk_state.borrow();
                if let Some(signal) = state.thread_signal {
                    drop(state);
                    if matches!(signal, ThreadSignal::Suspend) {
                        thunk_state.borrow_mut().yield_eip = address as u32;
                    }
                    let _ = uc.reg_write(Reg::EIP, THREAD_STOP as u64);
                    return;
                }
            }
            // A message wait yielded the CPU to other programs: stop here
            // and run the same API call again when resumed.
            {
                let mut state = thunk_state.borrow_mut();
                if state.yielding {
                    state.yielding = false;
                    state.yielded = true;
                    state.yield_eip = address as u32;
                    drop(state);
                    // emu_stop is ignored after an EIP write; reaching the
                    // run's end address stops it instead.
                    let _ = uc.reg_write(Reg::EIP, YIELD_ADDRESS as u64);
                    return;
                }
            }
            // Exception handling found a handler: continue there if its
            // frame is on this level's stack, otherwise leave this level.
            {
                let mut state = thunk_state.borrow_mut();
                if let Some(resume) = state.resume {
                    if std::env::var_os("OTW_TRACE_EH").is_some() {
                        eprintln!(
                            "[eh] cpu: resume esp={:#x} at {} levels={:x?} -> {}",
                            resume.esp,
                            entry.name.display(),
                            state.levels,
                            if resume_belongs_here(&state, &resume) {
                                "apply"
                            } else {
                                "unwind"
                            }
                        );
                    }
                    if resume_belongs_here(&state, &resume) {
                        state.resume = None;
                        state.api_calls += 1;
                        drop(state);
                        if let Err(error) = apply_resume_full(uc, &thunk_state, &resume) {
                            abort(
                                &thunk_state,
                                uc,
                                format!("could not resume after exception: {error:?}"),
                            );
                        }
                    } else {
                        // Finish just this callback level: every nested
                        // emulation runs until the sentinel, whereas
                        // emu_stop would also stop the outer levels.
                        drop(state);
                        let _ = uc.reg_write(Reg::EIP, SENTINEL_BASE as u64);
                    }
                    return;
                }
            }
            let Some(outcome) = outcome else {
                abort(
                    &thunk_state,
                    uc,
                    format!(
                        "API dispatch failed for {}!{}",
                        entry.dll,
                        entry.name.display()
                    ),
                );
                return;
            };
            if trace_results {
                eprintln!(
                    "ret {} = {:#x} (ebx={:x} esi={:x} edi={:x} ebp={:x})",
                    thunk_calls.describe(&entry.dll, &entry.name),
                    outcome.value,
                    uc.reg_read(Reg::EBX).unwrap_or(0),
                    uc.reg_read(Reg::ESI).unwrap_or(0),
                    uc.reg_read(Reg::EDI).unwrap_or(0),
                    uc.reg_read(Reg::EBP).unwrap_or(0)
                );
            }

            if let Some(exit_code) = outcome.exit_code {
                if let Err(error) = uc.reg_write(if x64 { Reg::RAX } else { Reg::EAX }, outcome.value as u64) {
                    abort(&thunk_state, uc, format!("could not write EAX: {error:?}"));
                    return;
                }
                {
                    let mut state = thunk_state.borrow_mut();
                    state.api_calls += 1;
                    state.exit_code = Some(exit_code);
                }
                let _ = uc.emu_stop();
                return;
            }

            // stdcall pops the arguments; in x64 the caller does.
            let register_writes = if x64 {
                let value = outcome.wide.unwrap_or_else(|| super::syscall::widen(outcome.value));
                let return_address = {
                    let mut bytes = [0u8; 8];
                    let _ = uc.mem_read(esp as u64, &mut bytes);
                    u64::from_le_bytes(bytes)
                };
                uc.reg_write(Reg::RAX, value)
                    .and_then(|()| uc.reg_write(Reg::RSP, esp as u64 + 8))
                    .and_then(|()| uc.reg_write(Reg::RIP, return_address))
            } else {
                let restored_esp = esp.wrapping_add(4 + outcome.arg_count as u32 * 4);
                uc.reg_write(Reg::EAX, outcome.value as u64)
                    .and_then(|()| uc.reg_write(Reg::ESP, restored_esp as u64))
                    .and_then(|()| uc.reg_write(Reg::EIP, return_address as u64))
            };
            if let Err(error) = register_writes {
                abort(
                    &thunk_state,
                    uc,
                    format!("could not restore guest registers: {error:?}"),
                );
                return;
            }

            thunk_state.borrow_mut().api_calls += 1;
        },
    )
    .map_err(|error| RunError::Unicorn("installing IAT hook", error))?;

    // KERNEL32's private registry/environment helpers enter protected-mode VxD
    // services through a far call through bffbc004. There is no real VMM in the
    // guest, so supply the small set of results those helpers need. The far
    // call is skipped; its net stack effect is zero because the VxD returns to
    // the following instruction.
    for &gate in &VXD_GATES {
        uc.add_code_hook(gate as u64, gate as u64, move |uc, address, size| {
            let service = uc.reg_read(Reg::EAX).unwrap_or(0) as u32;
            let esp = uc.reg_read(Reg::ESP).unwrap_or(0) as u32;
            let mut result = 0u32;

            if gate == 0xbff7_13db {
                // This wrapper's `pop [esp]` replaced the service number with
                // the original return address and consumed one stack slot.
                // Returning directly here avoids the wrapper's later lookups
                // into the real VMM's VM database, which the VM does not have.
                if service == 0x0001_0011 {
                    // VMM registry open: bff82d64 passes the output handle
                    // pointer as the third argument after the shuffle.
                    if let Ok(handle_ptr) = read_guest_u32(uc, esp.wrapping_add(12)) {
                        let _ = uc.mem_write(handle_ptr as u64, &1u32.to_le_bytes());
                    }
                } else if service == 0x0001_001a {
                    // VMM registry query: report "value not present".
                    result = 1;
                }
                if let Ok(return_address) = read_guest_u32(uc, esp) {
                    let _ = uc.reg_write(Reg::EAX, result as u64);
                    let _ = uc.reg_write(Reg::ESP, esp.wrapping_add(4) as u64);
                    let _ = uc.reg_write(Reg::EIP, return_address as u64);
                    return;
                }
            } else if service == 0x0001_001a {
                result = 1;
            }

            let _ = uc.reg_write(Reg::EAX, result as u64);
            let _ = uc.reg_write(Reg::EIP, address + size as u64);
        })
        .map_err(|error| RunError::Unicorn("installing VxD gate hook", error))?;
    }

    let invalid_state = state.clone();
    let invalid_trace = code_trace.clone();
    // Debugging: OTW_WATCH=address reports every write to that dword.
    if let Some(watch) = std::env::var("OTW_WATCH")
        .ok()
        .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
    {
        uc.add_mem_hook(HookType::MEM_WRITE, watch as u64, watch as u64 + 3, move |uc, _, address, size, value| {
            let eip = uc.reg_read(Reg::EIP).unwrap_or(0);
            eprintln!("[watch] write {value:#x} ({size} bytes) at {address:#x} from eip={eip:#x}");
            true
        })
        .map_err(|error| RunError::Unicorn("installing the write watch", error))?;
    }
    uc.add_mem_hook(
        HookType::MEM_READ_UNMAPPED
            | HookType::MEM_WRITE_UNMAPPED
            | HookType::MEM_FETCH_UNMAPPED
            | HookType::MEM_READ_PROT
            | HookType::MEM_WRITE_PROT
            | HookType::MEM_FETCH_PROT,
        0,
        u32::MAX as u64,
        move |uc, access, address, size, _value| {
            if trace_code {
                eprintln!("last instructions before invalid memory access {access:?} at {address:#010x}, size {size}:");
                for line in invalid_trace.borrow().iter() {
                    eprintln!("{line}");
                }
            }
            // Stop this emulation level with an error; `emulate` raises
            // the fault in the guest as an access violation.
            let write = matches!(access, MemType::WRITE_UNMAPPED | MemType::WRITE_PROT);
            let eip = uc.reg_read(Reg::EIP).unwrap_or(0) as u32;
            invalid_state.borrow_mut().fault = Some(Fault {
                code: crate::kernel::eh::STATUS_ACCESS_VIOLATION,
                eip,
                params: [write as u32, address as u32],
                param_count: 2,
            });
            false
        },
    )
    .map_err(|error| RunError::Unicorn("installing memory-fault hook", error))?;

    // CPU exceptions. #DE is raised in the guest; anything else (int3,
    // stray software interrupts) is a fatal VM error.
    let intr_state = state.clone();
    uc.add_intr_hook(move |uc, intno| {
        let eip = uc.reg_read(Reg::EIP).unwrap_or(0) as u32;
        if intno == 0 {
            intr_state.borrow_mut().fault = Some(Fault {
                code: crate::kernel::eh::STATUS_INTEGER_DIVIDE_BY_ZERO,
                eip,
                params: [0, 0],
                param_count: 0,
            });
            if deliver_fault(uc, &intr_state) {
                return;
            }
        }
        if intr_state.borrow().error.is_none() {
            abort(
                &intr_state,
                uc,
                format!("unhandled CPU interrupt {intno} at {eip:#010x}"),
            );
        } else {
            let _ = uc.emu_stop();
        }
    })
    .map_err(|error| RunError::Unicorn("installing interrupt hook", error))?;

    // Real MFC DLLs initialise their global state in DllMain. Call it with
    // DLL_PROCESS_ATTACH before handing control to the executable. DllMain is
    // stdcall(HMODULE, DWORD, LPVOID); returning lands on the sentinel `ret`.
    for module in process.modules.iter().skip(1) {
        if module.image.entry_rva == 0 {
            continue;
        }
        // Win95 system DLL PE entry points are not ordinary DllMains: KERNEL32
        // in particular expects a private loader initialisation block. Calling
        // them with documented DllMain arguments leaves internal tables null
        // and faults. Their exports are still genuine; only these loader-
        // private hooks are skipped.
        const SYSTEM_DLLS: &[&str] = &[
            "kernel32.dll",
            "user32.dll",
            "gdi32.dll",
            "advapi32.dll",
            "comctl32.dll",
            "shell32.dll",
            "comdlg32.dll",
        ];
        if SYSTEM_DLLS
            .iter()
            .any(|name| module.name.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let frame = STACK_TOP - if x64 { 0x48 } else { 0x40 };
        let writes = if x64 {
            // x64: the arguments in RCX, RDX, R8; a 64-bit return address
            // with the callee's shadow space above it.
            uc.mem_write(frame as u64, &(SENTINEL_BASE as u64).to_le_bytes())
                .and_then(|()| uc.reg_write(Reg::RCX, module.image.image_base as u64))
                .and_then(|()| uc.reg_write(Reg::RDX, 1))
                .and_then(|()| uc.reg_write(Reg::R8, 0))
        } else {
            uc.mem_write(frame as u64, &SENTINEL_BASE.to_le_bytes())
                .and_then(|()| uc.mem_write((frame + 4) as u64, &module.image.image_base.to_le_bytes()))
                .and_then(|()| uc.mem_write((frame + 8) as u64, &1u32.to_le_bytes()))
                .and_then(|()| uc.mem_write((frame + 12) as u64, &0u32.to_le_bytes()))
        };
        if let Err(error) = writes {
            return Err(RunError::Unicorn("building DllMain frame", error));
        }
        if let Err(error) = uc.reg_write(Reg::ESP, frame as u64) {
            return Err(RunError::Unicorn("setting DllMain stack", error));
        }
        if let Err(error) = emulate(
            &mut uc,
            &state,
            module.image.entry_va(),
            SENTINEL_BASE,
            TIMEOUT_SECONDS * SECOND_SCALE,
            MAX_INSTRUCTIONS,
        ) {
            if let Some(error) = &state.borrow().error {
                return Err(RunError::Guest(error.clone()));
            }
            if trace_code {
                eprintln!("last instructions before DllMain emulation error:");
                for line in code_trace.borrow().iter() {
                    eprintln!("{line}");
                }
            }
            return Err(RunError::Unicorn("running DllMain", error));
        }
        if let Some(error) = &state.borrow().error {
            return Err(RunError::Guest(error.clone()));
        }
    }

    let _ = uc.reg_write(Reg::ESP, STACK_TOP as u64);
    let _ = uc.reg_write(Reg::EBP, 0);
    if let Err(error) = uc.reg_write(Reg::EIP, image.entry_va() as u64) {
        return Err(RunError::Unicorn("resetting entry EIP", error));
    }
    Ok(Process {
        uc,
        state,
        code_trace,
        trace_code,
        entry: image.entry_va(),
        started: false,
    })
}
