//! The guest address space as the rest of the system sees it.
//!
//! [`GuestMemory`] is how code outside the CPU reads and writes a process's
//! memory and asks the CPU for services: calling guest code, unwinding to an
//! exception handler, yielding to other processes. System calls receive one
//! and return an [`ApiOutcome`].

/// Base of the guest heap used by LocalAlloc/GlobalAlloc/malloc.
pub const HEAP_BASE: u32 = 0x0080_0000;
/// Size of the guest heap (120 MiB, ending at 0x0800_0000).
pub const HEAP_SIZE: u32 = 0x0780_0000;

/// What became of a thread given the CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadRun {
    /// It waits again.
    Waiting,
    /// It ended with this exit code.
    Exited(u32),
}

/// Minimal guest-memory view available to API handlers.
pub trait GuestMemory {
    fn read_bytes(&mut self, va: u32, len: usize) -> Option<Vec<u8>>;
    fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Option<()>;
    fn write_u32(&mut self, va: u32, value: u32) -> Option<()>;

    fn read_u32(&mut self, va: u32) -> Option<u32> {
        let bytes = self.read_bytes(va, 4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Call guest code `func` as `stdcall(args...)` and return its EAX. Only
    /// a running CPU backend can do this; other views return `None`.
    fn call_guest(&mut self, func: u32, args: &[u32]) -> Option<u32> {
        self.call_guest_ex(func, args, None, None)
    }

    /// Like [`GuestMemory::call_guest`], optionally setting EBP (exception
    /// funclets address their frame through it) and ECX (`thiscall`).
    fn call_guest_ex(
        &mut self,
        _func: u32,
        _args: &[u32],
        _ebp: Option<u32>,
        _ecx: Option<u32>,
    ) -> Option<u32> {
        None
    }

    /// Map `size` bytes of zeroed memory at `preferred` when that range is
    /// free, otherwise at the lowest free 64 KiB-aligned range above it.
    /// Returns the address, or `None` without a running CPU or space.
    fn map_region(&mut self, _preferred: u32, _size: u32) -> Option<u32> {
        None
    }

    /// Create a thread of this process that will run `entry(param)` on a
    /// stack of its own. Returns its ID; it runs once given the CPU.
    fn create_thread(&mut self, _entry: u32, _param: u32, _stack_size: u32) -> Option<u32> {
        None
    }

    /// The running thread (1 is the process's first thread).
    fn current_thread(&self) -> u32 {
        1
    }

    /// The running thread's TIB (FS:0).
    fn current_tib(&self) -> u32 {
        crate::kernel::cpu::TIB_BASE
    }

    /// Threads that are not on the CPU and have not ended.
    fn waiting_threads(&self) -> Vec<u32> {
        Vec::new()
    }

    /// Give a waiting thread the CPU until it waits again or ends.
    fn run_thread(&mut self, _id: u32) -> Option<ThreadRun> {
        None
    }

    /// Take the running thread off the CPU when this call returns; the call
    /// is made again when the thread next runs. False when the thread cannot
    /// stop here: it is the first thread, or inside a callback.
    fn suspend_thread(&mut self) -> bool {
        false
    }

    /// End the running thread (not the first one) when this call returns.
    fn end_thread(&mut self, _code: u32) -> bool {
        false
    }

    /// Register values at the current API call (ESP points at the return
    /// address).
    fn registers(&mut self) -> Option<Registers> {
        None
    }

    /// Ask the CPU backend to continue at `resume` instead of returning from
    /// the current API call, unwinding nested callbacks as needed.
    fn request_resume(&mut self, _resume: Resume) {}

    fn resume_pending(&self) -> bool {
        false
    }

    /// Guest address of the user-mode exception dispatcher, where raised
    /// exceptions continue.
    fn exception_dispatcher(&self) -> Option<u32> {
        None
    }

    /// Whether the program may give the CPU to other programs now: it runs
    /// on a shared desktop and is at its top level (not inside a callback).
    fn can_yield(&self) -> bool {
        false
    }

    /// Give the CPU to other programs: the current API call is abandoned and
    /// made again when the program next runs. Returns false if not allowed.
    fn yield_now(&mut self) -> bool {
        false
    }

    /// Let the host window system deliver pending input. `block` allows a
    /// short wait when the guest has nothing else to do.
    fn pump(&mut self, _block: bool) {}

    /// Whether a host window system is attached.
    fn has_host(&self) -> bool {
        false
    }

    /// Whether the process is exiting (or faulted) inside a callback.
    fn stopping(&self) -> bool {
        false
    }
}

/// General registers as seen by an API thunk.
#[derive(Clone, Copy, Debug, Default)]
pub struct Registers {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
    pub esi: u32,
    pub edi: u32,
    pub ebp: u32,
    pub esp: u32,
}

/// A non-local transfer of control requested by exception handling.
/// `regs` restores eax, ebx, ecx, edx, esi, edi when present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resume {
    pub eip: u32,
    pub esp: u32,
    pub ebp: u32,
    pub regs: Option<[u32; 6]>,
}

/// Result of one emulated API call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApiOutcome {
    /// Value returned to the guest in EAX.
    pub value: u32,
    /// Number of `stdcall` bytes-worth of arguments the backend must pop.
    pub arg_count: usize,
    /// Set when the guest asked the VM to terminate.
    pub exit_code: Option<u32>,
}

impl ApiOutcome {
    pub const fn new(value: u32, arg_count: usize) -> Self {
        Self {
            value,
            arg_count,
            exit_code: None,
        }
    }

    /// Result of a `cdecl` call: arguments are popped by the guest caller.
    pub const fn cdecl(value: u32) -> Self {
        Self::new(value, 0)
    }
}

/// Read a NUL-terminated byte string (at most 4 KiB).
pub fn read_cstr<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> Vec<u8> {
    if va == 0 {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut offset = 0u32;
    while offset < 4096 {
        let Some(bytes) = memory.read_bytes(va.wrapping_add(offset), 32) else {
            break;
        };
        for byte in bytes {
            if byte == 0 {
                return result;
            }
            result.push(byte);
        }
        offset = offset.wrapping_add(32);
    }
    result
}

pub fn write_cstr<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, bytes: &[u8]) -> Option<()> {
    if va == 0 {
        return Some(());
    }
    let mut buffer = Vec::with_capacity(bytes.len() + 1);
    buffer.extend_from_slice(bytes);
    buffer.push(0);
    memory.write_bytes(va, &buffer)
}
