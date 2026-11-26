//! The volume the shell browses: the host directory behind guest drive C:
//! (writable, the same files programs open and save) or a FAT32 disk image
//! (read-only).

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::{
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::fat32::Fat32FileSystem;

/// A path on the volume as its components, root first. Empty is the root.
pub type VolPath = Vec<String>;

#[derive(Clone)]
pub enum Volume {
    Host(PathBuf),
    Image(Rc<Fat32FileSystem>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

const READ_ONLY: &str = "The disk is write-protected.";

/// `\A\B` (or `\` for the root).
pub fn path_text(path: &[String]) -> String {
    format!("\\{}", path.join("\\"))
}

/// Parse `C:\A\B`, `\A\B`, `A/B` into components.
pub fn parse_path(text: &str) -> VolPath {
    let text = text.trim().trim_matches('"');
    let rest = if text.len() >= 2 && text.as_bytes()[1] == b':' {
        &text[2..]
    } else {
        text
    };
    let mut parts: VolPath = Vec::new();
    for part in rest.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part.to_string()),
        }
    }
    parts
}

pub fn join(dir: &[String], name: &str) -> VolPath {
    let mut path = dir.to_vec();
    path.push(name.to_string());
    path
}

/// File-name extension in upper case, without the dot.
pub fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_uppercase())
        .unwrap_or_default()
}

/// Check a new file name the way Windows does.
pub fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim().trim_end_matches(['.', ' ']).to_string();
    if name.is_empty() {
        return Err("You must type a filename.".into());
    }
    if name
        .chars()
        .any(|c| "\\/:*?\"<>|".contains(c) || (c as u32) < 32)
    {
        return Err(
            "A filename cannot contain any of the following characters:\n\\ / : * ? \" < > |"
                .into(),
        );
    }
    Ok(name)
}

impl Volume {
    /// The host directory backing guest drive C:.
    pub fn drive_c() -> Self {
        Self::Host(crate::win32::crt::drive_root())
    }

    pub fn writable(&self) -> bool {
        matches!(self, Self::Host(_))
    }

    /// Display name of the volume root.
    pub fn label(&self) -> String {
        match self {
            Self::Host(_) => "(C:)".into(),
            Self::Image(fs) => {
                let label = fs.volume_label().trim().to_string();
                if label.is_empty() {
                    "Disk image".into()
                } else {
                    label
                }
            }
        }
    }

    /// The path a guest program uses for a volume path.
    pub fn guest_path(&self, path: &[String]) -> String {
        format!("C:{}", path_text(path))
    }

    /// Resolve a path case-insensitively, like FAT, on the host.
    pub fn host_path(&self, path: &[String]) -> Option<PathBuf> {
        let Self::Host(root) = self else {
            return None;
        };
        let mut host = root.clone();
        for component in path {
            let matched = fs::read_dir(&host).ok().and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name())
                    .find(|name| name.to_string_lossy().eq_ignore_ascii_case(component))
            });
            match matched {
                Some(name) => host.push(name),
                None => host.push(component),
            }
        }
        Some(host)
    }

    /// Directory contents: folders first, then files, each by name.
    pub fn list(&self, dir: &[String]) -> Result<Vec<Entry>, String> {
        let mut entries = match self {
            Self::Host(_) => {
                let host = self.host_path(dir).unwrap_or_default();
                let read = fs::read_dir(&host)
                    .map_err(|_| format!("{} is not accessible.", path_text(dir)))?;
                read.filter_map(Result::ok)
                    .filter_map(|entry| {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let meta = entry.metadata().ok()?;
                        Some(Entry {
                            name,
                            is_dir: meta.is_dir(),
                            size: if meta.is_dir() { 0 } else { meta.len() },
                        })
                    })
                    .collect::<Vec<_>>()
            }
            Self::Image(fs) => fs
                .list(&path_text(dir))
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|entry| entry.name != "." && entry.name != "..")
                .map(|entry| Entry {
                    is_dir: entry.is_directory(),
                    size: entry.size as u64,
                    name: entry.name,
                })
                .collect(),
        };
        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
        });
        Ok(entries)
    }

    /// The directory entry for a path (`None` for a missing path; the root
    /// is a directory).
    pub fn entry(&self, path: &[String]) -> Option<Entry> {
        let Some((name, parent)) = path.split_last() else {
            return Some(Entry {
                name: String::new(),
                is_dir: true,
                size: 0,
            });
        };
        self.list(parent)
            .ok()?
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
    }

    /// The path with each component spelled as stored on the volume.
    pub fn canonical(&self, path: &[String]) -> VolPath {
        let mut out: VolPath = Vec::with_capacity(path.len());
        for component in path {
            let stored = self
                .list(&out)
                .ok()
                .and_then(|entries| {
                    entries
                        .into_iter()
                        .find(|e| e.name.eq_ignore_ascii_case(component))
                })
                .map(|e| e.name)
                .unwrap_or_else(|| component.clone());
            out.push(stored);
        }
        out
    }

    pub fn exists(&self, path: &[String]) -> bool {
        self.entry(path).is_some()
    }

    pub fn is_dir(&self, path: &[String]) -> bool {
        self.entry(path).map(|entry| entry.is_dir).unwrap_or(false)
    }

    pub fn read(&self, path: &[String]) -> Result<Vec<u8>, String> {
        match self {
            Self::Host(_) => fs::read(self.host_path(path).unwrap_or_default())
                .map_err(|error| error.to_string()),
            Self::Image(fs) => fs
                .read_file(&path_text(path))
                .map_err(|error| error.to_string()),
        }
    }

    pub fn write(&self, path: &[String], bytes: &[u8]) -> Result<(), String> {
        let host = self.host_path(path).ok_or(READ_ONLY)?;
        fs::write(host, bytes).map_err(|error| error.to_string())
    }

    pub fn create_dir(&self, path: &[String]) -> Result<(), String> {
        let host = self.host_path(path).ok_or(READ_ONLY)?;
        fs::create_dir_all(host).map_err(|error| error.to_string())
    }

    /// Rename in place. Changing only the case of a name is allowed.
    pub fn rename(&self, path: &[String], new_name: &str) -> Result<VolPath, String> {
        let host = self.host_path(path).ok_or(READ_ONLY)?;
        let new_name = validate_name(new_name)?;
        let (old_name, parent) = path
            .split_last()
            .ok_or("The root folder cannot be renamed.")?;
        let target = join(parent, &new_name);
        if !old_name.eq_ignore_ascii_case(&new_name) && self.exists(&target) {
            return Err(format!(
                "Cannot rename {old_name}: A file with the name you specified already exists. Specify a different filename."
            ));
        }
        fs::rename(&host, host.with_file_name(&new_name)).map_err(|error| error.to_string())?;
        Ok(target)
    }

    /// Delete a file or a folder with everything in it.
    pub fn delete(&self, path: &[String]) -> Result<(), String> {
        if path.is_empty() {
            return Err("The root folder cannot be deleted.".into());
        }
        let host = self.host_path(path).ok_or(READ_ONLY)?;
        let result = if host.is_dir() {
            fs::remove_dir_all(&host)
        } else {
            fs::remove_file(&host)
        };
        result.map_err(|error| error.to_string())
    }

    /// Copy a file or folder tree to `target`.
    pub fn copy(&self, from: &[String], target: &[String]) -> Result<(), String> {
        let destination = self.host_path(target).ok_or(READ_ONLY)?;
        if target.len() > from.len()
            && target[..from.len()]
                .iter()
                .zip(from)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return Err("The destination folder is a subfolder of the source folder.".into());
        }
        copy_tree(&self.host_path(from).unwrap_or_default(), &destination)
            .map_err(|error| error.to_string())
    }

    /// Move a file or folder tree into another folder.
    pub fn move_to(&self, from: &[String], target: &[String]) -> Result<(), String> {
        let destination = self.host_path(target).ok_or(READ_ONLY)?;
        if target.len() > from.len()
            && target[..from.len()]
                .iter()
                .zip(from)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return Err("The destination folder is a subfolder of the source folder.".into());
        }
        fs::rename(self.host_path(from).unwrap_or_default(), destination)
            .map_err(|error| error.to_string())
    }

    /// The first free name of the form `stem.ext`, `stem (2).ext`, ...
    pub fn unique_name(&self, dir: &[String], stem: &str, ext: &str) -> String {
        let names: Vec<String> = self
            .list(dir)
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.name.to_ascii_lowercase())
            .collect();
        let make = |n: usize| {
            let base = if n == 1 {
                stem.to_string()
            } else {
                format!("{stem} ({n})")
            };
            if ext.is_empty() {
                base
            } else {
                format!("{base}.{ext}")
            }
        };
        (1..)
            .map(make)
            .find(|name| !names.contains(&name.to_ascii_lowercase()))
            .unwrap()
    }

    /// The name Windows gives a pasted copy: `Copy of X`, `Copy (2) of X`.
    pub fn copy_name(&self, dir: &[String], name: &str) -> String {
        if !self.exists(&join(dir, name)) {
            return name.to_string();
        }
        let names: Vec<String> = self
            .list(dir)
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.name.to_ascii_lowercase())
            .collect();
        (1..)
            .map(|n: usize| {
                if n == 1 {
                    format!("Copy of {name}")
                } else {
                    format!("Copy ({n}) of {name}")
                }
            })
            .find(|candidate| !names.contains(&candidate.to_ascii_lowercase()))
            .unwrap()
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    fn temp_volume(tag: &str) -> (Volume, PathBuf) {
        let root = std::env::temp_dir().join(format!("otw-volume-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        (Volume::Host(root.clone()), root)
    }

    #[test]
    fn paths_parse_from_guest_and_shell_forms() {
        assert_eq!(
            parse_path("C:\\My Documents\\a.txt"),
            vec!["My Documents", "a.txt"]
        );
        assert_eq!(parse_path("\\WINDOWS\\..\\TEMP\\"), vec!["TEMP"]);
        assert_eq!(parse_path("\"C:\\\""), Vec::<String>::new());
        assert_eq!(path_text(&parse_path("A/B")), "\\A\\B");
    }

    #[test]
    fn names_are_validated_like_windows() {
        assert_eq!(validate_name("  Notes.txt. ").unwrap(), "Notes.txt");
        assert!(validate_name("a:b").is_err());
        assert!(validate_name("   ").is_err());
    }

    #[test]
    fn host_volume_creates_renames_copies_and_deletes() {
        let (volume, root) = temp_volume("ops");
        volume.create_dir(&parse_path("\\Docs")).unwrap();
        assert_eq!(volume.unique_name(&[], "New Folder", ""), "New Folder");
        volume.create_dir(&parse_path("\\New Folder")).unwrap();
        assert_eq!(volume.unique_name(&[], "New Folder", ""), "New Folder (2)");
        volume
            .write(&parse_path("\\docs\\a.txt"), b"hello")
            .unwrap();
        // Case-insensitive lookup finds the existing folder.
        assert_eq!(fs::read(root.join("Docs/a.txt")).unwrap(), b"hello");
        let renamed = volume
            .rename(&parse_path("\\Docs\\a.txt"), "b.txt")
            .unwrap();
        assert_eq!(renamed, parse_path("\\Docs\\b.txt"));
        volume.write(&parse_path("\\Docs\\c.txt"), b"").unwrap();
        assert!(volume
            .rename(&parse_path("\\Docs\\c.txt"), "B.TXT")
            .is_err());
        let copy = volume.copy_name(&parse_path("\\Docs"), "b.txt");
        assert_eq!(copy, "Copy of b.txt");
        volume
            .copy(
                &parse_path("\\Docs\\b.txt"),
                &join(&parse_path("\\Docs"), &copy),
            )
            .unwrap();
        assert!(volume
            .copy(&parse_path("\\Docs"), &parse_path("\\Docs\\Inner"))
            .is_err());
        let names: Vec<String> = volume
            .list(&parse_path("\\Docs"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["b.txt", "c.txt", "Copy of b.txt"]);
        volume.delete(&parse_path("\\Docs")).unwrap();
        let top: Vec<Entry> = volume.list(&[]).unwrap();
        assert_eq!(
            top,
            vec![Entry {
                name: "New Folder".into(),
                is_dir: true,
                size: 0
            }]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn image_volume_is_read_only() {
        let Some(image) = crate::test_binary("disks/OTWDISK.IMG") else {
            return;
        };
        let fs = Fat32FileSystem::new(image).unwrap();
        let volume = Volume::Image(Rc::new(fs));
        assert!(volume
            .list(&[])
            .unwrap()
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case("NOTEPAD.EXE")));
        assert!(volume.create_dir(&parse_path("\\X")).is_err());
        assert!(!volume.writable());
    }
}
