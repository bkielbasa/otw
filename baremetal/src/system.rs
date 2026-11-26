//! What the kernel gives OTW's library (`otw_vm::platform::System`): the
//! clock, the log, configuration, waiting, and files — drive C: (the FAT32
//! partition) at `/C`, with what programs write kept in memory on top of
//! it until the kernel can write FAT32.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use otw_vm::fat32::Fat32FileSystem;
use otw_vm::platform::{FileInfo, System};

use crate::sync::SpinLock;
use crate::{apic, arch, io, log};

pub const DRIVE_C: &str = "/C";

pub struct Kernel {
    drive: Option<Fat32FileSystem>,
    boot_unix_time: u64,
    /// Files written since boot (None: deleted), by lower-case path.
    files: SpinLock<BTreeMap<String, Option<(String, Vec<u8>)>>>,
    /// Directories made since boot, by lower-case path.
    directories: SpinLock<BTreeSet<String>>,
}

impl Kernel {
    pub fn new(drive: Option<Fat32FileSystem>) -> Self {
        Self {
            drive,
            boot_unix_time: io::rtc_unix_time(),
            files: SpinLock::new(BTreeMap::new()),
            directories: SpinLock::new(BTreeSet::new()),
        }
    }
}

/// The path within drive C: (`/C/a/b` → `a/b`), or None outside it.
fn on_drive(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    let rest = path.strip_prefix(DRIVE_C)?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    Some(parts.join("/"))
}

fn key(path: &str) -> String {
    path.to_lowercase()
}

fn parent(path: &str) -> &str {
    path.rfind('/').map(|i| &path[..i]).unwrap_or("")
}

fn name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl Kernel {
    /// The volume's entries in a directory, as file infos.
    fn volume_list(&self, path: &str) -> Option<Vec<FileInfo>> {
        let drive = self.drive.as_ref()?;
        let entries = drive.list(path).ok()?;
        Some(
            entries
                .into_iter()
                .filter(|e| e.name != "." && e.name != "..")
                .map(|e| FileInfo { directory: e.is_directory(), size: e.size as u64, name: e.name, modified: self.boot_unix_time })
                .collect(),
        )
    }

    fn volume_info(&self, path: &str) -> Option<FileInfo> {
        if path.is_empty() {
            return Some(FileInfo { name: String::new(), directory: true, size: 0, modified: self.boot_unix_time });
        }
        let wanted = name(path);
        self.volume_list(parent(path))?.into_iter().find(|e| e.name.eq_ignore_ascii_case(wanted))
    }
}

impl System for Kernel {
    fn now_us(&self) -> u64 {
        apic::now_us()
    }

    fn unix_time(&self) -> u64 {
        self.boot_unix_time + apic::now_us() / 1_000_000
    }

    fn log(&self, text: &str) {
        let text = text.trim_end_matches('\n');
        if !text.is_empty() {
            log!("{text}");
        }
    }

    fn env(&self, name: &str) -> Option<String> {
        match name {
            "OTW_DRIVE_C" => Some(DRIVE_C.to_string()),
            "OTW_HOST_CLIPBOARD" => Some("0".to_string()),
            "HOME" => Some("/home".to_string()),
            _ => None,
        }
    }

    fn sleep_us(&self, us: u64) {
        let end = apic::now_us() + us;
        loop {
            // The timer interrupt (100 Hz) ends each wait.
            arch::wait_for_interrupt();
            if apic::now_us() >= end {
                break;
            }
        }
    }

    fn yield_now(&self) {}

    fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        let path = on_drive(path)?;
        if let Some(entry) = self.files.lock().get(&key(&path)) {
            return entry.as_ref().map(|(_, data)| data.clone());
        }
        self.drive.as_ref()?.read_file(&path).ok()
    }

    fn write_file(&self, path: &str, data: &[u8]) -> bool {
        let Some(path) = on_drive(path) else { return false };
        if path.is_empty() || self.info(&alloc::format!("{DRIVE_C}/{}", parent(&path))).is_none_or(|i| !i.directory) {
            return false;
        }
        self.files.lock().insert(key(&path), Some((name(&path).to_string(), data.to_vec())));
        true
    }

    fn info(&self, path: &str) -> Option<FileInfo> {
        let path = on_drive(path)?;
        if let Some(entry) = self.files.lock().get(&key(&path)) {
            return entry.as_ref().map(|(name, data)| FileInfo {
                name: name.clone(),
                directory: false,
                size: data.len() as u64,
                modified: self.unix_time(),
            });
        }
        if self.directories.lock().contains(&key(&path)) {
            return Some(FileInfo { name: name(&path).to_string(), directory: true, size: 0, modified: self.unix_time() });
        }
        self.volume_info(&path)
    }

    fn list(&self, path: &str) -> Option<Vec<FileInfo>> {
        let path = on_drive(path)?;
        let k = key(&path);
        let made = self.directories.lock().contains(&k);
        let mut entries = match self.volume_list(&path) {
            Some(entries) => entries,
            None if made => Vec::new(),
            None => return None,
        };
        let prefix = if k.is_empty() { String::new() } else { alloc::format!("{k}/") };
        let direct = |p: &String| p.starts_with(&prefix) && !p[prefix.len()..].contains('/') && p.len() > prefix.len();
        for (p, entry) in self.files.lock().iter().filter(|(p, _)| direct(p)) {
            let short = &p[prefix.len()..];
            entries.retain(|e| !e.name.eq_ignore_ascii_case(short));
            if let Some((name, data)) = entry {
                entries.push(FileInfo { name: name.clone(), directory: false, size: data.len() as u64, modified: self.unix_time() });
            }
        }
        for p in self.directories.lock().iter().filter(|p| direct(p)) {
            let short = &p[prefix.len()..];
            if !entries.iter().any(|e| e.name.eq_ignore_ascii_case(short)) {
                entries.push(FileInfo { name: short.to_string(), directory: true, size: 0, modified: self.unix_time() });
            }
        }
        Some(entries)
    }

    fn create_dir(&self, path: &str) -> bool {
        let Some(path) = on_drive(path) else { return false };
        if self.info(&alloc::format!("{DRIVE_C}/{path}")).is_some() {
            return false;
        }
        self.directories.lock().insert(key(&path));
        true
    }

    fn remove(&self, path: &str) -> bool {
        let Some(drive_path) = on_drive(path) else { return false };
        let Some(info) = self.info(path) else { return false };
        if info.directory {
            self.directories.lock().remove(&key(&drive_path));
        } else {
            self.files.lock().insert(key(&drive_path), None);
        }
        true
    }

    fn rename(&self, from: &str, to: &str) -> bool {
        let Some(data) = self.read_file(from) else { return false };
        self.write_file(to, &data) && self.remove(from)
    }
}
