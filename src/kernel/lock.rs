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
    /// When the owner took it (OTW_TRACE_LOCK).
    since: Option<std::time::Instant>,
    /// The owner's name (OTW_TRACE_LOCK).
    name: String,
}

#[derive(Default)]
pub struct BigLock {
    owner: Mutex<Owner>,
    free: Condvar,
}

fn trace_lock() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OTW_TRACE_LOCK").is_some())
}

thread_local! {
    /// Time holding the lock: total, longest since the last report, last
    /// report.
    static HELD: RefCell<(std::time::Duration, std::time::Duration, std::time::Instant)> = RefCell::new((std::time::Duration::ZERO, std::time::Duration::ZERO, std::time::Instant::now()));
    /// Contended acquisitions: count, total wait, last report.
    static WAITED: RefCell<(u64, std::time::Duration, std::time::Instant)> = RefCell::new((0, std::time::Duration::ZERO, std::time::Instant::now()));
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
        // Briefly spin before sleeping: the lock is held for short spells
        // (a system call), and waking a sleeper costs more.
        for _ in 0..200 {
            let owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
            if owner.thread.is_none() || owner.thread == Some(me) {
                break;
            }
            drop(owner);
            std::hint::spin_loop();
        }
        let mut owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
        if owner.thread.is_some_and(|t| t != me) {
            let started = std::time::Instant::now();
            while owner.thread.is_some_and(|t| t != me) {
                owner = self.free.wait(owner).unwrap_or_else(|p| p.into_inner());
            }
            if trace_lock() {
                WAITED.with(|w| {
                    let mut w = w.borrow_mut();
                    w.0 += 1;
                    w.1 += started.elapsed();
                    if w.2.elapsed() >= std::time::Duration::from_secs(10) {
                        eprintln!("[lock] {}: waited {} times, {:.2?} in all", std::thread::current().name().unwrap_or("?"), w.0, w.1);
                        w.2 = std::time::Instant::now();
                    }
                });
            }
        }
        if owner.thread.is_none() && std::env::var_os("OTW_TRACE_LOCK").is_some() {
            owner.name = std::thread::current().name().unwrap_or("?").to_string();
        }
        if owner.thread.is_none() && trace_lock() {
            owner.since = Some(std::time::Instant::now());
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
            if let Some(since) = owner.since.take() {
                let held = since.elapsed();
                HELD.with(|h| {
                    let mut h = h.borrow_mut();
                    h.0 += held;
                    h.1 = h.1.max(held);
                    if h.2.elapsed() >= std::time::Duration::from_secs(10) {
                        eprintln!("[lock] {}: held {:.2?} in all, longest {:.2?}", std::thread::current().name().unwrap_or("?"), h.0, h.1);
                        h.2 = std::time::Instant::now();
                        h.1 = std::time::Duration::ZERO;
                    }
                });
            }
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
