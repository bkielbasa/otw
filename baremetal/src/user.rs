//! Running program code in user mode, and coming back.
//!
//! `run` enters user mode with a program's registers and returns when the
//! program next needs the kernel: a system call (`int 0x80`, from an
//! import's thunk), the return of a callback (`int 0x81`), or a fault. The
//! program's registers, its floating point state included, are then in the
//! `UserRegs` given. Entries nest: a system call may call back into the
//! program, which may make system calls; each `run` has its own frame on
//! the kernel stack, and the innermost gets the next exit.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::{self, Frame};

/// A program's registers.
#[repr(C, align(64))]
#[derive(Clone)]
pub struct UserRegs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rsp: u64,
    pub rflags: u64,
    pub cs: u64,
    pub ss: u64,
    /// Why it stopped: the vector, the error code, the faulting address.
    pub vector: u64,
    pub error: u64,
    pub fault_address: u64,
    _pad: u64,
    /// FXSAVE image (x87, SSE).
    pub fx: [u8; 512],
}

impl UserRegs {
    pub fn new(x64: bool, rip: u64, rsp: u64) -> Self {
        let mut fx = [0u8; 512];
        // x87 control word as Windows sets it; MXCSR: all exceptions masked.
        fx[0..2].copy_from_slice(&(if x64 { 0x027Fu16 } else { 0x027Fu16 }).to_le_bytes());
        fx[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip,
            rsp,
            rflags: 0x202,
            cs: if x64 { arch::USER_CODE64 } else { arch::USER_CODE32 } as u64,
            ss: arch::USER_DATA as u64,
            vector: 0,
            error: 0,
            fault_address: 0,
            _pad: 0,
            fx,
        }
    }
}

/// The kernel stack pointer of the innermost `run` (where `leave` goes
/// back to).
#[no_mangle]
static OTW_KERNEL_RSP: AtomicU64 = AtomicU64::new(0);

// otw_enter_user(regs: *mut UserRegs): saves the kernel's callee-saved
// registers and the outer level, enters user mode with `regs`; returns
// (through otw_leave_user) when the program needs the kernel.
global_asm!(
    ".global otw_enter_user",
    "otw_enter_user:",
    "push rbx", "push rbp", "push r12", "push r13", "push r14", "push r15",
    "push qword ptr [rip + OTW_KERNEL_RSP]",
    "push rdi",
    "mov [rip + OTW_KERNEL_RSP], rsp",
    // Entries from user mode come in below this frame.
    "mov rdi, rsp",
    "sub rsp, 8",
    "call {set_stack}",
    "add rsp, 8",
    "mov rdi, [rsp]",
    "cli",
    "fxrstor64 [rdi + 192]",
    "mov ax, {data}",
    "mov ds, ax",
    "mov es, ax",
    // The interrupt return frame: SS, RSP, RFLAGS, CS, RIP.
    "push qword ptr [rdi + 152]",
    "push qword ptr [rdi + 128]",
    "push qword ptr [rdi + 136]",
    "push qword ptr [rdi + 144]",
    "push qword ptr [rdi + 120]",
    "mov rax, [rdi + 0]",
    "mov rbx, [rdi + 8]",
    "mov rcx, [rdi + 16]",
    "mov rdx, [rdi + 24]",
    "mov rsi, [rdi + 32]",
    "mov rbp, [rdi + 48]",
    "mov r8, [rdi + 56]",
    "mov r9, [rdi + 64]",
    "mov r10, [rdi + 72]",
    "mov r11, [rdi + 80]",
    "mov r12, [rdi + 88]",
    "mov r13, [rdi + 96]",
    "mov r14, [rdi + 104]",
    "mov r15, [rdi + 112]",
    "mov rdi, [rdi + 40]",
    "iretq",
    "",
    ".global otw_leave_user",
    "otw_leave_user:",
    "mov rsp, [rip + OTW_KERNEL_RSP]",
    "pop rdi",
    "pop qword ptr [rip + OTW_KERNEL_RSP]",
    "pop r15", "pop r14", "pop r13", "pop r12", "pop rbp", "pop rbx",
    "sti",
    "ret",
    set_stack = sym set_entry_stack,
    data = const arch::USER_DATA,
);

extern "sysv64" fn set_entry_stack(rsp: u64) {
    // Leave room under the frame; entries push their own frame below.
    arch::set_user_entry_stack(rsp & !15);
}

extern "sysv64" {
    fn otw_enter_user(regs: *mut UserRegs);
    fn otw_leave_user() -> !;
}

/// Run program code from `regs` until it needs the kernel; `regs` then
/// holds its state, `vector` saying why.
pub fn run(regs: &mut UserRegs) {
    unsafe { otw_enter_user(regs) };
}

/// From the interrupt entry: the program stopped. Save its state into the
/// innermost `run`'s registers and return there.
pub fn leave(frame: &Frame, fx: *const u8) -> ! {
    let saved = OTW_KERNEL_RSP.load(Ordering::SeqCst);
    let regs = unsafe { &mut **(saved as *const *mut UserRegs) };
    regs.rax = frame.rax;
    regs.rbx = frame.rbx;
    regs.rcx = frame.rcx;
    regs.rdx = frame.rdx;
    regs.rsi = frame.rsi;
    regs.rdi = frame.rdi;
    regs.rbp = frame.rbp;
    regs.r8 = frame.r8;
    regs.r9 = frame.r9;
    regs.r10 = frame.r10;
    regs.r11 = frame.r11;
    regs.r12 = frame.r12;
    regs.r13 = frame.r13;
    regs.r14 = frame.r14;
    regs.r15 = frame.r15;
    regs.rip = frame.rip;
    regs.rsp = frame.rsp;
    regs.rflags = frame.rflags;
    regs.cs = frame.cs;
    regs.ss = frame.ss;
    regs.vector = frame.vector;
    regs.error = frame.error;
    let cr2: u64;
    unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2) };
    regs.fault_address = cr2;
    unsafe { core::ptr::copy_nonoverlapping(fx, regs.fx.as_mut_ptr(), 512) };
    unsafe { otw_leave_user() }
}

const _: () = {
    assert!(core::mem::offset_of!(UserRegs, rip) == 120);
    assert!(core::mem::offset_of!(UserRegs, rsp) == 128);
    assert!(core::mem::offset_of!(UserRegs, rflags) == 136);
    assert!(core::mem::offset_of!(UserRegs, cs) == 144);
    assert!(core::mem::offset_of!(UserRegs, ss) == 152);
    assert!(core::mem::offset_of!(UserRegs, fx) == 192);
};
