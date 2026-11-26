//! Where things are in a 32-bit process's address space (shared by the
//! CPU backend and the subsystem).

pub const STACK_BASE: u32 = 0x0010_0000;

pub const STACK_SIZE: u32 = 0x0010_0000;

pub const STACK_TOP: u32 = STACK_BASE + STACK_SIZE - 0x100;

/// Synthetic Win95 pages sit above genuine high-based DLLs. OLE32 loads at
/// `0x7ff60000` with a `0x86000` image, ending at `0x7ffe6000`, so the TIB,
/// loader state and GDT must stay above that range.
///
/// Minimal Win95-style thread information block. MSVC CRT startup code reads
/// and writes `FS:[0]` to install its structured exception handler chain.
pub const TIB_BASE: u32 = 0x7FFF_0000;

/// A single `ret` instruction used as the return address for synthetic
/// DllMain calls. Unicorn stops before executing it.
pub const SENTINEL_BASE: u32 = 0x6FFF_0000;
