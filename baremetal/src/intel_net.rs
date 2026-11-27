//! Intel's gigabit network cards: the e1000 family (8254x, what most
//! virtual machines and old PCs have), e1000e (8257x, 82583 and the
//! I217/I218/I219 in chipsets, what most Intel PCs have) and igb/igc
//! (82575/82576, I350, I210/I211, I225/I226). One receive and one send
//! ring of descriptors, polled; the first two take the old (legacy)
//! descriptors, igb and igc the advanced ones.

use alloc::vec::Vec;

use crate::nic::{wait_for, Buffers, Nic, Registers};
use crate::pci;

const RING: usize = 64;
const BUFFER: usize = 2048;

const CTRL: u32 = 0x0000;
const STATUS: u32 = 0x0008;
const EERD: u32 = 0x0014;
const CTRL_EXT: u32 = 0x0018;
const MDIC: u32 = 0x0020;
const ICR: u32 = 0x00C0;
const IMC: u32 = 0x00D8;
const RCTL: u32 = 0x0100;
const TCTL: u32 = 0x0400;
const TIPG: u32 = 0x0410;
const EXTCNF_CTRL: u32 = 0x0F00;
const EIMC: u32 = 0x1528;
const MTA: u32 = 0x5200;
const RAL: u32 = 0x5400;
const RAH: u32 = 0x5404;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Family {
    /// 8254x.
    E1000,
    /// 8257x, 82583, ICH8–10 and the PCH ones (82577–82579, I217–I219).
    E1000e,
    /// The same, in a chipset (PCH): shares its PHY with the management
    /// engine.
    Pch,
    /// 82575/82576, I350, I354, I210/I211.
    Igb,
    /// I225/I226.
    Igc,
}

const E1000: &[u16] = &[
    0x1000, 0x1001, 0x1004, 0x1008, 0x1009, 0x100C, 0x100D, 0x100E, 0x100F, 0x1010, 0x1011, 0x1012, 0x1013, 0x1014,
    0x1015, 0x1016, 0x1017, 0x1018, 0x1019, 0x101A, 0x101D, 0x101E, 0x1026, 0x1027, 0x1028, 0x1075, 0x1076, 0x1077,
    0x1078, 0x1079, 0x107A, 0x107B, 0x107C, 0x108A, 0x1099, 0x10B5,
];
const E1000E: &[u16] = &[
    0x105E, 0x105F, 0x1060, 0x107D, 0x107E, 0x107F, 0x108B, 0x108C, 0x109A, 0x10A4, 0x10A5, 0x10B9, 0x10BC, 0x10D3,
    0x10D5, 0x10D9, 0x10DA, 0x10F6, 0x150C, 0x1049, 0x104A, 0x104B, 0x104C, 0x104D, 0x10BA, 0x10BB, 0x10BD, 0x10BF,
    0x10C0, 0x10C2, 0x10C3, 0x10C4, 0x10C5, 0x10CB, 0x10CC, 0x10CD, 0x10CE, 0x10DE, 0x10DF, 0x10E5, 0x294C,
];
const PCH: &[u16] = &[
    0x10EA, 0x10EB, 0x10EF, 0x10F0, 0x1502, 0x1503, 0x153A, 0x153B, 0x155A, 0x1559, 0x15A0, 0x15A1, 0x15A2, 0x15A3,
    0x156F, 0x1570, 0x15B7, 0x15B8, 0x15B9, 0x15BB, 0x15BC, 0x15BD, 0x15BE, 0x15D6, 0x15D7, 0x15D8, 0x15DF, 0x15E0,
    0x15E1, 0x15E2, 0x15E3, 0x15F4, 0x15F5, 0x15F9, 0x15FA, 0x15FB, 0x15FC, 0x0D4C, 0x0D4D, 0x0D4E, 0x0D4F, 0x0D53,
    0x0D55, 0x0DC5, 0x0DC6, 0x0DC7, 0x0DC8, 0x1A1C, 0x1A1D, 0x1A1E, 0x1A1F, 0x550A, 0x550B, 0x550C, 0x550D, 0x550E,
    0x550F, 0x5510, 0x5511, 0x57A0, 0x57A1, 0x57B3, 0x57B4, 0x57B5, 0x57B6, 0x57B7, 0x57B8, 0x57B9, 0x57BA,
];
const IGB: &[u16] = &[
    0x10A7, 0x10A9, 0x10D6, 0x10C9, 0x10E6, 0x10E7, 0x10E8, 0x150A, 0x150D, 0x1518, 0x1526, 0x1521, 0x1522, 0x1523,
    0x1524, 0x1546, 0x1533, 0x1534, 0x1535, 0x1536, 0x1537, 0x1538, 0x157B, 0x157C, 0x15F6, 0x1539, 0x1F40, 0x1F41,
    0x1F45, 0x0438, 0x043A, 0x043C, 0x0440,
];
const IGC: &[u16] = &[
    0x15F2, 0x15F3, 0x15F7, 0x15F8, 0x15FD, 0x0D9F, 0x125B, 0x125C, 0x125D, 0x125F, 0x3100, 0x3101, 0x3102, 0x5502,
    0x5503,
];

/// Which of Intel's cards a device is, if it is one this driver takes.
pub fn family(vendor: u16, device: u16) -> Option<Family> {
    if vendor != 0x8086 {
        return None;
    }
    [(E1000, Family::E1000), (E1000E, Family::E1000e), (PCH, Family::Pch), (IGB, Family::Igb), (IGC, Family::Igc)]
        .iter()
        .find(|(ids, _)| ids.contains(&device))
        .map(|&(_, family)| family)
}

impl Family {
    fn advanced(self) -> bool {
        matches!(self, Family::Igb | Family::Igc)
    }
    /// Where receive queue 0's registers start; send queue 0's.
    fn queues(self) -> (u32, u32) {
        if self.advanced() {
            (0xC000, 0xE000)
        } else {
            (0x2800, 0x3800)
        }
    }
}

pub struct IntelNet {
    family: Family,
    regs: Registers,
    mac: [u8; 6],
    receive: Buffers,
    receive_ring: Buffers,
    send: Buffers,
    send_ring: Buffers,
    /// The next receive descriptor the card fills.
    rx_next: usize,
    /// The next send descriptor to fill; the oldest not yet sent.
    tx_next: usize,
    tx_clean: usize,
    rx_tail: u32,
    tx_tail: u32,
    received: u64,
    sent: u64,
}

unsafe impl Send for IntelNet {}

fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// A legacy receive descriptor: where the frame goes.
pub fn legacy_rx(descriptor: &mut [u8], buffer: u64) {
    descriptor.fill(0);
    put64(descriptor, 0, buffer);
}

/// An advanced (one buffer) receive descriptor.
pub fn advanced_rx(descriptor: &mut [u8], buffer: u64) {
    descriptor.fill(0);
    put64(descriptor, 0, buffer);
}

/// A received descriptor's frame length, if the card is done with it
/// (and it holds a whole frame without errors; Some(0) otherwise).
pub fn rx_done(descriptor: &[u8], advanced: bool) -> Option<usize> {
    if advanced {
        let status = u32::from_le_bytes(descriptor[8..12].try_into().unwrap());
        if status & 1 == 0 {
            return None;
        }
        // End of packet; errors in the top byte.
        let good = status & 2 != 0 && status >> 20 == 0;
        Some(if good { u16::from_le_bytes([descriptor[12], descriptor[13]]) as usize } else { 0 })
    } else {
        let status = descriptor[12];
        if status & 1 == 0 {
            return None;
        }
        let good = status & 2 != 0 && descriptor[13] == 0;
        Some(if good { u16::from_le_bytes([descriptor[8], descriptor[9]]) as usize } else { 0 })
    }
}

/// A send descriptor for one whole frame, reported back when sent.
pub fn tx_descriptor(descriptor: &mut [u8], buffer: u64, length: usize, advanced: bool) {
    descriptor.fill(0);
    put64(descriptor, 0, buffer);
    if advanced {
        // Data descriptor (3), extended; end of packet, insert checksum,
        // report status.
        let command = length as u32 | 3 << 20 | 1 << 29 | 1 << 24 | 1 << 25 | 1 << 27;
        descriptor[8..12].copy_from_slice(&command.to_le_bytes());
        descriptor[12..16].copy_from_slice(&((length as u32) << 14).to_le_bytes());
    } else {
        descriptor[8..10].copy_from_slice(&(length as u16).to_le_bytes());
        descriptor[11] = 1 | 2 | 8; // end of packet, insert checksum, report status
    }
}

/// Whether the card has sent what a send descriptor holds.
pub fn tx_done(descriptor: &[u8]) -> bool {
    descriptor[12] & 1 != 0
}

pub fn probe(device: &pci::Device) -> Option<IntelNet> {
    let family = family(device.vendor, device.device)?;
    let regs = Registers::of(device)?;
    device.enable();
    let card = IntelNet::start(family, regs);
    crate::log!(
        "OTW kernel: Intel network card {:04x} ({:?}), link {}",
        device.device,
        family,
        if regs.r32(STATUS) & 2 != 0 { "up" } else { "down (yet)" }
    );
    Some(card)
}

impl IntelNet {
    fn start(family: Family, regs: Registers) -> Self {
        let mask_all = |regs: &Registers| {
            regs.w32(IMC, u32::MAX);
            if family.advanced() {
                regs.w32(EIMC, u32::MAX);
            }
            let _ = regs.r32(ICR);
        };
        mask_all(&regs);
        regs.w32(RCTL, 0);
        regs.w32(TCTL, 0);
        // On a chipset's card the PHY is shared with the management engine:
        // ask for it first.
        let shared = family == Family::Pch;
        if shared {
            regs.w32(EXTCNF_CTRL, regs.r32(EXTCNF_CTRL) | 1 << 5);
            wait_for(100_000, || regs.r32(EXTCNF_CTRL) & 1 << 5 != 0);
        }
        regs.w32(CTRL, regs.r32(CTRL) | 1 << 26);
        crate::nic::delay_us(2000);
        wait_for(200_000, || regs.r32(CTRL) & 1 << 26 == 0);
        crate::nic::delay_us(1000);
        if shared {
            regs.w32(EXTCNF_CTRL, regs.r32(EXTCNF_CTRL) & !(1 << 5));
        }
        mask_all(&regs);
        // Link up, speed found by itself; not in reset, no VLANs.
        let mut ctrl = regs.r32(CTRL) | 1 << 6;
        if !family.advanced() {
            ctrl |= 1 << 5;
        }
        ctrl &= !(1 << 3 | 1 << 31 | 1 << 7 | 1 << 30);
        regs.w32(CTRL, ctrl);
        if family != Family::E1000 {
            regs.w32(CTRL_EXT, regs.r32(CTRL_EXT) | 1 << 28); // a driver has it
        }
        Self::start_phy(family, &regs);
        let mac = Self::read_mac(family, &regs);
        regs.w32(RAL, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
        regs.w32(RAH, u16::from_le_bytes([mac[4], mac[5]]) as u32 | 1 << 31);
        for i in 0..128 {
            regs.w32(MTA + 4 * i, 0);
        }

        let card = Self {
            family,
            regs,
            mac,
            receive: Buffers::new(RING, BUFFER),
            receive_ring: Buffers::new(RING, 16),
            send: Buffers::new(RING, BUFFER),
            send_ring: Buffers::new(RING, 16),
            rx_next: 0,
            tx_next: 0,
            tx_clean: 0,
            rx_tail: family.queues().0 + 0x18,
            tx_tail: family.queues().1 + 0x18,
            received: 0,
            sent: 0,
        };
        let advanced = family.advanced();
        for i in 0..RING {
            let address = card.receive.address(i);
            if advanced {
                advanced_rx(card.receive_ring.get(i), address);
            } else {
                legacy_rx(card.receive_ring.get(i), address);
            }
        }
        let (rx, tx) = family.queues();
        let ring = card.receive_ring.address(0);
        regs.w32(rx, ring as u32);
        regs.w32(rx + 4, (ring >> 32) as u32);
        regs.w32(rx + 8, (RING * 16) as u32);
        regs.w32(rx + 0x10, 0);
        regs.w32(rx + 0x18, 0);
        if advanced {
            // 2 KiB buffers, advanced one-buffer descriptors, drop when full.
            regs.w32(rx + 0x0C, 2 | 1 << 25 | 1 << 31);
            regs.w32(rx + 0x28, regs.r32(rx + 0x28) | 1 << 25);
            wait_for(100_000, || regs.r32(rx + 0x28) & 1 << 25 != 0);
        }
        // Enabled; broadcasts and multicasts too; the checksum stripped;
        // 2 KiB buffers.
        regs.w32(RCTL, 1 << 1 | 1 << 4 | 1 << 15 | 1 << 26);
        regs.w32(card.rx_tail, (RING - 1) as u32);

        let ring = card.send_ring.address(0);
        regs.w32(tx, ring as u32);
        regs.w32(tx + 4, (ring >> 32) as u32);
        regs.w32(tx + 8, (RING * 16) as u32);
        regs.w32(tx + 0x10, 0);
        regs.w32(tx + 0x18, 0);
        if advanced {
            regs.w32(tx + 0x28, regs.r32(tx + 0x28) | 1 << 25);
            wait_for(100_000, || regs.r32(tx + 0x28) & 1 << 25 != 0);
        } else {
            regs.w32(TIPG, 10 | 8 << 10 | 6 << 20);
        }
        // Enabled, short frames padded, collision threshold and distance.
        regs.w32(TCTL, 1 << 1 | 1 << 3 | 0x0F << 4 | 0x3F << 12 | 1 << 24);
        card
    }

    /// Wake the PHY (firmware may leave it powered down) and have it
    /// negotiate the link.
    fn start_phy(family: Family, regs: &Registers) {
        let mdic = |command: u32| {
            regs.w32(MDIC, command | 1 << 21); // PHY 1
            wait_for(10_000, || regs.r32(MDIC) & (1 << 28 | 1 << 30) != 0) && regs.r32(MDIC) & 1 << 30 == 0
        };
        let shared = family == Family::Pch;
        if shared {
            regs.w32(EXTCNF_CTRL, regs.r32(EXTCNF_CTRL) | 1 << 5);
            wait_for(100_000, || regs.r32(EXTCNF_CTRL) & 1 << 5 != 0);
        }
        if mdic(2 << 26) {
            let control = regs.r32(MDIC) as u16;
            // Not powered down, isolated or in loopback; negotiate (again).
            let control = (control & !(1 << 11 | 1 << 10 | 1 << 14)) | 1 << 12 | 1 << 9;
            mdic(1 << 26 | control as u32);
        }
        if shared {
            regs.w32(EXTCNF_CTRL, regs.r32(EXTCNF_CTRL) & !(1 << 5));
        }
    }

    fn read_mac(family: Family, regs: &Registers) -> [u8; 6] {
        let low = regs.r32(RAL);
        let high = regs.r32(RAH);
        if low != 0 && low != u32::MAX {
            let l = low.to_le_bytes();
            let h = (high as u16).to_le_bytes();
            return [l[0], l[1], l[2], l[3], h[0], h[1]];
        }
        // From the EEPROM (8254x): three words. Two layouts of the read
        // register: the address at bit 8 (done at 4) or at 2 (done at 1).
        let mut mac = [0u8; 6];
        if family == Family::E1000 {
            for word in 0..3u32 {
                regs.w32(EERD, 1 | word << 8);
                let value = if wait_for(10_000, || regs.r32(EERD) & 1 << 4 != 0) {
                    regs.r32(EERD) >> 16
                } else {
                    regs.w32(EERD, 1 | word << 2);
                    wait_for(10_000, || regs.r32(EERD) & 1 << 1 != 0);
                    regs.r32(EERD) >> 16
                };
                mac[word as usize * 2] = value as u8;
                mac[word as usize * 2 + 1] = (value >> 8) as u8;
            }
        }
        if mac == [0; 6] {
            // Made up (locally administered).
            let t = crate::apic::now_us();
            mac = [0x02, 0x4F, 0x54, 0x57, t as u8, (t >> 8) as u8];
        }
        mac
    }

    fn reclaim(&mut self) {
        while self.tx_clean != self.tx_next && tx_done(self.send_ring.get(self.tx_clean)) {
            self.tx_clean = (self.tx_clean + 1) % RING;
        }
    }
}

impl Nic for IntelNet {
    fn name(&self) -> &'static str {
        "Intel"
    }

    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        loop {
            let i = self.rx_next;
            let length = rx_done(self.receive_ring.get(i), self.family.advanced())?;
            let frame = (length > 0).then(|| self.receive.get(i)[..length.min(BUFFER)].to_vec());
            let address = self.receive.address(i);
            if self.family.advanced() {
                advanced_rx(self.receive_ring.get(i), address);
            } else {
                legacy_rx(self.receive_ring.get(i), address);
            }
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            // Hand it back: the tail is the last descriptor the card may use.
            self.regs.w32(self.rx_tail, i as u32);
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
        let length = frame.len().min(BUFFER);
        self.send.get(i)[..length].copy_from_slice(&frame[..length]);
        let address = self.send.address(i);
        tx_descriptor(self.send_ring.get(i), address, length, self.family.advanced());
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.tx_next = (i + 1) % RING;
        self.regs.w32(self.tx_tail, self.tx_next as u32);
        self.sent += 1;
    }

    fn link_up(&mut self) -> bool {
        self.regs.r32(STATUS) & 2 != 0
    }

    fn counters(&self) -> (u64, u64) {
        (self.received, self.sent)
    }
}
