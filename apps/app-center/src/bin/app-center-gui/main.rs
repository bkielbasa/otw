//! `app-center-gui`: the graphical App Center.
//!
//! Everything it shows and decides lives in `app_center::ui` (plain Rust,
//! unit-tested on any system); this binary is the thin Win32 layer around
//! it: a custom-painted window (`window`), GDI drawing helpers (`gdi`),
//! worker threads for the network (`worker`) and the shell (`shell`).

#![cfg_attr(windows, windows_subsystem = "windows")]

/// Allocations from big blocks of the program's own (see Cargo.toml), not
/// a system heap call each.
#[cfg(windows)]
#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[cfg(windows)]
mod gdi;
#[cfg(windows)]
mod shell;
#[cfg(windows)]
mod window;
#[cfg(windows)]
mod worker;

#[cfg(windows)]
fn main() {
    window::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "app-center-gui is a Win32 program and needs Windows or OTW. \
         On this system, use the `app-center` command line instead."
    );
    std::process::exit(1);
}
