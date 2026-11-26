//! The kernel heap: first fit over a list of free blocks kept in address
//! order, merged with their neighbours when freed. Regions come from the
//! firmware's memory map.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use crate::sync::SpinLock;

/// A free block's header, at its start.
struct Free {
    size: usize,
    next: *mut Free,
}

const MIN_BLOCK: usize = core::mem::size_of::<Free>();
const ALIGN: usize = 16;

pub struct Heap {
    head: SpinLock<*mut Free>,
    total: core::sync::atomic::AtomicUsize,
}

unsafe impl Sync for Heap {}

impl Heap {
    pub const fn new() -> Self {
        Self { head: SpinLock::new(null_mut()), total: core::sync::atomic::AtomicUsize::new(0) }
    }

    /// Hand memory to the heap.
    pub unsafe fn add_region(&self, start: usize, size: usize) {
        let aligned = (start + ALIGN - 1) & !(ALIGN - 1);
        let size = (size - (aligned - start)) & !(ALIGN - 1);
        if size < MIN_BLOCK * 2 {
            return;
        }
        self.total.fetch_add(size, core::sync::atomic::Ordering::Relaxed);
        let mut head = self.head.lock();
        insert(&mut head, aligned as *mut Free, size);
    }

    pub fn total(&self) -> usize {
        self.total.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn free_bytes(&self) -> usize {
        let head = self.head.lock();
        let mut sum = 0;
        let mut block = *head;
        while !block.is_null() {
            unsafe {
                sum += (*block).size;
                block = (*block).next;
            }
        }
        sum
    }
}

/// Insert a free block in address order, merging with its neighbours.
unsafe fn insert(head: &mut *mut Free, block: *mut Free, size: usize) {
    let mut previous: *mut Free = null_mut();
    let mut current = *head;
    while !current.is_null() && (current as usize) < block as usize {
        previous = current;
        current = (*current).next;
    }
    (*block).size = size;
    (*block).next = current;
    // Merge with the following block.
    if !current.is_null() && block as usize + size == current as usize {
        (*block).size += (*current).size;
        (*block).next = (*current).next;
    }
    if previous.is_null() {
        *head = block;
    } else if previous as usize + (*previous).size == block as usize {
        (*previous).size += (*block).size;
        (*previous).next = (*block).next;
    } else {
        (*previous).next = block;
    }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(ALIGN);
        let size = (layout.size().max(MIN_BLOCK) + ALIGN - 1) & !(ALIGN - 1);
        let mut head = self.head.lock();
        let mut previous: *mut Free = null_mut();
        let mut current = *head;
        while !current.is_null() {
            let start = current as usize;
            let end = start + (*current).size;
            let mut at = (start + align - 1) & !(align - 1);
            // The gap before an aligned block must be able to stay free.
            if at != start && at - start < MIN_BLOCK {
                at = (start + MIN_BLOCK + align - 1) & !(align - 1);
            }
            if at + size <= end {
                let next = (*current).next;
                // Unlink, then give back the pieces before and after.
                if previous.is_null() {
                    *head = next;
                } else {
                    (*previous).next = next;
                }
                if at > start {
                    insert(&mut head, start as *mut Free, at - start);
                }
                let tail = end - (at + size);
                if tail >= MIN_BLOCK {
                    insert(&mut head, (at + size) as *mut Free, tail);
                    return at as *mut u8;
                }
                // Too small to keep: it stays with the allocation, which
                // `dealloc` cannot know about, so it is lost (at most 16
                // bytes).
                return at as *mut u8;
            }
            previous = current;
            current = (*current).next;
        }
        null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = (layout.size().max(MIN_BLOCK) + ALIGN - 1) & !(ALIGN - 1);
        let mut head = self.head.lock();
        insert(&mut head, ptr as *mut Free, size);
    }
}
