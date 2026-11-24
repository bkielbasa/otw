//! Shared by tests that run genuine Windows 95 programs. Those files are
//! copyrighted and not part of the repository; tests that need them skip
//! when they are missing (see the README, "Windows 95 files").

#![allow(dead_code)] // each test crate uses some of these

use std::path::Path;

/// Whether the files in `binaries/` a test needs are present. Prints why a
/// test is skipped otherwise.
pub fn have(files: &[&str]) -> bool {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries");
    let missing: Vec<&str> = files
        .iter()
        .copied()
        .filter(|file| !root.join(file).is_file())
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "skipping: missing {} (Windows 95 files are not distributed; see the README)",
            missing.join(", ")
        );
    }
    missing.is_empty()
}

pub const NOTEPAD: &[&str] = &["win95/NOTEPAD.EXE"];
pub const PAINT: &[&str] = &[
    "win95/MSPAINT.EXE",
    "win95/MFC30.DLL",
    "win95/MFCO30.DLL",
    "win95/MFCANS32.dll",
];
pub const DEMO_DISK: &[&str] = &["disks/OTWDISK.IMG"];
