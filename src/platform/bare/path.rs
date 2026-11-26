//! Paths and OS strings: `/`-separated UTF-8 (the kernel's file system
//! names are UTF-8).

use alloc::borrow::{Cow, ToOwned};
use alloc::string::String;
use core::borrow::Borrow;
use core::fmt;
use core::ops::Deref;

/// A string from the system (always UTF-8 here).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct OsString(String);

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct OsStr(str);

impl OsString {
    pub fn new() -> Self {
        Self(String::new())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn into_string(self) -> Result<String, OsString> {
        Ok(self.0)
    }
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.0)
    }
    pub fn to_str(&self) -> Option<&str> {
        Some(&self.0)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn push(&mut self, s: impl AsRef<OsStr>) {
        self.0.push_str(&s.as_ref().0);
    }
    pub fn as_os_str(&self) -> &OsStr {
        self
    }
}

impl OsStr {
    pub fn new<S: AsRef<OsStr> + ?Sized>(s: &S) -> &OsStr {
        s.as_ref()
    }
    fn from_str(s: &str) -> &OsStr {
        unsafe { &*(s as *const str as *const OsStr) }
    }
    pub fn to_str(&self) -> Option<&str> {
        Some(&self.0)
    }
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.0)
    }
    pub fn to_os_string(&self) -> OsString {
        OsString(self.0.into())
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn as_encoded_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
    pub fn eq_ignore_ascii_case(&self, other: impl AsRef<OsStr>) -> bool {
        self.0.eq_ignore_ascii_case(&other.as_ref().0)
    }
}

impl Deref for OsString {
    type Target = OsStr;
    fn deref(&self) -> &OsStr {
        OsStr::from_str(&self.0)
    }
}

impl Borrow<OsStr> for OsString {
    fn borrow(&self) -> &OsStr {
        self
    }
}

impl ToOwned for OsStr {
    type Owned = OsString;
    fn to_owned(&self) -> OsString {
        self.to_os_string()
    }
}

impl AsRef<OsStr> for OsStr {
    fn as_ref(&self) -> &OsStr {
        self
    }
}
impl AsRef<OsStr> for OsString {
    fn as_ref(&self) -> &OsStr {
        self
    }
}
impl AsRef<OsStr> for str {
    fn as_ref(&self) -> &OsStr {
        OsStr::from_str(self)
    }
}
impl AsRef<OsStr> for String {
    fn as_ref(&self) -> &OsStr {
        OsStr::from_str(self)
    }
}
impl AsRef<str> for OsString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl AsRef<str> for OsStr {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl From<String> for OsString {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl From<&str> for OsString {
    fn from(s: &str) -> Self {
        Self(s.into())
    }
}
impl PartialEq<str> for OsString {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}
impl PartialEq<&str> for OsString {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}
impl PartialEq<str> for OsStr {
    fn eq(&self, other: &str) -> bool {
        &self.0 == other
    }
}
impl PartialEq<&str> for OsStr {
    fn eq(&self, other: &&str) -> bool {
        &self.0 == *other
    }
}
impl PartialEq<OsString> for str {
    fn eq(&self, other: &OsString) -> bool {
        self == other.0
    }
}
impl PartialEq<OsString> for &str {
    fn eq(&self, other: &OsString) -> bool {
        *self == other.0
    }
}
impl fmt::Display for OsStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A path: `/`-separated.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Path(str);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PathBuf(String);

pub struct Display<'a>(&'a str);

impl fmt::Display for Display<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Path {
    pub fn new<S: AsRef<str> + ?Sized>(s: &S) -> &Path {
        unsafe { &*(s.as_ref() as *const str as *const Path) }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn to_str(&self) -> Option<&str> {
        Some(&self.0)
    }
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.0)
    }
    pub fn as_os_str(&self) -> &OsStr {
        OsStr::from_str(&self.0)
    }
    pub fn display(&self) -> Display<'_> {
        Display(&self.0)
    }
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf(self.0.into())
    }
    pub fn is_absolute(&self) -> bool {
        self.0.starts_with('/')
    }
    pub fn join(&self, other: impl AsRef<Path>) -> PathBuf {
        let mut buf = self.to_path_buf();
        buf.push(other);
        buf
    }
    fn trimmed(&self) -> &str {
        let t = self.0.trim_end_matches('/');
        if t.is_empty() && self.0.starts_with('/') {
            "/"
        } else {
            t
        }
    }
    pub fn parent(&self) -> Option<&Path> {
        let t = self.trimmed();
        if t == "/" || t.is_empty() {
            return None;
        }
        match t.rfind('/') {
            Some(0) => Some(Path::new("/")),
            Some(i) => Some(Path::new(&t[..i])),
            None => Some(Path::new("")),
        }
    }
    pub fn file_name(&self) -> Option<&OsStr> {
        let t = self.trimmed();
        let name = t.rsplit('/').next()?;
        (!name.is_empty() && name != "..").then(|| OsStr::from_str(name))
    }
    pub fn file_stem(&self) -> Option<&OsStr> {
        let name = &self.file_name()?.0;
        Some(OsStr::from_str(match name.rfind('.') {
            Some(0) | None => name,
            Some(i) => &name[..i],
        }))
    }
    pub fn extension(&self) -> Option<&OsStr> {
        let name = &self.file_name()?.0;
        match name.rfind('.') {
            Some(0) | None => None,
            Some(i) => Some(OsStr::from_str(&name[i + 1..])),
        }
    }
    pub fn with_extension(&self, extension: impl AsRef<str>) -> PathBuf {
        let mut buf = self.to_path_buf();
        buf.set_extension(extension);
        buf
    }
    pub fn with_file_name(&self, name: impl AsRef<str>) -> PathBuf {
        match self.parent() {
            Some(parent) => parent.join(name.as_ref()),
            None => PathBuf::from(name.as_ref()),
        }
    }
    pub fn starts_with(&self, base: impl AsRef<Path>) -> bool {
        self.strip_prefix(base).is_ok()
    }
    pub fn ends_with(&self, child: impl AsRef<Path>) -> bool {
        let child = child.as_ref().trimmed();
        let t = self.trimmed();
        t == child || t.ends_with(&alloc::format!("/{child}"))
    }
    pub fn strip_prefix(&self, base: impl AsRef<Path>) -> Result<&Path, StripPrefixError> {
        let base = base.as_ref().trimmed();
        let t = &self.0;
        if base == "/" {
            return t.strip_prefix('/').map(Path::new).ok_or(StripPrefixError(()));
        }
        match t.strip_prefix(base) {
            Some("") => Ok(Path::new("")),
            Some(rest) if rest.starts_with('/') => Ok(Path::new(rest.trim_start_matches('/'))),
            _ => Err(StripPrefixError(())),
        }
    }
    pub fn components(&self) -> impl Iterator<Item = Component<'_>> {
        let root = self.0.starts_with('/').then_some(Component::RootDir);
        root.into_iter().chain(self.0.split('/').filter(|c| !c.is_empty() && *c != ".").map(|c| {
            if c == ".." {
                Component::ParentDir
            } else {
                Component::Normal(OsStr::from_str(c))
            }
        }))
    }
    pub fn ancestors(&self) -> impl Iterator<Item = &Path> {
        core::iter::successors(Some(self), |p| p.parent())
    }
    pub fn exists(&self) -> bool {
        super::system().info(&self.0).is_some()
    }
    pub fn is_dir(&self) -> bool {
        super::system().info(&self.0).is_some_and(|i| i.directory)
    }
    pub fn is_file(&self) -> bool {
        super::system().info(&self.0).is_some_and(|i| !i.directory)
    }
    pub fn metadata(&self) -> super::io::Result<super::fs::Metadata> {
        super::fs::metadata(self)
    }
    pub fn read_dir(&self) -> super::io::Result<super::fs::ReadDir> {
        super::fs::read_dir(self)
    }
    pub fn canonicalize(&self) -> super::io::Result<PathBuf> {
        Ok(self.to_path_buf())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component<'a> {
    RootDir,
    CurDir,
    ParentDir,
    Normal(&'a OsStr),
}

impl<'a> Component<'a> {
    pub fn as_os_str(self) -> &'a OsStr {
        match self {
            Component::RootDir => OsStr::from_str("/"),
            Component::CurDir => OsStr::from_str("."),
            Component::ParentDir => OsStr::from_str(".."),
            Component::Normal(s) => s,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripPrefixError(());

impl fmt::Display for StripPrefixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("prefix not found")
    }
}

impl PathBuf {
    pub fn new() -> Self {
        Self(String::new())
    }
    pub fn as_path(&self) -> &Path {
        self
    }
    pub fn push(&mut self, part: impl AsRef<Path>) {
        let part = &part.as_ref().0;
        if part.starts_with('/') {
            self.0 = part.into();
            return;
        }
        if !self.0.is_empty() && !self.0.ends_with('/') {
            self.0.push('/');
        }
        self.0.push_str(part);
    }
    pub fn pop(&mut self) -> bool {
        match self.parent().map(|p| p.0.len()) {
            Some(len) => {
                self.0.truncate(len);
                true
            }
            None => false,
        }
    }
    pub fn set_extension(&mut self, extension: impl AsRef<str>) -> bool {
        let Some(stem) = self.file_stem().map(|s| s.0.len()) else { return false };
        let name_start = self.trimmed().len() - self.file_name().map(|n| n.0.len()).unwrap_or(0);
        self.0.truncate(name_start + stem);
        let extension = extension.as_ref();
        if !extension.is_empty() {
            self.0.push('.');
            self.0.push_str(extension);
        }
        true
    }
    pub fn set_file_name(&mut self, name: impl AsRef<str>) {
        if self.file_name().is_some() {
            self.pop();
        }
        self.push(name.as_ref());
    }
    pub fn into_os_string(self) -> OsString {
        OsString(self.0)
    }
}

impl Deref for PathBuf {
    type Target = Path;
    fn deref(&self) -> &Path {
        Path::new(self.0.as_str())
    }
}

impl Borrow<Path> for PathBuf {
    fn borrow(&self) -> &Path {
        self
    }
}

impl ToOwned for Path {
    type Owned = PathBuf;
    fn to_owned(&self) -> PathBuf {
        self.to_path_buf()
    }
}

macro_rules! as_path {
    ($($t:ty),*) => {$(
        impl AsRef<Path> for $t {
            fn as_ref(&self) -> &Path {
                Path::new(AsRef::<str>::as_ref(self))
            }
        }
    )*};
}
as_path!(str, String, OsStr, OsString);

impl AsRef<Path> for Path {
    fn as_ref(&self) -> &Path {
        self
    }
}
impl AsRef<Path> for PathBuf {
    fn as_ref(&self) -> &Path {
        self
    }
}
impl AsRef<Path> for Cow<'_, str> {
    fn as_ref(&self) -> &Path {
        Path::new(self.as_ref() as &str)
    }
}
impl AsRef<str> for Path {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl AsRef<str> for PathBuf {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl AsRef<OsStr> for Path {
    fn as_ref(&self) -> &OsStr {
        OsStr::from_str(&self.0)
    }
}
impl AsRef<OsStr> for PathBuf {
    fn as_ref(&self) -> &OsStr {
        OsStr::from_str(&self.0)
    }
}
impl<T: AsRef<str> + ?Sized> From<&T> for PathBuf {
    fn from(s: &T) -> Self {
        PathBuf(s.as_ref().into())
    }
}
impl From<String> for PathBuf {
    fn from(s: String) -> Self {
        PathBuf(s)
    }
}
impl From<OsString> for PathBuf {
    fn from(s: OsString) -> Self {
        PathBuf(s.0)
    }
}
impl<P: AsRef<Path>> FromIterator<P> for PathBuf {
    fn from_iter<I: IntoIterator<Item = P>>(iter: I) -> Self {
        let mut buf = PathBuf::new();
        for p in iter {
            buf.push(p);
        }
        buf
    }
}
impl<P: AsRef<Path>> Extend<P> for PathBuf {
    fn extend<I: IntoIterator<Item = P>>(&mut self, iter: I) {
        for p in iter {
            self.push(p);
        }
    }
}
