//! SHA-256 helpers.

use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::io::{self, Write};

pub fn sha256_hex(data: &[u8]) -> String {
    to_hex(&Sha256::digest(data))
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lower-cases a hex hash and checks that it is 64 hex digits.
pub fn normalize(hash: &str) -> Option<String> {
    let h = hash.trim().to_ascii_lowercase();
    (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

/// Checks `actual` (hex) against `expected` (hex, any case).
pub fn verify(what: &str, expected: &str, actual: &str) -> Result<()> {
    let exp = normalize(expected)
        .ok_or_else(|| Error::format(format!("{what}: {expected:?} is not a SHA-256 hash")))?;
    if exp == actual.to_ascii_lowercase() {
        Ok(())
    } else {
        Err(Error::HashMismatch {
            what: what.to_string(),
            expected: exp,
            actual: actual.to_ascii_lowercase(),
        })
    }
}

/// A writer that hashes everything written through it.
pub struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        HashingWriter {
            inner,
            hasher: Sha256::new(),
            written: 0,
        }
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// The writer and the hex SHA-256 of what was written.
    pub fn finish(self) -> (W, String) {
        (self.inner, to_hex(&self.hasher.finalize()))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn known_digest() {
        assert_eq!(sha256_hex(b"abc"), ABC);
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn verify_ignores_case() {
        verify("x", &ABC.to_uppercase(), ABC).unwrap();
        verify("x", ABC, &ABC.to_uppercase()).unwrap();
    }

    #[test]
    fn verify_reports_mismatch() {
        let other = sha256_hex(b"abd");
        match verify("file.exe", ABC, &other) {
            Err(Error::HashMismatch {
                what,
                expected,
                actual,
            }) => {
                assert_eq!(what, "file.exe");
                assert_eq!(expected, ABC);
                assert_eq!(actual, other);
            }
            r => panic!("unexpected {r:?}"),
        }
    }

    #[test]
    fn verify_rejects_malformed_expected_hash() {
        assert!(matches!(verify("x", "abc", ABC), Err(Error::Format(_))));
        assert!(normalize(&"g".repeat(64)).is_none());
    }

    #[test]
    fn hashing_writer_matches_one_shot() {
        let mut w = HashingWriter::new(Vec::new());
        w.write_all(b"a").unwrap();
        w.write_all(b"bc").unwrap();
        assert_eq!(w.written(), 3);
        let (buf, hash) = w.finish();
        assert_eq!(buf, b"abc");
        assert_eq!(hash, ABC);
    }
}
