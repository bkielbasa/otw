//! Library surface of the Windows 95 compatibility VM.
//!
//! With the `std` feature (the default) it runs on Linux. Without it, it
//! builds for bare metal: `platform` stands in for `std`, served by the
//! kernel in `baremetal/`.

#![cfg_attr(not(feature = "std"), no_std)]

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

#[macro_use]
extern crate alloc;

// Without std: its printing and thread-local macros, over `platform`.
#[cfg(not(feature = "std"))]
macro_rules! eprintln {
    () => { $crate::platform::_print(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::platform::_print(format_args!("{}\n", format_args!($($arg)*))) };
}
#[cfg(not(feature = "std"))]
#[allow(unused_macros)]
macro_rules! println {
    () => { $crate::platform::_print(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::platform::_print(format_args!("{}\n", format_args!($($arg)*))) };
}
#[cfg(not(feature = "std"))]
#[allow(unused_macros)]
macro_rules! eprint {
    ($($arg:tt)*) => { $crate::platform::_print(format_args!($($arg)*)) };
}
#[cfg(not(feature = "std"))]
#[allow(unused_macros)]
macro_rules! print {
    ($($arg:tt)*) => { $crate::platform::_print(format_args!($($arg)*)) };
}
#[cfg(not(feature = "std"))]
macro_rules! thread_local {
    () => {};
    ($(#[$attr:meta])* $vis:vis static $name:ident : $t:ty = const $init:block; $($rest:tt)*) => {
        thread_local!($(#[$attr])* $vis static $name: $t = $init; $($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident : $t:ty = $init:expr; $($rest:tt)*) => {
        $(#[$attr])* $vis static $name: $crate::platform::thread::LocalKey<$t> =
            $crate::platform::thread::LocalKey::new({ fn init() -> $t { $init } init });
        thread_local!($($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident : $t:ty = $init:expr) => {
        thread_local!($(#[$attr])* $vis static $name: $t = $init;);
    };
}

/// What the library needs from the system it runs on: `std` on Linux, the
/// bare-metal kernel's services otherwise. Every module names it `std`.
pub mod platform;

// The layers, from the bottom up. Each may use only the layers above it in
// this list; `tests/layering.rs` checks the rule.

/// The host side: the window the desktop is drawn in.
#[cfg(feature = "std")]
pub mod host;
/// The OTW kernel: processes and their CPU, scheduling, the guest address
/// space, PE loading and structured exception dispatch.
pub mod kernel;
/// A desktop session: several programs running side by side.
#[cfg(feature = "std")]
pub mod session;
/// The desktop shell: Start menu, folder windows, launching programs.
pub mod shell;
/// A runnable program: a kernel process bound to the Win32 subsystem.
#[cfg(feature = "std")]
pub mod vm;
/// The Win32 environment subsystem: the API DLLs programs call.
pub mod win32;

/// Read-only FAT32 disk image reader.
pub mod fat32;
/// Bare-metal PC machine for boot sectors (separate from the process VM).
#[cfg(feature = "std")]
pub mod machine;

/// Test support: a file under `binaries/`, or `None` (with a note) when it
/// is not in the checkout. Windows 95 files are not distributed with the
/// source, so tests that need them skip without them.
#[cfg(test)]
pub(crate) fn test_binary(path: &str) -> Option<Vec<u8>> {
    let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(path);
    match std::fs::read(&full) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            eprintln!("skipping: missing binaries/{path} (Windows 95 files are not distributed; see the README)");
            None
        }
    }
}

/// Self-contained guest artifacts for packaged builds.
#[cfg(feature = "embedded-target")]
pub mod assets {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    /// Genuine Windows 95 RTM NOTEPAD.EXE (SHA-256
    /// `0c90f92bedb3e437d0d9339219cb1f2898f44ed8172d88cf4bb62d29e9796f06`).
    pub const NOTEPAD_EXE: &[u8] = include_bytes!("../binaries/win95/NOTEPAD.EXE");

    /// Hand-built Win32 console guest used as the first execution test.
    pub const HELLO_EXE: &[u8] = include_bytes!("../binaries/guests/HELLO.EXE");

    /// Hand-encoded freestanding x86 kernel used to exercise the machine layer.
    pub const HELLO_KERNEL: &[u8] = include_bytes!("../binaries/kernels/HELLO.BIN");

    /// Hand-built 512-byte real-mode boot sector using BIOS INT 10h teletype.
    pub const BOOT_BIN: &[u8] = include_bytes!("../binaries/boot/OTWBOOT.BIN");

    /// Hand-built 512-byte boot sector that exercises VGA mode 13h.
    pub const VGA_BOOT_BIN: &[u8] = include_bytes!("../binaries/boot/OTWVGA.BIN");
}
