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

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = writeln!($crate::io::Serial, $($arg)*);
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
    // Keep translation (scan code set 1), interrupts off: the kernel polls.
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

pub fn ps2_poll() -> Option<Input> {
    let status = unsafe { inb(PS2_STATUS) };
    if status & 1 == 0 {
        return None;
    }
    let byte = unsafe { inb(PS2_DATA) };
    Some(if status & 0x20 != 0 { Input::Mouse(byte) } else { Input::Key(byte) })
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
