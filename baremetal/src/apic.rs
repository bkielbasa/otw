//! Each CPU's local APIC: its timer, the end of interrupts, and the
//! interrupts CPUs send each other (to start the others, and later to wake
//! them). x2APIC (registers as MSRs) where the CPU has it, the older
//! memory-mapped xAPIC otherwise. Also the clock: the time-stamp counter,
//! measured against the PC's interval timer (PIT) at boot.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::arch::{self, read_msr, write_msr};
use crate::io::{inb, outb};

static X2APIC: AtomicBool = AtomicBool::new(false);
static XAPIC_BASE: AtomicU64 = AtomicU64::new(0xFEE0_0000);
/// Time-stamp counter ticks per microsecond.
static TSC_PER_US: AtomicU64 = AtomicU64::new(1000);
/// APIC timer ticks per millisecond (divide by 16).
static TIMER_PER_MS: AtomicU32 = AtomicU32::new(0);
static BOOT_TSC: AtomicU64 = AtomicU64::new(0);

pub const MAX_CPUS: usize = 256;
/// APIC ids of the CPUs, by index (0: the one that booted); `CPUS` of them
/// are running.
static CPU_IDS: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(u32::MAX) }; MAX_CPUS];
static CPUS: AtomicU32 = AtomicU32::new(0);

const ID: u32 = 0x20;
const EOI: u32 = 0xB0;
const SPURIOUS: u32 = 0xF0;
const ICR_LOW: u32 = 0x300;
const ICR_HIGH: u32 = 0x310;
const LVT_TIMER: u32 = 0x320;
const TIMER_INITIAL: u32 = 0x380;
const TIMER_CURRENT: u32 = 0x390;
const TIMER_DIVIDE: u32 = 0x3E0;

fn read(register: u32) -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        read_msr(0x800 + (register >> 4)) as u32
    } else {
        unsafe { core::ptr::read_volatile((XAPIC_BASE.load(Ordering::Relaxed) + register as u64) as *const u32) }
    }
}

fn write(register: u32, value: u32) {
    if X2APIC.load(Ordering::Relaxed) {
        write_msr(0x800 + (register >> 4), value as u64);
    } else {
        unsafe { core::ptr::write_volatile((XAPIC_BASE.load(Ordering::Relaxed) + register as u64) as *mut u32, value) }
    }
}

/// Turn on this CPU's APIC (in x2APIC mode where the boot CPU chose it).
pub fn enable_on_this_cpu() {
    let mut base = read_msr(0x1B);
    base |= 1 << 11;
    if X2APIC.load(Ordering::Relaxed) {
        base |= 1 << 10;
    }
    write_msr(0x1B, base);
    write(SPURIOUS, 0x100 | arch::SPURIOUS_VECTOR as u32);
}

pub fn init_boot_cpu() {
    let x2 = arch::cpuid(1, 0)[2] & (1 << 21) != 0;
    X2APIC.store(x2, Ordering::Relaxed);
    let base = read_msr(0x1B) & 0x000F_FFFF_FFFF_F000;
    XAPIC_BASE.store(base, Ordering::Relaxed);
    if !x2 {
        arch::map_device(base, 4096);
    }
    // The old interrupt controllers (8259): all masked.
    unsafe {
        outb(0x21, 0xFF);
        outb(0xA1, 0xFF);
    }
    enable_on_this_cpu();
    register_this_cpu();
    calibrate();
    BOOT_TSC.store(arch::rdtsc(), Ordering::Relaxed);
}

pub fn x2apic() -> bool {
    X2APIC.load(Ordering::Relaxed)
}

pub fn apic_id() -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        read(ID)
    } else {
        read(ID) >> 24
    }
}

/// Count this CPU in; returns its index.
pub fn register_this_cpu() -> usize {
    let index = CPUS.fetch_add(1, Ordering::SeqCst) as usize;
    if index < MAX_CPUS {
        CPU_IDS[index].store(apic_id(), Ordering::SeqCst);
    }
    index
}

pub fn cpu_count() -> usize {
    CPUS.load(Ordering::SeqCst) as usize
}

/// This CPU's index (0 for the one that booted).
pub fn cpu_index() -> usize {
    let id = apic_id();
    let count = cpu_count().min(MAX_CPUS);
    (0..count).find(|&i| CPU_IDS[i].load(Ordering::Relaxed) == id).unwrap_or(usize::MAX)
}

pub fn end_of_interrupt() {
    write(EOI, 0);
}

/// Measure the time-stamp counter and the APIC timer over 10 ms of the PIT
/// (channel 2, which needs no interrupt).
fn calibrate() {
    write(TIMER_DIVIDE, 0x3); // divide by 16
    unsafe {
        // Channel 2 gate on, speaker off.
        outb(0x61, (inb(0x61) & !0x02) | 0x01);
        outb(0x43, 0xB0); // channel 2, lobyte/hibyte, mode 0
        let count: u16 = 11932; // 10 ms at 1.193182 MHz
        outb(0x42, count as u8);
        outb(0x42, (count >> 8) as u8);
        // Restart the count.
        let gate = inb(0x61) & !0x01;
        outb(0x61, gate);
        outb(0x61, gate | 0x01);
    }
    write(TIMER_INITIAL, u32::MAX);
    let tsc_start = arch::rdtsc();
    let mut spins = 0u64;
    while unsafe { inb(0x61) } & 0x20 == 0 {
        spins += 1;
        if spins > 100_000_000 {
            break; // no PIT: keep the guesses
        }
    }
    let elapsed = u32::MAX - read(TIMER_CURRENT);
    let tsc = arch::rdtsc() - tsc_start;
    write(TIMER_INITIAL, 0);
    if spins <= 100_000_000 {
        TSC_PER_US.store((tsc / 10_000).max(1), Ordering::Relaxed);
        TIMER_PER_MS.store((elapsed / 10).max(1), Ordering::Relaxed);
    } else {
        TIMER_PER_MS.store(100_000, Ordering::Relaxed);
    }
}

/// Start this CPU's periodic timer, `hz` times a second.
pub fn start_timer(hz: u32) {
    write(TIMER_DIVIDE, 0x3);
    write(LVT_TIMER, arch::TIMER_VECTOR as u32 | 1 << 17); // periodic
    write(TIMER_INITIAL, TIMER_PER_MS.load(Ordering::Relaxed) * 1000 / hz);
}

pub fn tsc_mhz() -> u64 {
    TSC_PER_US.load(Ordering::Relaxed)
}

/// Microseconds since the kernel started.
pub fn now_us() -> u64 {
    (arch::rdtsc() - BOOT_TSC.load(Ordering::Relaxed)) / TSC_PER_US.load(Ordering::Relaxed)
}

pub fn delay_us(us: u64) {
    let end = arch::rdtsc() + us * TSC_PER_US.load(Ordering::Relaxed);
    while arch::rdtsc() < end {
        core::hint::spin_loop();
    }
}

/// Send an interrupt command to the CPU with APIC id `target`.
pub fn send_ipi(target: u32, command: u32) {
    if X2APIC.load(Ordering::Relaxed) {
        write_msr(0x830, (target as u64) << 32 | command as u64);
    } else {
        write(ICR_HIGH, target << 24);
        write(ICR_LOW, command);
        while read(ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
    }
}

/// INIT, then two start-ups at the code in page `page` (below 1 MiB).
pub fn start_cpu(target: u32, page: u64) {
    send_ipi(target, 0x0000_4500); // INIT, level assert
    delay_us(10_000);
    for _ in 0..2 {
        send_ipi(target, 0x0000_4600 | (page >> 12) as u32); // start-up
        delay_us(200);
    }
}
