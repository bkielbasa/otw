//! A process's address space, shared by the CPUs that run its threads.
//!
//! Each thread of a process that runs on a host thread of its own has a
//! CPU (an emulator instance) of its own, and every CPU maps the same host
//! memory: what one thread writes the others see. Mapping and unmapping
//! go through this record, which keeps the memory alive and a log of the
//! changes; each CPU applies the changes the others made when it next
//! enters the system (an API call, a fault on memory it has not mapped
//! yet).

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use unicorn_engine::{Prot, Unicorn};

use super::memory::SharedMemory;

/// A mapped range: its memory and where in it the range starts.
#[derive(Clone)]
pub struct Mapping {
    pub size: u64,
    pub memory: Arc<SharedMemory>,
    pub offset: usize,
}

#[derive(Clone)]
enum Change {
    Map { start: u64, mapping: Mapping },
    /// Unmapped; the memory it held stays alive until every CPU has
    /// unmapped it too.
    Unmap { start: u64, size: u64, _keep: Vec<Arc<SharedMemory>> },
}

#[derive(Default)]
struct Inner {
    /// Mapped ranges by start address.
    mappings: BTreeMap<u64, Mapping>,
    /// The changes some CPU has yet to apply, the first being change
    /// number `base`.
    log: std::collections::VecDeque<Change>,
    base: usize,
    /// Each CPU's position: the number of the next change it applies.
    cpus: std::collections::HashMap<u32, usize>,
    next_cpu: u32,
    /// Each CPU's engine (to stop it) and flags.
    engines: std::collections::HashMap<u32, (usize, Arc<CpuFlags>)>,
    /// One range of host memory every CPU maps whole, once: mapping
    /// fresh memory inside it is bookkeeping (and zeroing), with no change
    /// to any CPU. Parts replaced by other memory (views of sections) are
    /// `holes` in the CPUs' mapping of it.
    pool: Option<Pool>,
    holes: BTreeMap<u64, u64>,
}

struct Pool {
    start: u64,
    end: u64,
    memory: Arc<SharedMemory>,
}

impl Pool {
    fn covers(&self, start: u64, size: u64) -> bool {
        start >= self.start && start + size <= self.end
    }

    fn mapping(&self, start: u64, size: u64) -> Mapping {
        Mapping { size, memory: self.memory.clone(), offset: (start - self.start) as usize }
    }
}

/// What another thread needs to know of a CPU to make it apply an unmap
/// before it runs on.
#[derive(Default)]
pub struct CpuFlags {
    /// It runs guest code (without the subsystem lock).
    pub in_guest: std::sync::atomic::AtomicBool,
    /// It was stopped to apply address-space changes; it goes on after.
    pub stop: std::sync::atomic::AtomicBool,
}

impl Inner {
    fn end(&self) -> usize {
        self.base + self.log.len()
    }

    /// Forget the changes every CPU has applied.
    fn trim(&mut self) {
        let oldest = self.cpus.values().copied().min().unwrap_or(self.end());
        while self.base < oldest && !self.log.is_empty() {
            self.log.pop_front();
            self.base += 1;
        }
    }

    /// Whether `[start, start + size)` is mapped.
    fn taken(&self, start: u64, size: u64) -> bool {
        self.mappings.range(..start + size).next_back().is_some_and(|(&s, m)| s + m.size > start)
    }

    fn push(&mut self, cpu: u32, change: Change) {
        self.log.push_back(change);
        let end = self.end();
        self.cpus.insert(cpu, end);
        self.trim();
    }
}

#[derive(Default)]
pub struct AddressSpace {
    inner: Mutex<Inner>,
}

/// A CPU attached to the address space.
#[derive(Clone, Copy, Debug)]
pub struct Cursor(u32);

impl AddressSpace {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The mapped ranges, as (first, last) addresses inclusive.
    pub fn ranges(&self) -> Vec<(u64, u64)> {
        let inner = self.lock();
        inner.mappings.iter().map(|(&start, m)| (start, start + m.size - 1)).collect()
    }

    /// Whether any of `[start, start + size)` is mapped.
    pub fn overlaps(&self, start: u64, size: u64) -> bool {
        self.lock().taken(start, size)
    }

    /// Back `[start, end)` with one range of host memory that every CPU
    /// maps now (the first CPU must be attached and nothing mapped there).
    pub fn set_pool(&self, uc: &mut Unicorn<'_, ()>, start: u64, end: u64) -> bool {
        let Some(memory) = SharedMemory::new((end - start) as usize) else {
            return false;
        };
        let pointer = memory.pointer(0) as *mut std::ffi::c_void;
        // SAFETY: the record keeps the pool alive.
        if unsafe { uc.mem_map_ptr(start, end - start, Prot::ALL, pointer) }.is_err() {
            return false;
        }
        self.lock().pool = Some(Pool { start, end, memory });
        true
    }

    /// Whether `[start, start + size)` lies in the pool.
    pub fn in_pool(&self, start: u64, size: u64) -> bool {
        self.lock().pool.as_ref().is_some_and(|pool| start < pool.end && start + size > pool.start)
    }

    /// Map fresh zeroed memory at `start` in this CPU and the record.
    pub fn map(&self, uc: &mut Unicorn<'_, ()>, cursor: &mut Cursor, start: u64, size: u64) -> bool {
        // A range across the pool's end: the pool part and the rest.
        let pool_end = self.lock().pool.as_ref().map(|pool| pool.end);
        if let Some(end) = pool_end.filter(|&end| start < end && start + size > end) {
            if self.overlaps(start, size) {
                return false;
            }
            return self.map(uc, cursor, start, end - start) && self.map(uc, cursor, end, start + size - end);
        }
        {
            let mut inner = self.lock();
            let in_pool = inner.pool.as_ref().is_some_and(|pool| pool.covers(start, size));
            let in_hole = inner.holes.range(..start + size).next_back().is_some_and(|(&s, &n)| s + n > start);
            if in_pool && !in_hole {
                if inner.taken(start, size) {
                    return false;
                }
                let mapping = inner.pool.as_ref().map(|pool| pool.mapping(start, size));
                if let Some(mapping) = mapping {
                    mapping.memory.zero(mapping.offset, size as usize);
                    inner.mappings.insert(start, mapping);
                    return true;
                }
            }
        }
        let Some(memory) = SharedMemory::new(size as usize) else {
            return false;
        };
        self.map_memory(uc, cursor, start, size, memory, 0)
    }

    /// Map `size` bytes of `memory` from `offset` at `start`.
    pub fn map_memory(&self, uc: &mut Unicorn<'_, ()>, cursor: &mut Cursor, start: u64, size: u64, memory: Arc<SharedMemory>, offset: usize) -> bool {
        self.sync(uc, cursor);
        let mut inner = self.lock();
        if inner.taken(start, size) {
            return false;
        }
        // Inside the pool: cut a hole in it first, in every CPU.
        let hole = inner.pool.as_ref().is_some_and(|pool| pool.covers(start, size));
        if hole {
            let _ = uc.mem_unmap(start, size);
            inner.holes.insert(start, size);
            inner.push(cursor.0, Change::Unmap { start, size, _keep: Vec::new() });
        }
        let pointer = memory.pointer(offset) as *mut std::ffi::c_void;
        // SAFETY: the record keeps the memory alive while it is mapped.
        if unsafe { uc.mem_map_ptr(start, size, Prot::ALL, pointer) }.is_err() {
            return false;
        }
        let mapping = Mapping { size, memory, offset };
        inner.mappings.insert(start, mapping.clone());
        inner.push(cursor.0, Change::Map { start, mapping });
        drop(inner);
        if hole {
            self.barrier(cursor.0);
        }
        true
    }

    /// Unmap `[start, start + size)` (whole pages) in this CPU and the
    /// record, splitting mappings it cuts through.
    pub fn unmap(&self, uc: &mut Unicorn<'_, ()>, cursor: &mut Cursor, start: u64, size: u64) -> bool {
        self.sync(uc, cursor);
        let mut inner = self.lock();
        let end = start + size;
        let cut: Vec<(u64, Mapping)> = inner
            .mappings
            .range(..end)
            .filter(|(&s, m)| s + m.size > start)
            .map(|(&s, m)| (s, m.clone()))
            .collect();
        if cut.is_empty() {
            return false;
        }
        // Pool memory stays mapped in every CPU: giving it back is
        // bookkeeping. Other memory is unmapped, and a hole it filled in
        // the pool gets the pool's memory back.
        let pool_memory = inner.pool.as_ref().map(|pool| pool.memory.clone());
        let in_pool = |m: &Mapping| pool_memory.as_ref().is_some_and(|p| Arc::ptr_eq(p, &m.memory));
        let only_pool = cut.iter().all(|(_, m)| in_pool(m));
        let keep: Vec<Arc<SharedMemory>> = cut.iter().map(|(_, m)| m.memory.clone()).collect();
        for (s, m) in cut {
            inner.mappings.remove(&s);
            if s < start {
                inner.mappings.insert(s, Mapping { size: start - s, memory: m.memory.clone(), offset: m.offset });
            }
            if s + m.size > end {
                let skip = (end - s) as usize;
                inner.mappings.insert(end, Mapping { size: s + m.size - end, memory: m.memory.clone(), offset: m.offset + skip });
            }
        }
        if only_pool {
            return true;
        }
        let _ = uc.mem_unmap(start, size);
        inner.push(cursor.0, Change::Unmap { start, size, _keep: keep });
        // Holes this range covers are filled with the pool again.
        let filled: Vec<(u64, u64)> = inner.holes.range(start..end).map(|(&s, &n)| (s, n)).collect();
        for (s, n) in filled {
            inner.holes.remove(&s);
            if let Some(mapping) = inner.pool.as_ref().map(|pool| pool.mapping(s, n)) {
                let pointer = mapping.memory.pointer(mapping.offset) as *mut std::ffi::c_void;
                // SAFETY: the pool lives as long as the record.
                let _ = unsafe { uc.mem_map_ptr(s, n, Prot::ALL, pointer) };
                inner.push(cursor.0, Change::Map { start: s, mapping });
            }
        }
        drop(inner);
        // No CPU may go on with the old memory there: the range may be
        // given out again at once.
        self.barrier(cursor.0);
        true
    }

    /// Apply to this CPU the changes made since it last did.
    pub fn sync(&self, uc: &mut Unicorn<'_, ()>, cursor: &mut Cursor) {
        let mut inner = self.lock();
        let position = inner.cpus.get(&cursor.0).copied().unwrap_or(inner.end());
        if position >= inner.end() {
            return;
        }
        let first = position - inner.base;
        for change in inner.log.iter().skip(first) {
            match change {
                Change::Map { start, mapping } => {
                    let pointer = mapping.memory.pointer(mapping.offset) as *mut std::ffi::c_void;
                    // SAFETY: the record keeps the memory alive while mapped.
                    let _ = unsafe { uc.mem_map_ptr(*start, mapping.size, Prot::ALL, pointer) };
                }
                Change::Unmap { start, size, .. } => {
                    let _ = uc.mem_unmap(*start, *size);
                }
            }
        }
        let end = inner.end();
        inner.cpus.insert(cursor.0, end);
        inner.trim();
    }

    /// Make every other CPU apply the changes so far: those running guest
    /// code are stopped, apply them and go on (the others apply them
    /// before they run guest code or touch memory again).
    fn barrier(&self, me: u32) {
        use std::sync::atomic::Ordering;
        unsafe extern "C" {
            fn uc_emu_stop(uc: *mut std::ffi::c_void) -> i32;
        }
        let (target, others) = {
            let inner = self.lock();
            let others: Vec<(u32, usize, Arc<CpuFlags>)> = inner
                .engines
                .iter()
                .filter(|(&id, _)| id != me)
                .map(|(&id, (handle, flags))| (id, *handle, flags.clone()))
                .collect();
            (inner.end(), others)
        };
        if others.is_empty() {
            return;
        }
        for (_, handle, flags) in &others {
            if flags.in_guest.load(Ordering::SeqCst) {
                flags.stop.store(true, Ordering::SeqCst);
                // SAFETY: an engine leaves this list (detach) before it is
                // closed; uc_emu_stop may be called from another thread.
                unsafe { uc_emu_stop(*handle as *mut std::ffi::c_void) };
            }
        }
        let started = std::time::Instant::now();
        loop {
            let pending = {
                let inner = self.lock();
                others.iter().any(|(id, _, flags)| {
                    inner.engines.contains_key(id)
                        && flags.in_guest.load(Ordering::SeqCst)
                        && inner.cpus.get(id).is_some_and(|&p| p < target)
                })
            };
            if !pending || started.elapsed() > std::time::Duration::from_secs(5) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
    }

    /// Register a CPU's engine and flags (for `barrier`).
    pub fn register(&self, cursor: Cursor, engine: usize, flags: Arc<CpuFlags>) {
        self.lock().engines.insert(cursor.0, (engine, flags));
    }

    /// Attach a CPU: map everything mapped now into it (none for the
    /// first, which maps as it goes).
    pub fn attach(&self, uc: &mut Unicorn<'_, ()>) -> Cursor {
        let mut inner = self.lock();
        let pool_memory = inner.pool.as_ref().map(|pool| pool.memory.clone());
        if let Some(pool) = &inner.pool {
            // The pool, around its holes.
            let mut cursor = pool.start;
            for (&s, &n) in inner.holes.iter().chain(std::iter::once((&pool.end, &0))) {
                if s > cursor {
                    let pointer = pool.memory.pointer((cursor - pool.start) as usize) as *mut std::ffi::c_void;
                    // SAFETY: as in `sync`.
                    let _ = unsafe { uc.mem_map_ptr(cursor, s - cursor, Prot::ALL, pointer) };
                }
                cursor = s + n;
            }
        }
        for (&start, mapping) in &inner.mappings {
            if pool_memory.as_ref().is_some_and(|p| Arc::ptr_eq(p, &mapping.memory)) {
                continue;
            }
            let pointer = mapping.memory.pointer(mapping.offset) as *mut std::ffi::c_void;
            // SAFETY: as in `sync`.
            let _ = unsafe { uc.mem_map_ptr(start, mapping.size, Prot::ALL, pointer) };
        }
        inner.next_cpu += 1;
        let id = inner.next_cpu;
        let end = inner.end();
        inner.cpus.insert(id, end);
        Cursor(id)
    }

    /// A CPU that is gone.
    pub fn detach(&self, cursor: Cursor) {
        let mut inner = self.lock();
        inner.cpus.remove(&cursor.0);
        inner.engines.remove(&cursor.0);
        inner.trim();
    }

    /// Whether `address` is mapped in the record (perhaps not yet in a CPU
    /// that has not synced).
    pub fn contains(&self, address: u64) -> bool {
        let inner = self.lock();
        inner.mappings.range(..=address).next_back().is_some_and(|(&s, m)| address < s + m.size)
    }
}
