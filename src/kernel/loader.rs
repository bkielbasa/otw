//! Multi-module PE loader.
//!
//! A genuine MFC application imports thousands of ordinal-only exports from
//! MFC30.DLL. Reimplementing that framework in Rust is not practical, so the
//! loader maps the real Win95 DLLs into guest memory at their preferred bases
//! and resolves ordinal imports to the genuine code. Only imports that still
//! point outside the loaded module set receive an IAT thunk handled by the
//! Rust Win32 layers.

use std::{collections::HashMap, fmt};

use super::{
    iat::{Iat, IatEntry, THUNK_BASE, THUNK_STRIDE},
    pe::{ImportName, PeError, PeImage},
    syscall::ImportPolicy,
};

/// One PE image and its mapped guest memory, indexed from `image_base`.
#[derive(Clone, Debug)]
pub struct MappedModule {
    pub name: String,
    pub image: PeImage,
    pub memory: Vec<u8>,
}

impl MappedModule {
    fn lower_name(&self) -> String {
        self.name.to_ascii_lowercase()
    }
}

#[derive(Debug)]
pub enum LoadError {
    Pe(String, PeError),
    OverlappingImages(String, String),
    SectionOutsideImage(String, String),
    MissingSection(String, u32),
    UnknownDll(String),
    MissingExport(String, String),
    BadForwarder(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pe(name, error) => write!(f, "could not parse {name}: {error}"),
            Self::OverlappingImages(a, b) => {
                write!(f, "guest image bases overlap for {a} and {b}")
            }
            Self::SectionOutsideImage(module, section) => {
                write!(f, "section {section} does not fit in {module}")
            }
            Self::MissingSection(module, rva) => {
                write!(f, "RVA {rva:#010x} is not mapped in {module}")
            }
            Self::UnknownDll(dll) => write!(f, "no loaded module provides {dll}"),
            Self::MissingExport(dll, name) => write!(f, "{dll} has no export {name}"),
            Self::BadForwarder(text) => write!(f, "malformed export forwarder {text:?}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Result of mapping a module set and resolving every import.
#[derive(Clone, Debug)]
pub struct LoadResult {
    /// Module zero is the executable that starts execution.
    pub modules: Vec<MappedModule>,
    pub iat: Iat,
}

/// A named PE image to map. Names are matched case-insensitively against
/// import descriptor names.
pub fn load(
    inputs: Vec<(String, PeImage)>,
    policy: &dyn ImportPolicy,
) -> Result<LoadResult, LoadError> {
    let mut modules = Vec::with_capacity(inputs.len());
    // 64-bit DLLs linked above 4 GiB were all given the same low base when
    // parsed: lay them out one after another instead.
    let mut next_base = 0x1000_0000u32;
    for (name, mut image) in inputs {
        if image.is64 && image.preferred_base > u32::MAX as u64 && image.image_base != 0x0040_0000 {
            image.image_base = next_base;
            next_base = next_base + ((image.size_of_image + 0xFFFF) & !0xFFFF);
        }
        let memory = map_image(&name, &image)?;
        modules.push(MappedModule {
            name,
            image,
            memory,
        });
    }

    for (left, first) in modules.iter().enumerate() {
        let first_start = first.image.image_base as u64;
        let first_end = first_start + first.memory.len() as u64;
        for second in modules.iter().skip(left + 1) {
            let second_start = second.image.image_base as u64;
            let second_end = second_start + second.memory.len() as u64;
            if first_start < second_end && second_start < first_end {
                return Err(LoadError::OverlappingImages(
                    first.name.clone(),
                    second.name.clone(),
                ));
            }
        }
    }

    let by_name: HashMap<String, usize> = modules
        .iter()
        .enumerate()
        .map(|(index, module)| (module.lower_name(), index))
        .collect();

    let mut entries = Vec::new();
    let mut next_thunk = THUNK_BASE;
    for importer in 0..modules.len() {
        let imports = modules[importer].image.imports.clone();
        for dll in imports {
            let target = by_name.get(&dll.dll.to_ascii_lowercase()).copied();
            for (index, name) in dll.names.iter().enumerate() {
                let slot_size = if modules[importer].image.is64 { 8 } else { 4 };
                let slot_va = modules[importer]
                    .image
                    .image_base
                    .wrapping_add(dll.first_thunk_rva)
                    .wrapping_add(index as u32 * slot_size);

                let resolved = if policy.prefers_subsystem(&dll.dll, name) {
                    None
                } else {
                    target.and_then(|target| resolve_export(&modules, target, name, 0))
                };
                let value = match resolved {
                    Some(va) => va,
                    None => {
                        let thunk_va = next_thunk;
                        next_thunk = next_thunk.wrapping_add(THUNK_STRIDE);
                        entries.push(IatEntry {
                            dll: dll.dll.clone(),
                            name: name.clone(),
                            slot_va,
                            thunk_va,
                        });
                        thunk_va
                    }
                };
                write_slot(&mut modules, importer, slot_va, value)?;
            }
        }
    }

    Ok(LoadResult {
        modules,
        iat: Iat::from_entries(entries),
    })
}

/// Lay out a PE image's headers and sections as they appear in memory.
pub fn map_image(name: &str, image: &PeImage) -> Result<Vec<u8>, LoadError> {
    let size = image.size_of_image as usize;
    if size < image.size_of_headers as usize {
        return Err(LoadError::SectionOutsideImage(
            name.to_owned(),
            "headers".to_owned(),
        ));
    }
    let mut memory = vec![0u8; size];
    let header_bytes = (image.size_of_headers as usize).min(image.bytes.len());
    memory[..header_bytes].copy_from_slice(&image.bytes[..header_bytes]);
    for section in &image.sections {
        let available = image
            .bytes
            .len()
            .saturating_sub(section.raw_offset as usize)
            .min(section.raw_size as usize);
        if available == 0 {
            continue;
        }
        let start = section.virtual_address as usize;
        let end = start + available;
        if end > memory.len() {
            return Err(LoadError::SectionOutsideImage(
                name.to_owned(),
                section.name_string(),
            ));
        }
        let raw = section.raw_offset as usize;
        memory[start..end].copy_from_slice(&image.bytes[raw..raw + available]);
    }
    if image.is64 && image.preferred_base != image.image_base as u64 {
        let delta = (image.image_base as u64).wrapping_sub(image.preferred_base);
        relocate64(&mut memory, image.relocations, delta);
        // The header records the base the image now has.
        let pe = u32::from_le_bytes([memory[0x3C], memory[0x3D], memory[0x3E], memory[0x3F]]) as usize;
        if let Some(field) = memory.get_mut(pe + 24 + 24..pe + 24 + 32) {
            field.copy_from_slice(&(image.image_base as u64).to_le_bytes());
        }
    }
    Ok(memory)
}

/// Apply a 64-bit image's base relocations (DIR64 and HIGHLOW fixups) for
/// a move by `delta`.
pub fn relocate64(memory: &mut [u8], relocations: Option<(u32, u32)>, delta: u64) {
    let Some((rva, size)) = relocations else {
        return;
    };
    let read = |memory: &[u8], at: usize| memory.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let mut block = rva as usize;
    let end = (rva as usize + size as usize).min(memory.len());
    while block + 8 <= end {
        let (Some(page), Some(length)) = (read(memory, block), read(memory, block + 4)) else {
            break;
        };
        if length < 8 {
            break;
        }
        for entry in (block + 8..block + length as usize).step_by(2) {
            let Some(bytes) = memory.get(entry..entry + 2) else {
                break;
            };
            let value = u16::from_le_bytes([bytes[0], bytes[1]]);
            let at = page as usize + (value & 0xFFF) as usize;
            match value >> 12 {
                10 => {
                    if let Some(slot) = memory.get_mut(at..at + 8) {
                        let old = u64::from_le_bytes(slot.try_into().expect("8 bytes"));
                        slot.copy_from_slice(&old.wrapping_add(delta).to_le_bytes());
                    }
                }
                3 => {
                    if let Some(old) = read(memory, at) {
                        memory[at..at + 4].copy_from_slice(&old.wrapping_add(delta as u32).to_le_bytes());
                    }
                }
                _ => {}
            }
        }
        block += length as usize;
    }
}

fn write_slot(
    modules: &mut [MappedModule],
    importer: usize,
    slot_va: u32,
    value: u32,
) -> Result<(), LoadError> {
    let name = modules[importer].name.clone();
    let offset = slot_va
        .checked_sub(modules[importer].image.image_base)
        .ok_or_else(|| LoadError::MissingSection(name.clone(), slot_va))? as usize;
    // A 64-bit image's slots are 8 bytes; addresses stay below 4 GiB.
    let width = if modules[importer].image.is64 { 8 } else { 4 };
    let slot = modules[importer]
        .memory
        .get_mut(offset..offset + width)
        .ok_or_else(|| LoadError::MissingSection(name, slot_va))?;
    slot.copy_from_slice(&(value as u64).to_le_bytes()[..width]);
    Ok(())
}

fn resolve_export(
    modules: &[MappedModule],
    index: usize,
    name: &ImportName,
    depth: usize,
) -> Option<u32> {
    if depth > 8 {
        return None;
    }
    let directory = modules[index].image.export_directory.as_ref()?;
    let (rva, table_index) = match name {
        ImportName::ByName(name) => (directory.rva_for_name(name)?, *directory.names.get(name)?),
        ImportName::Ordinal(ordinal) => (
            directory.rva_for_ordinal(*ordinal)?,
            directory.index_for_ordinal(*ordinal)?,
        ),
    };
    if let Some(forwarder) = directory.forwarder_for_index(table_index) {
        let (dll, symbol) = forwarder.rsplit_once('.')?;
        if dll.is_empty() || symbol.is_empty() {
            return None;
        }
        let target = modules
            .iter()
            .position(|module| module.lower_name() == dll.to_ascii_lowercase())?;
        let symbol = match symbol.strip_prefix('#') {
            Some(ordinal) => ImportName::Ordinal(ordinal.parse().ok()?),
            None => ImportName::ByName(symbol.to_owned()),
        };
        return resolve_export(modules, target, &symbol, depth + 1);
    }
    Some(modules[index].image.image_base.wrapping_add(rva))
}

/// Apply base relocations to a mapped image moved by `delta` bytes from its
/// preferred base. Only 32-bit (HIGHLOW) fixups occur in 32-bit images.
pub fn relocate(memory: &mut [u8], relocations: Option<(u32, u32)>, delta: u32) {
    let Some((rva, size)) = relocations else {
        return;
    };
    let read = |memory: &[u8], at: usize| {
        memory
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let mut block = rva as usize;
    let end = (rva as usize + size as usize).min(memory.len());
    while block + 8 <= end {
        let (Some(page), Some(length)) = (read(memory, block), read(memory, block + 4)) else {
            break;
        };
        if length < 8 {
            break;
        }
        for entry in (block + 8..block + length as usize).step_by(2) {
            let Some(bytes) = memory.get(entry..entry + 2) else {
                break;
            };
            let value = u16::from_le_bytes([bytes[0], bytes[1]]);
            if value >> 12 != 3 {
                continue;
            }
            let at = page as usize + (value & 0xFFF) as usize;
            if let Some(old) = read(memory, at) {
                memory[at..at + 4].copy_from_slice(&old.wrapping_add(delta).to_le_bytes());
            }
        }
        block += length as usize;
    }
}
