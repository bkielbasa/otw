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
    /// The owner's name (OTW_TRACE_LOCK).
    name: String,
}

#[derive(Default)]
pub struct BigLock {
    owner: Mutex<Owner>,
    free: Condvar,
}

thread_local! {
    /// The processes this host thread is in the system for (entered with
    /// a `Hold`), innermost last: the one whose waits give up its lock.
    static ACTIVE: RefCell<Vec<Arc<BigLock>>> = const { RefCell::new(Vec::new()) };
}

impl BigLock {
    pub fn new() -> Arc<Self> {
        let lock = Arc::new(Self::default());
        if std::env::var_os("OTW_TRACE_LOCK").is_some() {
            static LOCKS: Mutex<Vec<std::sync::Weak<BigLock>>> = Mutex::new(Vec::new());
            static WATCH: std::sync::Once = std::sync::Once::new();
            LOCKS.lock().unwrap_or_else(|p| p.into_inner()).push(Arc::downgrade(&lock));
            WATCH.call_once(|| {
                std::thread::spawn(|| loop {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    for lock in LOCKS.lock().unwrap_or_else(|p| p.into_inner()).iter().filter_map(std::sync::Weak::upgrade) {
                        let owner = lock.owner.lock().unwrap_or_else(|p| p.into_inner());
                        eprintln!("[lock] {:p}: held by {:?} ({}) depth {}", Arc::as_ptr(&lock), owner.thread, owner.name, owner.depth);
                    }
                });
            });
        }
        lock
    }

    /// Take the lock (again, if this host thread holds it).
    pub fn acquire(self: &Arc<Self>) {
        let me = std::thread::current().id();
        let mut owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
        while owner.thread.is_some_and(|t| t != me) {
            owner = self.free.wait(owner).unwrap_or_else(|p| p.into_inner());
        }
        if owner.thread.is_none() && std::env::var_os("OTW_TRACE_LOCK").is_some() {
            owner.name = std::thread::current().name().unwrap_or("?").to_string();
        }
        owner.thread = Some(me);
        owner.depth += 1;
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

/// Run `f` without the lock of the process this host thread is in the
/// system for (sleeping, waiting for another thread or process).
pub fn unlocked<R>(f: impl FnOnce() -> R) -> R {
    let lock = ACTIVE.with(|active| active.borrow().last().cloned());
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
    /// Take the lock and be in the system for its process.
    pub fn new(lock: &Arc<BigLock>) -> Self {
        lock.acquire();
        ACTIVE.with(|active| active.borrow_mut().push(lock.clone()));
        Self(lock.clone())
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if let Some(index) = active.iter().rposition(|l| Arc::ptr_eq(l, &self.0)) {
                active.remove(index);
            }
        });
        self.0.release();
    }
}
