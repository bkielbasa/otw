//! The CPU's own tables: segments (GDT and TSS), interrupts and exceptions
//! (IDT), and the page tables, all the kernel's rather than the firmware's.

use alloc::alloc::{alloc_zeroed, Layout};
use alloc::boxed::Box;
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::log;

// ── Segments ──

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
const TSS_SELECTOR: u16 = 0x28;

#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

#[repr(C, packed)]
struct TablePointer {
    limit: u16,
    base: u64,
}

/// A CPU's GDT (null, kernel code and data, user data and code, TSS) and
/// TSS, which gives exceptions that must not use the faulting stack (a
/// double fault) one of their own.
#[repr(C, align(16))]
pub struct CpuTables {
    gdt: [u64; 7],
    tss: Tss,
}

pub fn load_cpu_tables() {
    let fault_stack = Box::leak(alloc::vec![0u8; 64 * 1024].into_boxed_slice());
    let tables = Box::leak(Box::new(CpuTables {
        gdt: [
            0,
            0x00AF_9A00_0000_FFFF, // kernel code, 64-bit
            0x00CF_9200_0000_FFFF, // kernel data
            0x00CF_F200_0000_FFFF, // user data
            0x00AF_FA00_0000_FFFF, // user code, 64-bit
            0,
            0,
        ],
        tss: Tss {
            reserved0: 0,
            rsp: [0; 3],
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: core::mem::size_of::<Tss>() as u16,
        },
    }));
    tables.tss.ist[0] = fault_stack.as_ptr() as u64 + fault_stack.len() as u64;
    let tss = &tables.tss as *const Tss as u64;
    let limit = core::mem::size_of::<Tss>() as u64 - 1;
    tables.gdt[5] = (limit & 0xFFFF) | (tss & 0xFF_FFFF) << 16 | 0x89 << 40 | (limit >> 16 & 0xF) << 48 | (tss >> 24 & 0xFF) << 56;
    tables.gdt[6] = tss >> 32;
    let pointer = TablePointer { limit: (core::mem::size_of_val(&tables.gdt) - 1) as u16, base: tables.gdt.as_ptr() as u64 };
    unsafe {
        asm!(
            "lgdt [{pointer}]",
            // Reload CS with a far return, then the data segments.
            "push {code}",
            "lea {scratch}, [rip + 2f]",
            "push {scratch}",
            "retfq",
            "2:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            "mov fs, {data:x}",
            "mov gs, {data:x}",
            "ltr {tss:x}",
            pointer = in(reg) &pointer,
            code = in(reg) KERNEL_CODE as u64,
            scratch = out(reg) _,
            data = in(reg) KERNEL_DATA as u64,
            tss = in(reg) TSS_SELECTOR as u64,
        );
    }
}

// ── Interrupts ──

#[repr(C)]
#[derive(Clone, Copy)]
struct Gate {
    offset_low: u16,
    selector: u16,
    ist: u8,
    kind: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

static mut IDT: [Gate; 256] = [Gate { offset_low: 0, selector: 0, ist: 0, kind: 0, offset_middle: 0, offset_high: 0, reserved: 0 }; 256];

/// The registers an interrupt stub saves, in push order reversed.
#[repr(C)]
pub struct Frame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// One stub per vector: push a zero error code where the CPU pushes none,
// push the vector, and go to the common part, which saves the registers
// and calls `interrupt`.
macro_rules! stubs {
    ($($vector:literal $error:literal),* $(,)?) => {
        global_asm!(
            $(
                concat!(".global otw_vector_", $vector),
                concat!("otw_vector_", $vector, ":"),
                concat!(".if ", $error, " == 0"),
                "push 0",
                ".endif",
                concat!("push ", $vector),
                "jmp otw_interrupt_common",
            )*
        );
    };
}

global_asm!(
    "otw_interrupt_common:",
    "push rax", "push rbx", "push rcx", "push rdx", "push rsi", "push rdi", "push rbp",
    "push r8", "push r9", "push r10", "push r11", "push r12", "push r13", "push r14", "push r15",
    "mov rdi, rsp",
    // Align the stack for the call; keep the old pointer.
    "mov rbx, rsp",
    "and rsp, -16",
    "cld",
    "call {handler}",
    "mov rsp, rbx",
    "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9", "pop r8",
    "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx", "pop rax",
    "add rsp, 16",
    "iretq",
    handler = sym interrupt,
);

// Exceptions 8, 10-14, 17, 21, 29, 30 push an error code.
stubs!(
    0 0, 1 0, 2 0, 3 0, 4 0, 5 0, 6 0, 7 0, 8 1, 9 0, 10 1, 11 1, 12 1, 13 1, 14 1, 15 0,
    16 0, 17 1, 18 0, 19 0, 20 0, 21 1, 22 0, 23 0, 24 0, 25 0, 26 0, 27 0, 28 0, 29 1, 30 1, 31 0,
    32 0, 33 0, 34 0, 35 0, 36 0, 37 0, 38 0, 39 0, 40 0, 41 0, 42 0, 43 0, 44 0, 45 0, 46 0, 47 0,
    48 0, 49 0, 50 0, 51 0, 255 0,
);

extern "C" {
    fn otw_vector_0(); fn otw_vector_1(); fn otw_vector_2(); fn otw_vector_3(); fn otw_vector_4();
    fn otw_vector_5(); fn otw_vector_6(); fn otw_vector_7(); fn otw_vector_8(); fn otw_vector_9();
    fn otw_vector_10(); fn otw_vector_11(); fn otw_vector_12(); fn otw_vector_13(); fn otw_vector_14();
    fn otw_vector_15(); fn otw_vector_16(); fn otw_vector_17(); fn otw_vector_18(); fn otw_vector_19();
    fn otw_vector_20(); fn otw_vector_21(); fn otw_vector_22(); fn otw_vector_23(); fn otw_vector_24();
    fn otw_vector_25(); fn otw_vector_26(); fn otw_vector_27(); fn otw_vector_28(); fn otw_vector_29();
    fn otw_vector_30(); fn otw_vector_31(); fn otw_vector_32(); fn otw_vector_33(); fn otw_vector_34();
    fn otw_vector_35(); fn otw_vector_36(); fn otw_vector_37(); fn otw_vector_38(); fn otw_vector_39();
    fn otw_vector_40(); fn otw_vector_41(); fn otw_vector_42(); fn otw_vector_43(); fn otw_vector_44();
    fn otw_vector_45(); fn otw_vector_46(); fn otw_vector_47(); fn otw_vector_48(); fn otw_vector_49();
    fn otw_vector_50(); fn otw_vector_51(); fn otw_vector_255();
}

/// The vector of the local APIC timer.
pub const TIMER_VECTOR: u8 = 32;
/// Interrupts that wake a CPU from `hlt` (another CPU has work for it).
pub const WAKE_VECTOR: u8 = 33;
pub const SPURIOUS_VECTOR: u8 = 255;

pub fn load_idt() {
    let handlers: [(u8, unsafe extern "C" fn()); 53] = [
        (0, otw_vector_0), (1, otw_vector_1), (2, otw_vector_2), (3, otw_vector_3), (4, otw_vector_4),
        (5, otw_vector_5), (6, otw_vector_6), (7, otw_vector_7), (8, otw_vector_8), (9, otw_vector_9),
        (10, otw_vector_10), (11, otw_vector_11), (12, otw_vector_12), (13, otw_vector_13), (14, otw_vector_14),
        (15, otw_vector_15), (16, otw_vector_16), (17, otw_vector_17), (18, otw_vector_18), (19, otw_vector_19),
        (20, otw_vector_20), (21, otw_vector_21), (22, otw_vector_22), (23, otw_vector_23), (24, otw_vector_24),
        (25, otw_vector_25), (26, otw_vector_26), (27, otw_vector_27), (28, otw_vector_28), (29, otw_vector_29),
        (30, otw_vector_30), (31, otw_vector_31), (32, otw_vector_32), (33, otw_vector_33), (34, otw_vector_34),
        (35, otw_vector_35), (36, otw_vector_36), (37, otw_vector_37), (38, otw_vector_38), (39, otw_vector_39),
        (40, otw_vector_40), (41, otw_vector_41), (42, otw_vector_42), (43, otw_vector_43), (44, otw_vector_44),
        (45, otw_vector_45), (46, otw_vector_46), (47, otw_vector_47), (48, otw_vector_48), (49, otw_vector_49),
        (50, otw_vector_50), (51, otw_vector_51), (255, otw_vector_255),
    ];
    unsafe {
        let idt = &mut *core::ptr::addr_of_mut!(IDT);
        for (vector, handler) in handlers {
            let address = handler as usize as u64;
            idt[vector as usize] = Gate {
                offset_low: address as u16,
                selector: KERNEL_CODE,
                // A double fault runs on its own stack (IST 1).
                ist: if vector == 8 { 1 } else { 0 },
                kind: 0x8E,
                offset_middle: (address >> 16) as u16,
                offset_high: (address >> 32) as u32,
                reserved: 0,
            };
        }
    }
    load_idt_on_this_cpu();
}

pub fn load_idt_on_this_cpu() {
    let pointer = TablePointer { limit: (256 * 16 - 1) as u16, base: core::ptr::addr_of!(IDT) as u64 };
    unsafe { asm!("lidt [{}]", in(reg) &pointer) };
}

/// Timer interrupts so far, on the first CPU.
pub static TICKS: AtomicU64 = AtomicU64::new(0);

extern "sysv64" fn interrupt(frame: &mut Frame) {
    match frame.vector {
        v if v == TIMER_VECTOR as u64 => {
            if crate::apic::cpu_index() == 0 {
                TICKS.fetch_add(1, Ordering::Relaxed);
            }
            crate::apic::end_of_interrupt();
        }
        v if v == WAKE_VECTOR as u64 => crate::apic::end_of_interrupt(),
        v if v == crate::io::KEYBOARD_VECTOR as u64 || v == crate::io::MOUSE_VECTOR as u64 => {
            crate::io::ps2_interrupt();
            crate::apic::end_of_interrupt();
        }
        v if v == SPURIOUS_VECTOR as u64 => {}
        v if v < 32 => exception(frame),
        _ => crate::apic::end_of_interrupt(),
    }
}

fn exception(frame: &Frame) -> ! {
    const NAMES: [&str; 32] = [
        "divide error", "debug", "NMI", "breakpoint", "overflow", "bound range", "invalid opcode",
        "device not available", "double fault", "coprocessor segment", "invalid TSS", "segment not present",
        "stack fault", "general protection", "page fault", "reserved", "x87 floating point", "alignment check",
        "machine check", "SIMD floating point", "virtualization", "control protection", "reserved", "reserved",
        "reserved", "reserved", "reserved", "reserved", "hypervisor injection", "VMM communication", "security",
        "reserved",
    ];
    let cr2: u64;
    unsafe { asm!("mov {}, cr2", out(reg) cr2) };
    log!(
        "OTW kernel: CPU {} exception {} ({}), error {:#x}, at {:#x}, address {:#x}, stack {:#x}",
        crate::apic::cpu_index(),
        frame.vector,
        NAMES[frame.vector as usize & 31],
        frame.error,
        frame.rip,
        cr2,
        frame.rsp
    );
    log!(
        "  rax {:#x} rbx {:#x} rcx {:#x} rdx {:#x} rsi {:#x} rdi {:#x} rbp {:#x}",
        frame.rax, frame.rbx, frame.rcx, frame.rdx, frame.rsi, frame.rdi, frame.rbp
    );
    crate::fatal(&alloc::format!(
        "{} at {:#x} (address {:#x})",
        NAMES[frame.vector as usize & 31],
        frame.rip,
        cr2
    ))
}

pub fn enable_interrupts() {
    unsafe { asm!("sti") };
}

pub fn disable_interrupts() {
    unsafe { asm!("cli") };
}

/// Sleep until an interrupt.
pub fn wait_for_interrupt() {
    unsafe { asm!("sti; hlt", options(nomem, nostack)) };
}

pub fn read_msr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack)) };
    (high as u64) << 32 | low as u64
}

pub fn write_msr(msr: u32, value: u64) {
    unsafe { asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nomem, nostack)) };
}

pub fn cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
    let r = core::arch::x86_64::__cpuid_count(leaf, subleaf);
    [r.eax, r.ebx, r.ecx, r.edx]
}

pub fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

// ── Page tables ──

const PRESENT: u64 = 1;
const WRITABLE: u64 = 2;
const WRITE_THROUGH: u64 = 1 << 3;
const NO_CACHE: u64 = 1 << 4;
const HUGE: u64 = 1 << 7;
const ADDRESS: u64 = 0x000F_FFFF_FFFF_F000;
const TWO_MIB: u64 = 2 << 20;

static PML4: AtomicU64 = AtomicU64::new(0);

fn new_table() -> *mut u64 {
    let table = unsafe { alloc_zeroed(Layout::from_size_align(4096, 4096).unwrap()) } as *mut u64;
    assert!(!table.is_null(), "out of memory for page tables");
    table
}

/// Map physical memory at its own address (identity mapping) in 2 MiB
/// pages: [0, `end`) cached normally (the firmware's memory type ranges keep
/// device memory below 4 GiB uncached).
pub fn build_page_tables(end: u64) {
    let pml4 = new_table();
    // The table must be below 4 GiB: other CPUs load it while still in
    // 32-bit code when they start.
    assert!((pml4 as u64) < 1 << 32, "page tables above 4 GiB");
    PML4.store(pml4 as u64, Ordering::SeqCst);
    let end = end.div_ceil(TWO_MIB) * TWO_MIB;
    let mut address = 0;
    while address < end {
        map_2mib(address, 0);
        address += TWO_MIB;
    }
    unsafe { asm!("mov cr3, {}", in(reg) pml4 as u64) };
}

pub fn page_table_root() -> u64 {
    PML4.load(Ordering::SeqCst)
}

fn map_2mib(address: u64, flags: u64) {
    unsafe {
        let pml4 = PML4.load(Ordering::SeqCst) as *mut u64;
        let mut table = pml4;
        for shift in [39u64, 30] {
            let entry = table.add((address >> shift & 511) as usize);
            if *entry & PRESENT == 0 {
                *entry = new_table() as u64 | PRESENT | WRITABLE;
            }
            table = (*entry & ADDRESS) as *mut u64;
        }
        *table.add((address >> 21 & 511) as usize) = address | PRESENT | WRITABLE | HUGE | flags;
    }
}

/// The page attribute table: entry 1 (page tables' write-through bit
/// alone) becomes write-combining, for the framebuffer; the rest keep
/// their power-on meaning. Every CPU loads the same table.
pub fn load_page_attributes() {
    // WB, WC, UC-, UC, WB, WT, UC-, UC
    write_msr(0x277, 0x0007_0406_0007_0106);
}

/// Map the framebuffer write-combining: writes to it are batched rather
/// than sent one at a time, which is what makes drawing fast on real
/// hardware.
pub fn map_framebuffer(address: u64, size: u64) {
    let start = address / TWO_MIB * TWO_MIB;
    let end = (address + size).div_ceil(TWO_MIB) * TWO_MIB;
    let mut page = start;
    while page < end {
        map_2mib(page, WRITE_THROUGH);
        unsafe { asm!("invlpg [{}]", in(reg) page) };
        page += TWO_MIB;
    }
    // Drop whatever the caches hold for it.
    unsafe { asm!("wbinvd") };
}

/// Make device registers at [address, address + size) reachable, uncached.
pub fn map_device(address: u64, size: u64) {
    let start = address / TWO_MIB * TWO_MIB;
    let end = (address + size).div_ceil(TWO_MIB) * TWO_MIB;
    let mut page = start;
    while page < end {
        map_2mib(page, NO_CACHE | WRITE_THROUGH);
        unsafe { asm!("invlpg [{}]", in(reg) page) };
        page += TWO_MIB;
    }
}
