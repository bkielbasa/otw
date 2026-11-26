//! Input and output traits, errors, and the standard streams (the log).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    NotFound,
    PermissionDenied,
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    NotConnected,
    AddrInUse,
    AddrNotAvailable,
    BrokenPipe,
    AlreadyExists,
    WouldBlock,
    InvalidInput,
    InvalidData,
    TimedOut,
    WriteZero,
    Interrupted,
    Unsupported,
    UnexpectedEof,
    OutOfMemory,
    Other,
}

pub struct Error {
    kind: ErrorKind,
    message: Option<Box<dyn core::error::Error + Send + Sync>>,
}

impl Error {
    pub fn new<E: Into<Box<dyn core::error::Error + Send + Sync>>>(kind: ErrorKind, error: E) -> Self {
        Self { kind, message: Some(error.into()) }
    }
    pub fn other<E: Into<Box<dyn core::error::Error + Send + Sync>>>(error: E) -> Self {
        Self::new(ErrorKind::Other, error)
    }
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
    pub fn raw_os_error(&self) -> Option<i32> {
        None
    }
    pub fn last_os_error() -> Self {
        Self::from(ErrorKind::Other)
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Self { kind, message: None }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.message {
            Some(message) => write!(f, "{message}"),
            None => write!(f, "{:?}", self.kind),
        }
    }
}

impl core::error::Error for Error {}

pub trait Read {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize>;
    fn read_exact(&mut self, mut buffer: &mut [u8]) -> Result<()> {
        while !buffer.is_empty() {
            match self.read(buffer)? {
                0 => return Err(Error::from(ErrorKind::UnexpectedEof)),
                n => buffer = &mut buffer[n..],
            }
        }
        Ok(())
    }
    fn read_to_end(&mut self, out: &mut Vec<u8>) -> Result<usize> {
        let start = out.len();
        let mut chunk = [0u8; 4096];
        loop {
            match self.read(&mut chunk)? {
                0 => return Ok(out.len() - start),
                n => out.extend_from_slice(&chunk[..n]),
            }
        }
    }
    fn read_to_string(&mut self, out: &mut String) -> Result<usize> {
        let mut bytes = Vec::new();
        let n = self.read_to_end(&mut bytes)?;
        out.push_str(&String::from_utf8(bytes).map_err(|_| Error::from(ErrorKind::InvalidData))?);
        Ok(n)
    }
    fn take(self, limit: u64) -> Take<Self>
    where
        Self: Sized,
    {
        Take { inner: self, limit }
    }
}

pub struct Take<R> {
    inner: R,
    limit: u64,
}

impl<R: Read> Read for Take<R> {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
        let n = (buffer.len() as u64).min(self.limit) as usize;
        let n = self.inner.read(&mut buffer[..n])?;
        self.limit -= n as u64;
        Ok(n)
    }
}

pub trait Write {
    fn write(&mut self, buffer: &[u8]) -> Result<usize>;
    fn flush(&mut self) -> Result<()>;
    fn write_all(&mut self, mut buffer: &[u8]) -> Result<()> {
        while !buffer.is_empty() {
            match self.write(buffer)? {
                0 => return Err(Error::from(ErrorKind::WriteZero)),
                n => buffer = &buffer[n..],
            }
        }
        Ok(())
    }
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> Result<()> {
        let mut text = String::new();
        let _ = fmt::write(&mut text, args);
        self.write_all(text.as_bytes())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekFrom {
    Start(u64),
    End(i64),
    Current(i64),
}

pub trait Seek {
    fn seek(&mut self, from: SeekFrom) -> Result<u64>;
    fn stream_position(&mut self) -> Result<u64> {
        self.seek(SeekFrom::Current(0))
    }
    fn rewind(&mut self) -> Result<()> {
        self.seek(SeekFrom::Start(0)).map(|_| ())
    }
}

impl Read for &[u8] {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
        let n = buffer.len().min(self.len());
        buffer[..n].copy_from_slice(&self[..n]);
        *self = &self[n..];
        Ok(n)
    }
}

impl Write for Vec<u8> {
    fn write(&mut self, buffer: &[u8]) -> Result<usize> {
        self.extend_from_slice(buffer);
        Ok(buffer.len())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

pub struct Cursor<T> {
    inner: T,
    position: u64,
}

impl<T> Cursor<T> {
    pub fn new(inner: T) -> Self {
        Self { inner, position: 0 }
    }
    pub fn into_inner(self) -> T {
        self.inner
    }
    pub fn get_ref(&self) -> &T {
        &self.inner
    }
    pub fn position(&self) -> u64 {
        self.position
    }
    pub fn set_position(&mut self, position: u64) {
        self.position = position;
    }
}

impl<T: AsRef<[u8]>> Read for Cursor<T> {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
        let data = self.inner.as_ref();
        let start = (self.position as usize).min(data.len());
        let n = buffer.len().min(data.len() - start);
        buffer[..n].copy_from_slice(&data[start..start + n]);
        self.position += n as u64;
        Ok(n)
    }
}

impl<T: AsRef<[u8]>> Seek for Cursor<T> {
    fn seek(&mut self, from: SeekFrom) -> Result<u64> {
        let len = self.inner.as_ref().len() as i64;
        let position = match from {
            SeekFrom::Start(o) => o as i64,
            SeekFrom::End(o) => len + o,
            SeekFrom::Current(o) => self.position as i64 + o,
        };
        if position < 0 {
            return Err(Error::from(ErrorKind::InvalidInput));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}

/// Standard output and error: both go to the system's log.
pub struct Stdout;
pub struct Stderr;
pub type StdoutLock<'a> = Stdout;
pub type StderrLock<'a> = Stderr;

pub fn stdout() -> Stdout {
    Stdout
}

pub fn stderr() -> Stderr {
    Stderr
}

impl Stdout {
    pub fn lock(&self) -> Stdout {
        Stdout
    }
}

impl Stderr {
    pub fn lock(&self) -> Stderr {
        Stderr
    }
}

impl Write for Stdout {
    fn write(&mut self, buffer: &[u8]) -> Result<usize> {
        super::system().log(&String::from_utf8_lossy(buffer));
        Ok(buffer.len())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Write for Stderr {
    fn write(&mut self, buffer: &[u8]) -> Result<usize> {
        super::system().log(&String::from_utf8_lossy(buffer));
        Ok(buffer.len())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

pub struct Stdin;

pub fn stdin() -> Stdin {
    Stdin
}

impl Read for Stdin {
    fn read(&mut self, _: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
}
