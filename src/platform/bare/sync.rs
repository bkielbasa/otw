//! Locks and one-time initialisation. Until the kernel schedules threads
//! the library runs on one thread at a time; the locks spin, and waits
//! hand the CPU back to the system (`System::sleep_us`) between checks.

use core::cell::UnsafeCell;
use core::fmt;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use core::time::Duration;

pub use alloc::sync::{Arc, Weak};
pub use core::sync::atomic;

pub struct PoisonError<T>(T);

impl<T> PoisonError<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
    pub fn get_ref(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for PoisonError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PoisonError")
    }
}

impl<T> fmt::Display for PoisonError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("poisoned lock")
    }
}

pub enum TryLockError<T> {
    Poisoned(PoisonError<T>),
    WouldBlock,
}

pub type LockResult<T> = Result<T, PoisonError<T>>;
pub type TryLockResult<T> = Result<T, TryLockError<T>>;

#[derive(Default)]
pub struct Mutex<T: ?Sized> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }
    pub fn into_inner(self) -> LockResult<T> {
        Ok(self.value.into_inner())
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        let mut spins = 0u32;
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            spins += 1;
            if spins % 1024 == 0 {
                super::system().yield_now();
            }
            core::hint::spin_loop();
        }
        Ok(MutexGuard { mutex: self })
    }
    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            Ok(MutexGuard { mutex: self })
        } else {
            Err(TryLockError::WouldBlock)
        }
    }
    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        Ok(self.value.get_mut())
    }
    pub fn is_poisoned(&self) -> bool {
        false
    }
    fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Mutex")
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

/// Reader-writer lock: one holder at a time here.
#[derive(Default)]
pub struct RwLock<T: ?Sized>(Mutex<T>);

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }
}

impl<T: ?Sized> RwLock<T> {
    pub fn read(&self) -> LockResult<MutexGuard<'_, T>> {
        self.0.lock()
    }
    pub fn write(&self) -> LockResult<MutexGuard<'_, T>> {
        self.0.lock()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitTimeoutResult(bool);

impl WaitTimeoutResult {
    pub fn timed_out(&self) -> bool {
        self.0
    }
}

/// Waits release the lock, let the system run something else, and take
/// the lock again (a wake-up may come at any time, as with `std`).
#[derive(Default, Debug)]
pub struct Condvar(#[allow(dead_code)] AtomicU8);

impl Condvar {
    pub const fn new() -> Self {
        Self(AtomicU8::new(0))
    }

    fn pause<'a, T: ?Sized>(&self, guard: MutexGuard<'a, T>, us: u64) -> MutexGuard<'a, T> {
        let mutex = guard.mutex;
        drop(guard);
        // Short waits let other threads run; longer ones sleep.
        if us <= 100 {
            super::system().yield_now();
        } else {
            super::system().sleep_us(us);
        }
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn wait<'a, T: ?Sized>(&self, guard: MutexGuard<'a, T>) -> LockResult<MutexGuard<'a, T>> {
        Ok(self.pause(guard, 1000))
    }

    pub fn wait_while<'a, T: ?Sized, F: FnMut(&mut T) -> bool>(&self, mut guard: MutexGuard<'a, T>, mut condition: F) -> LockResult<MutexGuard<'a, T>> {
        while condition(&mut *guard) {
            guard = self.pause(guard, 1000);
        }
        Ok(guard)
    }

    pub fn wait_timeout<'a, T: ?Sized>(&self, guard: MutexGuard<'a, T>, timeout: Duration) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)> {
        let us = (timeout.as_micros() as u64).min(1000);
        let guard = self.pause(guard, us);
        Ok((guard, WaitTimeoutResult(us >= timeout.as_micros() as u64)))
    }

    pub fn wait_timeout_while<'a, T: ?Sized, F: FnMut(&mut T) -> bool>(
        &self,
        mut guard: MutexGuard<'a, T>,
        timeout: Duration,
        mut condition: F,
    ) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)> {
        let system = super::system();
        let deadline = system.now_us().saturating_add(timeout.as_micros() as u64);
        loop {
            if !condition(&mut *guard) {
                return Ok((guard, WaitTimeoutResult(false)));
            }
            let now = system.now_us();
            if now >= deadline {
                return Ok((guard, WaitTimeoutResult(true)));
            }
            guard = self.pause(guard, (deadline - now).min(1000));
        }
    }

    pub fn notify_one(&self) {}
    pub fn notify_all(&self) {}
}

pub struct OnceLock<T> {
    state: AtomicU8, // 0 empty, 1 being set, 2 set
    value: UnsafeCell<Option<T>>,
}

unsafe impl<T: Send + Sync> Sync for OnceLock<T> {}
unsafe impl<T: Send> Send for OnceLock<T> {}

impl<T> Default for OnceLock<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for OnceLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OnceLock").field(&self.get()).finish()
    }
}

impl<T> OnceLock<T> {
    pub const fn new() -> Self {
        Self { state: AtomicU8::new(0), value: UnsafeCell::new(None) }
    }
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == 2 {
            unsafe { (*self.value.get()).as_ref() }
        } else {
            None
        }
    }
    pub fn get_or_init(&self, init: impl FnOnce() -> T) -> &T {
        if let Some(value) = self.get() {
            return value;
        }
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire).is_ok() {
            unsafe { *self.value.get() = Some(init()) };
            self.state.store(2, Ordering::Release);
        } else {
            while self.state.load(Ordering::Acquire) != 2 {
                core::hint::spin_loop();
            }
        }
        self.get().unwrap()
    }
    pub fn set(&self, value: T) -> Result<(), T> {
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire).is_ok() {
            unsafe { *self.value.get() = Some(value) };
            self.state.store(2, Ordering::Release);
            Ok(())
        } else {
            Err(value)
        }
    }
}

pub struct Once(OnceLock<()>);

impl Once {
    pub const fn new() -> Self {
        Self(OnceLock::new())
    }
    pub fn call_once(&self, f: impl FnOnce()) {
        self.0.get_or_init(f);
    }
    pub fn is_completed(&self) -> bool {
        self.0.get().is_some()
    }
}

pub struct LazyLock<T, F = fn() -> T> {
    cell: OnceLock<T>,
    init: UnsafeCell<Option<F>>,
}

unsafe impl<T: Send + Sync, F: Send> Sync for LazyLock<T, F> {}

impl<T, F: FnOnce() -> T> LazyLock<T, F> {
    pub const fn new(init: F) -> Self {
        Self { cell: OnceLock::new(), init: UnsafeCell::new(Some(init)) }
    }
}

impl<T, F: FnOnce() -> T> Deref for LazyLock<T, F> {
    type Target = T;
    fn deref(&self) -> &T {
        self.cell.get_or_init(|| unsafe { (*self.init.get()).take().expect("LazyLock initialised twice")() })
    }
}
