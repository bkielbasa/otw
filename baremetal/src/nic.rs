//! What the TCP/IP stack needs of a network card, and finding one: virtio
//! (virtual machines), Intel's gigabit cards (e1000, e1000e, igb) and
//! Realtek's (RTL8169/8168/8111/8125). All polled, no interrupts.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::pci;

pub trait Nic: Send {
    /// What kind of card, for the log.
    fn name(&self) -> &'static str;
    fn mac(&self) -> [u8; 6];
    /// A frame the card received (Ethernet, without its checksum).
    fn receive(&mut self) -> Option<Vec<u8>>;
    /// Room to send a frame.
    fn can_send(&mut self) -> bool;
    /// Send an Ethernet frame (the card adds the checksum). Dropped when
    /// there is no room.
    fn send(&mut self, frame: &[u8]);
    /// Whether a cable is in and the link negotiated.
    fn link_up(&mut self) -> bool {
        true
    }
    /// Frames received and sent so far.
    fn counters(&self) -> (u64, u64);
}

/// The first network card a driver here takes.
pub fn find(devices: &[pci::Device]) -> Option<Box<dyn Nic>> {
    for device in devices {
        if let Some(card) = crate::virtio_net::probe(device) {
            return Some(Box::new(card));
        }
        if let Some(card) = crate::intel_net::probe(device) {
            return Some(Box::new(card));
        }
        if let Some(card) = crate::realtek_net::probe(device) {
            return Some(Box::new(card));
        }
        if device.class == 2 {
            crate::log!(
                "OTW kernel: network card {:04x}:{:04x} at {:02x}:{:02x}.{} has no driver here",
                device.vendor,
                device.device,
                device.bus,
                device.slot,
                device.function
            );
        }
    }
    None
}

/// A ring of buffers the card reads or writes, each `size` bytes, in one
/// physically contiguous block.
pub struct Buffers {
    block: crate::disk::DmaBuffer,
    size: usize,
}

impl Buffers {
    pub fn new(count: usize, size: usize) -> Self {
        Self { block: crate::disk::DmaBuffer::new(count * size), size }
    }

    pub fn address(&self, i: usize) -> u64 {
        self.block.address() + (i * self.size) as u64
    }

    pub fn get(&self, i: usize) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.block.pointer.add(i * self.size), self.size) }
    }
}

/// Registers a card has in memory.
#[derive(Clone, Copy)]
pub struct Registers(pub u64);

impl Registers {
    pub fn r8(&self, offset: u32) -> u8 {
        unsafe { core::ptr::read_volatile((self.0 + offset as u64) as *const u8) }
    }
    pub fn r32(&self, offset: u32) -> u32 {
        unsafe { core::ptr::read_volatile((self.0 + offset as u64) as *const u32) }
    }
    pub fn w8(&self, offset: u32, value: u8) {
        unsafe { core::ptr::write_volatile((self.0 + offset as u64) as *mut u8, value) }
    }
    pub fn w16(&self, offset: u32, value: u16) {
        unsafe { core::ptr::write_volatile((self.0 + offset as u64) as *mut u16, value) }
    }
    pub fn w32(&self, offset: u32, value: u32) {
        unsafe { core::ptr::write_volatile((self.0 + offset as u64) as *mut u32, value) }
    }

    /// Map the device's first memory BAR.
    pub fn of(device: &pci::Device) -> Option<Self> {
        let (_, base, size) = device.memory_bar()?;
        Some(Self(crate::arch::map_device(base, size)))
    }
}

/// Wait up to `us` microseconds for `done`.
pub fn wait_for(us: u64, mut done: impl FnMut() -> bool) -> bool {
    let start = crate::apic::now_us();
    while !done() {
        if crate::apic::now_us() - start > us {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// Wait `us` microseconds (spinning: at boot, before tasks).
pub fn delay_us(us: u64) {
    wait_for(us, || false);
}
