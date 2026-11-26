//! SATA disks behind an AHCI controller (most PCs of the last fifteen
//! years), one command slot per port, polled.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use crate::disk::{Disk, DmaBuffer};
use crate::sync::SpinLock;
use crate::{apic, arch, pci};

const MAX_READ: usize = 64 * 1024;

struct Port {
    registers: u64,
    command_list: DmaBuffer,
    received: DmaBuffer,
    table: DmaBuffer,
    buffer: DmaBuffer,
}

unsafe impl Send for Port {}

pub struct SataDisk {
    port: SpinLock<Port>,
    sectors: u64,
    name: String,
}

fn get(address: u64) -> u32 {
    unsafe { read_volatile(address as *const u32) }
}

fn set(address: u64, value: u32) {
    unsafe { write_volatile(address as *mut u32, value) }
}

impl Port {
    /// Run an ATA command reading `bytes` into the buffer.
    fn command(&mut self, ata: u8, lba: u64, count: u16, bytes: usize) -> Result<(), &'static str> {
        let r = self.registers;
        // Command header 0: FIS length 5 dwords, read, one PRD entry.
        let header = self.command_list.pointer as *mut u32;
        unsafe {
            write_volatile(header, 5 | (1 << 16));
            write_volatile(header.add(1), 0);
            write_volatile(header.add(2), self.table.address() as u32);
            write_volatile(header.add(3), (self.table.address() >> 32) as u32);
            let table = self.table.pointer;
            core::ptr::write_bytes(table, 0, 256);
            let fis = [
                0x27u8, 0x80, ata, 0,
                lba as u8, (lba >> 8) as u8, (lba >> 16) as u8, 0x40,
                (lba >> 24) as u8, (lba >> 32) as u8, (lba >> 40) as u8, 0,
                count as u8, (count >> 8) as u8, 0, 0,
            ];
            core::ptr::copy_nonoverlapping(fis.as_ptr(), table, fis.len());
            let prd = table.add(0x80) as *mut u32;
            write_volatile(prd, self.buffer.address() as u32);
            write_volatile(prd.add(1), (self.buffer.address() >> 32) as u32);
            write_volatile(prd.add(3), (bytes as u32 - 1) & 0x3F_FFFF);
        }
        set(r + 0x10, u32::MAX); // clear interrupt status
        let deadline = apic::now_us() + 2_000_000;
        while get(r + 0x20) & 0x88 != 0 {
            if apic::now_us() > deadline {
                return Err("the disk stayed busy");
            }
        }
        set(r + 0x38, 1); // issue slot 0
        let deadline = apic::now_us() + 5_000_000;
        loop {
            if get(r + 0x38) & 1 == 0 {
                break;
            }
            if get(r + 0x10) & (1 << 30) != 0 {
                return Err("the disk reported an error");
            }
            if apic::now_us() > deadline {
                return Err("the disk did not answer");
            }
            core::hint::spin_loop();
        }
        if get(r + 0x20) & 1 != 0 {
            return Err("the disk reported an error");
        }
        Ok(())
    }
}

/// The SATA disks on an AHCI controller.
pub fn probe(device: &pci::Device) -> Vec<Arc<dyn Disk>> {
    let mut disks: Vec<Arc<dyn Disk>> = Vec::new();
    device.enable();
    let (base, size) = device.bar(5);
    if base == 0 {
        return disks;
    }
    let base = arch::map_device(base, size.max(0x1100));
    set(base + 0x04, get(base + 0x04) | 1 << 31); // AHCI mode
    let implemented = get(base + 0x0C);
    for index in 0..32u64 {
        if implemented & (1 << index) == 0 {
            continue;
        }
        let r = base + 0x100 + index * 0x80;
        // A device present and talking (DET 3), and a disk (not ATAPI).
        if get(r + 0x28) & 0xF != 3 || get(r + 0x24) != 0x0000_0101 {
            continue;
        }
        // Stop the port, give it our memory, start it again.
        set(r + 0x18, get(r + 0x18) & !0x11);
        let deadline = apic::now_us() + 500_000;
        while get(r + 0x18) & 0xC000 != 0 && apic::now_us() < deadline {}
        let mut port = Port {
            registers: r,
            command_list: DmaBuffer::new(1024),
            received: DmaBuffer::new(256),
            table: DmaBuffer::new(256),
            buffer: DmaBuffer::new(MAX_READ),
        };
        set(r, port.command_list.address() as u32);
        set(r + 0x04, (port.command_list.address() >> 32) as u32);
        set(r + 0x08, port.received.address() as u32);
        set(r + 0x0C, (port.received.address() >> 32) as u32);
        set(r + 0x30, u32::MAX); // clear errors
        set(r + 0x18, get(r + 0x18) | 0x10); // receive FISes
        set(r + 0x18, get(r + 0x18) | 0x01); // start
        // IDENTIFY DEVICE: the size (LBA48 sectors) and model.
        if port.command(0xEC, 0, 0, 512).is_err() {
            continue;
        }
        let id = port.buffer.slice();
        let word = |w: usize| u16::from_le_bytes([id[w * 2], id[w * 2 + 1]]) as u64;
        let sectors = word(100) | word(101) << 16 | word(102) << 32 | word(103) << 48;
        let model: String = (27..47)
            .flat_map(|w| {
                let v = word(w) as u16;
                [(v >> 8) as u8 as char, v as u8 as char]
            })
            .collect();
        disks.push(Arc::new(SataDisk { port: SpinLock::new(port), sectors, name: format!("SATA {}", model.trim()) }));
    }
    disks
}

impl Disk for SataDisk {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn sector_size(&self) -> usize {
        512
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        let mut port = self.port.lock();
        let mut done = 0;
        while done < buffer.len() {
            let n = (buffer.len() - done).min(MAX_READ);
            let count = n.div_ceil(512);
            port.command(0x25, lba + (done / 512) as u64, count as u16, count * 512)?; // READ DMA EXT
            buffer[done..done + n].copy_from_slice(&port.buffer.slice()[..n]);
            done += n;
        }
        Ok(())
    }
}
