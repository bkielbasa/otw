//! Threads: what the library asks of them. Until the kernel schedules
//! threads, there is one; `spawn` is not available yet.

use alloc::boxed::Box;
use alloc::string::String;
use core::time::Duration;

use super::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadId(u64);

#[derive(Clone, Debug)]
pub struct Thread {
    id: ThreadId,
}

impl Thread {
    pub fn id(&self) -> ThreadId {
        self.id
    }
    pub fn name(&self) -> Option<&str> {
        Some("main")
    }
    pub fn unpark(&self) {}
}

pub fn current() -> Thread {
    Thread { id: ThreadId(super::system().thread_id()) }
}

pub fn sleep(duration: Duration) {
    super::system().sleep_us(duration.as_micros() as u64);
}

pub fn yield_now() {
    super::system().yield_now();
}

pub fn park() {
    yield_now();
}

pub fn park_timeout(duration: Duration) {
    sleep(duration.min(Duration::from_millis(1)));
}

pub struct JoinHandle<T>(core::marker::PhantomData<T>);

impl<T> JoinHandle<T> {
    pub fn join(self) -> Result<T, Box<dyn core::any::Any + Send>> {
        Err(Box::new("threads are not available yet"))
    }
    pub fn thread(&self) -> Thread {
        current()
    }
    pub fn is_finished(&self) -> bool {
        true
    }
}

pub fn spawn<F: FnOnce() -> T + Send + 'static, T: Send + 'static>(_f: F) -> JoinHandle<T> {
    panic!("threads are not available yet on bare metal")
}

#[derive(Default)]
pub struct Builder {
    _name: Option<String>,
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn name(self, name: String) -> Self {
        Self { _name: Some(name) }
    }
    pub fn stack_size(self, _size: usize) -> Self {
        self
    }
    pub fn spawn<F: FnOnce() -> T + Send + 'static, T: Send + 'static>(self, _f: F) -> io::Result<JoinHandle<T>> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

pub fn available_parallelism() -> io::Result<core::num::NonZeroUsize> {
    Ok(core::num::NonZeroUsize::new(1).unwrap())
}

/// A `thread_local!` value: one per thread (`System::thread_id`).
pub struct LocalKey<T: 'static> {
    init: fn() -> T,
    values: super::sync::Mutex<alloc::collections::BTreeMap<u64, Box<T>>>,
}

unsafe impl<T> Sync for LocalKey<T> {}

#[derive(Debug, Clone, Copy)]
pub struct AccessError;

impl<T: 'static> LocalKey<T> {
    #[doc(hidden)]
    pub const fn new(init: fn() -> T) -> Self {
        Self { init, values: super::sync::Mutex::new(alloc::collections::BTreeMap::new()) }
    }

    pub fn with<R>(&'static self, f: impl FnOnce(&T) -> R) -> R {
        let id = super::system().thread_id();
        let value: *const T = {
            let values = self.values.lock().unwrap_or_else(super::sync::PoisonError::into_inner);
            match values.get(&id) {
                Some(value) => &**value,
                None => {
                    drop(values);
                    // Initialising may use other thread locals: not under
                    // the lock.
                    let made = Box::new((self.init)());
                    let mut values = self.values.lock().unwrap_or_else(super::sync::PoisonError::into_inner);
                    &**values.entry(id).or_insert(made)
                }
            }
        };
        // The box lives as long as the key (values are never removed), and
        // only this thread uses it.
        f(unsafe { &*value })
    }

    pub fn try_with<R>(&'static self, f: impl FnOnce(&T) -> R) -> Result<R, AccessError> {
        Ok(self.with(f))
    }
}

impl<T: 'static> LocalKey<core::cell::RefCell<T>> {
    pub fn with_borrow<R>(&'static self, f: impl FnOnce(&T) -> R) -> R {
        self.with(|cell| f(&cell.borrow()))
    }
    pub fn with_borrow_mut<R>(&'static self, f: impl FnOnce(&mut T) -> R) -> R {
        self.with(|cell| f(&mut cell.borrow_mut()))
    }
    pub fn set(&'static self, value: T) {
        self.with(|cell| *cell.borrow_mut() = value);
    }
    pub fn take(&'static self) -> T
    where
        T: Default,
    {
        self.with(|cell| cell.take())
    }
    pub fn replace(&'static self, value: T) -> T {
        self.with(|cell| cell.replace(value))
    }
}

impl<T: Copy + 'static> LocalKey<core::cell::Cell<T>> {
    pub fn get(&'static self) -> T {
        self.with(|cell| cell.get())
    }
    pub fn set(&'static self, value: T) {
        self.with(|cell| cell.set(value));
    }
}
