//! The OTW kernel.
//!
//! Everything needed to run a program that is independent of any API it
//! calls: PE images and their loading ([`pe`], [`loader`], [`iat`]), the CPU
//! and processes ([`cpu`]), scheduling ([`scheduler`]), the guest address
//! space ([`memory`]), and structured exception dispatch ([`eh`]).
//! The kernel does not know Win32: imports that are not bound to genuine DLL
//! code reach the environment subsystem through the system call interface.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

pub mod context64;
#[cfg(feature = "std")]
pub mod cpu;
pub mod eh;
pub mod iat;
pub mod layout;
pub mod image;
pub mod loader;
pub mod lock;
pub mod memory;
pub mod pe;
#[cfg(feature = "std")]
pub mod scheduler;
#[cfg(feature = "std")]
pub mod space;
pub mod syscall;
