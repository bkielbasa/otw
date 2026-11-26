//! Virtual memory as Windows NT manages it, for 64-bit processes:
//! reservations at 64 KiB granularity anywhere in the address space, at
//! the address the program asks for when it names one, with pages
//! committed and decommitted inside them, and `VirtualQuery` describing
//! free, reserved and committed ranges. (32-bit programs keep the simpler
//! heap-backed scheme Windows 95 programs are content with.)
//!
//! A reservation is mapped in the CPU whole; commitment is bookkeeping (a
//! reserved page is readable, where Windows would fault).

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

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
    high: BTreeMap<u64, HighRegion>,
    /// The next free 4 GiB slot above the low space, for high reservations.
    next_slot: u64,
    /// Pages of loaded images whose protection was changed.
    image_pages: BTreeMap<u32, u32>,
}

/// A reservation too large for the 4 GiB OTW keeps a process in (the
/// WebAssembly sandboxes Firefox reserves 16 GiB each for, touching a few
/// megabytes): it lies above 4 GiB and is mapped piecewise as it is
/// committed. Its low 32 bits match a window reserved in the low space,
/// where the first `ALIAS_SIZE` bytes are mirrored: OTW reads guest memory
/// through 32-bit addresses, so a pointer into the reservation handed to
/// OTW (to `memcpy`, say) is cut to its low half and lands in the mirror.
#[derive(Debug)]
struct HighRegion {
    size: u64,
    alias: Option<u32>,
    /// Committed pieces: start to length.
    chunks: BTreeMap<u64, u64>,
}

const ALIAS_SIZE: u32 = 0x400_0000;
/// Reservations at least this large go above 4 GiB.
pub const HIGH_THRESHOLD: u64 = 0x1_0000_0000;

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
            // MEM_RESET / MEM_RESET_UNDO: the contents may be discarded.
            if kind & 0x0108_0000 != 0 {
                return Ok(address);
            }
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
        let preferred = if length >= 0x1000_0000 { 0x8000_0000 } else { crate::kernel::memory::POOL_BASE };
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
pub fn protect<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, address: u32, size: u32, protect: u32) -> Option<u32> {
    let first = address & !(PAGE - 1);
    let end = ((address as u64 + size.max(1) as u64 + PAGE as u64 - 1) & !(PAGE as u64 - 1)).min(u32::MAX as u64) as u32;
    if find(&ctx.shared.borrow().vmem, address).is_none() {
        // A loaded image: its pages start with their sections' protection.
        let old = image_protection(ctx, memory, first)?;
        let mut shared = ctx.shared.borrow_mut();
        for page in (first..end).step_by(PAGE as usize) {
            shared.vmem.image_pages.insert(page, protect);
        }
        return Some(old);
    }
    let mut shared = ctx.shared.borrow_mut();
    let (base, region) = find(&shared.vmem, address)?;
    let start = (first - base) / PAGE;
    let end = (end - base).min(region.size) / PAGE;
    let old = region.pages.get(start as usize).copied().filter(|&p| p != 0).unwrap_or(region.protect);
    let region = shared.vmem.regions.get_mut(&base)?;
    for page in &mut region.pages[start as usize..end as usize] {
        *page = protect;
    }
    Some(old)
}

/// The protection of a page of a loaded image: as changed by
/// `VirtualProtect`, otherwise its section's (read-only for the headers).
fn image_protection<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, page: u32) -> Option<u32> {
    if let Some(&protect) = ctx.shared.borrow().vmem.image_pages.get(&page) {
        return Some(protect);
    }
    let base = ctx.modules.borrow().iter().find(|m| page >= m.image_base && page < m.image_base + m.size)?.image_base;
    let pe = base + memory.read_u32(base + 0x3C)?;
    let sections = memory.read_u32(pe + 4)? >> 16;
    let first = pe + 24 + (memory.read_u32(pe + 20)? & 0xFFFF);
    for index in 0..sections.min(96) {
        let header = first + index * 40;
        let size = memory.read_u32(header + 8)?;
        let start = base + memory.read_u32(header + 12)?;
        if page >= start && page < start + ((size + PAGE - 1) & !(PAGE - 1)) {
            let flags = memory.read_u32(header + 36)?;
            let (execute, write) = (flags & 0x2000_0000 != 0, flags & 0x8000_0000 != 0);
            return Some(match (execute, write) {
                (true, true) => 0x40,
                (true, false) => 0x20,
                (false, true) => 0x04,
                (false, false) => 0x02,
            });
        }
    }
    Some(0x02)
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
        let image = ctx.modules.borrow().iter().find(|m| page >= m.image_base && page < m.image_base + m.size).map(|m| m.image_base);
        if let Some(base) = image {
            // One page at a time: sections differ in protection.
            let protect = image_protection(ctx, memory, page).unwrap_or(0x40);
            return [page, base, 0x80, PAGE, MEM_COMMIT, protect, MEM_IMAGE];
        }
        return [page, begin as u32, 0x40, (end + 1 - page as u64) as u32, MEM_COMMIT, 0x40, MEM_PRIVATE];
    }
    let next = mapped.iter().map(|&(b, _)| b).filter(|&b| b > page as u64).min().unwrap_or(0x8000_0000).max(page as u64 + PAGE as u64);
    [page, 0, 0, (next - page as u64).min(u32::MAX as u64) as u32 & !(PAGE - 1), MEM_FREE, 1, 0]
}

/// Whether an address is inside one of these reservations.
pub fn owns(ctx: &ApiContext, address: u32) -> bool {
    find(&ctx.shared.borrow().vmem, address).is_some()
}

/// Whether `address` is in a reservation above 4 GiB.
pub fn owns_high(ctx: &ApiContext, address: u64) -> bool {
    find_high(&ctx.shared.borrow().vmem, address).is_some()
}

fn find_high(state: &State, address: u64) -> Option<u64> {
    let (&base, region) = state.high.range(..=address).next_back()?;
    (address < base + region.size).then_some(base)
}

/// Reserve (and maybe commit) `size` bytes above 4 GiB, at `address` when
/// one is asked for.
pub fn reserve_high<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, address: u64, size: u64, kind: u32) -> Result<u64, u32> {
    let size = (size + PAGE as u64 - 1) & !(PAGE as u64 - 1);
    let (base, alias) = if address != 0 {
        let base = address & !(GRANULARITY as u64 - 1);
        let taken = {
            let shared = ctx.shared.borrow();
            shared.vmem.high.range(..base + size).next_back().is_some_and(|(&b, r)| b + r.size > base)
        };
        if base < HIGH_THRESHOLD || taken {
            return Err(487);
        }
        let low = base as u32;
        (base, (low != 0 && memory.map_exact(low, ALIAS_SIZE)).then_some(low))
    } else {
        let alias = memory.map_region(crate::kernel::memory::SEPARATE_BASE, ALIAS_SIZE);
        let mut shared = ctx.shared.borrow_mut();
        let slot = shared.vmem.next_slot.max(1);
        shared.vmem.next_slot = slot + (size >> 32) + 2;
        ((slot << 32) | alias.unwrap_or(0) as u64, alias)
    };
    if base + size > 1 << 47 {
        return Err(8);
    }
    {
        // Later automatic placements go past this one.
        let mut shared = ctx.shared.borrow_mut();
        shared.vmem.next_slot = shared.vmem.next_slot.max(((base + size) >> 32) + 1);
    }
    ctx.shared.borrow_mut().vmem.high.insert(base, HighRegion { size, alias, chunks: BTreeMap::new() });
    if kind & MEM_COMMIT != 0 {
        commit_high(ctx, memory, base, size)?;
    }
    Ok(base)
}

/// Commit pages of a high reservation: map the parts not yet mapped.
pub fn commit_high<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, address: u64, size: u64) -> Result<u64, u32> {
    let page = PAGE as u64;
    let start = address & !(page - 1);
    let end = (address + size.max(1) + page - 1) & !(page - 1);
    let (base, alias, gaps) = {
        let shared = ctx.shared.borrow();
        let base = find_high(&shared.vmem, address).ok_or(487u32)?;
        let region = &shared.vmem.high[&base];
        if end > base + region.size {
            return Err(487);
        }
        // The uncommitted stretches of [start, end).
        let mut gaps = Vec::new();
        let mut cursor = start;
        for (&chunk, &length) in region.chunks.range(..end) {
            let chunk_end = chunk + length;
            if chunk_end <= cursor {
                continue;
            }
            if chunk > cursor {
                gaps.push((cursor, chunk));
            }
            cursor = cursor.max(chunk_end);
        }
        if cursor < end {
            gaps.push((cursor, end));
        }
        (base, region.alias, gaps)
    };
    let alias_end = base + ALIAS_SIZE as u64;
    for (mut from, to) in gaps {
        // Split where the mirrored window ends.
        let mut pieces = Vec::new();
        if alias.is_some() && from < alias_end && to > alias_end {
            pieces.push((from, alias_end));
            from = alias_end;
        }
        pieces.push((from, to));
        for (from, to) in pieces {
            let low = alias.filter(|_| to <= alias_end).map(|low| low + (from - base) as u32);
            if !memory.map_shared(from, to - from, low) {
                return Err(8);
            }
            ctx.shared.borrow_mut().vmem.high.get_mut(&base).expect("region").chunks.insert(from, to - from);
        }
    }
    Ok(start)
}

/// Release a high reservation.
pub fn release_high<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, address: u64) -> Result<(), u32> {
    let region = ctx.shared.borrow_mut().vmem.high.remove(&address).ok_or(487u32)?;
    for (&chunk, &length) in &region.chunks {
        memory.unmap_shared(chunk);
        if let Some(low) = region.alias.filter(|_| chunk + length <= address + ALIAS_SIZE as u64) {
            // Refill the hole so the window can be unmapped whole.
            memory.map_exact(low + (chunk - address) as u32, length as u32);
        }
    }
    if let Some(low) = region.alias {
        memory.unmap(low, ALIAS_SIZE);
    }
    Ok(())
}

/// MEMORY_BASIC_INFORMATION values for an address in a high reservation:
/// base, allocation base, allocation protection, size, state, protection,
/// type.
pub fn query_high(ctx: &ApiContext, address: u64) -> Option<[u64; 7]> {
    let shared = ctx.shared.borrow();
    let base = find_high(&shared.vmem, address)?;
    let region = &shared.vmem.high[&base];
    let page = address & !(PAGE as u64 - 1);
    let containing = region.chunks.range(..=page).next_back().filter(|(&c, &l)| page < c + l);
    let (size, state, protect) = match containing {
        Some((&chunk, &length)) => {
            // Adjacent pieces form one committed run.
            let mut end = chunk + length;
            while let Some(&next) = region.chunks.get(&end) {
                end += next;
            }
            (end - page, MEM_COMMIT, 4)
        }
        None => {
            let next = region.chunks.range(page..).next().map(|(&c, _)| c).unwrap_or(base + region.size);
            (next - page, MEM_RESERVE, 0)
        }
    };
    Some([page, base, 4, size, state as u64, protect, MEM_PRIVATE as u64])
}

const _: u32 = MEM_DECOMMIT;
