//! Where the CPUs' time goes (OTW_PROFILE): at each timer interrupt a
//! CPU notes what it was doing — halted with nothing to do, running a
//! program's code (where), or kernel code (in which of the program's
//! calls, or which task). The samples are summed in the status report.
//!
//! The interrupt only writes into a fixed ring (no locks, no memory
//! allocated there); names of calls are interned beforehand.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::sync::SpinLock;

const SAMPLES: usize = 1 << 16;

pub const IDLE: u64 = 0;
pub const USER: u64 = 1;
pub const KERNEL: u64 = 2;

struct Slot {
    /// Kind | task id << 8.
    what: AtomicU64,
    /// A program's code address, or the tag of kernel work.
    at: AtomicU64,
    /// The address space of program code.
    space: AtomicU64,
}

static RING: [Slot; SAMPLES] =
    [const { Slot { what: AtomicU64::new(0), at: AtomicU64::new(0), space: AtomicU64::new(0) } }; SAMPLES];
static WRITTEN: AtomicU64 = AtomicU64::new(0);
static ON: AtomicBool = AtomicBool::new(false);

pub fn start() {
    ON.store(true, Ordering::Relaxed);
}

pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// A timer interrupt arrived: in program code at `rip` (`user`), else in
/// the kernel.
pub fn sample(user: bool, rip: u64) {
    if !on() {
        return;
    }
    let (idle, id, tag, cr3) = crate::sched::sample_current();
    let (kind, at) = if user {
        (USER, rip)
    } else if idle {
        (IDLE, 0)
    } else {
        (KERNEL, tag as u64)
    };
    let n = WRITTEN.fetch_add(1, Ordering::Relaxed) as usize % SAMPLES;
    RING[n].what.store(kind | id << 8, Ordering::Relaxed);
    RING[n].at.store(at, Ordering::Relaxed);
    RING[n].space.store(cr3, Ordering::Relaxed);
}

/// A sample: kind, task, address or tag, address space.
pub struct Sample {
    pub kind: u64,
    pub task: u64,
    pub at: u64,
    pub space: u64,
}

/// The samples since the last call.
pub fn take() -> Vec<Sample> {
    static READ: AtomicU64 = AtomicU64::new(0);
    let end = WRITTEN.load(Ordering::Relaxed);
    let start = READ.swap(end, Ordering::Relaxed).max(end.saturating_sub(SAMPLES as u64));
    (start..end)
        .map(|n| {
            let slot = &RING[n as usize % SAMPLES];
            let what = slot.what.load(Ordering::Relaxed);
            Sample { kind: what & 0xFF, task: what >> 8, at: slot.at.load(Ordering::Relaxed), space: slot.space.load(Ordering::Relaxed) }
        })
        .collect()
}

static TAGS: SpinLock<BTreeMap<String, usize>> = SpinLock::new(BTreeMap::new());

/// A lasting tag for kernel work named `name`: the address of a leaked
/// `&str` (what the interrupt stores).
pub fn tag(name: &str) -> usize {
    let mut tags = TAGS.lock();
    if let Some(&tag) = tags.get(name) {
        return tag;
    }
    let interned: &'static str = alloc::boxed::Box::leak(String::from(name).into_boxed_str());
    let holder: &'static &'static str = alloc::boxed::Box::leak(alloc::boxed::Box::new(interned));
    let tag = holder as *const &'static str as usize;
    tags.insert(String::from(name), tag);
    tag
}

/// The name a tag stands for.
pub fn name(tag: usize) -> &'static str {
    if tag == 0 {
        return "";
    }
    unsafe { *(tag as *const &'static str) }
}
