//! Minimal PE32 reader for the static loader skeleton.
//!
//! Only the pieces needed to map `NOTEPAD.EXE` and build its import table are
//! implemented: DOS/PE headers, section table, and the import directory. The
//! parser never executes the image.

use std::{collections::HashMap, fmt};

const DOS_HEADER_MIN: usize = 0x40;
const PE_SIGNATURE: &[u8; 4] = b"PE\0\0";
const COFF_HEADER_SIZE: usize = 20;
const SECTION_HEADER_SIZE: usize = 40;
const IMPORT_DESCRIPTOR_SIZE: usize = 20;
const MAX_IMPORT_DESCRIPTORS: usize = 64;
const MAX_NAMES_PER_DLL: usize = 4096;
const MAX_STRING_LEN: usize = 512;

/// Name of one imported function, either by symbol name or by ordinal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportName {
    ByName(String),
    Ordinal(u16),
}

impl ImportName {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            ImportName::ByName(name) => Some(name.as_str()),
            ImportName::Ordinal(_) => None,
        }
    }

    pub fn display(&self) -> String {
        match self {
            ImportName::ByName(name) => name.clone(),
            ImportName::Ordinal(ordinal) => format!("#{ordinal}"),
        }
    }
}

/// Imports from a single DLL.
#[derive(Clone, Debug)]
pub struct DllImport {
    pub dll: String,
    pub names: Vec<ImportName>,
    /// RVA of the IMAGE_IMPORT_BY_NAME lookup table.
    pub original_thunk_rva: u32,
    /// RVA of the IAT slots the loader must patch.
    pub first_thunk_rva: u32,
}

/// A parsed export directory. MFC30.DLL exports by ordinal only, so the
/// address table is the authoritative index even when the name table is empty.
#[derive(Clone, Debug, Default)]
pub struct ExportDirectory {
    pub ordinal_base: u32,
    /// Export address table entries; zero marks an unused ordinal.
    pub addresses: Vec<u32>,
    /// Named exports: name to index into `addresses`.
    pub names: HashMap<String, usize>,
    /// Forwarder strings, keyed by index into `addresses`.
    pub forwarders: HashMap<usize, String>,
    pub rva: u32,
    pub size: u32,
}

impl ExportDirectory {
    pub fn index_for_ordinal(&self, ordinal: u16) -> Option<usize> {
        let index = (ordinal as u32).checked_sub(self.ordinal_base)? as usize;
        (index < self.addresses.len()).then_some(index)
    }

    pub fn rva_for_ordinal(&self, ordinal: u16) -> Option<u32> {
        let rva = self
            .addresses
            .get(self.index_for_ordinal(ordinal)?)?
            .to_owned();
        (rva != 0).then_some(rva)
    }

    pub fn rva_for_name(&self, name: &str) -> Option<u32> {
        let rva = self.addresses.get(*self.names.get(name)?)?.to_owned();
        (rva != 0).then_some(rva)
    }

    pub fn forwarder_for_index(&self, index: usize) -> Option<&str> {
        self.forwarders.get(&index).map(String::as_str)
    }

    pub fn name_for_ordinal(&self, ordinal: u16) -> Option<&str> {
        let index = self.index_for_ordinal(ordinal)?;
        self.names
            .iter()
            .find(|(_, value)| **value == index)
            .map(|(name, _)| name.as_str())
    }
}

/// One mapped PE section.
#[derive(Clone, Debug)]
pub struct Section {
    pub name: [u8; 8],
    pub virtual_size: u32,
    pub virtual_address: u32,
    pub raw_size: u32,
    pub raw_offset: u32,
}

impl Section {
    pub fn name_string(&self) -> String {
        let end = self
            .name
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(self.name.len());
        String::from_utf8_lossy(&self.name[..end]).into_owned()
    }
}

/// Parsed PE32 image. The original bytes are retained so the caller can map
/// sections into emulator memory without re-reading the file.
#[derive(Clone, Debug)]
pub struct PeImage {
    pub bytes: Vec<u8>,
    pub machine: u16,
    pub image_base: u32,
    pub entry_rva: u32,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub sections: Vec<Section>,
    pub imports: Vec<DllImport>,
    pub export_directory: Option<ExportDirectory>,
    /// The base relocation directory (RVA, size), if the image has one.
    pub relocations: Option<(u32, u32)>,
}

impl PeImage {
    pub fn parse(bytes: Vec<u8>) -> Result<Self, PeError> {
        if bytes.len() < DOS_HEADER_MIN {
            return Err(PeError::TooShort(bytes.len()));
        }

        let pe_offset = read_u32(&bytes, 0x3c)? as usize;
        if bytes.get(pe_offset..pe_offset.saturating_add(4)) != Some(PE_SIGNATURE.as_slice()) {
            return Err(PeError::BadPeSignature);
        }

        let coff = pe_offset + 4;
        if coff + COFF_HEADER_SIZE > bytes.len() {
            return Err(PeError::TruncatedHeader("COFF"));
        }

        let machine = read_u16(&bytes, coff)?;
        let section_count = read_u16(&bytes, coff + 2)? as usize;
        let optional_size = read_u16(&bytes, coff + 16)? as usize;
        let optional = coff + COFF_HEADER_SIZE;
        if optional + optional_size > bytes.len() || optional_size < 96 {
            return Err(PeError::TruncatedHeader("optional"));
        }
        if read_u16(&bytes, optional)? != 0x10b {
            return Err(PeError::UnsupportedOptionalHeader(read_u16(
                &bytes, optional,
            )?));
        }

        let entry_rva = read_u32(&bytes, optional + 16)?;
        let image_base = read_u32(&bytes, optional + 28)?;
        let size_of_image = read_u32(&bytes, optional + 56)?;
        let size_of_headers = read_u32(&bytes, optional + 60)?;
        let directory_count = read_u32(&bytes, optional + 92)? as usize;
        let export_rva = if directory_count >= 1 {
            read_u32(&bytes, optional + 96)?
        } else {
            0
        };
        let export_size = if directory_count >= 1 {
            read_u32(&bytes, optional + 100)?
        } else {
            0
        };
        let import_rva = if directory_count >= 2 {
            read_u32(&bytes, optional + 96 + 8)?
        } else {
            0
        };

        let relocations = if directory_count >= 6 {
            let rva = read_u32(&bytes, optional + 96 + 5 * 8)?;
            let size = read_u32(&bytes, optional + 96 + 5 * 8 + 4)?;
            (rva != 0 && size != 0).then_some((rva, size))
        } else {
            None
        };

        let section_table = optional + optional_size;
        if section_table
            .checked_add(section_count.saturating_mul(SECTION_HEADER_SIZE))
            .map_or(true, |end| end > bytes.len())
        {
            return Err(PeError::TruncatedHeader("section table"));
        }

        let mut sections = Vec::with_capacity(section_count);
        for index in 0..section_count {
            let base = section_table + index * SECTION_HEADER_SIZE;
            let mut name = [0u8; 8];
            name.copy_from_slice(&bytes[base..base + 8]);
            sections.push(Section {
                name,
                virtual_size: read_u32(&bytes, base + 8)?,
                virtual_address: read_u32(&bytes, base + 12)?,
                raw_size: read_u32(&bytes, base + 16)?,
                raw_offset: read_u32(&bytes, base + 20)?,
            });
        }

        let mut image = Self {
            bytes,
            machine,
            image_base,
            entry_rva,
            size_of_image,
            size_of_headers,
            sections,
            imports: Vec::new(),
            export_directory: None,
            relocations,
        };
        if export_rva != 0 {
            image.export_directory = Some(image.parse_exports(export_rva, export_size)?);
        }
        if import_rva != 0 {
            image.imports = image.parse_imports(import_rva)?;
        }
        Ok(image)
    }

    fn parse_exports(&self, directory_rva: u32, size: u32) -> Result<ExportDirectory, PeError> {
        let offset = self
            .rva_to_offset(directory_rva)
            .ok_or(PeError::MissingSectionForRva(directory_rva))?;
        let ordinal_base = read_u32(&self.bytes, offset + 16)?;
        let function_count = read_u32(&self.bytes, offset + 20)? as usize;
        let name_count = read_u32(&self.bytes, offset + 24)? as usize;
        let address_table = read_u32(&self.bytes, offset + 28)?;
        let name_table = read_u32(&self.bytes, offset + 32)?;
        let ordinal_table = read_u32(&self.bytes, offset + 36)?;
        if function_count > MAX_NAMES_PER_DLL || name_count > MAX_NAMES_PER_DLL {
            return Err(PeError::TooManyExports);
        }

        let address_offset = self
            .rva_to_offset(address_table)
            .ok_or(PeError::MissingSectionForRva(address_table))?;
        let mut addresses = Vec::with_capacity(function_count);
        for index in 0..function_count {
            addresses.push(read_u32(&self.bytes, address_offset + index * 4)?);
        }

        let mut names = HashMap::new();
        if name_count != 0 && name_table != 0 && ordinal_table != 0 {
            let name_offset = self
                .rva_to_offset(name_table)
                .ok_or(PeError::MissingSectionForRva(name_table))?;
            let ordinal_offset = self
                .rva_to_offset(ordinal_table)
                .ok_or(PeError::MissingSectionForRva(ordinal_table))?;
            for index in 0..name_count {
                let string_rva = read_u32(&self.bytes, name_offset + index * 4)?;
                let ordinal_index = read_u16(&self.bytes, ordinal_offset + index * 2)? as usize;
                if ordinal_index >= function_count {
                    return Err(PeError::BadExportOrdinal);
                }
                let string_offset = self
                    .rva_to_offset(string_rva)
                    .ok_or(PeError::MissingSectionForRva(string_rva))?;
                names.insert(read_c_string(&self.bytes, string_offset)?, ordinal_index);
            }
        }

        // An export RVA inside the export directory is a forwarder string,
        // e.g. "NTDLL.RtlAllocateHeap" or "MFC30.#1234".
        let mut forwarders = HashMap::new();
        let directory_end = directory_rva as u64 + size as u64;
        for (index, rva) in addresses.iter().enumerate() {
            if *rva != 0 && (*rva as u64) >= directory_rva as u64 && (*rva as u64) < directory_end {
                let string_offset = self
                    .rva_to_offset(*rva)
                    .ok_or(PeError::MissingSectionForRva(*rva))?;
                forwarders.insert(index, read_c_string(&self.bytes, string_offset)?);
            }
        }

        Ok(ExportDirectory {
            ordinal_base,
            addresses,
            names,
            forwarders,
            rva: directory_rva,
            size,
        })
    }

    pub fn entry_va(&self) -> u32 {
        self.image_base.wrapping_add(self.entry_rva)
    }

    /// Translate an RVA to an offset in the original file bytes.
    pub fn rva_to_offset(&self, rva: u32) -> Option<usize> {
        if rva < self.size_of_headers {
            return (rva as usize <= self.bytes.len()).then_some(rva as usize);
        }
        for section in &self.sections {
            let span = section.virtual_size.max(section.raw_size);
            if rva >= section.virtual_address
                && (rva as u64) < (section.virtual_address as u64) + (span as u64)
            {
                let delta = rva - section.virtual_address;
                if delta >= section.raw_size {
                    return None;
                }
                let offset = (section.raw_offset as u64) + (delta as u64);
                return (offset <= self.bytes.len() as u64).then_some(offset as usize);
            }
        }
        None
    }

    /// Total number of imported functions across all DLLs.
    pub fn import_count(&self) -> usize {
        self.imports.iter().map(|dll| dll.names.len()).sum()
    }

    fn parse_imports(&self, directory_rva: u32) -> Result<Vec<DllImport>, PeError> {
        let mut offset = self
            .rva_to_offset(directory_rva)
            .ok_or(PeError::MissingSectionForRva(directory_rva))?;
        let mut result = Vec::new();

        for _ in 0..MAX_IMPORT_DESCRIPTORS {
            if offset + IMPORT_DESCRIPTOR_SIZE > self.bytes.len() {
                return Err(PeError::TruncatedImportTable);
            }
            let original_thunk = read_u32(&self.bytes, offset)?;
            let name_rva = read_u32(&self.bytes, offset + 12)?;
            let first_thunk = read_u32(&self.bytes, offset + 16)?;
            if original_thunk == 0 && name_rva == 0 && first_thunk == 0 {
                return Ok(result);
            }

            let name_offset = self
                .rva_to_offset(name_rva)
                .ok_or(PeError::MissingSectionForRva(name_rva))?;
            let dll = read_c_string(&self.bytes, name_offset)?;
            let thunk_rva = if original_thunk != 0 {
                original_thunk
            } else {
                first_thunk
            };
            let mut thunk_offset = self
                .rva_to_offset(thunk_rva)
                .ok_or(PeError::MissingSectionForRva(thunk_rva))?;
            let mut names = Vec::new();

            for _ in 0..MAX_NAMES_PER_DLL {
                let value = read_u32(&self.bytes, thunk_offset)?;
                if value == 0 {
                    break;
                }
                let import = if value & 0x8000_0000 != 0 {
                    ImportName::Ordinal((value & 0xffff) as u16)
                } else {
                    let hint_name_offset = self
                        .rva_to_offset(value)
                        .ok_or(PeError::MissingSectionForRva(value))?;
                    ImportName::ByName(read_c_string(&self.bytes, hint_name_offset + 2)?)
                };
                names.push(import);
                thunk_offset += 4;
            }
            result.push(DllImport {
                dll,
                names,
                original_thunk_rva: thunk_rva,
                first_thunk_rva: first_thunk,
            });
            offset += IMPORT_DESCRIPTOR_SIZE;
        }
        Err(PeError::TooManyImports)
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PeError> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or(PeError::TruncatedHeader("u16 read"))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PeError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(PeError::TruncatedHeader("u32 read"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_c_string(bytes: &[u8], offset: usize) -> Result<String, PeError> {
    if offset >= bytes.len() {
        return Err(PeError::MalformedString);
    }
    let tail = &bytes[offset..];
    let end = tail
        .iter()
        .position(|b| *b == 0)
        .ok_or(PeError::MalformedString)?;
    if end == 0 || end > MAX_STRING_LEN {
        return Err(PeError::MalformedString);
    }
    Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeError {
    TooShort(usize),
    BadPeSignature,
    UnsupportedOptionalHeader(u16),
    TruncatedHeader(&'static str),
    MissingSectionForRva(u32),
    TruncatedImportTable,
    MalformedString,
    TooManyImports,
    TooManyExports,
    BadExportOrdinal,
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeError::TooShort(len) => write!(f, "PE image is only {len} bytes"),
            PeError::BadPeSignature => write!(f, "missing PE\\0\\0 signature"),
            PeError::UnsupportedOptionalHeader(magic) => {
                write!(
                    f,
                    "unsupported optional-header magic 0x{magic:04x} (need PE32)"
                )
            }
            PeError::TruncatedHeader(part) => write!(f, "truncated {part} header"),
            PeError::MissingSectionForRva(rva) => write!(f, "RVA 0x{rva:08x} is not mapped"),
            PeError::TruncatedImportTable => write!(f, "truncated import descriptor table"),
            PeError::MalformedString => write!(f, "malformed import name string"),
            PeError::TooManyImports => write!(f, "import descriptor limit exceeded"),
            PeError::TooManyExports => write!(f, "export table limit exceeded"),
            PeError::BadExportOrdinal => write!(f, "export name ordinal is out of range"),
        }
    }
}

impl std::error::Error for PeError {}
