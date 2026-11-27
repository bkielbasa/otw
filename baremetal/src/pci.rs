//! PCI devices, through the configuration ports every PC has (0xCF8/0xCFC).

use alloc::vec::Vec;

use crate::io::{inb, outb};

#[derive(Clone, Copy, Debug)]
pub struct Device {
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub interface: u8,
}

fn outl(port: u16, value: u32) {
    unsafe { core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack)) };
}

fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe { core::arch::asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack)) };
    value
}

fn address(bus: u8, slot: u8, function: u8, offset: u8) -> u32 {
    1 << 31 | (bus as u32) << 16 | (slot as u32) << 11 | (function as u32) << 8 | (offset as u32 & 0xFC)
}

impl Device {
    pub fn read(&self, offset: u8) -> u32 {
        outl(0xCF8, address(self.bus, self.slot, self.function, offset));
        inl(0xCFC)
    }

    pub fn write(&self, offset: u8, value: u32) {
        outl(0xCF8, address(self.bus, self.slot, self.function, offset));
        outl(0xCFC, value);
    }

    /// The memory address a base address register points at (64-bit BARs
    /// take two), and the size of the region.
    pub fn bar(&self, index: u8) -> (u64, u64) {
        let offset = 0x10 + index * 4;
        let low = self.read(offset);
        if low & 1 != 0 {
            return (0, 0); // I/O ports
        }
        let wide = low & 0x6 == 0x4;
        let high = if wide { self.read(offset + 4) } else { 0 };
        // Size: write all ones, read back the mask, restore.
        self.write(offset, u32::MAX);
        let mask = self.read(offset) & !0xF;
        self.write(offset, low);
        let size = (!mask).wrapping_add(1) as u64;
        ((high as u64) << 32 | (low & !0xF) as u64, size)
    }

    /// The first base address register that points at memory, if any:
    /// (index, address, size).
    pub fn memory_bar(&self) -> Option<(u8, u64, u64)> {
        let mut index = 0;
        while index < 6 {
            let low = self.read(0x10 + index * 4);
            if low & 1 == 0 {
                let (base, size) = self.bar(index);
                if base != 0 && size != 0 {
                    return Some((index, base, size));
                }
            }
            // A 64-bit memory BAR takes two.
            index += if low & 1 == 0 && low & 0x6 == 0x4 { 2 } else { 1 };
        }
        None
    }

    /// Let the device reach memory (bus mastering) and its registers.
    pub fn enable(&self) {
        let command = self.read(0x04);
        // Memory space, bus master; interrupts off (the kernel polls).
        self.write(0x04, (command & 0xFFFF) | 0x2 | 0x4 | 0x400);
    }
}

/// Every PCI function on every bus.
pub fn scan() -> Vec<Device> {
    let _ = (inb, outb);
    let mut devices = Vec::new();
    for bus in 0..=255u8 {
        for slot in 0..32u8 {
            for function in 0..8u8 {
                outl(0xCF8, address(bus, slot, function, 0));
                let id = inl(0xCFC);
                if id & 0xFFFF == 0xFFFF {
                    if function == 0 {
                        break;
                    }
                    continue;
                }
                let device = Device {
                    bus,
                    slot,
                    function,
                    vendor: id as u16,
                    device: (id >> 16) as u16,
                    class: 0,
                    subclass: 0,
                    interface: 0,
                };
                let class = device.read(0x08);
                let device = Device { class: (class >> 24) as u8, subclass: (class >> 16) as u8, interface: (class >> 8) as u8, ..device };
                devices.push(device);
                // A single-function device answers on every function.
                if function == 0 && device.read(0x0C) >> 16 & 0x80 == 0 {
                    break;
                }
            }
        }
    }
    devices
}
