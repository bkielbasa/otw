//! A process image: the program and its DLLs mapped at their preferred
//! bases, their imports bound to DLL code or to system-call thunks, the
//! subsystem's extra entry points, and the command line.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::fmt;

use super::{
    iat::{Iat, IatEntry, THUNK_BASE, THUNK_STRIDE},
    loader::{self, MappedModule},
    pe::{ImportName, PeImage},
    syscall::{DataImport, ImportPolicy},
};

/// Where the command line string is placed in the main module's memory,
/// unless the image itself reaches that far: then it goes on the page after
/// the image.
pub const COMMAND_LINE_VA: u32 = 0x0050_0000;

/// Pseudo-DLL name of entry points implemented by the kernel itself.
pub const KERNEL_DLL: &str = "OTW_KERNEL";
/// The user-mode exception dispatcher (NTDLL's `KiUserExceptionDispatcher`):
/// faulting and raising threads continue here with an `EXCEPTION_RECORD`
/// and `CONTEXT` on their stack.
pub const EXCEPTION_DISPATCHER: &str = "KiUserExceptionDispatcher";
/// Where a thread's start routine returns to: the thread ends.
pub const THREAD_RETURN: &str = "ThreadReturn";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    ImageTooSmall(u32),
    SectionOutsideImage(String),
    IatSlotOutsideImage(u32),
    CommandLineOutsideImage,
    Loader(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ImageTooSmall(size) => write!(f, "SizeOfImage {size:#x} is too small"),
            Self::SectionOutsideImage(name) => {
                write!(f, "section {name} does not fit in SizeOfImage")
            }
            Self::IatSlotOutsideImage(va) => write!(f, "IAT slot {va:#010x} is unmapped"),
            Self::CommandLineOutsideImage => write!(f, "command line does not fit in memory"),
            Self::Loader(message) => write!(f, "module loader failed: {message}"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Place the variables the subsystem exports that the program's modules
/// import (`ImportPolicy::data_import`) after the command line in the
/// main module's memory, and point their import slots at them.
fn place_data_imports(
    main: &mut MappedModule,
    entries: &[IatEntry],
    policy: &dyn ImportPolicy,
    command_line_va: u32,
    command_line: &str,
) -> Result<(), ImageError> {
    let wanted: Vec<(&IatEntry, DataImport)> = entries
        .iter()
        .filter(|e| e.slot_va != 0)
        .filter_map(|e| policy.data_import(&e.dll, &e.name).map(|d| (e, d)))
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let base = main.image.image_base;
    let pointer = if main.image.is64 { 8u32 } else { 4 };
    let mut at = (command_line_va + command_line.len() as u32 + 1 + 15) & !15;
    let mut put = |memory: &mut Vec<u8>, bytes: &[u8]| -> u32 {
        let va = at;
        let offset = (va - base) as usize;
        if memory.len() < offset + bytes.len() {
            memory.resize(offset + bytes.len(), 0);
        }
        memory[offset..offset + bytes.len()].copy_from_slice(bytes);
        at = (at + bytes.len() as u32 + 15) & !15;
        va
    };
    let word = |value: u32| -> Vec<u8> { (value as u64).to_le_bytes()[..pointer as usize].to_vec() };
    let wide: Vec<u8> = command_line.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()).collect();
    let wide_va = put(&mut main.memory, &wide);
    let list = |memory: &mut Vec<u8>, put: &mut dyn FnMut(&mut Vec<u8>, &[u8]) -> u32, first: Option<u32>| {
        let mut bytes = first.map(word).unwrap_or_default();
        bytes.extend(word(0));
        put(memory, &bytes)
    };
    for (entry, data) in wanted {
        let value = match data {
            DataImport::Value(value) => value,
            DataImport::CommandLine => command_line_va,
            DataImport::WideCommandLine => wide_va,
            DataImport::Arguments => list(&mut main.memory, &mut put, Some(command_line_va)),
            DataImport::WideArguments => list(&mut main.memory, &mut put, Some(wide_va)),
            DataImport::EmptyList => list(&mut main.memory, &mut put, None),
        };
        // Slots of the main module only (its DLLs keep their thunks).
        let Some(slot) = entry.slot_va.checked_sub(base).map(|s| s as usize).filter(|&s| s + (pointer as usize) <= main.image.size_of_image as usize) else {
            continue;
        };
        let variable = put(&mut main.memory, &word(value));
        main.memory[slot..slot + pointer as usize].copy_from_slice(&word(variable));
    }
    Ok(())
}

pub struct Image {
    /// Module zero is the executable that starts execution.
    pub modules: Vec<MappedModule>,
    pub iat: Iat,
    pub command_line_va: u32,
}

impl Image {
    /// Map `inputs` (executable first), bind imports with `policy`, add a
    /// thunk for each subsystem entry point `(dll, name)`, and place the
    /// command line.
    pub fn load(
        inputs: Vec<(String, PeImage)>,
        command_line: &str,
        policy: &dyn ImportPolicy,
        entry_points: &[(String, String)],
    ) -> Result<Self, ImageError> {
        let result =
            loader::load(inputs, policy).map_err(|error| ImageError::Loader(error.to_string()))?;
        let mut modules = result.modules;
        let image_base = modules[0].image.image_base;
        let image_end = image_base as usize + modules[0].memory.len();
        let command_line_va = (COMMAND_LINE_VA as usize).max((image_end + 0xFFF) & !0xFFF) as u32;
        let cmd_offset = command_line_va
            .checked_sub(image_base)
            .ok_or(ImageError::CommandLineOutsideImage)? as usize;
        let cmd_size = command_line.len() + 1;
        if modules[0].memory.len() < cmd_offset + cmd_size {
            modules[0].memory.resize(cmd_offset + cmd_size, 0);
        }
        modules[0].memory[cmd_offset..cmd_offset + cmd_size]
            .copy_from_slice(format!("{command_line}\0").as_bytes());

        let mut entries = result.iat.entries.clone();
        place_data_imports(&mut modules[0], &entries, policy, command_line_va, command_line)?;
        let kernel_entries = [
            (KERNEL_DLL.to_string(), EXCEPTION_DISPATCHER.to_string()),
            (KERNEL_DLL.to_string(), THREAD_RETURN.to_string()),
        ];
        for (dll, name) in entry_points.iter().chain(&kernel_entries) {
            let thunk_va = THUNK_BASE.wrapping_add(entries.len() as u32 * THUNK_STRIDE);
            entries.push(IatEntry {
                dll: dll.clone(),
                name: ImportName::ByName(name.clone()),
                slot_va: 0,
                thunk_va,
            });
        }
        Ok(Self {
            modules,
            iat: Iat::from_entries(entries),
            command_line_va,
        })
    }

    /// The executable.
    pub fn main(&self) -> &PeImage {
        &self.modules[0].image
    }

    /// Thunk address of an entry point.
    pub fn entry_point(&self, dll: &str, name: &str) -> Option<u32> {
        self.iat
            .entries
            .iter()
            .find(|entry| {
                entry.slot_va == 0 && entry.dll == dll && entry.name.as_str() == Some(name)
            })
            .map(|entry| entry.thunk_va)
    }

    pub fn read_u32(&self, va: u32) -> Option<u32> {
        for module in &self.modules {
            let offset = va.checked_sub(module.image.image_base)? as usize;
            if let Some(bytes) = module.memory.get(offset..offset.checked_add(4)?) {
                return Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
            }
        }
        None
    }

    pub fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
        for module in &mut self.modules {
            let offset = va.checked_sub(module.image.image_base)? as usize;
            if let Some(bytes) = module.memory.get_mut(offset..offset.checked_add(4)?) {
                bytes.copy_from_slice(&value.to_le_bytes());
                return Some(());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    struct Variables;

    impl ImportPolicy for Variables {
        fn prefers_subsystem(&self, _dll: &str, _name: &ImportName) -> bool {
            false
        }
        fn data_import(&self, dll: &str, name: &ImportName) -> Option<DataImport> {
            if dll != "MSVCRT.dll" {
                return None;
            }
            match name.as_str()? {
                "_acmdln" => Some(DataImport::CommandLine),
                "_wcmdln" => Some(DataImport::WideCommandLine),
                "__argv" => Some(DataImport::Arguments),
                "__argc" => Some(DataImport::Value(1)),
                _ => None,
            }
        }
    }

    fn read(memory: &[u8], base: u32, va: u32) -> u32 {
        let at = (va - base) as usize;
        u32::from_le_bytes(memory[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn data_imports_point_at_their_variables() {
        let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/binaries/guests/HELLO.EXE")).unwrap();
        let image = PeImage::parse(bytes).unwrap();
        let memory = loader::map_image("HELLO.EXE", &image).unwrap();
        let base = image.image_base;
        let mut main = MappedModule { name: "HELLO.EXE".into(), image, memory };
        let entry = |name: &str, slot: u32, thunk: u32| IatEntry {
            dll: "MSVCRT.dll".into(),
            name: ImportName::ByName(name.into()),
            slot_va: base + slot,
            thunk_va: thunk,
        };
        let entries = [
            entry("_acmdln", 0x100, 1),
            entry("_wcmdln", 0x104, 2),
            entry("__argv", 0x108, 3),
            entry("__argc", 0x10C, 4),
            entry("printf", 0x110, 5),
        ];
        let command_line_va = base + main.memory.len() as u32;
        let text = "\"C:\\SETUP.EXE\" /S";
        main.memory.resize(main.memory.len() + text.len() + 1, 0);
        let function_slot = read(&main.memory, base, base + 0x110);
        place_data_imports(&mut main, &entries, &Variables, command_line_va, text).unwrap();
        let m = &main.memory;
        assert_eq!(read(m, base, read(m, base, base + 0x100)), command_line_va);
        let wide = read(m, base, read(m, base, base + 0x104));
        let at = (wide - base) as usize;
        let units: Vec<u16> = m[at..at + 2 * text.len()].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16(&units).unwrap(), text);
        let argv = read(m, base, read(m, base, base + 0x108));
        assert_eq!((read(m, base, argv), read(m, base, argv + 4)), (command_line_va, 0));
        assert_eq!(read(m, base, read(m, base, base + 0x10C)), 1);
        // A function keeps its slot.
        assert_eq!(read(m, base, base + 0x110), function_slot);
    }
}
