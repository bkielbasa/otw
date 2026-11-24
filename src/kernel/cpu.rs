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
    image::{Image, EXCEPTION_DISPATCHER, KERNEL_DLL},
    memory::{self, GuestMemory, HEAP_BASE, HEAP_SIZE},
    syscall::SystemCalls,
};

pub const STACK_BASE: u32 = 0x0010_0000;
pub const STACK_SIZE: u32 = 0x0010_0000;
pub const STACK_TOP: u32 = STACK_BASE + STACK_SIZE - 0x100;

/// Synthetic Win95 pages sit above genuine high-based DLLs. OLE32 loads at
/// `0x7ff60000` with a `0x86000` image, ending at `0x7ffe6000`, so the TIB,
/// loader state and GDT must stay above that range.
///
/// Minimal Win95-style thread information block. MSVC CRT startup code reads
/// and writes `FS:[0]` to install its structured exception handler chain.
pub const TIB_BASE: u32 = 0x7FFF_0000;
pub const TIB_SIZE: u32 = 0x1000;

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
const SENTINEL_BASE: u32 = 0x6FFF_0000;
/// Top-level emulation runs until EIP reaches this address; a program that
/// yields jumps here, which ends the run like the sentinel ends callbacks.
const YIELD_ADDRESS: u32 = SENTINEL_BASE + 0x100;
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
    let read = |uc: &Unicorn<'_, ()>, register| uc.reg_read(register).unwrap_or(0) as u32;
    let esp = read(uc, Reg::ESP);
    // Below the faulting frame: return address, &record, &context, record, context.
    let base = esp.wrapping_sub(0x40 + 12 + RECORD_SIZE + CONTEXT_SIZE) & !0xF;
    if dispatcher == 0 || !(STACK_BASE + 0x1000..STACK_TOP).contains(&base) {
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

/// Deepest Rust-to-guest callback nesting. Unicorn itself allows 64 levels.
const MAX_CALLBACK_DEPTH: usize = 48;

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
        let mut saved = [0u64; SAVED_REGISTERS.len()];
        for (slot, register) in SAVED_REGISTERS.iter().enumerate() {
            saved[slot] = self.uc.reg_read(*register).ok()?;
        }
        let esp = saved[7] as u32;
        // Leave a gap so the interrupted frame's locals are never touched.
        let frame = (esp.wrapping_sub(0x80 + 4 * (args.len() as u32 + 1))) & !0xF;
        let mut bytes = Vec::with_capacity(4 * (args.len() + 1));
        bytes.extend_from_slice(&SENTINEL_BASE.to_le_bytes());
        for arg in args {
            bytes.extend_from_slice(&arg.to_le_bytes());
        }
        self.uc.mem_write(frame as u64, &bytes).ok()?;
        self.uc.reg_write(Reg::ESP, frame as u64).ok()?;
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
        let eax = self.uc.reg_read(Reg::EAX).unwrap_or(0) as u32;
        for (slot, register) in SAVED_REGISTERS.iter().enumerate() {
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
            if let Err(error) = apply_resume(uc, &resume) {
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

    let mut uc = Unicorn::new(Arch::X86, Mode::MODE_32)
        .map_err(|error| RunError::Unicorn("creating the CPU", error))?;

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

    uc.mem_map(STACK_BASE as u64, STACK_SIZE as u64, Prot::ALL)
        .map_err(|error| RunError::Unicorn("mapping guest stack", error))?;

    uc.mem_map(HEAP_BASE as u64, HEAP_SIZE as u64, Prot::ALL)
        .map_err(|error| RunError::Unicorn("mapping guest heap", error))?;

    uc.mem_map(TIB_BASE as u64, TIB_SIZE as u64, Prot::ALL)
        .map_err(|error| RunError::Unicorn("mapping guest TIB", error))?;
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
    let fs_base = TIB_BASE;
    let fs_limit = TIB_SIZE - 1;
    gdt[24..32].copy_from_slice(&[
        fs_limit as u8,
        (fs_limit >> 8) as u8,
        fs_base as u8,
        (fs_base >> 8) as u8,
        (fs_base >> 16) as u8,
        0x92, // present, DPL 0, data segment, read/write
        0x40, // 32-bit segment, byte granularity, 4 KiB limit
        (fs_base >> 24) as u8,
    ]);
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

    let thunk_end = align_up(
        THUNK_BASE.wrapping_add((iat.thunk_count() as u32 + 1) * 4),
        PAGE_SIZE,
    );
    let thunk_size = thunk_end - THUNK_BASE;
    uc.mem_map(THUNK_BASE as u64, thunk_size as u64, Prot::ALL)
        .map_err(|error| RunError::Unicorn("mapping IAT thunks", error))?;
    uc.mem_write(THUNK_BASE as u64, &vec![0xC3; thunk_size as usize])
        .map_err(|error| RunError::Unicorn("writing IAT thunks", error))?;

    init_win95_state(process, &mut uc)?;

    uc.reg_write(Reg::ESP, STACK_TOP as u64)
        .map_err(|error| RunError::Unicorn("initialising ESP", error))?;
    uc.reg_write(Reg::EBP, 0)
        .map_err(|error| RunError::Unicorn("initialising EBP", error))?;

    let mut initial_state = RunState::default();
    initial_state.pump = pump;
    initial_state.dispatcher = process
        .entry_point(KERNEL_DLL, EXCEPTION_DISPATCHER)
        .unwrap_or(0);
    let state = Rc::new(RefCell::new(initial_state));

    let thunk_state = state.clone();
    let thunk_iat = iat.clone();
    let thunk_calls = syscalls;
    let trace_api = std::env::var_os("OTW_TRACE_API").is_some();
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
                if trace.len() >= 4096 {
                    trace.pop_front();
                }
                trace.push_back(format!(
                    "eip={address:#010x} bytes={bytes:<24} eax={:08x} ebx={:08x} ecx={:08x} edx={:08x} esi={:08x} edi={:08x} ebp={:08x} esp={:08x}",
                    values[0] as u32,
                    values[1] as u32,
                    values[2] as u32,
                    values[3] as u32,
                    values[4] as u32,
                    values[5] as u32,
                    values[6] as u32,
                    values[7] as u32,
                ));
            },
        )
        .map_err(|error| RunError::Unicorn("installing instruction trace hook", error))?;
    }
    uc.add_code_hook(
        THUNK_BASE as u64,
        (thunk_end - 1) as u64,
        move |uc, address, _size| {
            let Some(entry) = thunk_iat.entry_for_thunk(address as u32) else {
                return;
            };
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
            let arg_count = if kernel_entry {
                Some(2)
            } else {
                thunk_calls.arg_count(&entry.dll, &entry.name)
            };
            let Some(arg_count) = arg_count else {
                abort(
                    &thunk_state,
                    uc,
                    format!(
                        "unimplemented import {}!{}",
                        entry.dll,
                        entry.name.display()
                    ),
                );
                return;
            };

            let mut args = Vec::with_capacity(arg_count + 1);
            for index in 0..arg_count {
                let arg_va = esp.wrapping_add(4 + index as u32 * 4);
                match read_guest_u32(uc, arg_va) {
                    Ok(value) => args.push(value),
                    Err(error) => {
                        abort(
                            &thunk_state,
                            uc,
                            format!("could not read argument {index} at {arg_va:#010x}: {error:?}"),
                        );
                        return;
                    }
                }
            }

            if trace_api {
                eprintln!(
                    "api {}!{} esp={esp:#010x} ret={return_address:#010x} args={:08x?}",
                    entry.dll,
                    entry.name.display(),
                    args
                );
            }

            let outcome = {
                let mut memory = UnicornGuestMemory {
                    uc,
                    run: Some(&thunk_state),
                };
                if kernel_entry {
                    // The exception dispatcher: (record, context) on the stack.
                    Some(super::eh::dispatcher(&mut memory, args[0], args[1]))
                } else {
                    thunk_calls.call(&mut memory, &entry.dll, &entry.name, &args, return_address)
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
                        if let Err(error) = apply_resume(uc, &resume) {
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

            if let Some(exit_code) = outcome.exit_code {
                if let Err(error) = uc.reg_write(Reg::EAX, outcome.value as u64) {
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

            let restored_esp = esp.wrapping_add(4 + outcome.arg_count as u32 * 4);
            let register_writes = uc
                .reg_write(Reg::EAX, outcome.value as u64)
                .and_then(|()| uc.reg_write(Reg::ESP, restored_esp as u64))
                .and_then(|()| uc.reg_write(Reg::EIP, return_address as u64));
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
            "shlwapi.dll",
            "shdocvw.dll",
        ];
        if SYSTEM_DLLS
            .iter()
            .any(|name| module.name.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let frame = STACK_TOP - 0x40;
        let writes = uc
            .mem_write(frame as u64, &SENTINEL_BASE.to_le_bytes())
            .and_then(|()| uc.mem_write((frame + 4) as u64, &module.image.image_base.to_le_bytes()))
            .and_then(|()| uc.mem_write((frame + 8) as u64, &1u32.to_le_bytes()))
            .and_then(|()| uc.mem_write((frame + 12) as u64, &0u32.to_le_bytes()));
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
