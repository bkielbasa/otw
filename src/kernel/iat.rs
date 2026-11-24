//! Import Address Table construction.
//!
//! Every import not bound to genuine DLL code receives a synthetic thunk
//! address. The CPU recognises a call to a thunk and turns it into a system
//! call (see [`super::syscall`]).

use std::collections::HashMap;

use super::pe::{ImportName, PeError, PeImage};

/// Synthetic thunk region. Well above the guest image and below typical
/// guest stack addresses used by Win95 programs.
pub const THUNK_BASE: u32 = 0x7000_0000;
pub const THUNK_STRIDE: u32 = 4;

#[derive(Clone, Debug)]
pub struct IatEntry {
    pub dll: String,
    pub name: ImportName,
    /// VA of the guest IAT slot that holds this thunk.
    pub slot_va: u32,
    /// Synthetic VA written into the slot and used as the dispatch key.
    pub thunk_va: u32,
}

#[derive(Clone, Debug)]
pub struct Iat {
    pub entries: Vec<IatEntry>,
    by_thunk: HashMap<u32, usize>,
}

impl Iat {
    pub fn build(image: &PeImage) -> Result<Self, PeError> {
        let mut entries = Vec::new();
        let mut by_thunk = HashMap::new();
        let mut next_thunk = THUNK_BASE;

        for dll in &image.imports {
            for (index, name) in dll.names.iter().enumerate() {
                let slot_rva = dll
                    .first_thunk_rva
                    .wrapping_add(index as u32 * THUNK_STRIDE);
                let slot_va = image.image_base.wrapping_add(slot_rva);
                let entry = IatEntry {
                    dll: dll.dll.clone(),
                    name: name.clone(),
                    slot_va,
                    thunk_va: next_thunk,
                };
                by_thunk.insert(next_thunk, entries.len());
                entries.push(entry);
                next_thunk = next_thunk.wrapping_add(THUNK_STRIDE);
            }
        }
        Ok(Self { entries, by_thunk })
    }

    /// Build an IAT from entries created by the multi-module loader.
    pub fn from_entries(entries: Vec<IatEntry>) -> Self {
        let by_thunk = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.thunk_va, index))
            .collect();
        Self { entries, by_thunk }
    }

    /// Number of genuine import slots (synthetic built-in thunks excluded).
    pub fn len(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.slot_va != 0)
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of thunk addresses, including synthetic built-in procedures.
    pub fn thunk_count(&self) -> usize {
        self.entries.len()
    }

    pub fn entry_for_thunk(&self, thunk_va: u32) -> Option<&IatEntry> {
        self.by_thunk
            .get(&thunk_va)
            .and_then(|index| self.entries.get(*index))
    }
}
