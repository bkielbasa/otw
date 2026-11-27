//! The error type shared by the whole library.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// Reading or writing a local file failed.
    Io(std::io::Error),
    /// A network request failed.
    Http(String),
    /// Data from the source was not in the expected format.
    Format(String),
    /// A package, version or installer was not found.
    NotFound(String),
    /// A downloaded file did not have the expected SHA-256 hash.
    HashMismatch {
        what: String,
        expected: String,
        actual: String,
    },
    /// The command line was wrong.
    Usage(String),
    /// The user stopped the operation.
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn format(msg: impl Into<String>) -> Error {
        Error::Format(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Http(e) => write!(f, "network error: {e}"),
            Error::Format(e) => write!(f, "bad data: {e}"),
            Error::NotFound(e) => write!(f, "{e}"),
            Error::HashMismatch {
                what,
                expected,
                actual,
            } => write!(
                f,
                "SHA-256 mismatch for {what}: expected {expected}, got {actual}"
            ),
            Error::Usage(e) => write!(f, "{e}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
