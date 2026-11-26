//! Page tables. All physical memory is mapped at `DIRECT_BASE` (the
//! "direct map", in the upper half of the address space); the kernel runs
//! there, so that the lower half belongs to programs. Each process has a
//! page table root of its own: its lower half, the kernel's upper half.
//!
//! At boot the kernel still runs where the firmware loaded it (identity
//! mapped); `take_over` builds the tables, and `main.rs` moves the kernel
//! into the direct map (`relocate`).

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

pub const DIRECT_BASE: u64 = 0xFFFF_8000_0000_0000;

const PRESENT: u64 = 1;
const WRITABLE: u64 = 2;
pub const USER: u64 = 4;
const WRITE_THROUGH: u64 = 1 << 3;
const NO_CACHE: u64 = 1 << 4;
const HUGE: u64 = 1 << 7;
const ADDRESS: u64 = 0x000F_FFFF_FFFF_F000;
const TWO_MIB: u64 = 2 << 20;

/// Where physical memory is seen: 0 (identity) until the kernel moves into
/// the direct map, `DIRECT_BASE` after.
static OFFSET: AtomicU64 = AtomicU64::new(0);
/// The kernel's root table (physical).
static ROOT: AtomicU64 = AtomicU64::new(0);
/// Frames for page tables before there is a heap: [next, end) physical.
static BOOT_NEXT: AtomicU64 = AtomicU64::new(0);
static BOOT_END: AtomicU64 = AtomicU64::new(0);

pub fn to_virtual(physical: u64) -> u64 {
    physical + OFFSET.load(Ordering::Relaxed)
}

/// The physical address of kernel memory (the heap, statics, stacks).
pub fn to_physical(virtual_address: u64) -> u64 {
    if virtual_address >= DIRECT_BASE {
        virtual_address - DIRECT_BASE
    } else {
        virtual_address
    }
}

/// The heap makes page tables from now on.
pub fn end_boot_tables() {
    BOOT_END.store(0, Ordering::SeqCst);
}

pub fn set_direct() {
    OFFSET.store(DIRECT_BASE, Ordering::SeqCst);
}

/// A zeroed 4 KiB frame for a page table; its physical address.
fn new_table() -> u64 {
    let next = BOOT_NEXT.load(Ordering::SeqCst);
    if next != 0 && next + 4096 <= BOOT_END.load(Ordering::SeqCst) {
        BOOT_NEXT.store(next + 4096, Ordering::SeqCst);
        unsafe { core::ptr::write_bytes(to_virtual(next) as *mut u8, 0, 4096) };
        return next;
    }
    let layout = core::alloc::Layout::from_size_align(4096, 4096).unwrap();
    let table = unsafe { alloc::alloc::alloc_zeroed(layout) };
    assert!(!table.is_null(), "out of memory for page tables");
    to_physical(table as u64)
}

fn table(physical: u64) -> *mut u64 {
    to_virtual(physical) as *mut u64
}

/// Build the kernel's tables in [frames, frames + size) (physical, free):
/// physical memory [0, end) both where it is and in the direct map. Loads
/// them.
pub fn take_over(end: u64, frames: u64, size: u64) {
    BOOT_NEXT.store(frames, Ordering::SeqCst);
    BOOT_END.store(frames + size, Ordering::SeqCst);
    let root = new_table();
    // Other CPUs load it while still in 32-bit code when they start.
    assert!(root < 1 << 32, "page tables above 4 GiB");
    ROOT.store(root, Ordering::SeqCst);
    let end = end.div_ceil(TWO_MIB) * TWO_MIB;
    let mut address = 0;
    while address < end {
        map_2mib(root, address, address, 0);
        map_2mib(root, DIRECT_BASE + address, address, 0);
        address += TWO_MIB;
    }
    unsafe { asm!("mov cr3, {}", in(reg) root) };
}

/// The kernel's root table (physical).
pub fn page_table_root() -> u64 {
    ROOT.load(Ordering::SeqCst)
}

fn walk(root: u64, address: u64, levels: &[u64], user: bool) -> *mut u64 {
    let mut current = root;
    for &shift in levels {
        let entry = unsafe { table(current).add((address >> shift & 511) as usize) };
        unsafe {
            if *entry & PRESENT == 0 {
                *entry = new_table() | PRESENT | WRITABLE | if user { USER } else { 0 };
            } else if user {
                *entry |= USER;
            }
            current = *entry & ADDRESS;
        }
    }
    table(current)
}

fn map_2mib(root: u64, virtual_address: u64, physical: u64, flags: u64) {
    let pd = walk(root, virtual_address, &[39, 30], false);
    unsafe { *pd.add((virtual_address >> 21 & 511) as usize) = physical | PRESENT | WRITABLE | HUGE | flags };
}

/// The page attribute table: entry 1 (page tables' write-through bit
/// alone) becomes write-combining, for the framebuffer; the rest keep
/// their power-on meaning. Every CPU loads the same table.
pub fn load_page_attributes() {
    // WB, WC, UC-, UC, WB, WT, UC-, UC
    crate::arch::write_msr(0x277, 0x0007_0406_0007_0106);
}

fn remap(physical: u64, size: u64, flags: u64) -> u64 {
    let root = ROOT.load(Ordering::SeqCst);
    let start = physical / TWO_MIB * TWO_MIB;
    let end = (physical + size).div_ceil(TWO_MIB) * TWO_MIB;
    let mut page = start;
    while page < end {
        let virtual_address = to_virtual(page);
        map_2mib(root, virtual_address, page, flags);
        unsafe { asm!("invlpg [{}]", in(reg) virtual_address) };
        page += TWO_MIB;
    }
    to_virtual(physical)
}

/// Map the framebuffer write-combining: writes to it are batched rather
/// than sent one at a time, which is what makes drawing fast on real
/// hardware. Returns where it is mapped.
pub fn map_framebuffer(physical: u64, size: u64) -> u64 {
    let address = remap(physical, size, WRITE_THROUGH);
    unsafe { asm!("wbinvd") };
    address
}

/// Make device registers at physical [address, address + size) reachable,
/// uncached; returns where.
pub fn map_device(physical: u64, size: u64) -> u64 {
    remap(physical, size, NO_CACHE | WRITE_THROUGH)
}

// ── Processes' address spaces ──

/// A process's page tables: its lower half (4 KiB pages), the kernel's
/// upper half.
pub struct AddressSpace {
    pub root: u64,
}

impl AddressSpace {
    pub fn new() -> Self {
        let root = new_table();
        let kernel = table(ROOT.load(Ordering::SeqCst));
        unsafe {
            for i in 256..512 {
                *table(root).add(i) = *kernel.add(i);
            }
        }
        Self { root }
    }

    /// The entry for the 4 KiB page at `address`, made if `make`.
    fn entry(&self, address: u64, make: bool) -> Option<*mut u64> {
        let mut current = self.root;
        for shift in [39u64, 30, 21] {
            let entry = unsafe { table(current).add((address >> shift & 511) as usize) };
            unsafe {
                if *entry & PRESENT == 0 {
                    if !make {
                        return None;
                    }
                    *entry = new_table() | PRESENT | WRITABLE | USER;
                }
                current = *entry & ADDRESS;
            }
        }
        Some(unsafe { table(current).add((address >> 12 & 511) as usize) })
    }

    /// Map fresh zeroed pages over [address, address + size) (pages
    /// already mapped are kept).
    pub fn map(&self, address: u64, size: u64) -> bool {
        let start = address & !0xFFF;
        let end = (address + size + 0xFFF) & !0xFFF;
        let layout = core::alloc::Layout::from_size_align(4096, 4096).unwrap();
        let mut page = start;
        while page < end {
            let Some(entry) = self.entry(page, true) else { return false };
            unsafe {
                if *entry & PRESENT == 0 {
                    let frame = alloc::alloc::alloc_zeroed(layout);
                    if frame.is_null() {
                        return false;
                    }
                    *entry = to_physical(frame as u64) | PRESENT | WRITABLE | USER;
                }
            }
            page += 4096;
        }
        true
    }

    /// Unmap [address, address + size), freeing the frames.
    pub fn unmap(&self, address: u64, size: u64) {
        let layout = core::alloc::Layout::from_size_align(4096, 4096).unwrap();
        let mut page = address & !0xFFF;
        let end = (address + size + 0xFFF) & !0xFFF;
        while page < end {
            if let Some(entry) = self.entry(page, false) {
                unsafe {
                    if *entry & PRESENT != 0 {
                        alloc::alloc::dealloc(to_virtual(*entry & ADDRESS) as *mut u8, layout);
                        *entry = 0;
                        asm!("invlpg [{}]", in(reg) page);
                    }
                }
            }
            page += 4096;
        }
    }

    /// Where the kernel reaches the byte at `address`, if mapped.
    pub fn translate(&self, address: u64) -> Option<*mut u8> {
        let entry = self.entry(address, false)?;
        let value = unsafe { *entry };
        (value & PRESENT != 0).then(|| (to_virtual(value & ADDRESS) + (address & 0xFFF)) as *mut u8)
    }

    pub fn is_mapped(&self, address: u64) -> bool {
        self.translate(address).is_some()
    }

    /// Copy out of the process's memory.
    pub fn read(&self, address: u64, buffer: &mut [u8]) -> bool {
        let mut done = 0;
        while done < buffer.len() {
            let at = address + done as u64;
            let Some(pointer) = self.translate(at) else { return false };
            let n = (buffer.len() - done).min(4096 - (at & 0xFFF) as usize);
            unsafe { core::ptr::copy_nonoverlapping(pointer, buffer.as_mut_ptr().add(done), n) };
            done += n;
        }
        true
    }

    /// Copy into the process's memory.
    pub fn write(&self, address: u64, bytes: &[u8]) -> bool {
        let mut done = 0;
        while done < bytes.len() {
            let at = address + done as u64;
            let Some(pointer) = self.translate(at) else { return false };
            let n = (bytes.len() - done).min(4096 - (at & 0xFFF) as usize);
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr().add(done), pointer, n) };
            done += n;
        }
        true
    }

    pub fn activate(&self) {
        unsafe { asm!("mov cr3, {}", in(reg) self.root) };
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // Free the program's pages and tables (the lower half).
        let layout = core::alloc::Layout::from_size_align(4096, 4096).unwrap();
        unsafe fn free_level(physical: u64, level: u32, layout: core::alloc::Layout) {
            let t = table(physical);
            for i in 0..512 {
                let entry = *t.add(i);
                if entry & PRESENT == 0 {
                    continue;
                }
                if level > 1 {
                    free_level(entry & ADDRESS, level - 1, layout);
                } else {
                    alloc::alloc::dealloc(to_virtual(entry & ADDRESS) as *mut u8, layout);
                }
            }
            alloc::alloc::dealloc(t as *mut u8, layout);
        }
        unsafe {
            let root = table(self.root);
            for i in 0..256 {
                let entry = *root.add(i);
                if entry & PRESENT != 0 {
                    free_level(entry & ADDRESS, 3, layout);
                }
            }
            // The kernel's tables become current again before this.
            alloc::alloc::dealloc(root as *mut u8, layout);
        }
    }
}
