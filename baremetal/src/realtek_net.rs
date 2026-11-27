//! Realtek's PCI Express network cards: RTL8169, RTL8168/8111 (most
//! desktop boards with a Realtek port), RTL8101/8102 (fast Ethernet) and
//! RTL8125 (2.5 Gb/s). One receive and one send ring of descriptors,
//! polled. The PHY is left as the firmware set it up.

use alloc::vec::Vec;

use crate::nic::{wait_for, Buffers, Nic, Registers};
use crate::pci;

const RING: usize = 64;
const BUFFER: usize = 2048;

const TNPDS: u32 = 0x20;
const CHIP_CMD: u32 = 0x37;
const TX_POLL: u32 = 0x38;
const INTR_MASK: u32 = 0x3C;
const INTR_STATUS: u32 = 0x3E;
const TX_CONFIG: u32 = 0x40;
const RX_CONFIG: u32 = 0x44;
const CFG9346: u32 = 0x50;
const PHY_STATUS: u32 = 0x6C;
const RX_MAX_SIZE: u32 = 0xDA;
const RDSAR: u32 = 0xE4;
const MAX_TX_PACKET: u32 = 0xEC;
const MISC: u32 = 0xF0;
/// RTL8125's interrupt mask and status (32 bits), and send poll.
const INTR_MASK_8125: u32 = 0x38;
const INTR_STATUS_8125: u32 = 0x3C;
const TX_POLL_8125: u32 = 0x90;

const OWN: u32 = 1 << 31;
const END_OF_RING: u32 = 1 << 30;
const FIRST: u32 = 1 << 29;
const LAST: u32 = 1 << 28;
const RX_ERROR: u32 = 1 << 21;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Chip {
    Rtl8169,
    /// RTL8168/8111 and RTL8101/8102: the PCI Express ones.
    Rtl8168,
    Rtl8125,
}

/// Which of Realtek's cards (or ones built on them) a device is.
pub fn chip(vendor: u16, device: u16) -> Option<Chip> {
    match (vendor, device) {
        (0x10EC, 0x8169 | 0x8167) | (0x1186, 0x4300 | 0x4302) | (0x1259, 0xC107) | (0x16EC, 0x0116) | (0x1737, 0x1032) => {
            Some(Chip::Rtl8169)
        }
        (0x10EC, 0x8168 | 0x8161 | 0x8136 | 0x2502 | 0x2600) | (0x1186, 0x4B10) => Some(Chip::Rtl8168),
        (0x10EC, 0x8125 | 0x8126 | 0x3000 | 0x5000) => Some(Chip::Rtl8125),
        _ => None,
    }
}

/// A receive descriptor the card may fill.
pub fn rx_descriptor(descriptor: &mut [u8], buffer: u64, last: bool) {
    let opts = OWN | if last { END_OF_RING } else { 0 } | BUFFER as u32;
    descriptor[0..4].copy_from_slice(&opts.to_le_bytes());
    descriptor[4..8].fill(0);
    descriptor[8..16].copy_from_slice(&buffer.to_le_bytes());
}

/// A filled receive descriptor's frame length without its checksum, if
/// the card is done with it (Some(0) for a frame in error or in pieces).
pub fn rx_done(descriptor: &[u8]) -> Option<usize> {
    let opts = u32::from_le_bytes(descriptor[0..4].try_into().unwrap());
    if opts & OWN != 0 {
        return None;
    }
    let whole = opts & (FIRST | LAST) == FIRST | LAST && opts & RX_ERROR == 0;
    let length = (opts & 0x3FFF) as usize;
    Some(if whole && length > 4 { length - 4 } else { 0 })
}

/// A send descriptor for one whole frame.
pub fn tx_descriptor(descriptor: &mut [u8], buffer: u64, length: usize, last: bool) {
    descriptor[4..8].fill(0);
    descriptor[8..16].copy_from_slice(&buffer.to_le_bytes());
    let opts = OWN | FIRST | LAST | if last { END_OF_RING } else { 0 } | length as u32;
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    descriptor[0..4].copy_from_slice(&opts.to_le_bytes());
}

pub fn tx_done(descriptor: &[u8]) -> bool {
    u32::from_le_bytes(descriptor[0..4].try_into().unwrap()) & OWN == 0
}

pub struct RealtekNet {
    chip: Chip,
    regs: Registers,
    mac: [u8; 6],
    receive: Buffers,
    receive_ring: Buffers,
    send: Buffers,
    send_ring: Buffers,
    rx_next: usize,
    tx_next: usize,
    tx_clean: usize,
    received: u64,
    sent: u64,
}

unsafe impl Send for RealtekNet {}

pub fn probe(device: &pci::Device) -> Option<RealtekNet> {
    let chip = chip(device.vendor, device.device)?;
    let regs = Registers::of(device)?;
    device.enable();
    let card = RealtekNet::start(chip, regs);
    crate::log!(
        "OTW kernel: Realtek network card {:04x} ({:?}, version {:#x}), link {}",
        device.device,
        chip,
        regs.r32(TX_CONFIG) >> 20 & 0x7CF,
        if regs.r8(PHY_STATUS) & 2 != 0 { "up" } else { "down (yet)" }
    );
    Some(card)
}

impl RealtekNet {
    fn start(chip: Chip, regs: Registers) -> Self {
        let mask_all = || {
            if chip == Chip::Rtl8125 {
                regs.w32(INTR_MASK_8125, 0);
                regs.w32(INTR_STATUS_8125, u32::MAX);
            } else {
                regs.w16(INTR_MASK, 0);
                regs.w16(INTR_STATUS, 0xFFFF);
            }
        };
        mask_all();
        regs.w8(CHIP_CMD, 0x10);
        wait_for(100_000, || regs.r8(CHIP_CMD) & 0x10 == 0);
        mask_all();
        let mut mac = [0u8; 6];
        for (i, byte) in mac.iter_mut().enumerate() {
            *byte = regs.r8(i as u32);
        }
        let card = Self {
            chip,
            regs,
            mac,
            receive: Buffers::new(RING, BUFFER),
            receive_ring: Buffers::new(RING, 16),
            send: Buffers::new(RING, BUFFER),
            send_ring: Buffers::new(RING, 16),
            rx_next: 0,
            tx_next: 0,
            tx_clean: 0,
            received: 0,
            sent: 0,
        };
        for i in 0..RING {
            let address = card.receive.address(i);
            rx_descriptor(card.receive_ring.get(i), address, i == RING - 1);
            card.send_ring.get(i).fill(0);
        }
        regs.w8(CFG9346, 0xC0); // configuration writable
        if chip != Chip::Rtl8169 {
            // Newer chips come out of reset with receiving gated off.
            regs.w32(MISC, regs.r32(MISC) & !(1 << 19));
        }
        regs.w16(RX_MAX_SIZE, BUFFER as u16);
        regs.w8(MAX_TX_PACKET, 0x3F);
        let ring = card.send_ring.address(0);
        regs.w32(TNPDS, ring as u32);
        regs.w32(TNPDS + 4, (ring >> 32) as u32);
        let ring = card.receive_ring.address(0);
        regs.w32(RDSAR, ring as u32);
        regs.w32(RDSAR + 4, (ring >> 32) as u32);
        regs.w8(CHIP_CMD, 0x0C); // receive and send
        // Gap between frames (96 bits), any size of DMA burst.
        regs.w32(TX_CONFIG, 3 << 24 | 7 << 8);
        // No early threshold, any DMA burst; broadcasts, multicasts and
        // frames for this address.
        let fetch = if chip == Chip::Rtl8125 { 8 << 27 } else { 0 };
        regs.w32(RX_CONFIG, fetch | 7 << 13 | 7 << 8 | 0x0E);
        regs.w32(0x08, u32::MAX);
        regs.w32(0x0C, u32::MAX);
        regs.w8(CFG9346, 0);
        regs.w8(CHIP_CMD, 0x0C);
        card
    }

    fn reclaim(&mut self) {
        while self.tx_clean != self.tx_next && tx_done(self.send_ring.get(self.tx_clean)) {
            self.tx_clean = (self.tx_clean + 1) % RING;
        }
    }
}

impl Nic for RealtekNet {
    fn name(&self) -> &'static str {
        "Realtek"
    }

    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        loop {
            let i = self.rx_next;
            let length = rx_done(self.receive_ring.get(i))?;
            let frame = (length > 0).then(|| self.receive.get(i)[..length.min(BUFFER)].to_vec());
            let address = self.receive.address(i);
            rx_descriptor(self.receive_ring.get(i), address, i == RING - 1);
            self.rx_next = (i + 1) % RING;
            if let Some(frame) = frame {
                self.received += 1;
                return Some(frame);
            }
        }
    }

    fn can_send(&mut self) -> bool {
        self.reclaim();
        (self.tx_next + 1) % RING != self.tx_clean
    }

    fn send(&mut self, frame: &[u8]) {
        if !self.can_send() {
            return;
        }
        let i = self.tx_next;
        // Short frames padded here (not every chip does it).
        let length = frame.len().clamp(60, BUFFER);
        let buffer = self.send.get(i);
        buffer[..frame.len().min(BUFFER)].copy_from_slice(&frame[..frame.len().min(BUFFER)]);
        buffer[frame.len().min(length)..length].fill(0);
        let address = self.send.address(i);
        tx_descriptor(self.send_ring.get(i), address, length, i == RING - 1);
        self.tx_next = (i + 1) % RING;
        if self.chip == Chip::Rtl8125 {
            self.regs.w16(TX_POLL_8125, 1);
        } else {
            self.regs.w8(TX_POLL, 0x40);
        }
        self.sent += 1;
    }

    fn link_up(&mut self) -> bool {
        self.regs.r8(PHY_STATUS) & 2 != 0
    }

    fn counters(&self) -> (u64, u64) {
        (self.received, self.sent)
    }
}
