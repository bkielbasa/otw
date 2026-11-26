//! Network cards that virtual machines offer (virtio-net, version 1 of the
//! virtio PCI interface): a queue of buffers the card fills with frames it
//! receives, one of frames to send. Polled.

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use crate::disk::DmaBuffer;
use crate::{arch, pci};

const QUEUE_SIZE: u16 = 128;
const BUFFER: usize = 2048;
/// The header before each frame (virtio_net_hdr with num_buffers).
const HEADER: usize = 12;

struct Queue {
    size: u16,
    descriptors: DmaBuffer,
    available: DmaBuffer,
    used: DmaBuffer,
    buffers: Vec<DmaBuffer>,
    /// Where the card is told to look at this queue.
    notify: u64,
    /// Its number (what a notification says).
    number: u16,
    index: u16,
    /// Used entries already seen.
    seen: u16,
    /// Descriptors free to send with (transmit queue).
    free: Vec<u16>,
}

impl Queue {
    fn new(size: u16, notify: u64, number: u16) -> Self {
        let n = size as usize;
        Self {
            size,
            descriptors: DmaBuffer::new(16 * n),
            available: DmaBuffer::new(6 + 2 * n),
            used: DmaBuffer::new(6 + 8 * n),
            buffers: (0..n).map(|_| DmaBuffer::new(BUFFER)).collect(),
            notify,
            number,
            index: 0,
            seen: 0,
            free: (0..size).collect(),
        }
    }

    fn set_descriptor(&self, i: u16, length: u32, writable: bool) {
        let d = unsafe { self.descriptors.pointer.add(16 * i as usize) };
        unsafe {
            write_volatile(d as *mut u64, self.buffers[i as usize].address());
            write_volatile(d.add(8) as *mut u32, length);
            write_volatile(d.add(12) as *mut u16, if writable { 2 } else { 0 });
            write_volatile(d.add(14) as *mut u16, 0);
        }
    }

    /// Offer descriptor `i` to the card.
    fn offer(&mut self, i: u16) {
        let ring = self.available.pointer;
        unsafe {
            write_volatile(ring.add(4 + 2 * (self.index % self.size) as usize) as *mut u16, i);
            fence(Ordering::SeqCst);
            self.index = self.index.wrapping_add(1);
            write_volatile(ring.add(2) as *mut u16, self.index);
            fence(Ordering::SeqCst);
            write_volatile(self.notify as *mut u16, self.number);
        }
    }

    /// The next entry the card is done with: (descriptor, length).
    fn take_used(&mut self) -> Option<(u16, u32)> {
        let ring = self.used.pointer;
        let index = unsafe { read_volatile(ring.add(2) as *const u16) };
        if index == self.seen {
            return None;
        }
        fence(Ordering::SeqCst);
        let entry = unsafe { ring.add(4 + 8 * (self.seen % self.size) as usize) };
        let id = unsafe { read_volatile(entry as *const u32) } as u16;
        let length = unsafe { read_volatile(entry.add(4) as *const u32) };
        self.seen = self.seen.wrapping_add(1);
        Some((id, length))
    }
}

pub struct VirtioNet {
    pub mac: [u8; 6],
    pub received: u64,
    pub sent: u64,
    receive: Queue,
    send: Queue,
}

unsafe impl Send for VirtioNet {}

/// The card's capabilities: (BAR, offset, length, notify multiplier) by kind.
fn capabilities(device: &pci::Device) -> [(u8, u32, u32, u32); 5] {
    let byte = |offset: u8| (device.read(offset & !3) >> ((offset & 3) * 8)) as u8;
    let mut found = [(0xFF, 0, 0, 0); 5];
    if device.read(0x04) >> 16 & 0x10 == 0 {
        return found;
    }
    let mut at = byte(0x34) & !3;
    let mut guard = 0;
    while at != 0 && guard < 48 {
        guard += 1;
        if byte(at) == 0x09 {
            let kind = byte(at + 3);
            let bar = byte(at + 4);
            let offset = device.read(at + 8);
            let length = device.read(at + 12);
            let multiplier = if kind == 2 { device.read(at + 16) } else { 0 };
            if (1..=4).contains(&kind) && found[kind as usize].0 == 0xFF {
                found[kind as usize] = (bar, offset, length, multiplier);
            }
        }
        at = byte(at + 1) & !3;
    }
    found
}

pub fn probe(device: &pci::Device) -> Option<VirtioNet> {
    // Red Hat's virtio vendor; network: transitional 0x1000 or 0x1041.
    if device.vendor != 0x1AF4 || !(device.device == 0x1000 || device.device == 0x1041) {
        return None;
    }
    let caps = capabilities(device);
    let (common_cap, notify_cap, device_cap) = (caps[1], caps[2], caps[4]);
    if common_cap.0 == 0xFF || notify_cap.0 == 0xFF {
        crate::log!("OTW kernel: network card without virtio 1.0 registers");
        return None;
    }
    device.enable();
    let map = |cap: (u8, u32, u32, u32)| {
        let (base, size) = device.bar(cap.0);
        if base == 0 {
            return 0;
        }
        arch::map_device(base, size) + cap.1 as u64
    };
    let common = map(common_cap);
    let notify_base = map(notify_cap);
    let config = if device_cap.0 != 0xFF { map(device_cap) } else { 0 };
    if common == 0 || notify_base == 0 {
        return None;
    }
    let r8 = |o: u64| unsafe { read_volatile((common + o) as *const u8) };
    let w8 = |o: u64, v: u8| unsafe { write_volatile((common + o) as *mut u8, v) };
    let r16 = |o: u64| unsafe { read_volatile((common + o) as *const u16) };
    let w16 = |o: u64, v: u16| unsafe { write_volatile((common + o) as *mut u16, v) };
    let r32 = |o: u64| unsafe { read_volatile((common + o) as *const u32) };
    let w32 = |o: u64, v: u32| unsafe { write_volatile((common + o) as *mut u32, v) };
    let w64 = |o: u64, v: u64| {
        w32(o, v as u32);
        w32(o + 4, (v >> 32) as u32);
    };
    // Reset, then: we see it (1), we drive it (2).
    w8(0x14, 0);
    while r8(0x14) != 0 {}
    w8(0x14, 1);
    w8(0x14, 1 | 2);
    w32(0x00, 0);
    let low = r32(0x04);
    w32(0x00, 1);
    let high = r32(0x04);
    if high & 1 == 0 {
        return None; // no VERSION_1
    }
    // Features: its MAC address (5), version 1 (32).
    w32(0x08, 0);
    w32(0x0C, low & (1 << 5));
    w32(0x08, 1);
    w32(0x0C, 1);
    w8(0x14, 1 | 2 | 8);
    if r8(0x14) & 8 == 0 {
        crate::log!("OTW kernel: the network card refused its features");
        return None;
    }
    let mut queues = Vec::new();
    for index in 0..2u16 {
        w16(0x16, index);
        let size = r16(0x18).min(QUEUE_SIZE);
        if size == 0 {
            return None;
        }
        w16(0x18, size);
        let notify = notify_base + r16(0x1E) as u64 * notify_cap.3 as u64;
        let queue = Queue::new(size, notify, index);
        w64(0x20, queue.descriptors.address());
        w64(0x28, queue.available.address());
        w64(0x30, queue.used.address());
        w16(0x1C, 1);
        queues.push(queue);
    }
    let mut mac = [0x52, 0x54, 0, 0x12, 0x34, 0x56];
    if low & (1 << 5) != 0 && config != 0 {
        for (i, byte) in mac.iter_mut().enumerate() {
            *byte = unsafe { read_volatile((config + i as u64) as *const u8) };
        }
    }
    w8(0x14, 1 | 2 | 8 | 4); // ready
    let send = queues.pop()?;
    let mut receive = queues.pop()?;
    // Every receive buffer goes to the card.
    for i in 0..receive.size {
        receive.set_descriptor(i, BUFFER as u32, true);
        receive.offer(i);
    }
    receive.free.clear();
    Some(VirtioNet { mac, received: 0, sent: 0, receive, send })
}

impl VirtioNet {
    /// A frame the card received.
    pub fn receive(&mut self) -> Option<Vec<u8>> {
        let (id, length) = self.receive.take_used()?;
        let buffer = self.receive.buffers[id as usize].slice();
        let frame = buffer[HEADER.min(length as usize)..(length as usize).min(BUFFER)].to_vec();
        self.receive.offer(id);
        self.received += 1;
        Some(frame)
    }

    /// Room to send a frame.
    pub fn can_send(&mut self) -> bool {
        while let Some((id, _)) = self.send.take_used() {
            self.send.free.push(id);
        }
        !self.send.free.is_empty()
    }

    /// Send a frame of `length` bytes that `fill` writes.
    pub fn send<R>(&mut self, length: usize, fill: impl FnOnce(&mut [u8]) -> R) -> R {
        let length = length.min(BUFFER - HEADER);
        let id = self.can_send().then(|| self.send.free.pop()).flatten();
        let Some(id) = id else {
            // No room (smoltcp asked first, so rare): drop the frame.
            let mut scratch = alloc::vec![0u8; length];
            return fill(&mut scratch);
        };
        let buffer = &self.send.buffers[id as usize];
        let bytes = unsafe { core::slice::from_raw_parts_mut(buffer.pointer, BUFFER) };
        bytes[..HEADER].fill(0);
        let result = fill(&mut bytes[HEADER..HEADER + length]);
        self.send.set_descriptor(id, (HEADER + length) as u32, false);
        self.send.offer(id);
        self.sent += 1;
        result
    }
}
