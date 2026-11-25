//! The OTW kernel.
//!
//! Everything needed to run a program that is independent of any API it
//! calls: PE images and their loading ([`pe`], [`loader`], [`iat`]), the CPU
//! and processes ([`cpu`]), scheduling ([`scheduler`]), the guest address
//! space ([`memory`]), and structured exception dispatch ([`eh`]).
//! The kernel does not know Win32: imports that are not bound to genuine DLL
//! code reach the environment subsystem through the system call interface.

pub mod context64;
pub mod cpu;
pub mod eh;
pub mod iat;
pub mod image;
pub mod loader;
pub mod memory;
pub mod pe;
pub mod scheduler;
pub mod syscall;
