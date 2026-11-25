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
        args: &[u32],
        caller: u32,
    ) -> Option<ApiOutcome>;

    /// Number of arguments of an implemented import called from 64-bit
    /// code, where each argument is one 64-bit register or stack slot.
    fn arg_count64(&self, dll: &str, name: &ImportName) -> Option<usize> {
        self.arg_count(dll, name)
    }

    /// Perform a call from 64-bit code (the x64 calling convention: the
    /// caller cleans the stack). The value is returned in RAX.
    fn call64(
        &self,
        memory: &mut dyn GuestMemory,
        dll: &str,
        name: &ImportName,
        args: &[u64],
        caller: u32,
    ) -> Option<(u64, Option<u32>)> {
        let narrow: Vec<u32> = args.iter().map(|&a| a as u32).collect();
        let outcome = self.call(memory, dll, name, &narrow, caller)?;
        Some((widen(outcome.value), outcome.exit_code))
    }

    /// How to name an import in diagnostics.
    fn describe(&self, dll: &str, name: &ImportName) -> String {
        format!("{dll}!{}", name.display())
    }
}

/// How the loader binds imports.
pub trait ImportPolicy {
    /// Bind this import to the subsystem even when a mapped DLL exports it.
    fn prefers_subsystem(&self, dll: &str, name: &ImportName) -> bool;
}

/// Bind every import to genuine DLL code when it is available.
pub struct GenuineFirst;

impl ImportPolicy for GenuineFirst {
    fn prefers_subsystem(&self, _dll: &str, _name: &ImportName) -> bool {
        false
    }
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
