//! The system call interface: how a process reaches its environment
//! subsystem.
//!
//! Imports that are not bound to genuine DLL code become thunks. When the
//! CPU reaches one, the kernel asks the process's [`SystemCalls`] how many
//! stack arguments the function takes, calls it with a view of the
//! process's memory, and completes the `stdcall` return. The kernel knows
//! nothing about the functions themselves; the Win32 subsystem implements
//! this trait. [`ImportPolicy`] lets the subsystem decide, at load time,
//! which imports it provides even when a genuine DLL exports them.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use super::{
    memory::{ApiOutcome, GuestMemory},
    pe::ImportName,
};

/// A process's environment subsystem.
pub trait SystemCalls {
    /// Number of stack arguments of an implemented import; `None` if the
    /// subsystem does not implement it.
    fn arg_count(&self, dll: &str, name: &ImportName) -> Option<usize>;

    /// Perform the call. `caller` is the return address. `None` reports a
    /// failure the process cannot continue from.
    fn call(
        &self,
        memory: &mut dyn GuestMemory,
        dll: &str,
        name: &ImportName,
        args: &Args,
        caller: u32,
    ) -> Option<ApiOutcome>;

    /// Number of arguments of an implemented import called from 64-bit
    /// code, where each argument is one 64-bit register or stack slot.
    fn arg_count64(&self, dll: &str, name: &ImportName) -> Option<usize> {
        self.arg_count(dll, name)
    }

    /// How to name an import in diagnostics.
    fn describe(&self, dll: &str, name: &ImportName) -> String {
        format!("{dll}!{}", name.display())
    }

    /// Machine code to run in place of a system call for a small function
    /// that needs nothing of the subsystem (a C library leaf such as
    /// `strlen`), in a 64-bit process: called millions of times, it is far
    /// faster run by the CPU than dispatched.
    fn native_x64(&self, _dll: &str, _name: &ImportName) -> Option<Native> {
        None
    }

    /// Whether thread `id` was created suspended and not yet resumed (a
    /// thread with a CPU of its own waits before it starts).
    fn thread_suspended(&self, _id: u32) -> bool {
        false
    }

    /// Thread `id`, which ran on a CPU of its own, ended.
    fn thread_exited(&self, _memory: &mut dyn GuestMemory, _id: u32, _code: u32) {}
}

/// The arguments of a call: one 64-bit value per argument from 64-bit
/// code (the x64 convention), one 32-bit value per stack slot from 32-bit
/// code. Dereferences to the 32-bit values (each argument's low half).
#[derive(Clone, Debug, Default)]
pub struct Args {
    wide: Vec<u64>,
    narrow: Vec<u32>,
    x64: bool,
}

impl Args {
    pub fn new(wide: Vec<u64>, x64: bool) -> Self {
        let narrow = wide.iter().map(|&value| value as u32).collect();
        Self { wide, narrow, x64 }
    }

    /// Arguments of a call from 32-bit code.
    pub fn narrow(values: &[u32]) -> Self {
        Self::new(values.iter().map(|&value| value as u64).collect(), false)
    }

    /// Whether the caller is 64-bit code.
    pub fn x64(&self) -> bool {
        self.x64
    }

    /// Argument `index` in full.
    pub fn wide(&self, index: usize) -> u64 {
        self.wide.get(index).copied().unwrap_or(0)
    }

    pub fn all_wide(&self) -> &[u64] {
        &self.wide
    }
}

impl std::ops::Deref for Args {
    type Target = [u32];

    fn deref(&self) -> &[u32] {
        &self.narrow
    }
}

/// How the loader binds imports.
pub trait ImportPolicy {
    /// Bind this import to the subsystem even when a mapped DLL exports it.
    fn prefers_subsystem(&self, dll: &str, name: &ImportName) -> bool;

    /// A variable the subsystem exports (not a function): the loader
    /// places it in the program's memory and points the import at it.
    fn data_import(&self, _dll: &str, _name: &ImportName) -> Option<DataImport> {
        None
    }
}

/// A variable a DLL exports, as the loader places it: one pointer-sized
/// value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataImport {
    /// This value.
    Value(u32),
    /// The command line (a pointer to it).
    CommandLine,
    /// The command line in UTF-16.
    WideCommandLine,
    /// `{ command line, NULL }`.
    Arguments,
    /// `{ wide command line, NULL }`.
    WideArguments,
    /// `{ NULL }`: an empty list of strings.
    EmptyList,
}

/// Bind every import to genuine DLL code when it is available.
pub struct GenuineFirst;

impl ImportPolicy for GenuineFirst {
    fn prefers_subsystem(&self, _dll: &str, _name: &ImportName) -> bool {
        false
    }
}

/// Machine code an import runs as in a 64-bit process (`native_x64`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Native {
    pub code: &'static [u8],
    /// The code ends with `jmp rel32` to the import's own system call (for
    /// what it cannot finish alone); the last four bytes are its
    /// displacement, filled in by [`placed`].
    pub slow: bool,
}

/// `native`'s code as placed at `at`, its slow path jumping to `thunk`.
pub fn placed(native: Native, at: u64, thunk: u64) -> Vec<u8> {
    let mut code = native.code.to_vec();
    if native.slow {
        let end = at + code.len() as u64;
        let displacement = thunk.wrapping_sub(end) as i64 as i32;
        let len = code.len();
        code[len - 4..].copy_from_slice(&displacement.to_le_bytes());
    }
    code
}

/// A 32-bit result as a 64-bit one: small negative values (error codes such
/// as -1) are sign-extended, everything else (addresses and handles below
/// 4 GiB, counts, flags) zero-extended.
pub fn widen(value: u32) -> u64 {
    if value >= 0xFFFF_0000 {
        value as i32 as i64 as u64
    } else {
        value as u64
    }
}
