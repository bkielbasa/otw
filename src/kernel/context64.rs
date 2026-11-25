//! The x64 `CONTEXT` (0x4D0 bytes) and `EXCEPTION_RECORD` as Windows lays
//! them out, to and from [`FullRegisters`].

use super::memory::{FullRegisters, GuestMemory};

pub const CONTEXT_SIZE: u32 = 0x4D0;
pub const RECORD_SIZE: u32 = 0x98;
/// CONTEXT_AMD64 | CONTROL | INTEGER | SEGMENTS | FLOATING_POINT.
pub const CONTEXT_FULL: u32 = 0x10_000F;
pub const RSP: u32 = 0x98;
pub const RIP: u32 = 0xF8;

/// The context's bytes for a set of registers.
pub fn encode(full: &FullRegisters) -> Vec<u8> {
    let mut bytes = vec![0u8; CONTEXT_SIZE as usize];
    let put = |bytes: &mut Vec<u8>, at: usize, value: &[u8]| bytes[at..at + value.len()].copy_from_slice(value);
    put(&mut bytes, 0x30, &CONTEXT_FULL.to_le_bytes());
    put(&mut bytes, 0x34, &full.mxcsr.to_le_bytes());
    for (at, selector) in [(0x38, 0x33u16), (0x3A, 0x2B), (0x3C, 0x2B), (0x3E, 0x53), (0x40, 0x2B), (0x42, 0x2B)] {
        put(&mut bytes, at, &selector.to_le_bytes());
    }
    put(&mut bytes, 0x44, &(full.eflags as u32).to_le_bytes());
    for (index, value) in full.gpr.iter().enumerate() {
        put(&mut bytes, 0x78 + index * 8, &value.to_le_bytes());
    }
    put(&mut bytes, RIP as usize, &full.rip.to_le_bytes());
    // FltSave (XMM_SAVE_AREA32): control word, MXCSR, then the XMM registers.
    put(&mut bytes, 0x100, &0x27Fu16.to_le_bytes());
    put(&mut bytes, 0x118, &full.mxcsr.to_le_bytes());
    put(&mut bytes, 0x11C, &0xFFFFu32.to_le_bytes());
    for (index, xmm) in full.xmm.iter().enumerate() {
        put(&mut bytes, 0x1A0 + index * 16, xmm);
    }
    bytes
}

pub fn decode(bytes: &[u8]) -> FullRegisters {
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]));
    let mut full = FullRegisters::default();
    for (index, value) in full.gpr.iter_mut().enumerate() {
        *value = u64_at(0x78 + index * 8);
    }
    full.rip = u64_at(RIP as usize);
    full.eflags = u32::from_le_bytes(bytes[0x44..0x48].try_into().unwrap_or([0; 4])) as u64;
    full.mxcsr = u32::from_le_bytes(bytes[0x34..0x38].try_into().unwrap_or([0; 4]));
    if full.mxcsr == 0 {
        full.mxcsr = 0x1F80;
    }
    for (index, xmm) in full.xmm.iter_mut().enumerate() {
        xmm.copy_from_slice(&bytes[0x1A0 + index * 16..0x1B0 + index * 16]);
    }
    full
}

pub fn read<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> FullRegisters {
    decode(&memory.read_bytes(va, CONTEXT_SIZE as usize).unwrap_or_else(|| vec![0; CONTEXT_SIZE as usize]))
}

pub fn write<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, full: &FullRegisters) {
    let _ = memory.write_bytes(va, &encode(full));
}

/// An EXCEPTION_RECORD: code, flags, address and parameters.
pub fn record(code: u32, flags: u32, address: u64, params: &[u64]) -> Vec<u8> {
    let mut bytes = vec![0u8; RECORD_SIZE as usize];
    bytes[0..4].copy_from_slice(&code.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes[0x10..0x18].copy_from_slice(&address.to_le_bytes());
    let count = params.len().min(15);
    bytes[0x18..0x1C].copy_from_slice(&(count as u32).to_le_bytes());
    for (index, value) in params.iter().take(count).enumerate() {
        bytes[0x20 + index * 8..0x28 + index * 8].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}
