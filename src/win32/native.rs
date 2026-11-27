//! Machine code for the KERNEL32 calls 64-bit programs make most: thread
//! local storage, the last error, critical sections, slim reader/writer
//! locks and WaitOnAddress. Bound in place of the import's system call,
//! they run in the program without entering the subsystem, as on Windows;
//! what they cannot finish alone (a lock another thread holds, a wake-up
//! someone waits for) goes on to the import's system call, a `jmp` at the
//! end of the code whose displacement is filled in where it is placed.
//!
//! What they share with the subsystem (sync.rs, mod.rs):
//! - the TEB (GS) holds the thread id at 0x48, the last error at 0x68 and
//!   the 64 TLS slots at 0x1480, as on Windows;
//! - a critical section holds its recursion count at +8, its owner's
//!   thread id at +12 and the number of threads the subsystem has waiting
//!   for it in the low half of LockSemaphore (+24; +16 in 32-bit ones);
//! - a slim reader/writer lock's first 32 bits hold an exclusive owner
//!   (bit 0), waiters in the subsystem (bit 1) and readers (from bit 2, 4
//!   each). Only the subsystem, under its lock, changes bit 1; code here
//!   takes the system call whenever it is set;
//! - the PEB holds the clock QueryPerformanceCounter reads (`PEB_CLOCK`),
//!   when the kernel publishes one.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

pub use crate::kernel::syscall::Native;

/// TEB offsets.
pub const TEB_THREAD_ID: u32 = 0x48;
pub const TEB_LAST_ERROR: u32 = 0x68;
pub const TEB_TLS_SLOTS: u32 = 0x1480;
pub const TLS_SLOTS: u32 = 64;

/// The clock in the PEB (from `PEB_CLOCK`): the time-stamp counter at
/// the kernel's start, its ticks per microsecond (0: no clock published,
/// the system call answers) and the microsecond (of the kernel's clock)
/// the process started at. QueryPerformanceCounter counts microseconds
/// since the process started, as the subsystem's own does.
pub const PEB_CLOCK: u32 = 0xC00;

/// Critical section fields (see above).
pub const CS_RECURSION: u32 = 8;
pub const CS_OWNER: u32 = 12;
/// Where a critical section counts its waiters.
pub const fn cs_waiters(ptr_size: u32) -> u32 {
    if ptr_size == 8 { 24 } else { 16 }
}

/// Slim reader/writer lock bits.
pub const SRW_EXCLUSIVE: u32 = 1;
pub const SRW_WAITERS: u32 = 2;
pub const SRW_READER: u32 = 4;

const GET_LAST_ERROR: &[u8] = &[
    0x65, 0x8B, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00, // mov eax, gs:[0x68]
    0xC3, // ret
];

const SET_LAST_ERROR: &[u8] = &[
    0x65, 0x89, 0x0C, 0x25, 0x68, 0x00, 0x00, 0x00, // mov gs:[0x68], ecx
    0xC3, // ret
];

const CURRENT_THREAD_ID: &[u8] = &[
    0x65, 0x8B, 0x04, 0x25, 0x48, 0x00, 0x00, 0x00, // mov eax, gs:[0x48]
    0xC3, // ret
];

const CURRENT_THREAD: &[u8] = &[
    0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, // mov rax, -2
    0xC3, // ret
];

const CURRENT_PROCESS: &[u8] = &[
    0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, // mov rax, -1
    0xC3, // ret
];

const TLS_GET: &[u8] = &[
    0x89, 0xC9, // mov ecx, ecx
    0x83, 0xF9, 0x40, // cmp ecx, 64
    0x73, 0x16, // jae bad
    0x65, 0x48, 0x8B, 0x04, 0xCD, 0x80, 0x14, 0x00, 0x00, // mov rax, gs:[rcx*8+0x1480]
    0x65, 0xC7, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov dword gs:[0x68], 0
    0xC3, // ret
    0x65, 0xC7, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00, 0x57, 0x00, 0x00, 0x00, // bad: mov dword gs:[0x68], 87
    0x31, 0xC0, // xor eax, eax
    0xC3, // ret
];

const TLS_SET: &[u8] = &[
    0x89, 0xC9, // mov ecx, ecx
    0x83, 0xF9, 0x40, // cmp ecx, 64
    0x73, 0x0F, // jae bad
    0x65, 0x48, 0x89, 0x14, 0xCD, 0x80, 0x14, 0x00, 0x00, // mov gs:[rcx*8+0x1480], rdx
    0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
    0xC3, // ret
    0x65, 0xC7, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00, 0x57, 0x00, 0x00, 0x00, // bad: mov dword gs:[0x68], 87
    0x31, 0xC0, // xor eax, eax
    0xC3, // ret
];

/// Take the section (owner 0 to this thread, recursion 1) or count one
/// more recursion; spin a while for another owner, then wait in the
/// subsystem.
const ENTER_CRITICAL_SECTION: &[u8] = &[
    0x65, 0x8B, 0x14, 0x25, 0x48, 0x00, 0x00, 0x00, // mov edx, gs:[0x48]
    0x41, 0xB8, 0x00, 0x01, 0x00, 0x00, // mov r8d, 256
    0x31, 0xC0, // again: xor eax, eax
    0xF0, 0x0F, 0xB1, 0x51, 0x0C, // lock cmpxchg [rcx+12], edx
    0x74, 0x13, // je taken
    0x39, 0xD0, // cmp eax, edx
    0x74, 0x17, // je mine
    0xF3, 0x90, // spin: pause
    0x41, 0xFF, 0xC8, // dec r8d
    0x74, 0x14, // jz slow
    0x83, 0x79, 0x0C, 0x00, // cmp dword [rcx+12], 0
    0x75, 0xF3, // jne spin
    0xEB, 0xE4, // jmp again
    0xC7, 0x41, 0x08, 0x01, 0x00, 0x00, 0x00, // taken: mov dword [rcx+8], 1
    0xC3, // ret
    0xFF, 0x41, 0x08, // mine: inc dword [rcx+8]
    0xC3, // ret
    0xE9, 0x00, 0x00, 0x00, 0x00, // slow: jmp EnterCriticalSection
];

/// Count a recursion down; at zero give the section up and, when the
/// subsystem has threads waiting for it, have it wake one.
const LEAVE_CRITICAL_SECTION: &[u8] = &[
    0x65, 0x8B, 0x14, 0x25, 0x48, 0x00, 0x00, 0x00, // mov edx, gs:[0x48]
    0x39, 0x51, 0x0C, // cmp [rcx+12], edx
    0x75, 0x10, // jne done
    0xFF, 0x49, 0x08, // dec dword [rcx+8]
    0x75, 0x0B, // jnz done
    0x31, 0xC0, // xor eax, eax
    0x87, 0x41, 0x0C, // xchg [rcx+12], eax
    0x83, 0x79, 0x18, 0x00, // cmp dword [rcx+24], 0
    0x75, 0x01, // jne slow
    0xC3, // done: ret
    0xE9, 0x00, 0x00, 0x00, 0x00, // slow: jmp LeaveCriticalSection
];

const TRY_ENTER_CRITICAL_SECTION: &[u8] = &[
    0x65, 0x8B, 0x14, 0x25, 0x48, 0x00, 0x00, 0x00, // mov edx, gs:[0x48]
    0x31, 0xC0, // xor eax, eax
    0xF0, 0x0F, 0xB1, 0x51, 0x0C, // lock cmpxchg [rcx+12], edx
    0x74, 0x07, // je taken
    0x39, 0xD0, // cmp eax, edx
    0x74, 0x10, // je mine
    0x31, 0xC0, // xor eax, eax
    0xC3, // ret
    0xC7, 0x41, 0x08, 0x01, 0x00, 0x00, 0x00, // taken: mov dword [rcx+8], 1
    0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
    0xC3, // ret
    0xFF, 0x41, 0x08, // mine: inc dword [rcx+8]
    0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
    0xC3, // ret
];

const SRW_ACQUIRE_EXCLUSIVE: &[u8] = &[
    0x41, 0xB8, 0x00, 0x01, 0x00, 0x00, // mov r8d, 256
    0xBA, 0x01, 0x00, 0x00, 0x00, // mov edx, 1
    0x31, 0xC0, // again: xor eax, eax
    0xF0, 0x0F, 0xB1, 0x11, // lock cmpxchg [rcx], edx
    0x74, 0x0E, // je done
    0xF3, 0x90, // spin: pause
    0x41, 0xFF, 0xC8, // dec r8d
    0x74, 0x08, // jz slow
    0x83, 0x39, 0x00, // cmp dword [rcx], 0
    0x75, 0xF4, // jne spin
    0xEB, 0xEA, // jmp again
    0xC3, // done: ret
    0xE9, 0x00, 0x00, 0x00, 0x00, // slow: jmp AcquireSRWLockExclusive
];

const SRW_ACQUIRE_SHARED: &[u8] = &[
    0x41, 0xB8, 0x00, 0x01, 0x00, 0x00, // mov r8d, 256
    0x8B, 0x01, // again: mov eax, [rcx]
    0xA9, 0x03, 0x00, 0x00, 0x00, // test eax, 3
    0x75, 0x0A, // jnz spin
    0x8D, 0x50, 0x04, // lea edx, [rax+4]
    0xF0, 0x0F, 0xB1, 0x11, // lock cmpxchg [rcx], edx
    0x75, 0xEE, // jne again
    0xC3, // ret
    0xF3, 0x90, // spin: pause
    0x41, 0xFF, 0xC8, // dec r8d
    0x75, 0xE6, // jnz again
    0xE9, 0x00, 0x00, 0x00, 0x00, // jmp AcquireSRWLockShared
];

const SRW_RELEASE_EXCLUSIVE: &[u8] = &[
    0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
    0x31, 0xD2, // xor edx, edx
    0xF0, 0x0F, 0xB1, 0x11, // lock cmpxchg [rcx], edx
    0x75, 0x01, // jne slow
    0xC3, // ret
    0xE9, 0x00, 0x00, 0x00, 0x00, // slow: jmp ReleaseSRWLockExclusive
];

const SRW_RELEASE_SHARED: &[u8] = &[
    0x8B, 0x01, // again: mov eax, [rcx]
    0xA9, 0x02, 0x00, 0x00, 0x00, // test eax, 2
    0x75, 0x0A, // jnz slow
    0x8D, 0x50, 0xFC, // lea edx, [rax-4]
    0xF0, 0x0F, 0xB1, 0x11, // lock cmpxchg [rcx], edx
    0x75, 0xEE, // jne again
    0xC3, // ret
    0xE9, 0x00, 0x00, 0x00, 0x00, // slow: jmp ReleaseSRWLockShared
];

const SRW_TRY_EXCLUSIVE: &[u8] = &[
    0xBA, 0x01, 0x00, 0x00, 0x00, // mov edx, 1
    0x31, 0xC0, // xor eax, eax
    0xF0, 0x0F, 0xB1, 0x11, // lock cmpxchg [rcx], edx
    0x0F, 0x94, 0xC0, // sete al
    0x0F, 0xB6, 0xC0, // movzx eax, al
    0xC3, // ret
];

const SRW_TRY_SHARED: &[u8] = &[
    0x8B, 0x01, // again: mov eax, [rcx]
    0xA9, 0x03, 0x00, 0x00, 0x00, // test eax, 3
    0x75, 0x0F, // jnz no
    0x8D, 0x50, 0x04, // lea edx, [rax+4]
    0xF0, 0x0F, 0xB1, 0x11, // lock cmpxchg [rcx], edx
    0x75, 0xEE, // jne again
    0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
    0xC3, // ret
    0x31, 0xC0, // no: xor eax, eax
    0xC3, // ret
];

/// WaitOnAddress(address, compare, size, timeout): a value that already
/// differs returns TRUE at once; else the subsystem waits.
const WAIT_ON_ADDRESS: &[u8] = &[
    0x49, 0x83, 0xF8, 0x04, // cmp r8, 4
    0x74, 0x24, // je four
    0x49, 0x83, 0xF8, 0x08, // cmp r8, 8
    0x74, 0x26, // je eight
    0x49, 0x83, 0xF8, 0x01, // cmp r8, 1
    0x74, 0x10, // je one
    0x49, 0x83, 0xF8, 0x02, // cmp r8, 2
    0x75, 0x2A, // jne slow
    0x66, 0x8B, 0x01, // mov ax, [rcx]
    0x66, 0x3B, 0x02, // cmp ax, [rdx]
    0x75, 0x1C, // jne differs
    0xEB, 0x20, // jmp slow
    0x8A, 0x01, // one: mov al, [rcx]
    0x3A, 0x02, // cmp al, [rdx]
    0x75, 0x14, // jne differs
    0xEB, 0x18, // jmp slow
    0x8B, 0x01, // four: mov eax, [rcx]
    0x3B, 0x02, // cmp eax, [rdx]
    0x75, 0x0C, // jne differs
    0xEB, 0x10, // jmp slow
    0x48, 0x8B, 0x01, // eight: mov rax, [rcx]
    0x48, 0x3B, 0x02, // cmp rax, [rdx]
    0x75, 0x02, // jne differs
    0xEB, 0x06, // jmp slow
    0xB8, 0x01, 0x00, 0x00, 0x00, // differs: mov eax, 1
    0xC3, // ret
    0xE9, 0x00, 0x00, 0x00, 0x00, // slow: jmp WaitOnAddress
];

/// QueryPerformanceCounter(&count): microseconds since the process started,
/// from the time-stamp counter and the clock in the PEB.
const QUERY_PERFORMANCE_COUNTER: &[u8] = &[
    0x65, 0x4C, 0x8B, 0x04, 0x25, 0x60, 0x00, 0x00, 0x00, // mov r8, gs:[0x60] (the PEB)
    0x4D, 0x8B, 0x88, 0x08, 0x0C, 0x00, 0x00, // mov r9, [r8+0xC08] (ticks per microsecond)
    0x4D, 0x85, 0xC9, // test r9, r9
    0x74, 0x25, // jz slow
    0x0F, 0x31, // rdtsc
    0x48, 0xC1, 0xE2, 0x20, // shl rdx, 32
    0x48, 0x09, 0xD0, // or rax, rdx
    0x49, 0x2B, 0x80, 0x00, 0x0C, 0x00, 0x00, // sub rax, [r8+0xC00] (counter at the start)
    0x31, 0xD2, // xor edx, edx
    0x49, 0xF7, 0xF1, // div r9
    0x49, 0x2B, 0x80, 0x10, 0x0C, 0x00, 0x00, // sub rax, [r8+0xC10] (the process's start)
    0x48, 0x89, 0x01, // mov [rcx], rax
    0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
    0xC3, // ret
    0xE9, 0x00, 0x00, 0x00, 0x00, // slow: jmp QueryPerformanceCounter
];

/// QueryPerformanceFrequency(&frequency): microseconds.
const QUERY_PERFORMANCE_FREQUENCY: &[u8] = &[
    0x48, 0xC7, 0x01, 0x40, 0x42, 0x0F, 0x00, // mov qword [rcx], 1000000
    0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
    0xC3, // ret
];

/// The code for a KERNEL32 (or NTDLL, or API set) function, by name.
pub fn kernel_native(name: &str) -> Option<Native> {
    let (code, slow) = match name {
        "GetLastError" | "RtlGetLastWin32Error" => (GET_LAST_ERROR, false),
        "SetLastError" | "RtlSetLastWin32Error" => (SET_LAST_ERROR, false),
        "GetCurrentThreadId" => (CURRENT_THREAD_ID, false),
        "GetCurrentThread" => (CURRENT_THREAD, false),
        "GetCurrentProcess" => (CURRENT_PROCESS, false),
        "TlsGetValue" | "FlsGetValue" => (TLS_GET, false),
        "TlsSetValue" | "FlsSetValue" => (TLS_SET, false),
        "EnterCriticalSection" | "RtlEnterCriticalSection" => (ENTER_CRITICAL_SECTION, true),
        "LeaveCriticalSection" | "RtlLeaveCriticalSection" => (LEAVE_CRITICAL_SECTION, true),
        "TryEnterCriticalSection" | "RtlTryEnterCriticalSection" => (TRY_ENTER_CRITICAL_SECTION, false),
        "AcquireSRWLockExclusive" | "RtlAcquireSRWLockExclusive" => (SRW_ACQUIRE_EXCLUSIVE, true),
        "AcquireSRWLockShared" | "RtlAcquireSRWLockShared" => (SRW_ACQUIRE_SHARED, true),
        "ReleaseSRWLockExclusive" | "RtlReleaseSRWLockExclusive" => (SRW_RELEASE_EXCLUSIVE, true),
        "ReleaseSRWLockShared" | "RtlReleaseSRWLockShared" => (SRW_RELEASE_SHARED, true),
        "TryAcquireSRWLockExclusive" | "RtlTryAcquireSRWLockExclusive" => (SRW_TRY_EXCLUSIVE, false),
        "TryAcquireSRWLockShared" | "RtlTryAcquireSRWLockShared" => (SRW_TRY_SHARED, false),
        "WaitOnAddress" => (WAIT_ON_ADDRESS, true),
        "QueryPerformanceCounter" | "RtlQueryPerformanceCounter" => (QUERY_PERFORMANCE_COUNTER, true),
        "QueryPerformanceFrequency" | "RtlQueryPerformanceFrequency" => (QUERY_PERFORMANCE_FREQUENCY, false),
        _ => return None,
    };
    Some(Native { code, slow })
}

/// Whether a DLL's functions may run as this code.
pub fn is_kernel_dll(dll: &str) -> bool {
    let dll = dll.to_ascii_lowercase();
    matches!(dll.as_str(), "kernel32.dll" | "kernelbase.dll" | "ntdll.dll") || dll.starts_with("api-ms-win-core-")
}

pub use crate::kernel::syscall::placed;

/// The address of the machine code for `dll!name` in this process (a C
/// library leaf or one of the functions here), written on first use: for
/// the imports of DLLs loaded at run time and GetProcAddress (the
/// program's own imports are bound when it is loaded). `thunk` gives the
/// system call its slow path goes on to.
pub fn address<M: crate::win32::GuestMemory + ?Sized>(
    ctx: &crate::win32::ApiContext,
    memory: &mut M,
    dll: &str,
    name: &str,
    thunk: impl FnOnce() -> Option<u32>,
) -> Option<u32> {
    use crate::kernel::syscall::SystemCalls;
    if memory.ptr_size() != 8 {
        return None;
    }
    let native = ctx.native_x64(dll, &crate::win32::ImportName::ByName(name.into()))?;
    if let Some(&at) = ctx.shared.borrow().ucrt.native.get(name) {
        return Some(at);
    }
    let thunk = if native.slow { thunk()? } else { 0 };
    let (page, used) = match ctx.shared.borrow().ucrt.native_page {
        Some((page, used)) if used as usize + native.code.len() <= 0x1000 => (page, used),
        _ => (memory.map_region(crate::kernel::memory::POOL_BASE, 0x1000)?, 0),
    };
    let at = page + used;
    memory.write_bytes(at, &placed(native, at as u64, thunk as u64))?;
    let mut shared = ctx.shared.borrow_mut();
    shared.ucrt.native_page = Some((page, (used + native.code.len() as u32 + 15) & !15));
    shared.ucrt.native.insert(name.into(), at);
    Some(at)
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use unicorn_engine::{unicorn_const::{Arch, Mode}, Prot, RegisterX86 as Reg, Unicorn};

    const CODE: u64 = 0x10000;
    const THUNK: u64 = 0x18000;
    const DATA: u64 = 0x20000;
    const TEB: u64 = 0x30000;
    const STACK: u64 = 0x40000;
    const RETURN: u64 = 0x18800;

    /// How a call went: its result, or the system call it went on to.
    #[derive(Debug, PartialEq)]
    enum Ran {
        Returned(u64),
        SystemCall,
    }

    /// A CPU, and where the next call's code goes (each call's code at a
    /// new place: the emulator keeps what it translated).
    struct Cpu(Unicorn<'static, ()>, u64);

    impl Cpu {
        fn new(thread: u32) -> Self {
            let mut uc = Unicorn::new(Arch::X86, Mode::MODE_64).unwrap();
            for (base, size) in [(CODE, 0x10000), (DATA, 0x1000), (TEB, 0x3000), (STACK, 0x1000)] {
                uc.mem_map(base, size, Prot::ALL).unwrap();
            }
            uc.mem_write(THUNK, &[0xF4]).unwrap();
            uc.mem_write(RETURN, &[0xF4]).unwrap();
            uc.reg_write(Reg::GS_BASE, TEB).unwrap();
            uc.mem_write(TEB + TEB_THREAD_ID as u64, &(thread as u64).to_le_bytes()).unwrap();
            Cpu(uc, CODE)
        }

        fn call(&mut self, name: &str, args: [u64; 4]) -> Ran {
            let native = kernel_native(name).unwrap();
            let at = self.1;
            let code = placed(native, at, THUNK);
            self.1 += (code.len() as u64 + 15) & !15;
            assert!(self.1 < THUNK);
            self.0.mem_write(at, &code).unwrap();
            self.0.mem_write(STACK + 0x800, &RETURN.to_le_bytes()).unwrap();
            for (reg, value) in [(Reg::RSP, STACK + 0x800), (Reg::RCX, args[0]), (Reg::RDX, args[1]), (Reg::R8, args[2]), (Reg::R9, args[3])] {
                self.0.reg_write(reg, value).unwrap();
            }
            // Runs until the `hlt` at the return address or the thunk.
            let _ = self.0.emu_start(at, 0, 0, 10_000);
            let rip = self.0.reg_read(Reg::RIP).unwrap();
            if rip == THUNK + 1 || rip == THUNK {
                assert_eq!(self.0.reg_read(Reg::RSP).unwrap(), STACK + 0x800, "the system call sees the caller's stack");
                return Ran::SystemCall;
            }
            assert!(rip == RETURN || rip == RETURN + 1, "{name} stopped at {rip:#x}");
            Ran::Returned(self.0.reg_read(Reg::RAX).unwrap())
        }

        fn u32_at(&self, va: u64) -> u32 {
            let mut bytes = [0u8; 4];
            self.0.mem_read(va, &mut bytes).unwrap();
            u32::from_le_bytes(bytes)
        }

        fn set_u32(&mut self, va: u64, value: u32) {
            self.0.mem_write(va, &value.to_le_bytes()).unwrap();
        }
    }

    #[test]
    fn last_error_and_thread_id_live_in_the_teb() {
        let mut cpu = Cpu::new(7);
        assert!(matches!(cpu.call("SetLastError", [5, 0, 0, 0]), Ran::Returned(_)));
        assert_eq!(cpu.u32_at(TEB + 0x68), 5);
        assert_eq!(cpu.call("GetLastError", [0; 4]), Ran::Returned(5));
        assert_eq!(cpu.call("GetCurrentThreadId", [0; 4]), Ran::Returned(7));
        assert_eq!(cpu.call("GetCurrentThread", [0; 4]), Ran::Returned(u64::MAX - 1));
        assert_eq!(cpu.call("GetCurrentProcess", [0; 4]), Ran::Returned(u64::MAX));
    }

    #[test]
    fn tls_slots_hold_64_bit_values() {
        let mut cpu = Cpu::new(1);
        let value = 0x1234_5678_9ABC_DEF0;
        assert_eq!(cpu.call("TlsSetValue", [3, value, 0, 0]), Ran::Returned(1));
        cpu.set_u32(TEB + 0x68, 99);
        assert_eq!(cpu.call("TlsGetValue", [3, 0, 0, 0]), Ran::Returned(value));
        assert_eq!(cpu.u32_at(TEB + 0x68), 0, "a successful TlsGetValue clears the last error");
        // Upper bits of the DWORD index are ignored.
        assert_eq!(cpu.call("FlsGetValue", [0xFFFF_FFFF_0000_0003, 0, 0, 0]), Ran::Returned(value));
        assert_eq!(cpu.call("TlsGetValue", [64, 0, 0, 0]), Ran::Returned(0));
        assert_eq!(cpu.u32_at(TEB + 0x68), 87);
        assert_eq!(cpu.call("TlsSetValue", [100, 1, 0, 0]), Ran::Returned(0));
    }

    #[test]
    fn critical_sections_are_taken_and_given_back_in_the_program() {
        let mut cpu = Cpu::new(4);
        let cs = DATA;
        assert!(matches!(cpu.call("EnterCriticalSection", [cs, 0, 0, 0]), Ran::Returned(_)));
        assert_eq!((cpu.u32_at(cs + 8), cpu.u32_at(cs + 12)), (1, 4));
        assert!(matches!(cpu.call("EnterCriticalSection", [cs, 0, 0, 0]), Ran::Returned(_)));
        assert_eq!(cpu.u32_at(cs + 8), 2);
        assert_eq!(cpu.call("TryEnterCriticalSection", [cs, 0, 0, 0]), Ran::Returned(1));
        assert_eq!(cpu.u32_at(cs + 8), 3);
        for _ in 0..3 {
            assert!(matches!(cpu.call("LeaveCriticalSection", [cs, 0, 0, 0]), Ran::Returned(_)));
        }
        assert_eq!((cpu.u32_at(cs + 8), cpu.u32_at(cs + 12)), (0, 0));
        // Another thread's: Try fails, Enter spins and then waits in the
        // subsystem, Leave leaves it alone.
        cpu.set_u32(cs + 12, 9);
        cpu.set_u32(cs + 8, 1);
        assert_eq!(cpu.call("TryEnterCriticalSection", [cs, 0, 0, 0]), Ran::Returned(0));
        assert_eq!(cpu.call("EnterCriticalSection", [cs, 0, 0, 0]), Ran::SystemCall);
        assert!(matches!(cpu.call("LeaveCriticalSection", [cs, 0, 0, 0]), Ran::Returned(_)));
        assert_eq!((cpu.u32_at(cs + 8), cpu.u32_at(cs + 12)), (1, 9));
        // Giving it back with a thread waiting asks the subsystem to wake it.
        cpu.set_u32(cs + 12, 4);
        cpu.set_u32(cs + 24, 1);
        assert_eq!(cpu.call("LeaveCriticalSection", [cs, 0, 0, 0]), Ran::SystemCall);
        assert_eq!((cpu.u32_at(cs + 8), cpu.u32_at(cs + 12)), (0, 0));
    }

    #[test]
    fn slim_locks_count_readers_and_take_writers() {
        let mut cpu = Cpu::new(1);
        let lock = DATA + 0x100;
        assert!(matches!(cpu.call("AcquireSRWLockShared", [lock, 0, 0, 0]), Ran::Returned(_)));
        assert!(matches!(cpu.call("AcquireSRWLockShared", [lock, 0, 0, 0]), Ran::Returned(_)));
        assert_eq!(cpu.u32_at(lock), 8);
        assert_eq!(cpu.call("TryAcquireSRWLockExclusive", [lock, 0, 0, 0]), Ran::Returned(0));
        assert_eq!(cpu.call("AcquireSRWLockExclusive", [lock, 0, 0, 0]), Ran::SystemCall);
        assert_eq!(cpu.call("TryAcquireSRWLockShared", [lock, 0, 0, 0]), Ran::Returned(1));
        for _ in 0..3 {
            assert!(matches!(cpu.call("ReleaseSRWLockShared", [lock, 0, 0, 0]), Ran::Returned(_)));
        }
        assert_eq!(cpu.u32_at(lock), 0);
        assert_eq!(cpu.call("TryAcquireSRWLockExclusive", [lock, 0, 0, 0]), Ran::Returned(1));
        assert_eq!(cpu.u32_at(lock), 1);
        assert_eq!(cpu.call("AcquireSRWLockShared", [lock, 0, 0, 0]), Ran::SystemCall);
        assert_eq!(cpu.call("TryAcquireSRWLockShared", [lock, 0, 0, 0]), Ran::Returned(0));
        assert!(matches!(cpu.call("ReleaseSRWLockExclusive", [lock, 0, 0, 0]), Ran::Returned(_)));
        assert_eq!(cpu.u32_at(lock), 0);
        // With waiters in the subsystem, every change goes there.
        cpu.set_u32(lock, SRW_EXCLUSIVE | SRW_WAITERS);
        assert_eq!(cpu.call("ReleaseSRWLockExclusive", [lock, 0, 0, 0]), Ran::SystemCall);
        cpu.set_u32(lock, SRW_READER | SRW_WAITERS);
        assert_eq!(cpu.call("ReleaseSRWLockShared", [lock, 0, 0, 0]), Ran::SystemCall);
        assert_eq!(cpu.call("AcquireSRWLockShared", [lock, 0, 0, 0]), Ran::SystemCall);
        assert_eq!(cpu.u32_at(lock), SRW_READER | SRW_WAITERS);
    }

    #[test]
    fn wait_on_address_returns_when_the_value_differs() {
        let mut cpu = Cpu::new(1);
        let (address, compare) = (DATA + 0x200, DATA + 0x300);
        for size in [1u64, 2, 4, 8] {
            cpu.0.mem_write(address, &[0u8; 8]).unwrap();
            cpu.0.mem_write(compare, &[0u8; 8]).unwrap();
            assert_eq!(cpu.call("WaitOnAddress", [address, compare, size, u32::MAX as u64]), Ran::SystemCall, "size {size}");
            cpu.0.mem_write(address + size - 1, &[1]).unwrap();
            assert_eq!(cpu.call("WaitOnAddress", [address, compare, size, u32::MAX as u64]), Ran::Returned(1), "size {size}");
        }
        assert_eq!(cpu.call("WaitOnAddress", [address, compare, 3, 0]), Ran::SystemCall);
    }

    #[test]
    fn the_performance_counter_reads_the_clock_in_the_peb() {
        let mut cpu = Cpu::new(1);
        let peb = TEB + 0x2000;
        cpu.0.mem_write(TEB + 0x60, &peb.to_le_bytes()).unwrap();
        let out = DATA + 0x400;
        // No clock published: the subsystem answers.
        assert_eq!(cpu.call("QueryPerformanceCounter", [out, 0, 0, 0]), Ran::SystemCall);
        // A counter at 1 tick per microsecond since 0, the process started
        // at 0: the counter itself, which only grows.
        let clock = peb + PEB_CLOCK as u64;
        cpu.0.mem_write(clock, &0u64.to_le_bytes()).unwrap();
        cpu.0.mem_write(clock + 8, &1u64.to_le_bytes()).unwrap();
        assert_eq!(cpu.call("QueryPerformanceCounter", [out, 0, 0, 0]), Ran::Returned(1));
        let read = |cpu: &Cpu| {
            let mut bytes = [0u8; 8];
            cpu.0.mem_read(out, &mut bytes).unwrap();
            u64::from_le_bytes(bytes)
        };
        let first = read(&cpu);
        assert!(first > 0);
        cpu.call("QueryPerformanceCounter", [out, 0, 0, 0]);
        assert!(read(&cpu) >= first);
        // A process that started later counts from its start.
        cpu.0.mem_write(clock + 16, &first.to_le_bytes()).unwrap();
        cpu.call("QueryPerformanceCounter", [out, 0, 0, 0]);
        assert!(read(&cpu) < first);
        assert_eq!(cpu.call("QueryPerformanceFrequency", [out, 0, 0, 0]), Ran::Returned(1));
        assert_eq!(read(&cpu), 1_000_000);
    }

    #[test]
    fn slow_paths_jump_to_the_thunk() {
        let native = kernel_native("EnterCriticalSection").unwrap();
        let code = placed(native, 0x1000, 0x800);
        let end = 0x1000 + code.len() as i64;
        let displacement = i32::from_le_bytes(code[code.len() - 4..].try_into().unwrap()) as i64;
        assert_eq!(end + displacement, 0x800);
        assert_eq!(code[code.len() - 5], 0xE9);
        assert!(kernel_native("strlen").is_none());
        assert!(is_kernel_dll("api-ms-win-core-synch-l1-2-0.dll") && is_kernel_dll("KERNEL32.dll") && !is_kernel_dll("user32.dll"));
    }
}
