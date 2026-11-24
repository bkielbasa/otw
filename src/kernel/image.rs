//! A process image: the program and its DLLs mapped at their preferred
//! bases, their imports bound to DLL code or to system-call thunks, the
//! subsystem's extra entry points, and the command line.

use std::fmt;

use super::{
    iat::{Iat, IatEntry, THUNK_BASE, THUNK_STRIDE},
    loader::{self, MappedModule},
    pe::{ImportName, PeImage},
    syscall::ImportPolicy,
};

/// Where the command line string is placed in the main module's memory.
pub const COMMAND_LINE_VA: u32 = 0x0050_0000;

/// Pseudo-DLL name of entry points implemented by the kernel itself.
pub const KERNEL_DLL: &str = "OTW_KERNEL";
/// The user-mode exception dispatcher (NTDLL's `KiUserExceptionDispatcher`):
/// faulting and raising threads continue here with an `EXCEPTION_RECORD`
/// and `CONTEXT` on their stack.
pub const EXCEPTION_DISPATCHER: &str = "KiUserExceptionDispatcher";

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
        let cmd_offset = COMMAND_LINE_VA
            .checked_sub(image_base)
            .ok_or(ImageError::CommandLineOutsideImage)? as usize;
        let cmd_size = command_line.len() + 1;
        if modules[0].memory.len() < cmd_offset + cmd_size {
            modules[0].memory.resize(cmd_offset + cmd_size, 0);
        }
        modules[0].memory[cmd_offset..cmd_offset + cmd_size]
            .copy_from_slice(format!("{command_line}\0").as_bytes());

        let mut entries = result.iat.entries.clone();
        let kernel_entries = [(KERNEL_DLL.to_string(), EXCEPTION_DISPATCHER.to_string())];
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
            command_line_va: COMMAND_LINE_VA,
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
