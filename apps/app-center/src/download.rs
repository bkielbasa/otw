//! Downloading an installer and checking its SHA-256.
//!
//! The file is written as `<name>.part`, hashed while it streams, and only
//! renamed to its real name once the hash matches the manifest; on a
//! mismatch the partial file is deleted.

use crate::error::{Error, Result};
use crate::hash::{self, HashingWriter};
use crate::manifest::Installer;
use crate::source::Fetch;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Downloaded {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

/// Downloads `installer` into `dir`. `progress` is called with the bytes
/// so far and the total, when known.
pub fn download(
    fetch: &dyn Fetch,
    installer: &Installer,
    dir: &Path,
    fallback_name: &str,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Downloaded> {
    let expected = hash::normalize(&installer.sha256).ok_or_else(|| {
        Error::format(format!(
            "the manifest has no valid InstallerSha256 for {}; refusing to download unverified",
            installer.url
        ))
    })?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(installer.file_name(fallback_name));
    let mut part_name = path.file_name().unwrap_or_default().to_os_string();
    part_name.push(".part");
    let part = dir.join(part_name);

    let result = (|| {
        let response = fetch.open(&installer.url)?;
        let mut body = response.body;
        let mut out = HashingWriter::new(BufWriter::new(File::create(&part)?));
        let mut buf = vec![0u8; 64 * 1024];
        progress(0, response.length);
        loop {
            let n = body
                .read(&mut buf)
                .map_err(|e| Error::Http(format!("{}: {e}", installer.url)))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            progress(out.written(), response.length);
        }
        let size = out.written();
        let (writer, actual) = out.finish();
        writer
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        hash::verify(&installer.url, &expected, &actual)?;
        Ok((actual, size))
    })();

    match result {
        Ok((sha256, size)) => {
            std::fs::rename(&part, &path)?;
            Ok(Downloaded { path, sha256, size })
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::tests::{temp_dir, FakeFetch};

    fn setup(content: &[u8], sha: &str) -> (FakeFetch, Installer) {
        let mut fetch = FakeFetch::default();
        fetch.files.insert(
            "https://dl.test/files/setup-1.0.exe".into(),
            content.to_vec(),
        );
        let installer = Installer {
            url: "https://dl.test/files/setup-1.0.exe".into(),
            sha256: sha.into(),
            installer_type: "exe".into(),
            ..Default::default()
        };
        (fetch, installer)
    }

    #[test]
    fn verified_download_is_kept() {
        let dir = temp_dir("dl-ok");
        let content = b"MZ pretend installer".repeat(10_000);
        let (fetch, inst) = setup(&content, &hash::sha256_hex(&content).to_uppercase());
        let mut calls = Vec::new();
        let d = download(&fetch, &inst, &dir, "x.exe", &mut |n, t| calls.push((n, t))).unwrap();
        assert_eq!(d.path, dir.join("setup-1.0.exe"));
        assert_eq!(d.size, content.len() as u64);
        assert_eq!(std::fs::read(&d.path).unwrap(), content);
        assert_eq!(calls.first(), Some(&(0, Some(content.len() as u64))));
        assert_eq!(
            calls.last(),
            Some(&(content.len() as u64, Some(content.len() as u64)))
        );
        assert!(!dir.join("setup-1.0.exe.part").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mismatch_deletes_the_file() {
        let dir = temp_dir("dl-bad");
        let (fetch, inst) = setup(b"tampered", &hash::sha256_hex(b"original"));
        let err = download(&fetch, &inst, &dir, "x.exe", &mut |_, _| {}).unwrap_err();
        assert!(matches!(err, Error::HashMismatch { .. }), "{err}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_hash_is_refused_before_downloading() {
        let dir = temp_dir("dl-nohash");
        let (fetch, inst) = setup(b"x", "");
        assert!(matches!(
            download(&fetch, &inst, &dir, "x.exe", &mut |_, _| {}),
            Err(Error::Format(_))
        ));
        assert!(fetch.requests.borrow().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn http_errors_leave_nothing_behind() {
        let dir = temp_dir("dl-404");
        let (mut fetch, inst) = setup(b"x", &hash::sha256_hex(b"x"));
        fetch.files.clear();
        assert!(matches!(
            download(&fetch, &inst, &dir, "x.exe", &mut |_, _| {}),
            Err(Error::NotFound(_))
        ));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
