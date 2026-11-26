//! Files, through the system (`System`): read whole when opened, written
//! back whole when flushed or dropped.

use alloc::string::String;
use alloc::vec::Vec;

use super::io::{self, Error, ErrorKind, Read, Seek, SeekFrom, Write};
use super::path::{Path, PathBuf};
use super::system;
use super::time::SystemTime;

fn not_found() -> Error {
    Error::from(ErrorKind::NotFound)
}

fn failed() -> Error {
    Error::from(ErrorKind::Other)
}

fn text(path: &impl AsRef<Path>) -> &str {
    path.as_ref().as_str()
}

pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    system().read_file(text(&path)).ok_or_else(not_found)
}

pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|_| Error::from(ErrorKind::InvalidData))
}

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    system().write_file(text(&path), contents.as_ref()).then_some(()).ok_or_else(failed)
}

pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    system().create_dir(text(&path)).then_some(()).ok_or_else(failed)
}

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    if path.as_str().is_empty() || path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    create_dir(path)
}

pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    system().remove(text(&path)).then_some(()).ok_or_else(not_found)
}

pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    remove_file(path)
}

pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(entries) = system().list(path.as_str()) {
        for entry in entries {
            let child = path.join(&entry.name);
            if entry.directory {
                remove_dir_all(&child)?;
            } else {
                remove_file(&child)?;
            }
        }
    }
    remove_file(path)
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    system().rename(text(&from), text(&to)).then_some(()).ok_or_else(failed)
}

pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let data = read(from)?;
    write(to, &data)?;
    Ok(data.len() as u64)
}

#[derive(Clone, Debug)]
pub struct Metadata {
    info: super::FileInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileType {
    directory: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.directory
    }
    pub fn is_file(&self) -> bool {
        !self.directory
    }
    pub fn is_symlink(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permissions {
    readonly: bool,
}

impl Permissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }
    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}

impl Metadata {
    pub fn len(&self) -> u64 {
        self.info.size
    }
    pub fn is_dir(&self) -> bool {
        self.info.directory
    }
    pub fn is_file(&self) -> bool {
        !self.info.directory
    }
    pub fn file_type(&self) -> FileType {
        FileType { directory: self.info.directory }
    }
    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::from_unix(self.info.modified))
    }
    pub fn created(&self) -> io::Result<SystemTime> {
        self.modified()
    }
    pub fn accessed(&self) -> io::Result<SystemTime> {
        self.modified()
    }
    pub fn permissions(&self) -> Permissions {
        Permissions { readonly: false }
    }
}

pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    system().info(text(&path)).map(|info| Metadata { info }).ok_or_else(not_found)
}

pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    metadata(path)
}

pub fn set_permissions(_path: impl AsRef<Path>, _permissions: Permissions) -> io::Result<()> {
    Ok(())
}

pub struct DirEntry {
    directory: PathBuf,
    info: super::FileInfo,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.directory.join(&self.info.name)
    }
    pub fn file_name(&self) -> super::path::OsString {
        self.info.name.clone().into()
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        Ok(Metadata { info: self.info.clone() })
    }
    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(FileType { directory: self.info.directory })
    }
}

pub struct ReadDir(alloc::vec::IntoIter<DirEntry>);

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(Ok)
    }
}

pub fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let directory = path.as_ref().to_path_buf();
    let entries = system().list(directory.as_str()).ok_or_else(not_found)?;
    Ok(ReadDir(
        entries
            .into_iter()
            .map(|info| DirEntry { directory: directory.clone(), info })
            .collect::<Vec<_>>()
            .into_iter(),
    ))
}

/// An open file: its contents in memory.
pub struct File(super::sync::Mutex<OpenFile>);

struct OpenFile {
    path: PathBuf,
    data: Vec<u8>,
    position: u64,
    writable: bool,
    dirty: bool,
}

impl core::fmt::Debug for File {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("File").field(&self.0.lock().unwrap_or_else(super::sync::PoisonError::into_inner).path).finish()
    }
}

impl OpenFile {
    fn flush(&mut self) -> io::Result<()> {
        if self.dirty {
            write(&self.path, &self.data)?;
            self.dirty = false;
        }
        Ok(())
    }
}

impl File {
    pub fn open(path: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new().read(true).open(path)
    }
    pub fn create(path: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new().write(true).create(true).truncate(true).open(path)
    }
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        let file = self.0.lock().unwrap_or_else(super::sync::PoisonError::into_inner);
        Ok(Metadata {
            info: super::FileInfo {
                name: file.path.file_name().map(|n| n.to_string_lossy().into()).unwrap_or_default(),
                directory: false,
                size: file.data.len() as u64,
                modified: system().unix_time(),
            },
        })
    }
    pub fn set_len(&self, size: u64) -> io::Result<()> {
        let mut file = self.0.lock().unwrap_or_else(super::sync::PoisonError::into_inner);
        file.data.resize(size as usize, 0);
        file.dirty = true;
        Ok(())
    }
    pub fn sync_all(&self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(super::sync::PoisonError::into_inner).flush()
    }
    pub fn sync_data(&self) -> io::Result<()> {
        self.sync_all()
    }
    pub fn try_clone(&self) -> io::Result<File> {
        let file = self.0.lock().unwrap_or_else(super::sync::PoisonError::into_inner);
        Ok(File(super::sync::Mutex::new(OpenFile {
            path: file.path.clone(),
            data: file.data.clone(),
            position: file.position,
            writable: false,
            dirty: false,
        })))
    }
    pub fn set_permissions(&self, _: Permissions) -> io::Result<()> {
        Ok(())
    }
}

impl Read for &File {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut file = self.0.lock().unwrap_or_else(super::sync::PoisonError::into_inner);
        let start = (file.position as usize).min(file.data.len());
        let n = buffer.len().min(file.data.len() - start);
        buffer[..n].copy_from_slice(&file.data[start..start + n]);
        file.position += n as u64;
        Ok(n)
    }
}

impl Write for &File {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut file = self.0.lock().unwrap_or_else(super::sync::PoisonError::into_inner);
        if !file.writable {
            return Err(Error::from(ErrorKind::PermissionDenied));
        }
        let start = file.position as usize;
        if file.data.len() < start + buffer.len() {
            file.data.resize(start + buffer.len(), 0);
        }
        file.data[start..start + buffer.len()].copy_from_slice(buffer);
        file.position += buffer.len() as u64;
        file.dirty = true;
        Ok(buffer.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(super::sync::PoisonError::into_inner).flush()
    }
}

impl Seek for &File {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let mut file = self.0.lock().unwrap_or_else(super::sync::PoisonError::into_inner);
        let position = match from {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => file.data.len() as i64 + offset,
            SeekFrom::Current(offset) => file.position as i64 + offset,
        };
        if position < 0 {
            return Err(Error::from(ErrorKind::InvalidInput));
        }
        file.position = position as u64;
        Ok(file.position)
    }
}

impl Read for File {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buffer)
    }
}

impl Write for File {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        (&*self).write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

impl Seek for File {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        (&*self).seek(from)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let _ = self.0.lock().unwrap_or_else(super::sync::PoisonError::into_inner).flush();
    }
}

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn read(&mut self, on: bool) -> &mut Self {
        self.read = on;
        self
    }
    pub fn write(&mut self, on: bool) -> &mut Self {
        self.write = on;
        self
    }
    pub fn append(&mut self, on: bool) -> &mut Self {
        self.append = on;
        self
    }
    pub fn truncate(&mut self, on: bool) -> &mut Self {
        self.truncate = on;
        self
    }
    pub fn create(&mut self, on: bool) -> &mut Self {
        self.create = on;
        self
    }
    pub fn create_new(&mut self, on: bool) -> &mut Self {
        self.create_new = on;
        self
    }
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let path = path.as_ref().to_path_buf();
        let existing = system().read_file(path.as_str());
        if self.create_new && existing.is_some() {
            return Err(Error::from(ErrorKind::AlreadyExists));
        }
        let writable = self.write || self.append;
        let (data, dirty) = match existing {
            Some(_) if self.truncate => (Vec::new(), true),
            Some(data) => (data, false),
            None if self.create || self.create_new => (Vec::new(), true),
            None => return Err(not_found()),
        };
        let position = if self.append { data.len() as u64 } else { 0 };
        let mut file = OpenFile { path, data, position, writable, dirty: dirty && writable };
        file.flush()?;
        Ok(File(super::sync::Mutex::new(file)))
    }
}
