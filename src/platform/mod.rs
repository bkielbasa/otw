//! What the library needs from the system it runs on.
//!
//! Every module of the library names this module `std`
//! (`use crate::platform as std;`), so the code reads as ordinary Rust.
//! With the `std` feature it *is* the standard library. Without it, it is
//! the part of the standard library's interface the library uses, built on
//! `core` and `alloc` and on what the bare-metal kernel provides through
//! the `System` trait (files, clock, log).

#[cfg(feature = "std")]
pub use ::std::*;

#[cfg(not(feature = "std"))]
mod bare;
#[cfg(not(feature = "std"))]
pub use bare::*;

/// Methods `std` adds to `core` types (floating point maths); empty with
/// `std`, which has them.
pub mod prelude {
    #[cfg(not(feature = "std"))]
    pub use super::bare::float::FloatExt;
    #[cfg(not(feature = "std"))]
    pub use alloc::{
        borrow::ToOwned,
        boxed::Box,
        string::{String, ToString},
        vec::Vec,
    };
}
