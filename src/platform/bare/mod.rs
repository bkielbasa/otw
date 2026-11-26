//! The standard library's interface, as much as the library uses, without
//! an operating system underneath (see `platform`). What only a system can
//! do — the clock, files, the log, waiting — goes to the `System` the
//! bare-metal kernel installs with `set_system`.

pub use alloc::{borrow, boxed, fmt, format, rc, slice, str, string, vec};
pub use core::{
    any, array, ascii, cell, char, clone, cmp, convert, default, f32, f64, hash, hint, iter, marker, mem, num, ops,
    option, pin, primitive, ptr, result,
};
pub use core::{assert, assert_eq, assert_ne, debug_assert, debug_assert_eq, matches, todo, unimplemented, unreachable, write, writeln};

pub mod float;

use alloc::string::String;
use alloc::vec::Vec;

/// A file or directory, as the system reports it.
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub name: String,
    pub directory: bool,
    pub size: u64,
    /// Seconds since 1970.
    pub modified: u64,
}

/// What the kernel provides. Paths are `/`-separated and absolute.
pub trait System: Send + Sync {
    /// Microseconds since the system started.
    fn now_us(&self) -> u64;
    /// Seconds since 1970 (UTC).
    fn unix_time(&self) -> u64;
    fn log(&self, text: &str);
    fn env(&self, name: &str) -> Option<String>;
    /// Every configuration variable (name, value).
    fn vars(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn sleep_us(&self, us: u64);
    fn yield_now(&self) {
        self.sleep_us(0);
    }
    fn read_file(&self, path: &str) -> Option<Vec<u8>>;
    fn write_file(&self, path: &str, data: &[u8]) -> bool;
    fn info(&self, path: &str) -> Option<FileInfo>;
    fn list(&self, path: &str) -> Option<Vec<FileInfo>>;
    fn create_dir(&self, path: &str) -> bool;
    fn remove(&self, path: &str) -> bool;
    fn rename(&self, from: &str, to: &str) -> bool;
    /// An identifier for the running thread (0 until there are threads).
    fn thread_id(&self) -> u64 {
        0
    }
}

struct NoSystem;

impl System for NoSystem {
    fn now_us(&self) -> u64 {
        0
    }
    fn unix_time(&self) -> u64 {
        0
    }
    fn log(&self, _: &str) {}
    fn env(&self, _: &str) -> Option<String> {
        None
    }
    fn sleep_us(&self, _: u64) {}
    fn read_file(&self, _: &str) -> Option<Vec<u8>> {
        None
    }
    fn write_file(&self, _: &str, _: &[u8]) -> bool {
        false
    }
    fn info(&self, _: &str) -> Option<FileInfo> {
        None
    }
    fn list(&self, _: &str) -> Option<Vec<FileInfo>> {
        None
    }
    fn create_dir(&self, _: &str) -> bool {
        false
    }
    fn remove(&self, _: &str) -> bool {
        false
    }
    fn rename(&self, _: &str, _: &str) -> bool {
        false
    }
}

static SYSTEM: core::sync::atomic::AtomicPtr<&'static dyn System> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Install the system's services (once, before using the library).
pub fn set_system(system: &'static dyn System) {
    let boxed: &'static mut &'static dyn System = alloc::boxed::Box::leak(alloc::boxed::Box::new(system));
    SYSTEM.store(boxed, core::sync::atomic::Ordering::SeqCst);
}

pub fn system() -> &'static dyn System {
    let pointer = SYSTEM.load(core::sync::atomic::Ordering::SeqCst);
    if pointer.is_null() {
        &NoSystem
    } else {
        unsafe { *pointer }
    }
}

/// For `print!` and friends.
#[doc(hidden)]
pub fn _print(args: core::fmt::Arguments<'_>) {
    let mut text = String::new();
    let _ = core::fmt::write(&mut text, args);
    system().log(&text);
}

pub mod error {
    pub use core::error::Error;
}

pub mod ffi {
    pub use alloc::ffi::{CString, NulError};
    pub use core::ffi::{c_char, c_int, c_void, CStr};
    pub use super::path::{OsStr, OsString};
}

pub mod collections {
    pub use alloc::collections::*;
    pub use hashbrown::{HashMap, HashSet};
    pub mod hash_map {
        pub use hashbrown::hash_map::*;
    }
    pub mod hash_set {
        pub use hashbrown::hash_set::*;
    }
}

pub mod time {
    pub use core::time::Duration;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(u64);

    impl Instant {
        pub fn now() -> Self {
            Self(super::system().now_us())
        }
        pub fn elapsed(&self) -> Duration {
            Self::now().duration_since(*self)
        }
        pub fn duration_since(&self, earlier: Instant) -> Duration {
            Duration::from_micros(self.0.saturating_sub(earlier.0))
        }
        pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            self.duration_since(earlier)
        }
        pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
            self.0.checked_sub(earlier.0).map(Duration::from_micros)
        }
        pub fn checked_add(&self, duration: Duration) -> Option<Instant> {
            self.0.checked_add(duration.as_micros() as u64).map(Instant)
        }
        pub fn checked_sub(&self, duration: Duration) -> Option<Instant> {
            self.0.checked_sub(duration.as_micros() as u64).map(Instant)
        }
    }

    impl core::ops::Add<Duration> for Instant {
        type Output = Instant;
        fn add(self, d: Duration) -> Instant {
            Instant(self.0 + d.as_micros() as u64)
        }
    }

    impl core::ops::AddAssign<Duration> for Instant {
        fn add_assign(&mut self, d: Duration) {
            self.0 += d.as_micros() as u64;
        }
    }

    impl core::ops::Sub<Duration> for Instant {
        type Output = Instant;
        fn sub(self, d: Duration) -> Instant {
            Instant(self.0.saturating_sub(d.as_micros() as u64))
        }
    }

    impl core::ops::Sub<Instant> for Instant {
        type Output = Duration;
        fn sub(self, other: Instant) -> Duration {
            self.duration_since(other)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct SystemTime(Duration);

    pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::ZERO);

    #[derive(Clone, Copy, Debug)]
    pub struct SystemTimeError(Duration);

    impl SystemTimeError {
        pub fn duration(&self) -> Duration {
            self.0
        }
    }

    impl core::fmt::Display for SystemTimeError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "time before the epoch")
        }
    }

    impl SystemTime {
        pub const UNIX_EPOCH: SystemTime = UNIX_EPOCH;
        pub fn now() -> Self {
            let s = super::system();
            SystemTime(Duration::from_secs(s.unix_time()) + Duration::from_micros(s.now_us() % 1_000_000))
        }
        pub fn duration_since(&self, earlier: SystemTime) -> Result<Duration, SystemTimeError> {
            self.0.checked_sub(earlier.0).ok_or_else(|| SystemTimeError(earlier.0 - self.0))
        }
        pub fn elapsed(&self) -> Result<Duration, SystemTimeError> {
            SystemTime::now().duration_since(*self)
        }
        pub fn checked_add(&self, d: Duration) -> Option<SystemTime> {
            self.0.checked_add(d).map(SystemTime)
        }
        pub(crate) fn from_unix(seconds: u64) -> SystemTime {
            SystemTime(Duration::from_secs(seconds))
        }
    }

    impl core::ops::Add<Duration> for SystemTime {
        type Output = SystemTime;
        fn add(self, d: Duration) -> SystemTime {
            SystemTime(self.0 + d)
        }
    }

    impl core::ops::Sub<Duration> for SystemTime {
        type Output = SystemTime;
        fn sub(self, d: Duration) -> SystemTime {
            SystemTime(self.0.saturating_sub(d))
        }
    }
}

pub mod env {
    use super::path::{OsString, PathBuf};
    use alloc::string::String;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum VarError {
        NotPresent,
        NotUnicode(OsString),
    }

    impl core::fmt::Display for VarError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "environment variable not found")
        }
    }

    pub fn var(name: impl AsRef<str>) -> Result<String, VarError> {
        super::system().env(name.as_ref()).ok_or(VarError::NotPresent)
    }

    pub fn var_os(name: impl AsRef<str>) -> Option<OsString> {
        super::system().env(name.as_ref()).map(OsString::from)
    }

    /// Variables can only be read (the kernel's configuration).
    pub fn set_var(_name: impl AsRef<str>, _value: impl AsRef<str>) {}

    pub fn vars() -> impl Iterator<Item = (String, String)> {
        super::system().vars().into_iter()
    }

    pub fn temp_dir() -> PathBuf {
        PathBuf::from("/tmp")
    }

    pub fn split_paths(value: &OsString) -> impl Iterator<Item = PathBuf> + '_ {
        value.as_str().split(':').filter(|p| !p.is_empty()).map(PathBuf::from)
    }
}

pub mod path;

pub mod fs;

pub mod io;

pub mod sync;

pub mod thread;

pub mod process {
    use alloc::string::String;

    pub fn id() -> u32 {
        1
    }

    pub fn exit(code: i32) -> ! {
        panic!("process exit {code}")
    }

    pub struct Stdio;

    impl Stdio {
        pub fn null() -> Self {
            Stdio
        }
        pub fn piped() -> Self {
            Stdio
        }
        pub fn inherit() -> Self {
            Stdio
        }
    }

    /// Host programs: there are none without an operating system.
    pub struct Command;

    impl Command {
        pub fn new(_program: impl AsRef<str>) -> Self {
            Command
        }
        pub fn arg(&mut self, _arg: impl AsRef<str>) -> &mut Self {
            self
        }
        pub fn args<I: IntoIterator<Item = S>, S: AsRef<str>>(&mut self, _args: I) -> &mut Self {
            self
        }
        pub fn stdin(&mut self, _: Stdio) -> &mut Self {
            self
        }
        pub fn stdout(&mut self, _: Stdio) -> &mut Self {
            self
        }
        pub fn stderr(&mut self, _: Stdio) -> &mut Self {
            self
        }
        pub fn status(&mut self) -> super::io::Result<ExitStatus> {
            Err(super::io::Error::from(super::io::ErrorKind::Unsupported))
        }
        pub fn output(&mut self) -> super::io::Result<Output> {
            Err(super::io::Error::from(super::io::ErrorKind::Unsupported))
        }
        pub fn spawn(&mut self) -> super::io::Result<Child> {
            Err(super::io::Error::from(super::io::ErrorKind::Unsupported))
        }
    }

    pub struct Child;

    impl Child {
        pub fn wait(&mut self) -> super::io::Result<ExitStatus> {
            Err(super::io::Error::from(super::io::ErrorKind::Unsupported))
        }
    }

    #[derive(Clone, Copy, Debug)]
    pub struct ExitStatus;

    impl ExitStatus {
        pub fn success(&self) -> bool {
            false
        }
        pub fn code(&self) -> Option<i32> {
            None
        }
    }

    pub struct Output {
        pub status: ExitStatus,
        pub stdout: alloc::vec::Vec<u8>,
        pub stderr: alloc::vec::Vec<u8>,
    }

    #[allow(dead_code)]
    fn _unused(_: String) {}
}

pub mod net;
