//! Virtual memory as Windows NT manages it, for 64-bit processes:
//! reservations at 64 KiB granularity anywhere in the address space, at
//! the address the program asks for when it names one, with pages
//! committed and decommitted inside them, and `VirtualQuery` describing
//! free, reserved and committed ranges. (32-bit programs keep the simpler
//! heap-backed scheme Windows 95 programs are content with.)
//!
//! A reservation is mapped in the CPU whole; commitment is bookkeeping (a
//! reserved page is readable, where Windows would fault).

use std::collections::BTreeMap;

use crate::win32::{ApiContext, GuestMemory};

const PAGE: u32 = 0x1000;
const GRANULARITY: u32 = 0x1_0000;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_DECOMMIT: u32 = 0x4000;
const MEM_RELEASE: u32 = 0x8000;
const MEM_FREE: u32 = 0x1_0000;
const MEM_PRIVATE: u32 = 0x2_0000;
const MEM_IMAGE: u32 = 0x100_0000;

#[derive(Clone, Debug)]
struct Region {
    size: u32,
    protect: u32,
    /// Per page: 0 reserved, otherwise the page's protection.
    pages: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct State {
    regions: BTreeMap<u32, Region>,
}

fn find(state: &State, address: u32) -> Option<(u32, &Region)> {
    let (&base, region) = state.regions.range(..=address).next_back()?;
    (address < base + region.size).then_some((base, region))
}

/// VirtualAlloc(address, size, type, protect): the region's address, or 0
/// with the error.
pub fn alloc<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, address: u32, size: u32, kind: u32, protect: u32) -> Result<u32, u32> {
    if size == 0 {
        return Err(87);
    }
    let mut shared = ctx.shared.borrow_mut();
    // Committing inside a reservation.
    if address != 0 {
        if let Some((base, region)) = find(&shared.vmem, address) {
            if kind & MEM_COMMIT == 0 {
                return Err(487); // ERROR_INVALID_ADDRESS
            }
            let start = address & !(PAGE - 1);
            let end = (address as u64 + size as u64 + PAGE as u64 - 1) as u32 & !(PAGE - 1);
            if end > base + region.size {
                return Err(487);
            }
            let first = ((start - base) / PAGE) as usize;
            let last = ((end - base) / PAGE) as usize;
            let region = shared.vmem.regions.get_mut(&base).expect("found region");
            for page in &mut region.pages[first..last] {
                if *page == 0 {
                    *page = protect.max(1);
                }
            }
            return Ok(start);
        }
    }
    if kind & (MEM_RESERVE | MEM_COMMIT) == 0 {
        return Err(87);
    }
    drop(shared);
    let length = (size + PAGE - 1) & !(PAGE - 1);
    let base = if address != 0 {
        let base = address & !(GRANULARITY - 1);
        let length = (address - base + size + PAGE - 1) & !(PAGE - 1);
        if !memory.map_exact(base, length) {
            return Err(487);
        }
        (base, length)
    } else {
        // Large reservations go high, keeping the low space for the rest.
        let preferred = if length >= 0x1000_0000 { 0x8000_0000 } else { 0x2000_0000 };
        match memory.map_region(preferred, length) {
            Some(base) if base % GRANULARITY == 0 => (base, length),
            Some(base) => {
                // Keep reservations on 64 KiB boundaries.
                memory.unmap(base, length);
                let aligned = (base + GRANULARITY - 1) & !(GRANULARITY - 1);
                if !memory.map_exact(aligned, length) {
                    return Err(8);
                }
                (aligned, length)
            }
            None => return Err(8), // ERROR_NOT_ENOUGH_MEMORY
        }
    };
    let (base, length) = base;
    let committed = if kind & MEM_COMMIT != 0 { protect.max(1) } else { 0 };
    let region = Region { size: length, protect, pages: vec![committed; (length / PAGE) as usize] };
    ctx.shared.borrow_mut().vmem.regions.insert(base, region);
    Ok(if address != 0 && kind & MEM_COMMIT != 0 { address & !(PAGE - 1) } else { base })
}

/// VirtualFree(address, size, type).
pub fn free<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, address: u32, size: u32, kind: u32) -> Result<(), u32> {
    let mut shared = ctx.shared.borrow_mut();
    let Some((base, region)) = find(&shared.vmem, address) else {
        return Err(487);
    };
    if kind & MEM_RELEASE != 0 {
        if base != address || size != 0 {
            return Err(87);
        }
        let length = region.size;
        shared.vmem.regions.remove(&base);
        drop(shared);
        memory.unmap(base, length);
        return Ok(());
    }
    if kind & MEM_DECOMMIT != 0 {
        let end = if size == 0 { base + region.size } else { (address as u64 + size as u64 + PAGE as u64 - 1) as u32 & !(PAGE - 1) };
        let start = address & !(PAGE - 1);
        let first = ((start - base) / PAGE) as usize;
        let last = (((end.min(base + region.size)) - base) / PAGE) as usize;
        let region = shared.vmem.regions.get_mut(&base).expect("found region");
        for page in &mut region.pages[first..last] {
            *page = 0;
        }
        drop(shared);
        let _ = memory.write_bytes(start, &vec![0u8; ((last - first) as u32 * PAGE) as usize]);
        return Ok(());
    }
    Err(87)
}

/// VirtualProtect: the old protection of the first page.
pub fn protect(ctx: &ApiContext, address: u32, size: u32, protect: u32) -> Option<u32> {
    let mut shared = ctx.shared.borrow_mut();
    let (base, region) = find(&shared.vmem, address)?;
    let start = ((address & !(PAGE - 1)) - base) / PAGE;
    let end = (((address as u64 + size.max(1) as u64 + PAGE as u64 - 1) as u32 & !(PAGE - 1)) - base).min(region.size) / PAGE;
    let old = region.pages.get(start as usize).copied().filter(|&p| p != 0).unwrap_or(region.protect);
    let region = shared.vmem.regions.get_mut(&base)?;
    for page in &mut region.pages[start as usize..end as usize] {
        *page = protect;
    }
    Some(old)
}

/// MEMORY_BASIC_INFORMATION values for an address: base, allocation base,
/// allocation protection, size, state, protection, type.
pub fn query<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, address: u32) -> [u32; 7] {
    let page = address & !(PAGE - 1);
    {
        let shared = ctx.shared.borrow();
        if let Some((base, region)) = find(&shared.vmem, address) {
            let index = ((page - base) / PAGE) as usize;
            let state = region.pages[index];
            let run = region.pages[index..].iter().take_while(|&&p| p == state).count() as u32;
            return [
                page,
                base,
                region.protect,
                run * PAGE,
                if state == 0 { MEM_RESERVE } else { MEM_COMMIT },
                state,
                MEM_PRIVATE,
            ];
        }
    }
    // Other mappings (images, stacks, the heap) are committed; the rest
    // is free up to the next mapping.
    let mapped = memory.mapped();
    if let Some(&(begin, end)) = mapped.iter().find(|&&(b, e)| page as u64 >= b && page as u64 <= e) {
        let image = ctx.modules.borrow().iter().any(|m| page >= m.image_base && page < m.image_base + m.size);
        return [page, begin as u32, 0x40, (end + 1 - page as u64) as u32, MEM_COMMIT, 0x40, if image { MEM_IMAGE } else { MEM_PRIVATE }];
    }
    let next = mapped.iter().map(|&(b, _)| b).filter(|&b| b > page as u64).min().unwrap_or(0x8000_0000).max(page as u64 + PAGE as u64);
    [page, 0, 0, (next - page as u64).min(u32::MAX as u64) as u32 & !(PAGE - 1), MEM_FREE, 1, 0]
}

/// Whether an address is inside one of these reservations.
pub fn owns(ctx: &ApiContext, address: u32) -> bool {
    find(&ctx.shared.borrow().vmem, address).is_some()
}

const _: u32 = MEM_DECOMMIT;
