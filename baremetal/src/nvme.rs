//! NVMe disks (most new PCs): one admin queue pair, one I/O queue pair,
//! polled for completions; read and written.

use alloc::format;
use alloc::string::String;
use core::ptr::{read_volatile, write_volatile};

use crate::disk::{Disk, DmaBuffer};
use crate::sync::SpinLock;
use crate::{apic, arch, log, pci};

const QUEUE: usize = 64;
/// Most bytes one read moves (a list of page addresses, PRP list, covers
/// it).
const MAX_READ: usize = 128 * 1024;

struct Queue {
    submission: DmaBuffer,
    completion: DmaBuffer,
    tail: usize,
    head: usize,
    phase: u32,
    id: u16,
    next_command: u16,
}

struct Controller {
    registers: u64,
    stride: u64,
    admin: Queue,
    io: Queue,
    buffer: DmaBuffer,
    prp_list: DmaBuffer,
}

pub struct Nvme {
    controller: SpinLock<Controller>,
    sector_size: usize,
    sectors: u64,
    name: String,
}

unsafe impl Send for Controller {}

impl Queue {
    fn new(id: u16) -> Self {
        Self {
            submission: DmaBuffer::new(QUEUE * 64),
            completion: DmaBuffer::new(QUEUE * 16),
            tail: 0,
            head: 0,
            phase: 1,
            id,
            next_command: 0,
        }
    }
}

impl Controller {
    fn reg32(&self, offset: u64) -> u32 {
        unsafe { read_volatile((self.registers + offset) as *const u32) }
    }

    fn set32(&self, offset: u64, value: u32) {
        unsafe { write_volatile((self.registers + offset) as *mut u32, value) }
    }

    fn set64(&self, offset: u64, value: u64) {
        self.set32(offset, value as u32);
        self.set32(offset + 4, (value >> 32) as u32);
    }

    /// Submit a command on the admin (false) or I/O (true) queue and wait
    /// for its completion; returns the status.
    fn run(&mut self, io: bool, mut command: [u32; 16]) -> Result<u32, &'static str> {
        let (registers, stride) = (self.registers, self.stride);
        let queue = if io { &mut self.io } else { &mut self.admin };
        let id = queue.next_command;
        queue.next_command = queue.next_command.wrapping_add(1);
        command[0] |= (id as u32) << 16;
        unsafe {
            let slot = (queue.submission.pointer as *mut u32).add(queue.tail * 16);
            for (i, word) in command.iter().enumerate() {
                write_volatile(slot.add(i), *word);
            }
        }
        queue.tail = (queue.tail + 1) % QUEUE;
        let doorbell = registers + 0x1000 + (2 * queue.id as u64) * (4 << stride);
        unsafe { write_volatile(doorbell as *mut u32, queue.tail as u32) };
        let deadline = apic::now_us() + 5_000_000;
        loop {
            let entry = unsafe { (queue.completion.pointer as *const u32).add(queue.head * 4) };
            let status = unsafe { read_volatile(entry.add(3)) };
            if (status >> 16) & 1 == queue.phase {
                queue.head = (queue.head + 1) % QUEUE;
                if queue.head == 0 {
                    queue.phase ^= 1;
                }
                let doorbell = registers + 0x1000 + (2 * queue.id as u64 + 1) * (4 << stride);
                unsafe { write_volatile(doorbell as *mut u32, queue.head as u32) };
                let code = (status >> 17) & 0x7FFF;
                return if code == 0 { Ok(status) } else { Err("the disk reported an error") };
            }
            if apic::now_us() > deadline {
                return Err("the disk did not answer");
            }
            core::hint::spin_loop();
        }
    }
}

impl Controller {
    /// Read (0x02) or write (0x01) `count` sectors at `lba` through the
    /// buffer.
    fn transfer(&mut self, opcode: u32, lba: u64, count: usize, sector_size: usize) -> Result<(), &'static str> {
        let address = self.buffer.address();
        let mut command = [0u32; 16];
        command[0] = opcode;
        command[1] = 1;
        command[6] = address as u32;
        command[7] = (address >> 32) as u32;
        // More than two pages: the second pointer is a list of the rest.
        let pages = (count * sector_size).div_ceil(4096);
        let second = if pages <= 1 {
            0
        } else if pages == 2 {
            address + 4096
        } else {
            let list = self.prp_list.pointer as *mut u64;
            for page in 1..pages {
                unsafe { write_volatile(list.add(page - 1), address + page as u64 * 4096) };
            }
            self.prp_list.address()
        };
        command[8] = second as u32;
        command[9] = (second >> 32) as u32;
        command[10] = lba as u32;
        command[11] = (lba >> 32) as u32;
        command[12] = count as u32 - 1;
        self.run(true, command).map(|_| ())
    }
}

pub fn probe(device: &pci::Device) -> Option<Nvme> {
    device.enable();
    let (base, size) = device.bar(0);
    if base == 0 {
        return None;
    }
    let base = arch::map_device(base, size.max(0x4000));
    let read64 = |offset: u64| unsafe { read_volatile((base + offset) as *const u64) };
    let capabilities = read64(0);
    let stride = (capabilities >> 32) & 0xF;
    let timeout_ms = ((capabilities >> 24) & 0xFF) * 500;
    let mut controller = Controller {
        registers: base,
        stride,
        admin: Queue::new(0),
        io: Queue::new(1),
        buffer: DmaBuffer::new(MAX_READ),
        prp_list: DmaBuffer::new(4096),
    };
    let wait_ready = |c: &Controller, ready: bool| {
        let deadline = apic::now_us() + timeout_ms.max(1000) * 1000;
        while (c.reg32(0x1C) & 1 != 0) != ready {
            if apic::now_us() > deadline {
                return false;
            }
            core::hint::spin_loop();
        }
        true
    };
    // Reset, give it the admin queues, start it.
    controller.set32(0x14, 0);
    if !wait_ready(&controller, false) {
        log!("OTW kernel: NVMe controller did not stop");
        return None;
    }
    controller.set32(0x24, ((QUEUE as u32 - 1) << 16) | (QUEUE as u32 - 1));
    controller.set64(0x28, controller.admin.submission.address());
    controller.set64(0x30, controller.admin.completion.address());
    controller.set32(0x14, 1 | (6 << 16) | (4 << 20)); // enable; 64-byte commands, 16-byte completions
    if !wait_ready(&controller, true) {
        log!("OTW kernel: NVMe controller did not start");
        return None;
    }

    // Identify the controller (model) and namespace 1 (size, sector size).
    let identify = DmaBuffer::new(4096);
    let mut command = [0u32; 16];
    command[0] = 0x06;
    command[6] = identify.address() as u32;
    command[7] = (identify.address() >> 32) as u32;
    command[10] = 1;
    controller.run(false, command).ok()?;
    let model: String = String::from_utf8_lossy(&identify.slice()[24..64]).trim().into();
    command[1] = 1;
    command[10] = 0;
    controller.run(false, command).ok()?;
    let data = identify.slice();
    let sectors = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let format = (data[26] & 0xF) as usize;
    let lba_shift = data[128 + format * 4 + 2];
    let sector_size = 1usize << lba_shift;

    // The I/O queues: completions first.
    let mut command = [0u32; 16];
    command[0] = 0x05;
    command[6] = controller.io.completion.address() as u32;
    command[7] = (controller.io.completion.address() >> 32) as u32;
    command[10] = ((QUEUE as u32 - 1) << 16) | 1;
    command[11] = 1; // physically contiguous, no interrupts
    controller.run(false, command).ok()?;
    let mut command = [0u32; 16];
    command[0] = 0x01;
    command[6] = controller.io.submission.address() as u32;
    command[7] = (controller.io.submission.address() >> 32) as u32;
    command[10] = ((QUEUE as u32 - 1) << 16) | 1;
    command[11] = (1 << 16) | 1; // completion queue 1, contiguous
    controller.run(false, command).ok()?;

    Some(Nvme {
        controller: SpinLock::new(controller),
        sector_size,
        sectors,
        name: format!("NVMe {model}"),
    })
}

impl Disk for Nvme {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn sector_size(&self) -> usize {
        self.sector_size
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        let mut controller = self.controller.lock();
        let mut done = 0;
        while done < buffer.len() {
            let n = (buffer.len() - done).min(MAX_READ);
            let first = lba + (done / self.sector_size) as u64;
            controller.transfer(0x02, first, n.div_ceil(self.sector_size), self.sector_size)?;
            buffer[done..done + n].copy_from_slice(&controller.buffer.slice()[..n]);
            done += n;
        }
        Ok(())
    }

    fn writable(&self) -> bool {
        true
    }

    fn write(&self, lba: u64, data: &[u8]) -> Result<(), &'static str> {
        if data.len() % self.sector_size != 0 {
            return Err("a write of part of a sector");
        }
        let mut controller = self.controller.lock();
        let mut done = 0;
        while done < data.len() {
            let n = (data.len() - done).min(MAX_READ);
            unsafe { core::ptr::copy_nonoverlapping(data[done..].as_ptr(), controller.buffer.pointer, n) };
            let first = lba + (done / self.sector_size) as u64;
            controller.transfer(0x01, first, n / self.sector_size, self.sector_size)?;
            done += n;
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), &'static str> {
        let mut controller = self.controller.lock();
        let mut command = [0u32; 16];
        command[0] = 0x00; // FLUSH
        command[1] = 1;
        controller.run(true, command).map(|_| ())
    }
}
