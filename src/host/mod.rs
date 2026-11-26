//! The host: the native window the emulated desktop is drawn in.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

#[cfg(feature = "display")]
pub mod display;
pub mod font;
