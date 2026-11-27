//! Writing a FAT32 volume: files made, replaced, removed and renamed,
//! directories made.
//!
//! Each change writes in an order that keeps the volume readable at every
//! step: a file's new clusters are filled and chained before its directory
//! entry points at them, and its old ones are freed only after. One writer
//! at a time (`WriteState`); readers do not wait.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use super::{
    normalize_path, short_name_checksum, Fat32Entry, Fat32Error, Fat32FileSystem, Volume,
    DIR_ENTRY_SIZE, END_OF_CHAIN, LFN_OFFSETS,
};

/// What writers share.
#[derive(Default)]
pub(super) struct WriteState {
    /// Where to look for a free cluster first.
    next_free: u32,
    /// Whether the FSInfo sector's free count was marked unknown (it is
    /// not kept up to date).
    fsinfo_cleared: bool,
}

/// Where an entry is in its directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Place {
    /// The index of its short entry among the directory's slots.
    pub slot: u32,
    /// How many long-name slots come right before it.
    pub lfn_slots: u32,
}

/// An image in memory that can be written (tests, or a volume built in
/// memory).
pub struct MemoryVolume(std::sync::Mutex<Vec<u8>>);

impl MemoryVolume {
    pub fn new(image: Vec<u8>) -> Self {
        Self(std::sync::Mutex::new(image))
    }

    /// The image as it is now.
    pub fn image(&self) -> Vec<u8> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

impl Volume for MemoryVolume {
    fn size(&self) -> u64 {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len() as u64
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), Fat32Error> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).read_at(offset, buffer)
    }

    fn writable(&self) -> bool {
        true
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), Fat32Error> {
        let mut image = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let end = offset
            .checked_add(data.len() as u64)
            .filter(|&end| end <= image.len() as u64)
            .ok_or(Fat32Error::OutOfBounds(offset, data.len()))?;
        image[offset as usize..end as usize].copy_from_slice(data);
        Ok(())
    }
}

/// Unix time of a FAT date and time (0 for none).
pub(super) fn unix_time(date: u16, time: u16) -> u64 {
    if date == 0 {
        return 0;
    }
    let year = 1980 + (date >> 9) as i64;
    let month = ((date >> 5) & 0xF).clamp(1, 12) as i64;
    let day = (date & 0x1F).max(1) as i64;
    // Days since 1970 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let seconds = (time >> 11) as i64 * 3600 + ((time >> 5) & 0x3F) as i64 * 60 + (time & 0x1F) as i64 * 2;
    (days * 86400 + seconds).max(0) as u64
}

/// The FAT date and time of a Unix time (1980-01-01 for earlier ones).
pub(super) fn fat_time(unix: u64) -> (u16, u16) {
    let days = (unix / 86400) as i64;
    let seconds = unix % 86400;
    // civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    if !(1980..=2107).contains(&year) {
        return (if year < 1980 { 0x21 } else { 0xFF9F }, 0);
    }
    let date = (((year - 1980) as u16) << 9) | ((month as u16) << 5) | day as u16;
    let time = (((seconds / 3600) as u16) << 11) | ((((seconds / 60) % 60) as u16) << 5) | ((seconds % 60) / 2) as u16;
    (date, time)
}

/// Characters a short name may hold besides letters and digits.
const SHORT_EXTRA: &[u8] = b"$%'-_@~`!(){}^#&";

fn short_char(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || SHORT_EXTRA.contains(&c)
}

/// A name as it is in a short entry, if it is one as given (upper case,
/// 8.3, nothing a short name cannot hold): it then needs no long name.
fn exact_short_name(name: &str) -> Option<[u8; 11]> {
    let bytes = name.as_bytes();
    let (base, extension) = match name.rfind('.') {
        Some(dot) => (&bytes[..dot], &bytes[dot + 1..]),
        None => (bytes, &[][..]),
    };
    if base.is_empty() || base.len() > 8 || extension.len() > 3 || (name.contains('.') && extension.is_empty()) {
        return None;
    }
    if !base.iter().chain(extension).all(|&c| short_char(c)) {
        return None;
    }
    let mut raw = [b' '; 11];
    raw[..base.len()].copy_from_slice(base);
    raw[8..8 + extension.len()].copy_from_slice(extension);
    Some(raw)
}

/// The short name for `name` that no entry in `taken` has: `BASE~N.EXT`.
fn generated_short_name(name: &str, taken: &[[u8; 11]]) -> Result<[u8; 11], Fat32Error> {
    let clean = |part: &str| -> Vec<u8> {
        part.chars()
            .filter(|c| *c != ' ' && *c != '.')
            .map(|c| {
                let c = c.to_ascii_uppercase();
                if c.is_ascii() && short_char(c as u8) {
                    c as u8
                } else {
                    b'_'
                }
            })
            .collect()
    };
    let trimmed = name.trim_start_matches('.');
    let (base, extension) = match trimmed.rfind('.') {
        Some(dot) => (clean(&trimmed[..dot]), clean(&trimmed[dot + 1..])),
        None => (clean(trimmed), Vec::new()),
    };
    let base = if base.is_empty() { b"FILE".to_vec() } else { base };
    for n in 1..1_000_000u32 {
        let tail = format!("~{n}");
        let keep = base.len().min(8 - tail.len());
        let mut raw = [b' '; 11];
        raw[..keep].copy_from_slice(&base[..keep]);
        raw[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
        let ext = extension.len().min(3);
        raw[8..8 + ext].copy_from_slice(&extension[..ext]);
        if !taken.contains(&raw) {
            return Ok(raw);
        }
    }
    Err(Fat32Error::Full)
}

fn valid_name(name: &str) -> Result<(), Fat32Error> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.encode_utf16().count() > 255
        || name.ends_with(' ')
        || name.chars().any(|c| (c as u32) < 0x20 || "\"*/:<>?\\|".contains(c));
    if bad {
        Err(Fat32Error::InvalidName(name.to_string()))
    } else {
        Ok(())
    }
}

/// The slots of an entry: its long name's (last part first), then the
/// short one (`short` holds all but the name).
fn entry_slots(name: &str, short_name: [u8; 11], short: &[u8; 32], needs_long: bool) -> Vec<[u8; 32]> {
    let mut slots = Vec::new();
    if needs_long {
        let units: Vec<u16> = name.encode_utf16().collect();
        let count = units.len().div_ceil(13);
        let checksum = short_name_checksum(&short_name);
        for ordinal in (1..=count).rev() {
            let mut slot = [0u8; 32];
            slot[0] = ordinal as u8 | if ordinal == count { 0x40 } else { 0 };
            slot[11] = 0x0F;
            slot[13] = checksum;
            for (i, &offset) in LFN_OFFSETS.iter().enumerate() {
                let at = (ordinal - 1) * 13 + i;
                let unit = match at.cmp(&units.len()) {
                    core::cmp::Ordering::Less => units[at],
                    core::cmp::Ordering::Equal => 0,
                    core::cmp::Ordering::Greater => 0xFFFF,
                };
                slot[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
            }
            slots.push(slot);
        }
    }
    let mut entry = *short;
    entry[..11].copy_from_slice(&short_name);
    slots.push(entry);
    slots
}

/// A short entry (all but its name) for a new file or directory.
fn new_short_entry(attributes: u8, cluster: u32, size: u32, (date, time): (u16, u16)) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[11] = attributes;
    entry[14..16].copy_from_slice(&time.to_le_bytes());
    entry[16..18].copy_from_slice(&date.to_le_bytes());
    entry[18..20].copy_from_slice(&date.to_le_bytes());
    entry[22..24].copy_from_slice(&time.to_le_bytes());
    entry[24..26].copy_from_slice(&date.to_le_bytes());
    set_entry_cluster(&mut entry, cluster);
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

fn set_entry_cluster(entry: &mut [u8], cluster: u32) {
    entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
}

/// An entry found by path, with what a change needs.
struct Found {
    entry: Fat32Entry,
    place: Place,
    /// The directory it is in.
    parent: u32,
}

impl Fat32FileSystem {
    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * self.bytes_per_sector as usize
    }

    fn lock_writer(&self) -> Result<std::sync::MutexGuard<'_, WriteState>, Fat32Error> {
        if !self.volume.writable() {
            return Err(Fat32Error::ReadOnly);
        }
        let mut state = self.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.fsinfo_cleared {
            state.fsinfo_cleared = true;
            // Free count and next free cluster: unknown (others count).
            if self.fsinfo_sector != 0 {
                let at = self.fsinfo_sector as u64 * self.bytes_per_sector as u64;
                let mut sector = vec![0u8; 512];
                self.volume.read_at(at, &mut sector)?;
                if sector[0..4] == 0x41615252u32.to_le_bytes() {
                    self.volume.write_at(at + 488, &[0xFF; 8])?;
                }
            }
        }
        Ok(state)
    }

    /// A directory's cluster by path components.
    fn directory_cluster(&self, components: &[String], path: &str) -> Result<u32, Fat32Error> {
        if components.is_empty() {
            return Ok(self.root_cluster);
        }
        let entry = self.resolve(components)?;
        if !entry.is_directory() {
            return Err(Fat32Error::NotADirectory(path.to_string()));
        }
        Ok(entry.cluster)
    }

    /// The entry named `name` in a directory, if there is one.
    fn find_in(&self, directory: u32, name: &str) -> Result<Option<Found>, Fat32Error> {
        Ok(self
            .read_directory_at(directory)?
            .into_iter()
            .find(|(e, _)| e.name != "." && e.name != ".." && (e.name.eq_ignore_ascii_case(name) || e.short_name.eq_ignore_ascii_case(name)))
            .map(|(entry, place)| Found { entry, place, parent: directory }))
    }

    /// The entry at `path` (not the root).
    fn find(&self, path: &str) -> Result<Found, Fat32Error> {
        let components = normalize_path(path);
        let Some((name, parents)) = components.split_last() else {
            return Err(Fat32Error::InvalidName(path.to_string()));
        };
        let parent = self.directory_cluster(parents, path)?;
        self.find_in(parent, name)?.ok_or_else(|| Fat32Error::NotFound(name.clone()))
    }

    // ── The FAT ──

    /// Set FAT entries (every copy), a sector run at a time.
    fn set_fat(&self, changes: &[(u32, u32)]) -> Result<(), Fat32Error> {
        let sector = self.bytes_per_sector as u64;
        let fat_start = self.reserved_sectors as u64 * sector;
        let mut sectors: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        for &(cluster, value) in changes {
            if cluster < 2 || cluster >= self.cluster_count + 2 {
                return Err(Fat32Error::InvalidCluster(cluster));
            }
            let byte = cluster as u64 * 4;
            let index = byte / sector;
            if !sectors.contains_key(&index) {
                let mut data = vec![0u8; sector as usize];
                self.volume.read_at(fat_start + index * sector, &mut data)?;
                sectors.insert(index, data);
            }
            let data = sectors.get_mut(&index).unwrap();
            let within = (byte % sector) as usize;
            let old = u32::from_le_bytes(data[within..within + 4].try_into().unwrap());
            // The top four bits are reserved: kept.
            let new = (old & 0xF000_0000) | (value & 0x0FFF_FFFF);
            data[within..within + 4].copy_from_slice(&new.to_le_bytes());
        }
        // Runs of neighbouring sectors, each written once to each FAT.
        let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
        for (index, data) in sectors {
            match runs.last_mut() {
                Some((first, bytes)) if *first + bytes.len() as u64 / sector == index => bytes.extend_from_slice(&data),
                _ => runs.push((index, data)),
            }
        }
        for fat in 0..self.fat_count as u64 {
            let base = fat_start + fat * self.fat_sectors as u64 * sector;
            for (index, bytes) in &runs {
                self.volume.write_at(base + index * sector, bytes)?;
            }
        }
        Ok(())
    }

    /// `count` free clusters (not yet marked used).
    fn free_clusters(&self, state: &mut WriteState, count: usize) -> Result<Vec<u32>, Fat32Error> {
        let mut found = Vec::with_capacity(count);
        if count == 0 {
            return Ok(found);
        }
        let last = self.cluster_count + 2;
        let start = state.next_free.clamp(2, last - 1);
        let fat_start = self.reserved_sectors as u64 * self.bytes_per_sector as u64;
        const CHUNK: u32 = 16 * 1024;
        // From the hint to the end, then from the start to the hint.
        for (from, to) in [(start, last), (2, start)] {
            let mut cluster = from;
            while cluster < to {
                let n = CHUNK.min(to - cluster);
                let mut data = vec![0u8; n as usize * 4];
                self.volume.read_at(fat_start + cluster as u64 * 4, &mut data)?;
                for (i, entry) in data.chunks_exact(4).enumerate() {
                    if u32::from_le_bytes(entry.try_into().unwrap()) & 0x0FFF_FFFF == 0 {
                        found.push(cluster + i as u32);
                        if found.len() == count {
                            state.next_free = cluster + i as u32 + 1;
                            return Ok(found);
                        }
                    }
                }
                cluster += n;
            }
        }
        Err(Fat32Error::Full)
    }

    /// Mark clusters as a chain, in order.
    fn chain(&self, clusters: &[u32]) -> Result<(), Fat32Error> {
        let changes: Vec<(u32, u32)> = clusters
            .iter()
            .enumerate()
            .map(|(i, &c)| (c, clusters.get(i + 1).copied().unwrap_or(END_OF_CHAIN)))
            .collect();
        self.set_fat(&changes)
    }

    /// Give a chain's clusters back.
    fn free_chain(&self, state: &mut WriteState, first: u32) -> Result<(), Fat32Error> {
        if first < 2 {
            return Ok(());
        }
        let clusters = self.cluster_chain(first)?;
        let changes: Vec<(u32, u32)> = clusters.iter().map(|&c| (c, 0)).collect();
        self.set_fat(&changes)?;
        if let Some(&lowest) = clusters.iter().min() {
            state.next_free = state.next_free.min(lowest);
        }
        Ok(())
    }

    /// Write `data` over clusters, a run of neighbouring ones at a time.
    fn write_clusters(&self, clusters: &[u32], data: &[u8]) -> Result<(), Fat32Error> {
        let size = self.cluster_bytes();
        let mut i = 0;
        while i < clusters.len() {
            let mut j = i + 1;
            while j < clusters.len() && clusters[j] == clusters[j - 1] + 1 {
                j += 1;
            }
            let start = (i * size).min(data.len());
            let end = (j * size).min(data.len());
            if start < end {
                self.volume.write_at(self.cluster_offset(clusters[i])?, &data[start..end])?;
            }
            i = j;
        }
        Ok(())
    }

    /// New clusters holding `data` (none for no data), chained.
    fn store(&self, state: &mut WriteState, data: &[u8]) -> Result<u32, Fat32Error> {
        if data.len() as u64 > u32::MAX as u64 {
            return Err(Fat32Error::TooLarge(data.len() as u64));
        }
        let clusters = self.free_clusters(state, data.len().div_ceil(self.cluster_bytes()))?;
        self.write_clusters(&clusters, data)?;
        self.chain(&clusters)?;
        Ok(clusters.first().copied().unwrap_or(0))
    }

    // ── Directory slots ──

    fn slot_offset(&self, chain: &[u32], slot: u32) -> Result<u64, Fat32Error> {
        let per_cluster = (self.cluster_bytes() / DIR_ENTRY_SIZE) as u32;
        let cluster = *chain.get((slot / per_cluster) as usize).ok_or(Fat32Error::BrokenChain(slot))?;
        Ok(self.cluster_offset(cluster)? + (slot % per_cluster) as u64 * DIR_ENTRY_SIZE as u64)
    }

    /// Write consecutive slots from `first`.
    fn write_slots(&self, chain: &[u32], first: u32, slots: &[[u8; 32]]) -> Result<(), Fat32Error> {
        let per_cluster = (self.cluster_bytes() / DIR_ENTRY_SIZE) as u32;
        let mut i = 0usize;
        while i < slots.len() {
            let slot = first + i as u32;
            // As many as are left in this cluster, in one write.
            let n = ((per_cluster - slot % per_cluster) as usize).min(slots.len() - i);
            let bytes: Vec<u8> = slots[i..i + n].iter().flatten().copied().collect();
            self.volume.write_at(self.slot_offset(chain, slot)?, &bytes)?;
            i += n;
        }
        Ok(())
    }

    fn read_slot(&self, chain: &[u32], slot: u32) -> Result<[u8; 32], Fat32Error> {
        let mut raw = [0u8; 32];
        self.volume.read_at(self.slot_offset(chain, slot)?, &mut raw)?;
        Ok(raw)
    }

    /// Mark an entry's slots (its long name's too) deleted.
    fn delete_slots(&self, directory: u32, place: Place) -> Result<(), Fat32Error> {
        let chain = self.cluster_chain(directory)?;
        for slot in place.slot - place.lfn_slots..=place.slot {
            self.volume.write_at(self.slot_offset(&chain, slot)?, &[0xE5])?;
        }
        Ok(())
    }

    /// Add an entry named `name` to a directory (`short`: the short
    /// entry's fields but its name), growing the directory if it is full.
    fn add_entry(&self, state: &mut WriteState, directory: u32, name: &str, short: &[u8; 32]) -> Result<(), Fat32Error> {
        valid_name(name)?;
        let mut chain = self.cluster_chain(directory)?;
        let per_cluster = self.cluster_bytes() / DIR_ENTRY_SIZE;
        // Every slot's first byte and the short names taken.
        let mut firsts = Vec::with_capacity(chain.len() * per_cluster);
        let mut taken = Vec::new();
        let mut end = None;
        for &cluster in &chain {
            let data = self.read_cluster(cluster)?;
            for raw in data.chunks_exact(DIR_ENTRY_SIZE) {
                if end.is_none() && raw[0] == 0 {
                    end = Some(firsts.len());
                }
                if end.is_none() && raw[0] != 0xE5 && raw[11] != 0x0F {
                    taken.push(<[u8; 11]>::try_from(&raw[..11]).unwrap());
                }
                firsts.push(raw[0]);
            }
        }
        let end = end.unwrap_or(firsts.len());
        let (short_name, needs_long) = match exact_short_name(name) {
            Some(raw) if !taken.contains(&raw) => (raw, false),
            _ => (generated_short_name(name, &taken)?, true),
        };
        let slots = entry_slots(name, short_name, short, needs_long);
        // The first run of free slots long enough: deleted ones before the
        // end, or from the end on.
        let free = |i: usize| i >= end || firsts[i] == 0xE5;
        let mut start = None;
        let mut run = 0;
        for i in 0..firsts.len() {
            if free(i) {
                run += 1;
                if run == slots.len() {
                    start = Some(i + 1 - run);
                    break;
                }
            } else {
                run = 0;
            }
        }
        let start = match start {
            Some(start) => start,
            None => {
                // Grow by zeroed clusters; the run goes on into them.
                let start = firsts.len() - run;
                let needed = (start + slots.len()).div_ceil(per_cluster) - chain.len();
                let new = self.free_clusters(state, needed)?;
                let zero = vec![0u8; self.cluster_bytes()];
                for &cluster in &new {
                    self.volume.write_at(self.cluster_offset(cluster)?, &zero)?;
                }
                self.chain(&new)?;
                self.set_fat(&[(*chain.last().unwrap(), new[0])])?;
                firsts.extend(core::iter::repeat_n(0, new.len() * per_cluster));
                chain.extend(new);
                start
            }
        };
        let after = start + slots.len();
        self.write_slots(&chain, start as u32, &slots)?;
        // The entry took the end's place: the slot after it ends the
        // directory now.
        if after > end && after < firsts.len() && firsts[after] != 0 {
            self.volume.write_at(self.slot_offset(&chain, after as u32)?, &[0])?;
        }
        Ok(())
    }

    // ── What programs do ──

    /// Make the file at `path` hold `data` (made, or replaced).
    pub fn write_file(&self, path: &str, data: &[u8]) -> Result<(), Fat32Error> {
        let mut state = self.lock_writer()?;
        let components = normalize_path(path);
        let Some((name, parents)) = components.split_last() else {
            return Err(Fat32Error::IsADirectory(path.to_string()));
        };
        valid_name(name)?;
        let parent = self.directory_cluster(parents, path)?;
        let existing = self.find_in(parent, name)?;
        if existing.as_ref().is_some_and(|f| f.entry.is_directory()) {
            return Err(Fat32Error::IsADirectory(path.to_string()));
        }
        let first = self.store(&mut state, data)?;
        let now = fat_time((self.clock)());
        match existing {
            Some(found) => {
                let chain = self.cluster_chain(parent)?;
                let mut short = self.read_slot(&chain, found.place.slot)?;
                set_entry_cluster(&mut short, first);
                short[28..32].copy_from_slice(&(data.len() as u32).to_le_bytes());
                short[22..24].copy_from_slice(&now.1.to_le_bytes());
                short[24..26].copy_from_slice(&now.0.to_le_bytes());
                short[18..20].copy_from_slice(&now.0.to_le_bytes());
                short[11] |= 0x20; // archive: changed
                self.write_slots(&chain, found.place.slot, &[short])?;
                self.free_chain(&mut state, found.entry.cluster)?;
            }
            None => {
                let short = new_short_entry(0x20, first, data.len() as u32, now);
                self.add_entry(&mut state, parent, name, &short)?;
            }
        }
        self.volume.flush()
    }

    /// Make a directory at `path` (its parent must be there).
    pub fn create_dir(&self, path: &str) -> Result<(), Fat32Error> {
        let mut state = self.lock_writer()?;
        let components = normalize_path(path);
        let Some((name, parents)) = components.split_last() else {
            return Err(Fat32Error::AlreadyExists(path.to_string()));
        };
        valid_name(name)?;
        let parent = self.directory_cluster(parents, path)?;
        if self.find_in(parent, name)?.is_some() {
            return Err(Fat32Error::AlreadyExists(path.to_string()));
        }
        let now = fat_time((self.clock)());
        let cluster = self.free_clusters(&mut state, 1)?[0];
        let mut data = vec![0u8; self.cluster_bytes()];
        let mut dot = new_short_entry(0x10, cluster, 0, now);
        dot[..11].copy_from_slice(b".          ");
        // ".." of a directory in the root says cluster 0.
        let mut dotdot = new_short_entry(0x10, if parent == self.root_cluster { 0 } else { parent }, 0, now);
        dotdot[..11].copy_from_slice(b"..         ");
        data[..32].copy_from_slice(&dot);
        data[32..64].copy_from_slice(&dotdot);
        self.volume.write_at(self.cluster_offset(cluster)?, &data)?;
        self.chain(&[cluster])?;
        let short = new_short_entry(0x10, cluster, 0, now);
        self.add_entry(&mut state, parent, name, &short)?;
        self.volume.flush()
    }

    /// Remove the file or empty directory at `path`.
    pub fn remove(&self, path: &str) -> Result<(), Fat32Error> {
        let mut state = self.lock_writer()?;
        let found = self.find(path)?;
        if found.entry.is_directory() && found.entry.cluster >= 2 {
            let inside = self.read_directory(found.entry.cluster)?;
            if inside.iter().any(|e| e.name != "." && e.name != "..") {
                return Err(Fat32Error::DirectoryNotEmpty(path.to_string()));
            }
        }
        self.delete_slots(found.parent, found.place)?;
        self.free_chain(&mut state, found.entry.cluster)?;
        self.volume.flush()
    }

    /// Move or rename `from` to `to`, replacing a file there.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), Fat32Error> {
        let mut state = self.lock_writer()?;
        let source = self.find(from)?;
        let to_components = normalize_path(to);
        let Some((name, parents)) = to_components.split_last() else {
            return Err(Fat32Error::AlreadyExists(to.to_string()));
        };
        valid_name(name)?;
        let from_components = normalize_path(from);
        let inside_itself = source.entry.is_directory()
            && to_components.len() > from_components.len()
            && from_components.iter().zip(&to_components).all(|(a, b)| a.eq_ignore_ascii_case(b));
        if inside_itself {
            return Err(Fat32Error::InvalidName(to.to_string()));
        }
        let parent = self.directory_cluster(parents, to)?;
        if let Some(target) = self.find_in(parent, name)? {
            let same = target.parent == source.parent && target.place == source.place;
            if !same {
                if target.entry.is_directory() || source.entry.is_directory() {
                    return Err(Fat32Error::AlreadyExists(to.to_string()));
                }
                self.delete_slots(target.parent, target.place)?;
                self.free_chain(&mut state, target.entry.cluster)?;
            }
        }
        let source_chain = self.cluster_chain(source.parent)?;
        let short = self.read_slot(&source_chain, source.place.slot)?;
        self.add_entry(&mut state, parent, name, &short)?;
        // The old entry's slots may have moved in the directory if it grew;
        // they stay where they were (directories only grow at the end).
        self.delete_slots(source.parent, source.place)?;
        if source.entry.is_directory() && parent != source.parent && source.entry.cluster >= 2 {
            let chain = self.cluster_chain(source.entry.cluster)?;
            let mut dotdot = self.read_slot(&chain, 1)?;
            if &dotdot[..2] == b".." {
                set_entry_cluster(&mut dotdot, if parent == self.root_cluster { 0 } else { parent });
                self.write_slots(&chain, 1, &[dotdot])?;
            }
        }
        self.volume.flush()
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;

    use super::super::tests::test_image;
    use super::*;
    use alloc::sync::Arc;

    fn writable(image: Vec<u8>) -> (Arc<MemoryVolume>, Fat32FileSystem) {
        let volume = Arc::new(MemoryVolume::new(image));
        let fs = Fat32FileSystem::from_volume(volume.clone()).unwrap();
        (volume, fs)
    }

    fn names(fs: &Fat32FileSystem, path: &str) -> Vec<String> {
        fs.list(path).unwrap().into_iter().map(|e| e.name).filter(|n| n != "." && n != "..").collect()
    }

    #[test]
    fn fat_times_round_trip() {
        // 2026-11-27 13:45:58 UTC.
        let unix = 1_795_787_158;
        let (date, time) = fat_time(unix);
        assert_eq!(date >> 9, 46);
        assert_eq!((date >> 5) & 0xF, 11);
        assert_eq!(date & 0x1F, 27);
        assert_eq!(unix_time(date, time), unix);
        assert_eq!(fat_time(0), (0x21, 0));
        assert_eq!(unix_time(0x21, 0), 315_532_800);
    }

    #[test]
    fn short_names() {
        assert_eq!(&exact_short_name("README.TXT").unwrap(), b"README  TXT");
        assert!(exact_short_name("readme.txt").is_none());
        assert!(exact_short_name("LONGERNAME.TXT").is_none());
        assert!(exact_short_name("A.B.C").is_none());
        let taken = [*b"FIREFO~1EXE"];
        assert_eq!(&generated_short_name("Firefox Setup 140.exe", &taken).unwrap(), b"FIREFO~2EXE");
        assert_eq!(&generated_short_name(".hidden", &[]).unwrap(), b"HIDDEN~1   ");
        assert_eq!(&generated_short_name("żółw.txt", &[]).unwrap(), b"___W~1  TXT");
    }

    #[test]
    fn a_read_only_volume_refuses_writes() {
        let fs = Fat32FileSystem::new(test_image()).unwrap();
        assert!(!fs.writable());
        assert_eq!(fs.write_file("/A.TXT", b"x"), Err(Fat32Error::ReadOnly));
    }

    #[test]
    fn writes_a_new_file_with_a_long_name() {
        let (volume, mut fs) = writable(test_image());
        fs.set_clock(|| 1_795_787_158);
        fs.write_file("/Documents/Firefox Setup.exe", b"MZ downloaded").unwrap();
        assert_eq!(fs.read_file("/Documents/Firefox Setup.exe").unwrap(), b"MZ downloaded");
        assert_eq!(fs.read_file("/Documents/FIREFO~1.EXE").unwrap(), b"MZ downloaded");
        let entry = fs.list("/Documents").unwrap().into_iter().find(|e| e.name == "Firefox Setup.exe").unwrap();
        assert_eq!(entry.size, 13);
        assert_eq!(entry.modified, 1_795_787_158);
        // Seen the same after mounting the image again.
        let again = Fat32FileSystem::new(volume.image()).unwrap();
        assert_eq!(again.read_file("/Documents/firefox setup.exe").unwrap(), b"MZ downloaded");
        // Existing files are untouched.
        assert_eq!(again.read_file("/Documents/readme.txt").unwrap(), b"ok\n");
        assert_eq!(again.read_file("/HELLO.TXT").unwrap().len(), 600);
    }

    #[test]
    fn a_short_upper_case_name_needs_no_long_name() {
        let (volume, fs) = writable(test_image());
        fs.write_file("/NOTES.TXT", b"").unwrap();
        let entries = fs.list("/").unwrap();
        let notes = entries.iter().find(|e| e.name == "NOTES.TXT").unwrap();
        assert_eq!((notes.size, notes.cluster), (0, 0));
        // One slot: the root's fifth, right after HELLO.TXT.
        let image = volume.image();
        let root = 12 * 512;
        assert_eq!(&image[root + 5 * 32..root + 5 * 32 + 11], b"NOTES   TXT");
        assert_eq!(image[root + 6 * 32], 0);
    }

    #[test]
    fn replaces_a_file_and_frees_its_old_clusters() {
        let (volume, fs) = writable(test_image());
        let big: Vec<u8> = (0..2000).map(|i| (i % 253) as u8).collect();
        fs.write_file("/HELLO.TXT", &big).unwrap();
        assert_eq!(fs.read_file("/hello.txt").unwrap(), big);
        let image = volume.image();
        let fat = |cluster: usize| u32::from_le_bytes(image[4 * 512 + cluster * 4..4 * 512 + cluster * 4 + 4].try_into().unwrap());
        // Its old clusters 4 and 5 are free again; the second FAT agrees.
        assert_eq!((fat(4), fat(5)), (0, 0));
        assert_eq!(image[4 * 512..8 * 512], image[8 * 512..12 * 512]);
        // A file in one cluster, then: the freed ones are used again.
        fs.write_file("/HELLO.TXT", b"small").unwrap();
        assert_eq!(fs.read_file("/HELLO.TXT").unwrap(), b"small");
        assert_eq!(fs.list("/").unwrap().iter().filter(|e| e.name.eq_ignore_ascii_case("hello.txt")).count(), 1);
    }

    #[test]
    fn makes_directories_and_files_in_them() {
        let (_volume, fs) = writable(test_image());
        fs.create_dir("/My Downloads").unwrap();
        assert_eq!(fs.create_dir("/my downloads"), Err(Fat32Error::AlreadyExists("/my downloads".into())));
        fs.create_dir("/My Downloads/Inner").unwrap();
        fs.write_file("/My Downloads/Inner/a.bin", &[7u8; 1500]).unwrap();
        assert_eq!(names(&fs, "/My Downloads"), ["Inner"]);
        assert_eq!(fs.read_file("/My Downloads/Inner/a.bin").unwrap(), vec![7u8; 1500]);
        let dir = fs.list("/My Downloads").unwrap();
        assert_eq!(dir[0].name, ".");
        assert_eq!(dir[1].name, "..");
        assert_eq!(dir[1].cluster, 0);
        let inner = fs.list("/My Downloads/Inner").unwrap();
        assert_eq!(inner[1].cluster, dir[0].cluster);
        assert_eq!(fs.write_file("/Missing/a.txt", b"x"), Err(Fat32Error::NotFound("Missing".into())));
        assert!(matches!(fs.write_file("/My Downloads", b"x"), Err(Fat32Error::IsADirectory(_))));
        assert!(matches!(fs.write_file("/a?.txt", b"x"), Err(Fat32Error::InvalidName(_))));
    }

    #[test]
    fn a_full_directory_grows() {
        let (volume, fs) = writable(test_image());
        // A cluster holds 16 slots; each of these names takes 3.
        for i in 0..20 {
            fs.write_file(&format!("/Documents/a file with a long name {i}"), &[i as u8]).unwrap();
        }
        let listed = names(&fs, "/Documents");
        assert_eq!(listed.len(), 22, "{listed:?}");
        for i in 0..20 {
            assert_eq!(fs.read_file(&format!("/Documents/a file with a long name {i}")).unwrap(), [i as u8]);
        }
        let again = Fat32FileSystem::new(volume.image()).unwrap();
        assert_eq!(names(&again, "/Documents").len(), 22);
    }

    #[test]
    fn removes_files_and_empty_directories() {
        let (_volume, fs) = writable(test_image());
        fs.remove("/Documents/a long file name.txt").unwrap();
        assert_eq!(names(&fs, "/Documents"), ["readme.txt"]);
        assert!(matches!(fs.remove("/Documents"), Err(Fat32Error::DirectoryNotEmpty(_))));
        fs.remove("/Documents/readme.txt").unwrap();
        fs.remove("/Documents").unwrap();
        assert_eq!(names(&fs, "/"), ["HELLO.TXT"]);
        assert!(matches!(fs.remove("/Documents"), Err(Fat32Error::NotFound(_))));
        // The freed slots are used again (a long name's and the short one).
        fs.write_file("/Long name.txt", b"x").unwrap();
        assert_eq!(names(&fs, "/"), ["Long name.txt", "HELLO.TXT"]);
    }

    #[test]
    fn renames_and_moves() {
        let (_volume, fs) = writable(test_image());
        fs.write_file("/download.part", b"setup program").unwrap();
        fs.rename("/download.part", "/Documents/Setup Program.exe").unwrap();
        assert_eq!(fs.read_file("/Documents/Setup Program.exe").unwrap(), b"setup program");
        assert!(fs.read_file("/download.part").is_err());
        // Over an existing file: replaced.
        fs.rename("/HELLO.TXT", "/Documents/readme.txt").unwrap();
        assert_eq!(fs.read_file("/Documents/readme.txt").unwrap().len(), 600);
        assert_eq!(names(&fs, "/"), ["Documents"]);
        // Only the case changed.
        fs.rename("/Documents/readme.txt", "/Documents/README.txt").unwrap();
        assert!(names(&fs, "/Documents").contains(&"README.txt".to_string()));
        // A directory moved: its ".." follows.
        fs.create_dir("/Target").unwrap();
        fs.rename("/Documents", "/Target/Docs").unwrap();
        assert_eq!(fs.read_file("/Target/Docs/Setup Program.exe").unwrap(), b"setup program");
        let target = fs.list("/Target").unwrap()[0].cluster;
        assert_eq!(fs.list("/Target/Docs").unwrap()[1].cluster, target);
        assert!(fs.rename("/Target", "/Target/Docs/Inside").is_err());
    }
}
