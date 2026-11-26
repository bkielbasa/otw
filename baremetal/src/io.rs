//! Port I/O and the simple devices every PC has: the serial port (the
//! kernel's log), the PS/2 controller (keyboard and mouse) and the CMOS
//! clock.

use core::arch::asm;
use core::fmt;

pub unsafe fn outb(port: u16, value: u8) {
    asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
}

pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    value
}

// ── Serial port COM1: the kernel log ──

const COM1: u16 = 0x3F8;

pub fn serial_init() {
    unsafe {
        outb(COM1 + 1, 0x00); // no interrupts
        outb(COM1 + 3, 0x80); // divisor follows
        outb(COM1, 0x01); // 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8 bits, no parity, one stop bit
        outb(COM1 + 2, 0xC7); // FIFO on
        outb(COM1 + 4, 0x03);
    }
}

fn serial_write(byte: u8) {
    unsafe {
        let mut spins = 0;
        while inb(COM1 + 5) & 0x20 == 0 && spins < 100_000 {
            spins += 1;
        }
        outb(COM1, byte);
    }
}

pub struct Serial;

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                serial_write(b'\r');
            }
            serial_write(byte);
        }
        Ok(())
    }
}

static LOG_BUSY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Write a line to the log, whole (CPUs take turns; one that waits long,
/// as an interrupt handler on the CPU writing would, goes ahead).
pub fn log_line(args: fmt::Arguments<'_>) {
    use core::sync::atomic::Ordering;
    let mut spins = 0u32;
    while LOG_BUSY.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() && spins < 20_000_000 {
        spins += 1;
        core::hint::spin_loop();
    }
    let _ = fmt::Write::write_fmt(&mut Serial, format_args!("{args}\n"));
    LOG_BUSY.store(false, Ordering::Release);
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        $crate::io::log_line(format_args!($($arg)*));
    }};
}

// ── PS/2 controller ──

const PS2_DATA: u16 = 0x60;
const PS2_STATUS: u16 = 0x64;

fn ps2_wait_write() {
    for _ in 0..100_000 {
        if unsafe { inb(PS2_STATUS) } & 2 == 0 {
            return;
        }
    }
}

fn ps2_wait_read() -> bool {
    for _ in 0..100_000 {
        if unsafe { inb(PS2_STATUS) } & 1 != 0 {
            return true;
        }
    }
    false
}

fn ps2_command(command: u8) {
    ps2_wait_write();
    unsafe { outb(PS2_STATUS, command) };
}

fn ps2_mouse_write(value: u8) -> Option<u8> {
    ps2_command(0xD4);
    ps2_wait_write();
    unsafe { outb(PS2_DATA, value) };
    ps2_wait_read().then(|| unsafe { inb(PS2_DATA) })
}

/// Turn on the mouse port and the mouse's reports. Whether a PS/2 controller
/// is there at all (many new machines have none; USB comes later).
pub fn ps2_init() -> bool {
    // No controller: the status port reads all ones.
    if unsafe { inb(PS2_STATUS) } == 0xFF {
        return false;
    }
    // Empty the output buffer.
    for _ in 0..32 {
        if unsafe { inb(PS2_STATUS) } & 1 == 0 {
            break;
        }
        unsafe { inb(PS2_DATA) };
    }
    ps2_command(0xA8); // enable the mouse port
    ps2_command(0x20); // read the configuration byte
    let config = if ps2_wait_read() { unsafe { inb(PS2_DATA) } } else { 0 };
    // Keep translation (scan code set 1), both ports on; interrupts as
    // `ps2_route_interrupts` decides (off until then: the kernel polls).
    let config = (config | 0x40) & !0x03 & !0x20 & !0x10;
    ps2_command(0x60);
    ps2_wait_write();
    unsafe { outb(PS2_DATA, config) };
    ps2_mouse_write(0xF6); // defaults
    ps2_mouse_write(0xF4); // report movement
    true
}

pub enum Input {
    /// A scan code (set 1) from the keyboard.
    Key(u8),
    /// A byte of a mouse packet.
    Mouse(u8),
}

/// The next byte from the keyboard or mouse: from what the interrupt
/// handler collected, or straight from the controller while it does not
/// interrupt.
pub fn ps2_poll() -> Option<Input> {
    use core::sync::atomic::Ordering;
    if INTERRUPTS.load(Ordering::Acquire) {
        let tail = TAIL.load(Ordering::Acquire);
        if tail == HEAD.load(Ordering::Acquire) {
            return None;
        }
        let entry = RING[tail % RING.len()].load(Ordering::Acquire);
        TAIL.store(tail + 1, Ordering::Release);
        let byte = entry as u8;
        return Some(if entry & 0x100 != 0 { Input::Mouse(byte) } else { Input::Key(byte) });
    }
    let status = unsafe { inb(PS2_STATUS) };
    if status & 1 == 0 {
        return None;
    }
    let byte = unsafe { inb(PS2_DATA) };
    Some(if status & 0x20 != 0 { Input::Mouse(byte) } else { Input::Key(byte) })
}

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize};

static INTERRUPTS: AtomicBool = AtomicBool::new(false);
/// Bytes the interrupt handler read (bit 8: from the mouse); the handler
/// adds at HEAD, the desktop takes from TAIL.
static RING: [AtomicU16; 512] = [const { AtomicU16::new(0) }; 512];
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);

pub const KEYBOARD_VECTOR: u8 = 40;
pub const MOUSE_VECTOR: u8 = 41;

/// The keyboard or mouse interrupted: collect what it has.
pub fn ps2_interrupt() {
    use core::sync::atomic::Ordering;
    for _ in 0..16 {
        let status = unsafe { inb(PS2_STATUS) };
        if status & 1 == 0 {
            break;
        }
        let byte = unsafe { inb(PS2_DATA) } as u16;
        let entry = byte | if status & 0x20 != 0 { 0x100 } else { 0 };
        let head = HEAD.load(Ordering::Relaxed);
        if head - TAIL.load(Ordering::Acquire) < RING.len() {
            RING[head % RING.len()].store(entry, Ordering::Release);
            HEAD.store(head + 1, Ordering::Release);
        }
    }
}

/// Wire the keyboard (IRQ 1) and mouse (IRQ 12) through the I/O APIC to
/// this CPU, and let the controller interrupt.
pub fn ps2_route_interrupts(machine: &crate::acpi::Machine) {
    use core::sync::atomic::Ordering;
    let mut routed = 0;
    for (irq, vector) in [(1u8, KEYBOARD_VECTOR), (12u8, MOUSE_VECTOR)] {
        // An override names the line and its polarity and trigger.
        let (line, flags) = machine
            .overrides
            .iter()
            .find(|(source, _, _)| *source == irq)
            .map(|&(_, line, flags)| (line, flags))
            .unwrap_or((irq as u32, 0));
        let Some(&(base, first)) = machine.io_apics.iter().find(|(base, first)| {
            line >= *first && line < first + io_apic_lines(*base)
        }) else {
            continue;
        };
        let mut low = vector as u32;
        if flags & 0x3 == 0x3 {
            low |= 1 << 13; // active low
        }
        if flags & 0xC == 0xC {
            low |= 1 << 15; // level
        }
        let index = line - first;
        let apic_id = crate::apic::apic_id();
        io_apic_write(base, 0x11 + 2 * index, apic_id << 24);
        io_apic_write(base, 0x10 + 2 * index, low);
        routed += 1;
    }
    if routed == 2 {
        INTERRUPTS.store(true, Ordering::Release);
        ps2_command(0x20);
        let config = if ps2_wait_read() { unsafe { inb(PS2_DATA) } } else { 0x40 };
        ps2_command(0x60);
        ps2_wait_write();
        unsafe { outb(PS2_DATA, config | 0x03) };
        crate::log!("OTW kernel: keyboard and mouse interrupt");
    }
}

fn io_apic_lines(base: u64) -> u32 {
    ((io_apic_read(base, 1) >> 16) & 0xFF) + 1
}

fn io_apic_read(base: u64, register: u32) -> u32 {
    let base = crate::arch::map_device(base, 0x20);
    unsafe {
        core::ptr::write_volatile(base as *mut u32, register);
        core::ptr::read_volatile((base + 0x10) as *const u32)
    }
}

fn io_apic_write(base: u64, register: u32, value: u32) {
    let base = crate::arch::map_device(base, 0x20);
    unsafe {
        core::ptr::write_volatile(base as *mut u32, register);
        core::ptr::write_volatile((base + 0x10) as *mut u32, value);
    }
}

// ── CMOS real-time clock ──

fn cmos(register: u8) -> u8 {
    unsafe {
        outb(0x70, register);
        inb(0x71)
    }
}

/// The time of day (hours, minutes, seconds) from the CMOS clock.
pub fn rtc_time() -> (u8, u8, u8) {
    while cmos(0x0A) & 0x80 != 0 {}
    let (mut s, mut m, mut h) = (cmos(0x00), cmos(0x02), cmos(0x04));
    let status_b = cmos(0x0B);
    if status_b & 0x04 == 0 {
        let bcd = |v: u8| (v & 0x0F) + (v >> 4) * 10;
        s = bcd(s);
        m = bcd(m);
        h = bcd(h & 0x7F) | (h & 0x80);
    }
    if status_b & 0x02 == 0 && h & 0x80 != 0 {
        h = ((h & 0x7F) + 12) % 24;
    }
    (h, m, s)
}

/// Seconds since 1970 from the CMOS clock (taken as UTC).
pub fn rtc_unix_time() -> u64 {
    while cmos(0x0A) & 0x80 != 0 {}
    let status_b = cmos(0x0B);
    let bcd = |v: u8| if status_b & 0x04 == 0 { (v & 0x0F) + (v >> 4) * 10 } else { v };
    let (h, m, s) = rtc_time();
    let day = bcd(cmos(0x07)) as i64;
    let month = bcd(cmos(0x08)) as i64;
    let year = 2000 + bcd(cmos(0x09)) as i64;
    // Days from a civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 86_400 + h as i64 * 3600 + m as i64 * 60 + s as i64).max(0) as u64
}
