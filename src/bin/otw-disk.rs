//! otw-disk: change the drive C: partition of an OTW disk image in place,
//! with OTW's own FAT32 writer (what the kernel uses at run time), so a
//! rebuilt image keeps what was saved, downloaded and installed on it.
//!
//!     otw-disk sync IMAGE LABEL [--replace] DIRECTORY[+DIRECTORY...]
//!
//! Copies each directory's tree into partition LABEL (by its GPT name):
//! folders and files that are not there are added; with `--replace`, files
//! that are there with other contents are written again. Nothing on the
//! partition is removed.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use otw_vm::fat32::{Fat32Error, Fat32FileSystem, Volume};

const SECTOR: u64 = 512;

/// A partition of an image file, as a FAT32 volume.
struct PartitionFile {
    file: Mutex<File>,
    offset: u64,
    size: u64,
}

impl Volume for PartitionFile {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), Fat32Error> {
        if offset + buffer.len() as u64 > self.size {
            return Err(Fat32Error::OutOfBounds(offset, buffer.len()));
        }
        let mut file = self.file.lock().unwrap();
        file.seek(SeekFrom::Start(self.offset + offset)).map_err(|_| Fat32Error::Io("seek"))?;
        file.read_exact(buffer).map_err(|_| Fat32Error::Io("read"))
    }

    fn writable(&self) -> bool {
        true
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), Fat32Error> {
        if offset + data.len() as u64 > self.size {
            return Err(Fat32Error::OutOfBounds(offset, data.len()));
        }
        let mut file = self.file.lock().unwrap();
        file.seek(SeekFrom::Start(self.offset + offset)).map_err(|_| Fat32Error::Io("seek"))?;
        file.write_all(data).map_err(|_| Fat32Error::Io("write"))
    }

    fn flush(&self) -> Result<(), Fat32Error> {
        Ok(())
    }
}

/// The first and last sector of the GPT partition named `label`.
fn find_partition(file: &mut File, label: &str) -> Result<(u64, u64), String> {
    let mut header = [0u8; 92];
    file.seek(SeekFrom::Start(SECTOR)).map_err(|e| e.to_string())?;
    file.read_exact(&mut header).map_err(|e| e.to_string())?;
    if &header[0..8] != b"EFI PART" {
        return Err("no GPT partition table".into());
    }
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let (entries_lba, count, size) = (u64_at(&header, 72), u32_at(&header, 80) as usize, u32_at(&header, 84) as usize);
    let mut entries = vec![0u8; count * size];
    file.seek(SeekFrom::Start(entries_lba * SECTOR)).map_err(|e| e.to_string())?;
    file.read_exact(&mut entries).map_err(|e| e.to_string())?;
    for entry in entries.chunks(size) {
        let units: Vec<u16> = entry[56..128].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        if String::from_utf16_lossy(&units) == label {
            return Ok((u64_at(entry, 32), u64_at(entry, 40)));
        }
    }
    Err(format!("no partition named {label}"))
}

#[derive(Default)]
struct Counts {
    added: usize,
    replaced: usize,
    kept: usize,
    folders: usize,
}

/// Copy `host`'s tree into `guest` (a path on the volume, "" for the root).
fn sync(fs: &Fat32FileSystem, host: &Path, guest: &str, replace: bool, counts: &mut Counts) -> Result<(), String> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(host)
        .map_err(|e| format!("{}: {e}", host.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    names.sort();
    let existing = fs.list(if guest.is_empty() { "/" } else { guest }).map_err(|e| format!("{guest}: {e}"))?;
    for path in names {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let target = format!("{guest}/{name}");
        let there = existing.iter().find(|e| e.name.eq_ignore_ascii_case(name));
        if meta.is_dir() {
            match there {
                Some(entry) if entry.is_directory() => {}
                Some(_) => return Err(format!("{target} is a file on the disk, a folder here")),
                None => {
                    fs.create_dir(&target).map_err(|e| format!("{target}: {e}"))?;
                    counts.folders += 1;
                }
            }
            sync(fs, &path, &target, replace, counts)?;
        } else if meta.is_file() {
            match there {
                Some(entry) if entry.is_directory() => return Err(format!("{target} is a folder on the disk, a file here")),
                Some(entry) => {
                    if !replace {
                        counts.kept += 1;
                        continue;
                    }
                    let data = std::fs::read(&path).map_err(|e| e.to_string())?;
                    if entry.size as u64 == data.len() as u64 && fs.read_file_entry(entry).ok().as_deref() == Some(&data[..]) {
                        counts.kept += 1;
                        continue;
                    }
                    fs.write_file(&target, &data).map_err(|e| format!("{target}: {e}"))?;
                    counts.replaced += 1;
                }
                None => {
                    let data = std::fs::read(&path).map_err(|e| e.to_string())?;
                    fs.write_file(&target, &data).map_err(|e| format!("{target}: {e}"))?;
                    counts.added += 1;
                }
            }
        }
    }
    Ok(())
}

fn run(args: &[String]) -> Result<String, String> {
    let usage = "usage: otw-disk sync IMAGE LABEL [--replace] DIRECTORY[+DIRECTORY...]";
    let [command, image, label, rest @ ..] = args else { return Err(usage.into()) };
    if command != "sync" {
        return Err(usage.into());
    }
    let replace = rest.first().is_some_and(|a| a == "--replace");
    let [directories] = &rest[usize::from(replace)..] else { return Err(usage.into()) };
    let mut file = OpenOptions::new().read(true).write(true).open(image).map_err(|e| format!("{image}: {e}"))?;
    let (first, last) = find_partition(&mut file, label)?;
    let volume = Arc::new(PartitionFile { file: Mutex::new(file), offset: first * SECTOR, size: (last + 1 - first) * SECTOR });
    let mut fs = Fat32FileSystem::from_volume(volume).map_err(|e| format!("{label}: {e}"))?;
    fs.set_clock(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    let mut counts = Counts::default();
    for directory in directories.split('+').filter(|d| !d.is_empty()) {
        sync(&fs, Path::new(directory), "", replace, &mut counts)?;
    }
    Ok(format!(
        "{label}: {} files added, {} replaced, {} kept; {} folders made",
        counts.added, counts.replaced, counts.kept, counts.folders
    ))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("otw-disk: {error}");
            ExitCode::FAILURE
        }
    }
}
