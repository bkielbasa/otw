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
/// NTDLL's module handle in a 64-bit process: a page of PE headers
/// (without exports; GetProcAddress serves the functions) at the top of
/// the address space, clear of large reservations.
pub const NTDLL_HANDLE: u32 = 0xFFF0_0000;
/// The size of NTDLL's image.
pub const NTDLL_SIZE: u32 = 0x1_0000;

/// More heap for 64-bit processes, whose programs allocate far more.
pub const HEAP64_EXTRA_BASE: u32 = 0x4000_0000;
pub const HEAP64_EXTRA_SIZE: u32 = 0x1000_0000;

/// Memory several processes map (a section backed by the paging file):
/// page-aligned host memory that each process's CPU maps directly, so what
/// one writes the others see at once.
pub struct SharedMemory {
    pointer: *mut u8,
    size: usize,
}

// SAFETY: the bytes are only reached through guest mappings and raw
// copies; like memory shared between Windows processes, it is up to the
// programs to synchronise.
unsafe impl Send for SharedMemory {}
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
    /// `size` zeroed bytes, rounded up to whole pages.
    pub fn new(size: usize) -> Option<std::sync::Arc<Self>> {
        let size = (size.max(1) + 0xFFF) & !0xFFF;
        let layout = std::alloc::Layout::from_size_align(size, 0x1000).ok()?;
        // SAFETY: the layout has a non-zero size.
        let pointer = unsafe { std::alloc::alloc_zeroed(layout) };
        (!pointer.is_null()).then(|| std::sync::Arc::new(Self { pointer, size }))
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// The host address of byte `offset`.
    pub fn pointer(&self, offset: usize) -> *mut u8 {
        // SAFETY: callers keep offsets inside the allocation.
        unsafe { self.pointer.add(offset.min(self.size)) }
    }

    /// A copy of `len` bytes from `offset`.
    pub fn read(&self, offset: usize, len: usize) -> Vec<u8> {
        let end = (offset + len).min(self.size);
        let start = offset.min(end);
        // SAFETY: the range is inside the allocation.
        unsafe { std::slice::from_raw_parts(self.pointer.add(start), end - start).to_vec() }
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        if let Ok(layout) = std::alloc::Layout::from_size_align(self.size, 0x1000) {
            // SAFETY: allocated in `new` with this layout.
            unsafe { std::alloc::dealloc(self.pointer, layout) };
        }
    }
}

/// How a process with nothing to do waits: the subsystem may wake it
/// early when another process does something it may wait for.
static IDLE_WAIT: std::sync::OnceLock<fn(std::time::Duration)> = std::sync::OnceLock::new();

pub fn set_idle_wait(wait: fn(std::time::Duration)) {
    let _ = IDLE_WAIT.set(wait);
}

/// Wait up to `timeout` with nothing to do.
pub fn idle_wait(timeout: std::time::Duration) {
    match IDLE_WAIT.get() {
        Some(wait) => wait(timeout),
        None => std::thread::sleep(timeout),
    }
}

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

    fn read_u64(&mut self, va: u32) -> Option<u64> {
        let bytes = self.read_bytes(va, 8)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    fn write_u64(&mut self, va: u32, value: u64) -> Option<()> {
        self.write_bytes(va, &value.to_le_bytes())
    }

    /// The size of a pointer (and of handles and `*_PTR` values) in the
    /// process: 8 in a 64-bit one.
    fn ptr_size(&self) -> u32 {
        4
    }

    /// Read a pointer-sized value.
    fn read_ptr(&mut self, va: u32) -> Option<u64> {
        if self.ptr_size() == 8 {
            self.read_u64(va)
        } else {
            self.read_u32(va).map(u64::from)
        }
    }

    /// Write a pointer-sized value (a handle, an address, an interface
    /// pointer): all of it, so a 64-bit program's variable holds no stale
    /// upper half.
    fn write_ptr(&mut self, va: u32, value: u64) -> Option<()> {
        if self.ptr_size() == 8 {
            self.write_u64(va, value)
        } else {
            self.write_u32(va, value as u32)
        }
    }

    /// Read guest memory at a full 64-bit address (memory a 64-bit program
    /// has above 4 GiB).
    fn read_bytes_at(&mut self, va: u64, len: usize) -> Option<Vec<u8>> {
        self.read_bytes(u32::try_from(va).ok()?, len)
    }

    /// Write guest memory at a full 64-bit address.
    fn write_bytes_at(&mut self, va: u64, bytes: &[u8]) -> Option<()> {
        self.write_bytes(u32::try_from(va).ok()?, bytes)
    }

    /// Map `size` bytes at exactly `va` if that range is free.
    fn map_exact(&mut self, _va: u32, _size: u32) -> bool {
        false
    }

    /// Unmap a range mapped with `map_region` or `map_exact`.
    fn unmap(&mut self, _va: u32, _size: u32) -> bool {
        false
    }

    /// Map `size` zeroed bytes at `va`, which may lie above 4 GiB (memory a
    /// 64-bit program reserves past what OTW's 32-bit addresses reach). With
    /// `alias`, the same bytes also replace the reserved low range there, so
    /// a pointer to them that reaches OTW cut to 32 bits still finds them.
    fn map_shared(&mut self, _va: u64, _size: u64, _alias: Option<u32>) -> bool {
        false
    }

    /// Unmap a range mapped with `map_shared` (and its alias, which is left
    /// unmapped).
    fn unmap_shared(&mut self, _va: u64) -> bool {
        false
    }

    /// Map `size` bytes of `memory` from `offset` (page-aligned) somewhere
    /// free from `preferred` up: a view of a section other processes may
    /// map too. Returns the address.
    fn map_section(&mut self, _memory: std::sync::Arc<SharedMemory>, _offset: usize, _size: u32, _preferred: u32) -> Option<u32> {
        None
    }

    /// Unmap a view mapped with `map_section`.
    fn unmap_section(&mut self, _va: u32) -> bool {
        false
    }

    /// The mapped ranges, as (first, last) addresses inclusive.
    fn mapped(&mut self) -> Vec<(u64, u64)> {
        Vec::new()
    }

    /// Add a static TLS template (a DLL loaded at run time with
    /// `__declspec(thread)` data): every thread gets a copy. Returns the
    /// module's TLS index.
    fn add_tls(&mut self, _template: &[u8]) -> Option<u32> {
        None
    }

    /// The low 64 bits of XMM register `index` (an x64 floating-point
    /// argument).
    fn xmm(&mut self, _index: usize) -> u64 {
        0
    }

    /// Set the low 64 bits of XMM0 (an x64 floating-point result).
    fn set_xmm0(&mut self, _bits: u64) {}

    /// Call guest code `func` as `stdcall(args...)` and return its EAX. Only
    /// a running CPU backend can do this; other views return `None`.
    fn call_guest(&mut self, func: u32, args: &[u32]) -> Option<u32> {
        self.call_guest_ex(func, args, None, None)
    }

    /// Call guest code with full 64-bit arguments, returning all of RAX (EAX
    /// in a 32-bit process).
    fn call_guest_wide(&mut self, func: u32, args: &[u64]) -> Option<u64> {
        let narrow: Vec<u32> = args.iter().map(|&value| value as u32).collect();
        self.call_guest(func, &narrow).map(u64::from)
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

    /// Resume with every register of a 64-bit thread set.
    fn request_resume_full(&mut self, _registers: FullRegisters) {}

    /// Set the running thread's general and XMM registers (not RIP).
    fn set_full_registers(&mut self, _registers: &FullRegisters) {}

    /// The running thread's registers (64-bit processes).
    fn full_registers(&mut self) -> Option<FullRegisters> {
        None
    }

    /// The stack pointers at which the subsystem called into guest code
    /// (outermost first): where each callback's caller continues.
    fn callback_levels(&self) -> Vec<u32> {
        Vec::new()
    }

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

/// Every register of a 64-bit thread that exception handling saves and
/// restores (CONTEXT order: RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI, R8-R15).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FullRegisters {
    pub gpr: [u64; 16],
    pub rip: u64,
    pub eflags: u64,
    pub mxcsr: u32,
    pub xmm: [[u8; 16]; 16],
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
    /// The full 64-bit result (RAX) when the function produced one; a
    /// 64-bit caller otherwise gets `value` widened (see
    /// [`crate::kernel::syscall::widen`]).
    pub wide: Option<u64>,
    /// Number of `stdcall` bytes-worth of arguments the backend must pop.
    pub arg_count: usize,
    /// Set when the guest asked the VM to terminate.
    pub exit_code: Option<u32>,
}

impl ApiOutcome {
    pub const fn new(value: u32, arg_count: usize) -> Self {
        Self {
            value,
            wide: None,
            arg_count,
            exit_code: None,
        }
    }

    /// A result with all 64 bits given (for a 32-bit caller, the low half).
    pub const fn wide(value: u64, arg_count: usize) -> Self {
        Self {
            value: value as u32,
            wide: Some(value),
            arg_count,
            exit_code: None,
        }
    }

    /// A signed result: -1 is all ones in RAX too.
    pub const fn signed(value: i32, arg_count: usize) -> Self {
        Self::wide(value as i64 as u64, arg_count)
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
