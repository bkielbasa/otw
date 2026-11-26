//! Library surface of the Windows 95 compatibility VM.

// The layers, from the bottom up. Each may use only the layers above it in
// this list; `tests/layering.rs` checks the rule.

/// The host side: the window the desktop is drawn in.
extern crate alloc;

pub mod host;
/// The OTW kernel: processes and their CPU, scheduling, the guest address
/// space, PE loading and structured exception dispatch.
pub mod kernel;
/// A desktop session: several programs running side by side.
pub mod session;
/// The desktop shell: Start menu, folder windows, launching programs.
pub mod shell;
/// A runnable program: a kernel process bound to the Win32 subsystem.
pub mod vm;
/// The Win32 environment subsystem: the API DLLs programs call.
pub mod win32;

/// Read-only FAT32 disk image reader.
pub mod fat32;
/// Bare-metal PC machine for boot sectors (separate from the process VM).
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
