//! The state, layout and text of the graphical App Center
//! (`app-center-gui`), kept free of Win32 so that it can be unit-tested on
//! any system. The binary in `src/bin/app-center-gui` only paints what
//! these modules describe and feeds them input.

pub mod app;
pub mod format;
pub mod geom;
pub mod layout;
pub mod scroll;
pub mod text_input;
pub mod theme;
