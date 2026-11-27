//! FAT32: directories, files and their cluster chains, read and (on a
//! writable volume, `write`) written: files made, replaced, removed and
//! renamed, directories made, with long names.
//!
//! It needs only `core` and `alloc`: the bare-metal kernel (`baremetal/`)
//! builds it too, over a disk instead of an image in memory (`Volume`); its
//! drive C: keeps what programs write there.

mod write;

pub use write::MemoryVolume;

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use alloc::{
    collections::BTreeSet,
    format,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

/// Where a volume's bytes come from: an image in memory, or a disk.
pub trait Volume: Send + Sync {
    /// The volume's size in bytes.
    fn size(&self) -> u64;
    /// Fill `buffer` from `offset`.
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), Fat32Error>;
    /// Whether `write_at` can change it.
    fn writable(&self) -> bool {
        false
    }
    /// Put `data` at `offset`.
    fn write_at(&self, _offset: u64, _data: &[u8]) -> Result<(), Fat32Error> {
        Err(Fat32Error::ReadOnly)
    }
    /// Make what was written stay (a disk's write cache written out).
    fn flush(&self) -> Result<(), Fat32Error> {
        Ok(())
    }
}

impl Volume for Vec<u8> {
    fn size(&self) -> u64 {
        self.len() as u64
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), Fat32Error> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .filter(|&end| end <= self.len() as u64)
            .ok_or(Fat32Error::OutOfBounds(offset, buffer.len()))?;
        buffer.copy_from_slice(&self[offset as usize..end as usize]);
        Ok(())
    }
}

const DIR_ENTRY_SIZE: usize = 32;
const LFN_OFFSETS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fat32Error {
    ImageTooSmall,
    InvalidBootSector(String),
    OutOfBounds(u64, usize),
    Unsupported(String),
    InvalidCluster(u32),
    BrokenChain(u32),
    NotFound(String),
    NotADirectory(String),
    IsADirectory(String),
    InvalidFileSize(u32),
    ReadOnly,
    /// No free clusters left.
    Full,
    AlreadyExists(String),
    DirectoryNotEmpty(String),
    InvalidName(String),
    /// Larger than a FAT32 file can be (4 GiB - 1).
    TooLarge(u64),
    /// The disk under the volume failed.
    Io(&'static str),
}

impl core::fmt::Display for Fat32Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ImageTooSmall => write!(f, "FAT32 image is smaller than one boot sector"),
            Self::InvalidBootSector(message) => write!(f, "invalid FAT32 boot sector: {message}"),
            Self::OutOfBounds(offset, len) => {
                write!(
                    f,
                    "FAT32 read of {len} bytes at {offset:#x} is outside the image"
                )
            }
            Self::Unsupported(message) => write!(f, "unsupported FAT32 geometry: {message}"),
            Self::InvalidCluster(cluster) => write!(f, "invalid FAT32 cluster {cluster:#x}"),
            Self::BrokenChain(cluster) => write!(f, "broken FAT32 chain at cluster {cluster}"),
            Self::NotFound(path) => write!(f, "FAT32 path not found: {path}"),
            Self::NotADirectory(path) => write!(f, "FAT32 path is not a directory: {path}"),
            Self::IsADirectory(path) => write!(f, "FAT32 path is a directory: {path}"),
            Self::InvalidFileSize(size) => write!(f, "FAT32 file size {size} exceeds the image"),
            Self::ReadOnly => write!(f, "the FAT32 volume is read-only"),
            Self::Full => write!(f, "the FAT32 volume is full"),
            Self::AlreadyExists(path) => write!(f, "FAT32 path exists: {path}"),
            Self::DirectoryNotEmpty(path) => write!(f, "FAT32 directory is not empty: {path}"),
            Self::InvalidName(name) => write!(f, "not a valid FAT32 name: {name:?}"),
            Self::TooLarge(size) => write!(f, "{size} bytes is too large for a FAT32 file"),
            Self::Io(message) => write!(f, "disk error: {message}"),
        }
    }
}

impl core::error::Error for Fat32Error {}

/// One decoded directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fat32Entry {
    pub name: String,
    pub short_name: String,
    pub attributes: u8,
    pub cluster: u32,
    pub size: u32,
    /// When it was last written (Unix time; 0: not said).
    pub modified: u64,
}

impl Fat32Entry {
    pub fn is_directory(&self) -> bool {
        self.attributes & 0x10 != 0
    }
}

#[derive(Clone, Copy)]
struct LfnSlot {
    checksum: u8,
    chars: [u16; 13],
}

/// A parsed, read-only FAT32 volume. The volume is shared so host-side UI
/// can keep it alive without borrowing from a temporary buffer.
#[derive(Clone)]
pub struct Fat32FileSystem {
    volume: Arc<dyn Volume>,
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    fat_count: u32,
    fat_sectors: u32,
    total_sectors: u32,
    root_cluster: u32,
    data_start_sector: u32,
    cluster_count: u32,
    volume_label: String,
    /// The FSInfo sector (0: none).
    fsinfo_sector: u32,
    /// What writers share: one writes at a time.
    writer: Arc<std::sync::Mutex<write::WriteState>>,
    /// The time to put on what is written (Unix seconds).
    clock: fn() -> u64,
}

impl core::fmt::Debug for Fat32FileSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fat32FileSystem")
            .field("volume_label", &self.volume_label)
            .field("total_sectors", &self.total_sectors)
            .finish()
    }
}

impl Fat32FileSystem {
    /// A volume whose image is in memory.
    pub fn new(image: Vec<u8>) -> Result<Self, Fat32Error> {
        Self::from_volume(Arc::new(image))
    }

    pub fn from_volume(volume: Arc<dyn Volume>) -> Result<Self, Fat32Error> {
        if volume.size() < 512 {
            return Err(Fat32Error::ImageTooSmall);
        }
        let mut image = vec![0u8; 512];
        volume.read_at(0, &mut image)?;
        if image[510] != 0x55 || image[511] != 0xaa {
            return Err(Fat32Error::InvalidBootSector(
                "missing 0x55aa signature".to_string(),
            ));
        }

        let bytes_per_sector = le_u16(&image, 11)? as u32;
        if !(512..=4096).contains(&bytes_per_sector) || !bytes_per_sector.is_power_of_two() {
            return Err(Fat32Error::Unsupported(format!(
                "bytes per sector {bytes_per_sector}"
            )));
        }
        let sectors_per_cluster = *image.get(13).ok_or(Fat32Error::ImageTooSmall)? as u32;
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return Err(Fat32Error::Unsupported(format!(
                "sectors per cluster {sectors_per_cluster}"
            )));
        }
        let reserved_sectors = le_u16(&image, 14)? as u32;
        let fat_count = *image.get(16).ok_or(Fat32Error::ImageTooSmall)? as u32;
        let root_entries = le_u16(&image, 17)?;
        let total_sectors_16 = le_u16(&image, 19)? as u32;
        let fat_sectors_16 = le_u16(&image, 22)? as u32;
        let total_sectors = le_u32(&image, 32)?;
        let fat_sectors = le_u32(&image, 36)?;
        let root_cluster = le_u32(&image, 44)?;
        let fsinfo_sector = le_u16(&image, 48)? as u32;

        if reserved_sectors == 0 {
            return Err(Fat32Error::InvalidBootSector(
                "reserved sector count is zero".to_string(),
            ));
        }
        if fat_count == 0 {
            return Err(Fat32Error::InvalidBootSector(
                "FAT count is zero".to_string(),
            ));
        }
        if root_entries != 0 {
            return Err(Fat32Error::InvalidBootSector(
                "FAT32 must have zero root entries".to_string(),
            ));
        }
        if total_sectors == 0 && total_sectors_16 == 0 {
            return Err(Fat32Error::InvalidBootSector(
                "sector count is zero".to_string(),
            ));
        }
        let total_sectors = if total_sectors == 0 {
            total_sectors_16
        } else {
            total_sectors
        };
        let fat_sectors = if fat_sectors == 0 {
            fat_sectors_16
        } else {
            fat_sectors
        };
        if fat_sectors == 0 {
            return Err(Fat32Error::InvalidBootSector(
                "FAT size is zero".to_string(),
            ));
        }

        let data_start_sector =
            reserved_sectors
                .checked_add(fat_count.checked_mul(fat_sectors).ok_or_else(|| {
                    Fat32Error::InvalidBootSector("FAT region overflow".to_string())
                })?)
                .ok_or_else(|| Fat32Error::InvalidBootSector("data start overflow".to_string()))?;
        if data_start_sector >= total_sectors {
            return Err(Fat32Error::InvalidBootSector(
                "FAT region does not fit in the volume".to_string(),
            ));
        }
        let cluster_count = (total_sectors - data_start_sector) / sectors_per_cluster;
        if cluster_count == 0 {
            return Err(Fat32Error::InvalidBootSector(
                "volume has no data clusters".to_string(),
            ));
        }
        if root_cluster < 2 || root_cluster >= cluster_count + 2 {
            return Err(Fat32Error::InvalidBootSector(format!(
                "root cluster {root_cluster} is outside the data region"
            )));
        }

        let required_len = total_sectors as u64 * bytes_per_sector as u64;
        if volume.size() < required_len {
            return Err(Fat32Error::InvalidBootSector(format!(
                "image has {} bytes but the volume declares {required_len}",
                volume.size()
            )));
        }

        let label_bytes = image
            .get(71..82)
            .ok_or(Fat32Error::ImageTooSmall)?
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let volume_label = String::from_utf8_lossy(&label_bytes).trim_end().to_string();

        Ok(Self {
            volume,
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            fat_count,
            fat_sectors,
            total_sectors,
            root_cluster,
            data_start_sector,
            cluster_count,
            volume_label,
            fsinfo_sector: if fsinfo_sector < reserved_sectors { fsinfo_sector } else { 0 },
            writer: Arc::new(std::sync::Mutex::new(write::WriteState::default())),
            clock: || 0,
        })
    }

    /// Where the time put on what is written comes from (Unix seconds).
    pub fn set_clock(&mut self, clock: fn() -> u64) {
        self.clock = clock;
    }

    /// Whether files can be written.
    pub fn writable(&self) -> bool {
        self.volume.writable()
    }

    pub fn volume_label(&self) -> &str {
        &self.volume_label
    }

    pub fn bytes_per_sector(&self) -> u32 {
        self.bytes_per_sector
    }

    pub fn sectors_per_cluster(&self) -> u32 {
        self.sectors_per_cluster
    }

    pub fn total_sectors(&self) -> u32 {
        self.total_sectors
    }

    pub fn fat_count(&self) -> u32 {
        self.fat_count
    }

    pub fn fat_sectors(&self) -> u32 {
        self.fat_sectors
    }

    pub fn cluster_count(&self) -> u32 {
        self.cluster_count
    }

    pub fn root_cluster(&self) -> u32 {
        self.root_cluster
    }

    fn slice(&self, offset: u64, len: usize) -> Result<Vec<u8>, Fat32Error> {
        let mut buffer = vec![0u8; len];
        self.volume.read_at(offset, &mut buffer)?;
        Ok(buffer)
    }

    fn cluster_offset(&self, cluster: u32) -> Result<u64, Fat32Error> {
        if cluster < 2 || cluster >= self.cluster_count + 2 {
            return Err(Fat32Error::InvalidCluster(cluster));
        }
        let data_sector =
            self.data_start_sector as u64 + (cluster as u64 - 2) * self.sectors_per_cluster as u64;
        Ok(data_sector * self.bytes_per_sector as u64)
    }

    fn fat_entry(&self, cluster: u32) -> Result<u32, Fat32Error> {
        if cluster >= self.cluster_count + 2 {
            return Err(Fat32Error::InvalidCluster(cluster));
        }
        let offset =
            self.reserved_sectors as u64 * self.bytes_per_sector as u64 + cluster as u64 * 4;
        Ok(u32::from_le_bytes(self.slice(offset, 4)?[..4].try_into().unwrap()) & END_OF_CHAIN)
    }

    fn next_cluster(&self, cluster: u32) -> Result<Option<u32>, Fat32Error> {
        let next = self.fat_entry(cluster)?;
        // 0x0FFFFFF8 and up end a chain (0x0FFFFFF7: a bad cluster).
        Ok((2..0x0FFF_FFF7).contains(&next).then_some(next))
    }

    fn cluster_chain(&self, start: u32) -> Result<Vec<u32>, Fat32Error> {
        if start < 2 || start >= self.cluster_count + 2 {
            return Err(Fat32Error::InvalidCluster(start));
        }
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut current = start;
        loop {
            if current < 2 || current >= self.cluster_count + 2 {
                return Err(Fat32Error::BrokenChain(current));
            }
            if !seen.insert(current) {
                return Err(Fat32Error::BrokenChain(current));
            }
            chain.push(current);
            match self.next_cluster(current)? {
                Some(next) => current = next,
                None => return Ok(chain),
            }
        }
    }

    fn read_cluster(&self, cluster: u32) -> Result<Vec<u8>, Fat32Error> {
        let offset = self.cluster_offset(cluster)?;
        let len = self.sectors_per_cluster as usize * self.bytes_per_sector as usize;
        self.slice(offset, len)
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, Fat32Error> {
        let components = normalize_path(path);
        if components.is_empty() {
            return Err(Fat32Error::IsADirectory(path.to_string()));
        }
        let entry = self.resolve(&components)?;
        if entry.is_directory() {
            return Err(Fat32Error::IsADirectory(path.to_string()));
        }
        self.read_file_entry(&entry)
    }

    pub fn read_file_entry(&self, entry: &Fat32Entry) -> Result<Vec<u8>, Fat32Error> {
        if entry.is_directory() {
            return Err(Fat32Error::IsADirectory(entry.name.clone()));
        }
        let size = entry.size as usize;
        if size as u64 > self.volume.size() {
            return Err(Fat32Error::InvalidFileSize(entry.size));
        }
        let mut contents = Vec::with_capacity(size);
        if size == 0 {
            return Ok(contents);
        }
        for cluster in self.cluster_chain(entry.cluster)? {
            let cluster_data = self.read_cluster(cluster)?;
            let remaining = size - contents.len();
            if remaining == 0 {
                break;
            }
            let take = remaining.min(cluster_data.len());
            contents.extend_from_slice(&cluster_data[..take]);
        }
        if contents.len() != size {
            return Err(Fat32Error::BrokenChain(entry.cluster));
        }
        Ok(contents)
    }

    pub fn list(&self, path: &str) -> Result<Vec<Fat32Entry>, Fat32Error> {
        let components = normalize_path(path);
        let cluster = if components.is_empty() {
            self.root_cluster
        } else {
            let entry = self.resolve(&components)?;
            if !entry.is_directory() {
                return Err(Fat32Error::NotADirectory(path.to_string()));
            }
            entry.cluster
        };
        self.read_directory(cluster)
    }

    fn resolve(&self, components: &[String]) -> Result<Fat32Entry, Fat32Error> {
        let mut cluster = self.root_cluster;
        let mut found = Fat32Entry {
            name: "\\".to_string(),
            short_name: "\\".to_string(),
            attributes: 0x10,
            cluster,
            size: 0,
            modified: 0,
        };
        for (index, component) in components.iter().enumerate() {
            let entries = self.read_directory(cluster)?;
            let last = index + 1 == components.len();
            let Some(entry) = entries.iter().find(|entry| {
                entry.name.eq_ignore_ascii_case(component)
                    || entry.short_name.eq_ignore_ascii_case(component)
            }) else {
                return Err(Fat32Error::NotFound(component.clone()));
            };
            if !last && !entry.is_directory() {
                return Err(Fat32Error::NotADirectory(component.clone()));
            }
            found = entry.clone();
            cluster = entry.cluster;
        }
        Ok(found)
    }

    fn read_directory(&self, cluster: u32) -> Result<Vec<Fat32Entry>, Fat32Error> {
        Ok(self.read_directory_at(cluster)?.into_iter().map(|(entry, _)| entry).collect())
    }

    /// A directory's entries, each with where it is: the index of its
    /// short entry among the directory's 32-byte slots, and how many long
    /// name slots come before it.
    fn read_directory_at(&self, cluster: u32) -> Result<Vec<(Fat32Entry, write::Place)>, Fat32Error> {
        let mut entries = Vec::new();
        let mut slot = 0u32;
        let mut lfn: Vec<Option<LfnSlot>> = vec![None; 20];
        let mut lfn_len = 0usize;

        for cluster in self.cluster_chain(cluster)? {
            let data = self.read_cluster(cluster)?;
            for offset in (0..data.len()).step_by(DIR_ENTRY_SIZE) {
                if offset + DIR_ENTRY_SIZE > data.len() {
                    break;
                }
                let index = slot;
                slot += 1;
                let raw = &data[offset..offset + DIR_ENTRY_SIZE];
                match raw[0] {
                    0x00 => return Ok(entries),
                    0xe5 => {
                        lfn.clear();
                        lfn.resize(20, None);
                        lfn_len = 0;
                        continue;
                    }
                    _ => {}
                }
                if raw[11] == 0x0f {
                    let ordinal = raw[0] & 0x1f;
                    if ordinal == 0 || ordinal as usize > lfn.len() {
                        lfn.clear();
                        lfn.resize(20, None);
                        lfn_len = 0;
                        continue;
                    }
                    if raw[0] & 0x40 != 0 || ordinal as usize > lfn_len {
                        lfn.clear();
                        lfn.resize(20, None);
                        lfn_len = ordinal as usize;
                    }
                    let mut chars = [0xffffu16; 13];
                    for (index, &source) in LFN_OFFSETS.iter().enumerate() {
                        chars[index] = u16::from_le_bytes([raw[source], raw[source + 1]]);
                    }
                    lfn[ordinal as usize - 1] = Some(LfnSlot {
                        checksum: raw[13],
                        chars,
                    });
                    continue;
                }
                if raw[11] & 0x08 != 0 {
                    // Volume label: not a browsable file.
                    lfn.clear();
                    lfn.resize(20, None);
                    lfn_len = 0;
                    continue;
                }

                let short_name = decode_short_name(raw);
                let long = (lfn_len != 0).then(|| build_long_name(&lfn, lfn_len, &raw[0..11])).flatten();
                let lfn_slots = if long.is_some() { lfn_len as u32 } else { 0 };
                let name = long.unwrap_or_else(|| short_name.clone());
                lfn.clear();
                lfn.resize(20, None);
                lfn_len = 0;

                let cluster_low = u16::from_le_bytes([raw[26], raw[27]]) as u32;
                let cluster_high = u16::from_le_bytes([raw[20], raw[21]]) as u32;
                let modified = write::unix_time(
                    u16::from_le_bytes([raw[24], raw[25]]),
                    u16::from_le_bytes([raw[22], raw[23]]),
                );
                entries.push((
                    Fat32Entry {
                        name,
                        short_name,
                        attributes: raw[11],
                        cluster: cluster_low | (cluster_high << 16),
                        size: u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]),
                        modified,
                    },
                    write::Place { slot: index, lfn_slots },
                ));
            }
        }
        Ok(entries)
    }
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16, Fat32Error> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or(Fat32Error::ImageTooSmall)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<u32, Fat32Error> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(Fat32Error::ImageTooSmall)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn normalize_path(path: &str) -> Vec<String> {
    path.split(|ch| ch == '\\' || ch == '/')
        .filter(|part| !part.is_empty() && *part != ".")
        .fold(Vec::new(), |mut parts, part| {
            if part == ".." {
                parts.pop();
            } else {
                parts.push(part.to_string());
            }
            parts
        })
}

fn decode_short_name(raw: &[u8]) -> String {
    let mut name = [b' '; 11];
    name.copy_from_slice(&raw[0..11]);
    if name[0] == 0x05 {
        name[0] = 0xe5;
    }
    let nt_reserved = raw[12];
    let lower_base = nt_reserved & 0x08 != 0;
    let lower_ext = nt_reserved & 0x10 != 0;
    let base = trim_oem(&name[0..8], lower_base);
    let extension = trim_oem(&name[8..11], lower_ext);
    if extension.is_empty() {
        base
    } else {
        format!("{base}.{extension}")
    }
}

fn trim_oem(bytes: &[u8], lowercase: bool) -> String {
    let trimmed = bytes
        .iter()
        .rposition(|byte| *byte != b' ')
        .map(|last| &bytes[..=last])
        .unwrap_or(&[]);
    let mut value = String::from_utf8_lossy(trimmed).into_owned();
    if lowercase {
        value.make_ascii_lowercase();
    }
    value
}

fn short_name_checksum(name: &[u8]) -> u8 {
    let mut checksum = 0u8;
    for &byte in name {
        checksum = (((checksum & 1) << 7) | (checksum >> 1)).wrapping_add(byte);
    }
    checksum
}

fn build_long_name(slots: &[Option<LfnSlot>], len: usize, short_name: &[u8]) -> Option<String> {
    let checksum = short_name_checksum(short_name);
    let mut units = Vec::new();
    // Slot i holds characters 13 * i onwards.
    for slot in slots.iter().take(len) {
        let slot = slot.as_ref()?;
        if slot.checksum != checksum {
            return None;
        }
        for &unit in &slot.chars {
            if unit == 0x0000 || unit == 0xffff {
                break;
            }
            units.push(unit);
        }
    }
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    fn put_u16(image: &mut [u8], offset: usize, value: u16) {
        image[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(image: &mut [u8], offset: usize, value: u32) {
        image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn short_entry(
        image: &mut [u8],
        cluster: u32,
        index: usize,
        name: &[u8; 11],
        attributes: u8,
        entry_cluster: u32,
        size: u32,
    ) {
        let offset = cluster_offset_for_test(cluster) + index * DIR_ENTRY_SIZE;
        image[offset..offset + 11].copy_from_slice(name);
        image[offset + 11] = attributes;
        image[offset + 20..offset + 22]
            .copy_from_slice(&((entry_cluster >> 16) as u16).to_le_bytes());
        image[offset + 26..offset + 28].copy_from_slice(&(entry_cluster as u16).to_le_bytes());
        image[offset + 28..offset + 32].copy_from_slice(&size.to_le_bytes());
    }

    fn lfn_entry(
        image: &mut [u8],
        cluster: u32,
        index: usize,
        name: &str,
        short_name: &[u8; 11],
        ordinal: u8,
        total: u8,
    ) {
        let offset = cluster_offset_for_test(cluster) + index * DIR_ENTRY_SIZE;
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut chars = [0xffffu16; 13];
        for (slot, unit) in chars.iter_mut().zip(units.iter()) {
            *slot = *unit;
        }
        image[offset] = ordinal | if ordinal == total { 0x40 } else { 0 };
        image[offset + 11] = 0x0f;
        image[offset + 13] = short_name_checksum(short_name);
        for (index, &unit) in chars.iter().enumerate() {
            put_u16(image, offset + LFN_OFFSETS[index], unit);
        }
    }

    fn name11(text: &str) -> [u8; 11] {
        let mut name = [b' '; 11];
        let bytes = text.as_bytes();
        let len = bytes.len().min(11);
        name[..len].copy_from_slice(&bytes[..len]);
        name
    }

    fn cluster_offset_for_test(cluster: u32) -> usize {
        (12 * 512) + (cluster as usize - 2) * 512
    }

    fn fat_offset_for_test(fat: u32, cluster: u32) -> usize {
        (4 + fat as usize * 4) * 512 + cluster as usize * 4
    }

    fn set_fat(image: &mut [u8], cluster: u32, value: u32) {
        for fat in 0..2 {
            put_u32(image, fat_offset_for_test(fat, cluster), value);
        }
    }

    pub(super) fn test_image() -> Vec<u8> {
        let mut image = vec![0u8; 64 * 512];
        image[0..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
        image[3..11].copy_from_slice(b"OTWTEST ");
        put_u16(&mut image, 11, 512);
        image[13] = 1;
        put_u16(&mut image, 14, 4);
        image[16] = 2;
        put_u16(&mut image, 17, 0);
        put_u16(&mut image, 19, 0);
        image[21] = 0xf8;
        put_u16(&mut image, 22, 0);
        put_u32(&mut image, 32, 64);
        put_u32(&mut image, 36, 4);
        put_u32(&mut image, 44, 2);
        put_u16(&mut image, 48, 1);
        put_u16(&mut image, 50, 6);
        image[71..82].copy_from_slice(b"OTW DISK   ");
        image[82..90].copy_from_slice(b"FAT32   ");
        image[510] = 0x55;
        image[511] = 0xaa;

        put_u32(&mut image, 512, 0x41615252);
        put_u32(&mut image, 512 + 484, 0x61417272);
        put_u32(&mut image, 512 + 508, 0xaa550000);

        set_fat(&mut image, 0, 0x0fff_fff8);
        set_fat(&mut image, 1, 0x0fff_ffff);
        set_fat(&mut image, 2, 0x0fff_ffff);
        set_fat(&mut image, 3, 0x0fff_ffff);
        set_fat(&mut image, 4, 5);
        set_fat(&mut image, 5, 0x0fff_ffff);
        set_fat(&mut image, 6, 0x0fff_ffff);

        let dot = name11(".");
        let dotdot = name11("..");
        short_entry(&mut image, 2, 0, &dot, 0x10, 2, 0);
        short_entry(&mut image, 2, 1, &dotdot, 0x10, 2, 0);
        let docs_short = name11("DOCUMEN~1");
        lfn_entry(&mut image, 2, 2, "Documents", &docs_short, 1, 1);
        short_entry(&mut image, 2, 3, &docs_short, 0x10, 3, 0);
        let hello_short = name11("HELLO   TXT");
        short_entry(&mut image, 2, 4, &hello_short, 0x20, 4, 600);

        short_entry(&mut image, 3, 0, &dot, 0x10, 3, 0);
        short_entry(&mut image, 3, 1, &dotdot, 0x10, 2, 0);
        let readme_short = name11("README~1TXT");
        lfn_entry(&mut image, 3, 2, "readme.txt", &readme_short, 1, 1);
        short_entry(&mut image, 3, 3, &readme_short, 0x20, 6, 3);
        // A long name in two parts (the last part's entry comes first).
        let long_short = name11("ALONGF~1TXT");
        lfn_entry(&mut image, 3, 4, "ame.txt", &long_short, 2, 2);
        lfn_entry(&mut image, 3, 5, "a long file n", &long_short, 1, 2);
        short_entry(&mut image, 3, 6, &long_short, 0x20, 6, 3);

        let first: Vec<u8> = (0..512).map(|index| (index % 251) as u8).collect();
        let second: Vec<u8> = (0..88).map(|index| (index % 97) as u8).collect();
        image[cluster_offset_for_test(4)..cluster_offset_for_test(4) + 512].copy_from_slice(&first);
        image[cluster_offset_for_test(5)..cluster_offset_for_test(5) + 88].copy_from_slice(&second);
        image[cluster_offset_for_test(6)..cluster_offset_for_test(6) + 3].copy_from_slice(b"ok\n");

        image
    }

    #[test]
    fn long_names_in_several_entries() {
        let fs = Fat32FileSystem::new(test_image()).unwrap();
        let names: Vec<String> = fs.list("Documents").unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.iter().any(|n| n == "a long file name.txt"), "{names:?}");
        assert_eq!(fs.read_file("Documents/a long file name.txt").unwrap(), b"ok\n");
    }

    #[test]
    fn parses_boot_sector_geometry() {
        let fs = Fat32FileSystem::new(test_image()).unwrap();
        assert_eq!(fs.volume_label(), "OTW DISK");
        assert_eq!(fs.bytes_per_sector(), 512);
        assert_eq!(fs.sectors_per_cluster(), 1);
        assert_eq!(fs.total_sectors(), 64);
        assert_eq!(fs.cluster_count(), 52);
        assert_eq!(fs.root_cluster(), 2);
    }

    #[test]
    fn root_directory_decodes_short_and_long_names() {
        let fs = Fat32FileSystem::new(test_image()).unwrap();
        let entries = fs.list("/").unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, [".", "..", "Documents", "HELLO.TXT"]);
        assert!(entries[2].is_directory());
        assert_eq!(entries[3].size, 600);
        assert_eq!(entries[3].short_name, "HELLO.TXT");

        let docs = fs.list("\\Documents").unwrap();
        assert_eq!(docs[2].name, "readme.txt");
    }

    #[test]
    fn follows_a_multi_cluster_chain() {
        let fs = Fat32FileSystem::new(test_image()).unwrap();
        let contents = fs.read_file("/HELLO.TXT").unwrap();
        assert_eq!(contents.len(), 600);
        assert_eq!(contents[0], 0);
        assert_eq!(contents[511], 9);
        assert_eq!(contents[512], 0);
        assert_eq!(contents[599], 87);
        assert_eq!(fs.read_file("/Documents/readme.txt").unwrap(), b"ok\n");
    }

    #[test]
    fn normalizes_paths_and_reports_missing_entries() {
        let fs = Fat32FileSystem::new(test_image()).unwrap();
        assert_eq!(fs.list("/Documents/../").unwrap().len(), 4);
        assert_eq!(
            fs.list("/Missing"),
            Err(Fat32Error::NotFound("Missing".to_string()))
        );
        assert_eq!(
            fs.read_file("/Documents"),
            Err(Fat32Error::IsADirectory("/Documents".to_string()))
        );
        assert_eq!(
            fs.list("/HELLO.TXT"),
            Err(Fat32Error::NotADirectory("/HELLO.TXT".to_string()))
        );
    }

    #[test]
    fn rejects_truncated_or_invalid_images() {
        assert_eq!(
            Fat32FileSystem::new(vec![0u8; 100]).unwrap_err(),
            Fat32Error::ImageTooSmall
        );
        let mut image = test_image();
        image[510] = 0;
        assert!(matches!(
            Fat32FileSystem::new(image),
            Err(Fat32Error::InvalidBootSector(_))
        ));
    }

    #[test]
    fn any_end_of_chain_mark_ends_a_chain() {
        // mkfs.fat ends the root's chain with 0x0FFFFFF8.
        let mut image = test_image();
        set_fat(&mut image, 2, 0x0fff_fff8);
        let fs = Fat32FileSystem::new(image).unwrap();
        assert_eq!(fs.list("/").unwrap().len(), 4);
    }

    #[test]
    fn detects_a_cyclic_chain() {
        let mut image = test_image();
        set_fat(&mut image, 5, 4);
        let fs = Fat32FileSystem::new(image).unwrap();
        assert_eq!(fs.read_file("/HELLO.TXT"), Err(Fat32Error::BrokenChain(4)));
    }
}
