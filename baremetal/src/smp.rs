//! Starting the other CPUs. A CPU starts in 16-bit real mode at a page below
//! 1 MiB; the code put there (the trampoline, machine code written out
//! below) switches it straight to 64-bit mode with the kernel's page tables
//! and jumps to `cpu_main` on a stack of its own.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{apic, arch, log};

const GDT: usize = 0xC0;
const GDT_POINTER: usize = 0xD8;
const FAR_POINTER: usize = 0xE0;
const CR3: usize = 0xE8;
const STACK: usize = 0xF0;
const ENTRY: usize = 0xF8;
const ARGUMENT: usize = 0x100;
const CODE64: usize = 0x80;

/// What a starting CPU is told, and tells back.
struct Start {
    started: AtomicBool,
}

/// The trampoline for page `base`.
fn trampoline(base: u32) -> [u8; 0x110] {
    let mut t = [0u8; 0x110];
    let code16: &[u8] = &[
        0xFA, // cli
        0x8C, 0xC8, // mov ax, cs
        0x8E, 0xD8, // mov ds, ax
        0x66, 0x0F, 0x01, 0x16, GDT_POINTER as u8, 0x00, // lgdt [GDT_POINTER] (32-bit base)
        0x0F, 0x20, 0xE0, // mov eax, cr4
        0x66, 0x0D, 0x20, 0x06, 0x00, 0x00, // or eax, PAE | OSFXSR | OSXMMEXCPT
        0x0F, 0x22, 0xE0, // mov cr4, eax
        0x66, 0xA1, CR3 as u8, 0x00, // mov eax, [CR3]
        0x0F, 0x22, 0xD8, // mov cr3, eax
        0x66, 0xB9, 0x80, 0x00, 0x00, 0xC0, // mov ecx, EFER
        0x0F, 0x32, // rdmsr
        0x66, 0x0D, 0x00, 0x01, 0x00, 0x00, // or eax, LME
        0x0F, 0x30, // wrmsr
        0x0F, 0x20, 0xC0, // mov eax, cr0
        0x66, 0x25, 0xFB, 0xFF, 0xFF, 0x9F, // and eax, ~(CD | NW | EM)
        0x66, 0x0D, 0x03, 0x00, 0x00, 0x80, // or eax, PG | MP | PE
        0x0F, 0x22, 0xC0, // mov cr0, eax
        0x66, 0xFF, 0x2E, FAR_POINTER as u8, 0x00, // jmp far dword [FAR_POINTER]
    ];
    t[..code16.len()].copy_from_slice(code16);

    // 64-bit: data segments, then the stack, argument and entry, all read
    // relative to the instruction pointer.
    let mut code64: alloc::vec::Vec<u8> = alloc::vec![
        0x66, 0xB8, arch::KERNEL_DATA as u8, 0x00, // mov ax, KERNEL_DATA
        0x8E, 0xD8, 0x8E, 0xC0, 0x8E, 0xD0, 0x8E, 0xE0, 0x8E, 0xE8, // ds es ss fs gs
    ];
    let rip_load = |code: &mut alloc::vec::Vec<u8>, opcode: &[u8], field: usize| {
        code.extend_from_slice(opcode);
        let end = CODE64 + code.len() + 4;
        code.extend_from_slice(&((field as i32) - end as i32).to_le_bytes());
    };
    rip_load(&mut code64, &[0x48, 0x8B, 0x25], STACK); // mov rsp, [rip + STACK]
    rip_load(&mut code64, &[0x48, 0x8B, 0x3D], ARGUMENT); // mov rdi, [rip + ARGUMENT]
    rip_load(&mut code64, &[0x48, 0x8B, 0x05], ENTRY); // mov rax, [rip + ENTRY]
    code64.extend_from_slice(&[0xFF, 0xD0, 0xF4, 0xEB, 0xFD]); // call rax; hlt; jmp $-1
    t[CODE64..CODE64 + code64.len()].copy_from_slice(&code64);

    // A GDT of null, 64-bit code and data, at the kernel's selectors.
    let gdt: [u64; 3] = [0, 0x00AF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF];
    for (i, entry) in gdt.iter().enumerate() {
        t[GDT + i * 8..GDT + i * 8 + 8].copy_from_slice(&entry.to_le_bytes());
    }
    t[GDT_POINTER..GDT_POINTER + 2].copy_from_slice(&23u16.to_le_bytes());
    t[GDT_POINTER + 2..GDT_POINTER + 6].copy_from_slice(&(base + GDT as u32).to_le_bytes());
    t[FAR_POINTER..FAR_POINTER + 4].copy_from_slice(&(base + CODE64 as u32).to_le_bytes());
    t[FAR_POINTER + 4..FAR_POINTER + 6].copy_from_slice(&arch::KERNEL_CODE.to_le_bytes());
    t
}

/// Start every CPU in `ids` but this one, with the trampoline at `page`.
pub fn start_others(ids: &[u32], page: u64) {
    let me = apic::apic_id();
    let code = trampoline(page as u32);
    for &id in ids.iter().filter(|&&id| id != me) {
        if apic::cpu_count() >= apic::MAX_CPUS {
            break;
        }
        let stack = Box::leak(alloc::vec![0u8; 256 * 1024].into_boxed_slice());
        let start = Box::leak(Box::new(Start { started: AtomicBool::new(false) }));
        unsafe {
            let t = page as *mut u8;
            core::ptr::copy_nonoverlapping(code.as_ptr(), t, code.len());
            let field = |offset: usize, value: u64| core::ptr::write_unaligned(t.add(offset) as *mut u64, value);
            core::ptr::write_unaligned(t.add(CR3) as *mut u32, arch::page_table_root() as u32);
            field(STACK, (stack.as_ptr() as u64 + stack.len() as u64) & !15);
            field(ENTRY, cpu_main as *const () as u64);
            field(ARGUMENT, start as *const Start as u64);
        }
        apic::start_cpu(id, page);
        // Wait for it (up to a second) before reusing the trampoline.
        let deadline = apic::now_us() + 1_000_000;
        while !start.started.load(Ordering::SeqCst) && apic::now_us() < deadline {
            core::hint::spin_loop();
        }
        if !start.started.load(Ordering::SeqCst) {
            log!("OTW kernel: CPU with APIC id {id} did not start");
        }
    }
}

/// Where other CPUs begin in 64-bit mode.
extern "sysv64" fn cpu_main(start: &Start) -> ! {
    arch::load_cpu_tables();
    arch::load_idt_on_this_cpu();
    apic::enable_on_this_cpu();
    apic::register_this_cpu();
    start.started.store(true, Ordering::SeqCst);
    // Nothing to run yet: sleep until woken.
    loop {
        arch::wait_for_interrupt();
    }
}
