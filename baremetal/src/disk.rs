//! Disks: what a driver offers (whole sectors), the partitions on them
//! (GPT), and a partition as a FAT32 volume, read through a cache.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

pub use otw_vm::fat32;

use crate::sync::SpinLock;

pub trait Disk: Send + Sync {
    fn name(&self) -> String;
    fn sector_size(&self) -> usize;
    fn sectors(&self) -> u64;
    /// Read `buffer.len() / sector_size` sectors from `lba`.
    fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str>;
}

pub struct Partition {
    pub disk: Arc<dyn Disk>,
    pub name: String,
    pub first_lba: u64,
    pub last_lba: u64,
}

/// The partitions in a disk's GPT.
pub fn partitions(disk: &Arc<dyn Disk>) -> Vec<Partition> {
    let size = disk.sector_size();
    let mut header = vec![0u8; size];
    if disk.read(1, &mut header).is_err() || &header[0..8] != b"EFI PART" {
        return Vec::new();
    }
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let entries_lba = u64_at(&header, 72);
    let count = u32_at(&header, 80).min(256) as usize;
    let entry_size = u32_at(&header, 84) as usize;
    if entry_size < 128 {
        return Vec::new();
    }
    let bytes = (count * entry_size).div_ceil(size) * size;
    let mut entries = vec![0u8; bytes];
    if disk.read(entries_lba, &mut entries).is_err() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for i in 0..count {
        let e = &entries[i * entry_size..(i + 1) * entry_size];
        if e[0..16].iter().all(|&b| b == 0) {
            continue;
        }
        let units: Vec<u16> = e[56..128].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        found.push(Partition {
            disk: disk.clone(),
            name: String::from_utf16_lossy(&units),
            first_lba: u64_at(e, 32),
            last_lba: u64_at(e, 40),
        });
    }
    found
}

const BLOCK: u64 = 64 * 1024;
const CACHE_BLOCKS: usize = 512; // 32 MiB

/// A partition as bytes, read a block at a time and kept in a cache.
pub struct PartitionVolume {
    disk: Arc<dyn Disk>,
    offset: u64,
    size: u64,
    cache: SpinLock<(BTreeMap<u64, (u64, Arc<Vec<u8>>)>, u64)>,
}

impl PartitionVolume {
    pub fn new(partition: &Partition) -> Self {
        let sector = partition.disk.sector_size() as u64;
        Self {
            disk: partition.disk.clone(),
            offset: partition.first_lba * sector,
            size: (partition.last_lba + 1 - partition.first_lba) * sector,
            cache: SpinLock::new((BTreeMap::new(), 0)),
        }
    }

    fn block(&self, index: u64) -> Result<Arc<Vec<u8>>, fat32::Fat32Error> {
        {
            let mut cache = self.cache.lock();
            cache.1 += 1;
            let clock = cache.1;
            if let Some(entry) = cache.0.get_mut(&index) {
                entry.0 = clock;
                return Ok(entry.1.clone());
            }
        }
        let sector = self.disk.sector_size() as u64;
        let start = index * BLOCK;
        let length = BLOCK.min(self.size - start).div_ceil(sector) * sector;
        let mut data = vec![0u8; length as usize];
        self.disk
            .read((self.offset + start) / sector, &mut data)
            .map_err(|_| fat32::Fat32Error::OutOfBounds(start, length as usize))?;
        let data = Arc::new(data);
        let mut cache = self.cache.lock();
        if cache.0.len() >= CACHE_BLOCKS {
            // Drop the least recently used block.
            if let Some(oldest) = cache.0.iter().min_by_key(|(_, (used, _))| *used).map(|(&k, _)| k) {
                cache.0.remove(&oldest);
            }
        }
        let clock = cache.1;
        cache.0.insert(index, (clock, data.clone()));
        Ok(data)
    }
}

impl fat32::Volume for PartitionVolume {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), fat32::Fat32Error> {
        if offset + buffer.len() as u64 > self.size {
            return Err(fat32::Fat32Error::OutOfBounds(offset, buffer.len()));
        }
        let mut done = 0;
        while done < buffer.len() {
            let at = offset + done as u64;
            let block = self.block(at / BLOCK)?;
            let within = (at % BLOCK) as usize;
            let n = (buffer.len() - done).min(block.len() - within);
            buffer[done..done + n].copy_from_slice(&block[within..within + n]);
            done += n;
        }
        Ok(())
    }
}

/// Memory for a device to read into or from: whole, aligned pages below
/// what the kernel maps (identity mapped, so its address is physical).
pub struct DmaBuffer {
    pub pointer: *mut u8,
    pub size: usize,
}

unsafe impl Send for DmaBuffer {}
unsafe impl Sync for DmaBuffer {}

impl DmaBuffer {
    pub fn new(size: usize) -> Self {
        let layout = core::alloc::Layout::from_size_align(size.max(4096), 4096).unwrap();
        let pointer = unsafe { alloc::alloc::alloc_zeroed(layout) };
        assert!(!pointer.is_null(), "out of memory for a device buffer");
        Self { pointer, size: size.max(4096) }
    }

    pub fn address(&self) -> u64 {
        self.pointer as u64
    }

    pub fn slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.pointer, self.size) }
    }
}
