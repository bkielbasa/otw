//! The subsystem lock of a process whose threads run on several host
//! threads at once.
//!
//! Guest code runs without it; everything else — system calls, faults,
//! the subsystem's state — runs with it, so the subsystem itself stays
//! single-threaded. A host thread takes it on entering the system from
//! guest code and gives it up when it returns to guest code, when it runs
//! guest code on the subsystem's behalf (a callback), and while it sleeps.
//!
//! It is re-entrant: a host thread counts its holds. Each host thread
//! knows the innermost process lock it holds (a program started inside
//! another's call runs on its parent's host thread with a lock of its own).

use std::{
    cell::RefCell,
    sync::{Arc, Condvar, Mutex},
    thread::ThreadId,
};

#[derive(Default)]
struct Owner {
    thread: Option<ThreadId>,
    depth: usize,
}

#[derive(Default)]
pub struct BigLock {
    owner: Mutex<Owner>,
    free: Condvar,
}

thread_local! {
    /// The locks this host thread holds, innermost last.
    static HELD: RefCell<Vec<Arc<BigLock>>> = const { RefCell::new(Vec::new()) };
}

impl BigLock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Take the lock (again, if this host thread holds it).
    pub fn acquire(self: &Arc<Self>) {
        let me = std::thread::current().id();
        let mut owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
        while owner.thread.is_some_and(|t| t != me) {
            owner = self.free.wait(owner).unwrap_or_else(|p| p.into_inner());
        }
        owner.thread = Some(me);
        owner.depth += 1;
        if owner.depth == 1 {
            HELD.with(|held| held.borrow_mut().push(self.clone()));
        }
    }

    /// Give up one hold.
    pub fn release(self: &Arc<Self>) {
        let mut owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
        if owner.thread != Some(std::thread::current().id()) || owner.depth == 0 {
            return;
        }
        owner.depth -= 1;
        if owner.depth == 0 {
            owner.thread = None;
            self.free.notify_one();
            HELD.with(|held| {
                let mut held = held.borrow_mut();
                if let Some(index) = held.iter().rposition(|l| Arc::ptr_eq(l, self)) {
                    held.remove(index);
                }
            });
        }
    }

    /// Give up every hold this host thread has; returns how many, for
    /// `reacquire`.
    pub fn release_all(self: &Arc<Self>) -> usize {
        let depth = {
            let owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
            if owner.thread != Some(std::thread::current().id()) {
                return 0;
            }
            owner.depth
        };
        for _ in 0..depth {
            self.release();
        }
        depth
    }

    /// Take the lock again as many times as `release_all` gave it up.
    pub fn reacquire(self: &Arc<Self>, depth: usize) {
        for _ in 0..depth {
            self.acquire();
        }
    }
}

/// Run `f` without the innermost process lock this host thread holds
/// (sleeping, waiting for another thread or process).
pub fn unlocked<R>(f: impl FnOnce() -> R) -> R {
    let lock = HELD.with(|held| held.borrow().last().cloned());
    match lock {
        Some(lock) => {
            let depth = lock.release_all();
            let result = f();
            lock.reacquire(depth);
            result
        }
        None => f(),
    }
}

/// Holds a lock for a scope.
pub struct Hold(Arc<BigLock>);

impl Hold {
    pub fn new(lock: &Arc<BigLock>) -> Self {
        lock.acquire();
        Self(lock.clone())
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.release();
    }
}
