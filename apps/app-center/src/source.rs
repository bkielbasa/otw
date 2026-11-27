//! Access to a winget "source2" REST-less source, such as the community
//! repository at <https://cdn.winget.microsoft.com/cache>.
//!
//! The source is a static file tree:
//!
//! * `source2.msix` – an MSIX (ZIP) package whose `Public/index.db` is an
//!   SQLite database listing every package (id, name, moniker, latest
//!   version, tags, commands and a hash of its version list);
//! * `packages/<id>/<first 8 hex digits of that hash>/versionData.mszyml` –
//!   the package's versions (MSZIP-compressed YAML), each with the path and
//!   SHA-256 of its manifest;
//! * `manifests/...` – merged YAML manifests, one per version.

use crate::error::{Error, Result};
use crate::index::{Index, Package};
use crate::manifest::{parse_manifest, parse_version_data, Manifest, VersionEntry};
use crate::version::Version;
use crate::{hash, mszip, util, zip};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_SOURCE: &str = "https://cdn.winget.microsoft.com/cache";

/// The largest metadata file (index package, version list, manifest) read
/// into memory.
const MAX_METADATA: u64 = 128 << 20;

pub struct Response {
    pub body: Box<dyn Read + Send>,
    /// The length of the body, when the server said.
    pub length: Option<u64>,
}

/// Something that can fetch a URL: the network, or a fake in tests.
pub trait Fetch {
    fn open(&self, url: &str) -> Result<Response>;

    fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.get_with_progress(url, &mut |_, _| {})
    }

    /// Like [`Fetch::get`], calling `progress` with the bytes read so far
    /// and the total, when known.
    fn get_with_progress(
        &self,
        url: &str,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Vec<u8>> {
        let response = self.open(url)?;
        let total = response.length;
        let mut body = response.body.take(MAX_METADATA + 1);
        let mut out = Vec::with_capacity(total.unwrap_or(0).min(MAX_METADATA) as usize);
        let mut buf = vec![0u8; 64 * 1024];
        progress(0, total);
        loop {
            let n = body
                .read(&mut buf)
                .map_err(|e| Error::Http(format!("{url}: {e}")))?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            progress(out.len() as u64, total);
        }
        if out.len() as u64 > MAX_METADATA {
            return Err(Error::format(format!("{url} is too large")));
        }
        Ok(out)
    }
}

/// What [`Source::index_with_progress`] is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexProgress {
    /// Reading the cached copy.
    ReadingCache,
    /// Downloading `source2.msix`: bytes so far and the total, when known.
    Downloading { done: u64, total: Option<u64> },
    /// Unpacking and checking the downloaded index.
    Unpacking,
}

/// HTTPS through ureq with rustls and compiled-in root certificates, so
/// it needs neither OpenSSL nor the system's certificate store.
pub struct Http {
    agent: ureq::Agent,
}

impl Http {
    pub fn new() -> Http {
        Http {
            agent: ureq::AgentBuilder::new()
                .user_agent(concat!("app-center/", env!("CARGO_PKG_VERSION")))
                .timeout_connect(Duration::from_secs(30))
                .timeout_read(Duration::from_secs(120))
                .build(),
        }
    }
}

impl Default for Http {
    fn default() -> Self {
        Http::new()
    }
}

impl Fetch for Http {
    fn open(&self, url: &str) -> Result<Response> {
        let resp = self.agent.get(url).call().map_err(|e| match e {
            ureq::Error::Status(404, _) => Error::NotFound(format!("{url}: not found (404)")),
            ureq::Error::Status(code, r) => {
                Error::Http(format!("{url}: HTTP {code} {}", r.status_text()))
            }
            other => Error::Http(format!("{url}: {other}")),
        })?;
        let length = resp.header("Content-Length").and_then(|v| v.parse().ok());
        Ok(Response {
            body: Box::new(resp.into_reader()),
            length,
        })
    }
}

/// Where downloaded index files are kept: `APP_CENTER_CACHE` if set, else
/// `%LOCALAPPDATA%\AppCenter` on Windows and `$XDG_CACHE_HOME/app-center`
/// or `~/.cache/app-center` elsewhere.
pub fn default_cache_dir() -> PathBuf {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = env("APP_CENTER_CACHE") {
        return dir;
    }
    if cfg!(windows) {
        if let Some(dir) = env("LOCALAPPDATA") {
            return dir.join("AppCenter");
        }
    }
    if let Some(dir) = env("XDG_CACHE_HOME") {
        return dir.join("app-center");
    }
    if let Some(home) = env("HOME").or_else(|| env("USERPROFILE")) {
        return home.join(".cache").join("app-center");
    }
    std::env::temp_dir().join("app-center")
}

pub struct Source<F> {
    pub base: String,
    pub fetch: F,
    pub cache_dir: PathBuf,
    /// How old the cached index may get before it is downloaded again.
    pub max_age: Duration,
}

/// Where a cached index came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOrigin {
    Cache,
    Downloaded,
}

impl<F: Fetch> Source<F> {
    pub fn new(base: &str, fetch: F, cache_dir: PathBuf) -> Self {
        Source {
            base: base.trim_end_matches('/').to_string(),
            fetch,
            cache_dir,
            max_age: Duration::from_secs(24 * 60 * 60),
        }
    }

    fn cached_index_path(&self) -> PathBuf {
        self.cache_dir.join("index.db")
    }

    fn cache_is_fresh(&self, path: &Path) -> bool {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < self.max_age)
    }

    /// Downloads `source2.msix` and stores its `index.db` in the cache.
    pub fn update(&self) -> Result<Vec<u8>> {
        self.update_with_progress(&mut |_| {})
    }

    /// [`Source::update`], reporting what it is doing.
    pub fn update_with_progress(&self, progress: &mut dyn FnMut(IndexProgress)) -> Result<Vec<u8>> {
        let msix = self.fetch.get_with_progress(
            &util::join(&self.base, "source2.msix"),
            &mut |done, total| progress(IndexProgress::Downloading { done, total }),
        )?;
        progress(IndexProgress::Unpacking);
        let db = zip::Archive::open(&msix)?.read("Public/index.db")?;
        // Make sure it loads before replacing a good cached copy.
        Index::from_sqlite(&db)?;
        std::fs::create_dir_all(&self.cache_dir)?;
        let path = self.cached_index_path();
        let tmp = path.with_extension("db.part");
        std::fs::write(&tmp, &db)?;
        std::fs::rename(&tmp, &path)?;
        Ok(db)
    }

    /// The package index, from the cache when it is fresh enough.
    pub fn index(&self, force_update: bool) -> Result<(Index, IndexOrigin)> {
        self.index_with_progress(force_update, &mut |_| {})
    }

    /// [`Source::index`], reporting what it is doing.
    pub fn index_with_progress(
        &self,
        force_update: bool,
        progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<(Index, IndexOrigin)> {
        let path = self.cached_index_path();
        if !force_update && self.cache_is_fresh(&path) {
            progress(IndexProgress::ReadingCache);
            if let Ok(index) = std::fs::read(&path)
                .map_err(Error::from)
                .and_then(|d| Index::from_sqlite(&d))
            {
                return Ok((index, IndexOrigin::Cache));
            }
        }
        match self.update_with_progress(progress) {
            Ok(db) => Ok((Index::from_sqlite(&db)?, IndexOrigin::Downloaded)),
            // Offline: an old index is better than none.
            Err(e) => match std::fs::read(&path) {
                Ok(d) if !force_update => Index::from_sqlite(&d)
                    .map(|i| (i, IndexOrigin::Cache))
                    .map_err(|_| e),
                _ => Err(e),
            },
        }
    }

    /// All versions of a package, newest first.
    pub fn versions(&self, package: &Package) -> Result<Vec<VersionEntry>> {
        let url = util::join(&self.base, &package.version_data_path()?);
        let raw = self.fetch.get(&url)?;
        let actual = hash::sha256_hex(&raw);
        if hash::normalize(&package.hash).is_some() {
            hash::verify(
                &format!("version data of {}", package.id),
                &package.hash,
                &actual,
            )?;
        }
        let text = mszip::decompress_buffer(&raw)?;
        parse_version_data(&String::from_utf8_lossy(&text))
    }

    /// Picks a version: the given one (compared as a version, so "1.0"
    /// finds "1.0.0"), or else the newest.
    pub fn pick_version<'v>(
        versions: &'v [VersionEntry],
        wanted: Option<&str>,
    ) -> Result<&'v VersionEntry> {
        match wanted {
            None => versions
                .first()
                .ok_or_else(|| Error::NotFound("the package has no versions".into())),
            Some(w) => {
                let wv = Version::parse(w);
                versions
                    .iter()
                    .find(|e| e.version == w)
                    .or_else(|| versions.iter().find(|e| Version::parse(&e.version) == wv))
                    .ok_or_else(|| {
                        let list: Vec<&str> = versions
                            .iter()
                            .take(10)
                            .map(|e| e.version.as_str())
                            .collect();
                        Error::NotFound(format!(
                            "version {w} not found (newest: {})",
                            list.join(", ")
                        ))
                    })
            }
        }
    }

    /// The merged manifest of one version, checked against its SHA-256.
    pub fn manifest(&self, entry: &VersionEntry) -> Result<Manifest> {
        let raw = self
            .fetch
            .get(&util::join(&self.base, &entry.relative_path))?;
        if !entry.sha256.is_empty() {
            hash::verify(
                &format!("manifest {}", entry.relative_path),
                &entry.sha256,
                &hash::sha256_hex(&raw),
            )?;
        }
        parse_manifest(&String::from_utf8_lossy(&raw))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Serves files from memory and records what was asked for.
    #[derive(Default)]
    pub struct FakeFetch {
        pub files: HashMap<String, Vec<u8>>,
        pub requests: RefCell<Vec<String>>,
    }

    impl Fetch for FakeFetch {
        fn open(&self, url: &str) -> Result<Response> {
            self.requests.borrow_mut().push(url.to_string());
            let data = self
                .files
                .get(url)
                .cloned()
                .ok_or_else(|| Error::NotFound(format!("{url}: not found (404)")))?;
            Ok(Response {
                length: Some(data.len() as u64),
                body: Box::new(std::io::Cursor::new(data)),
            })
        }
    }

    pub fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// A ZIP file with one stored entry, standing in for source2.msix.
    pub fn make_zip(name: &str, data: &[u8]) -> Vec<u8> {
        let crc = 0u32; // not checked by the reader
        let mut out = Vec::new();
        let n = name.len() as u16;
        let size = data.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        let cd = out.len() as u32;
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&[0; 12]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        let cd_size = out.len() as u32 - cd;
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    pub const BASE: &str = "https://example.test/cache";

    /// A fake source with the fixture index, 7-Zip's version list and its
    /// 26.06 manifest.
    pub fn fake_source(cache: &Path) -> Source<FakeFetch> {
        let mut fetch = FakeFetch::default();
        fetch.files.insert(
            format!("{BASE}/source2.msix"),
            make_zip("Public/index.db", &fixture("index.db")),
        );
        fetch.files.insert(
            format!("{BASE}/packages/7zip.7zip/a701c043/versionData.mszyml"),
            fixture("7zip.versionData.mszyml"),
        );
        fetch.files.insert(
            format!("{BASE}/manifests/7/7zip/7zip/26.06/67fd"),
            fixture("7zip.7zip-26.06.yaml"),
        );
        Source::new(&format!("{BASE}/"), fetch, cache.to_path_buf())
    }

    pub fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("app-center-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn downloads_and_caches_the_index() {
        let cache = temp_dir("cache");
        let src = fake_source(&cache);
        let (index, origin) = src.index(false).unwrap();
        assert_eq!(origin, IndexOrigin::Downloaded);
        assert!(index.get("7zip.7zip").is_some());
        assert!(cache.join("index.db").exists());
        let (_, origin) = src.index(false).unwrap();
        assert_eq!(origin, IndexOrigin::Cache);
        assert_eq!(src.fetch.requests.borrow().len(), 1);
        let (_, origin) = src.index(true).unwrap();
        assert_eq!(origin, IndexOrigin::Downloaded);
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn index_progress_reports_download_then_cache() {
        let cache = temp_dir("progress");
        let src = fake_source(&cache);
        let mut seen = Vec::new();
        src.index_with_progress(false, &mut |p| seen.push(p))
            .unwrap();
        let size = make_zip("Public/index.db", &fixture("index.db")).len() as u64;
        assert_eq!(
            seen.first(),
            Some(&IndexProgress::Downloading {
                done: 0,
                total: Some(size)
            })
        );
        assert!(seen.contains(&IndexProgress::Downloading {
            done: size,
            total: Some(size)
        }));
        assert_eq!(seen.last(), Some(&IndexProgress::Unpacking));
        seen.clear();
        src.index_with_progress(false, &mut |p| seen.push(p))
            .unwrap();
        assert_eq!(seen, vec![IndexProgress::ReadingCache]);
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn get_with_progress_counts_bytes() {
        let mut fetch = FakeFetch::default();
        fetch.files.insert("u".into(), vec![7u8; 200_000]);
        let mut calls = Vec::new();
        let data = fetch
            .get_with_progress("u", &mut |n, t| calls.push((n, t)))
            .unwrap();
        assert_eq!(data.len(), 200_000);
        assert_eq!(calls.first(), Some(&(0, Some(200_000))));
        assert_eq!(calls.last(), Some(&(200_000, Some(200_000))));
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[test]
    fn stale_cache_is_used_when_offline() {
        let cache = temp_dir("offline");
        let mut src = fake_source(&cache);
        src.index(false).unwrap();
        src.fetch.files.clear();
        src.max_age = Duration::ZERO;
        let (_, origin) = src.index(false).unwrap();
        assert_eq!(origin, IndexOrigin::Cache);
        assert!(src.index(true).is_err());
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn corrupt_index_download_is_rejected() {
        let cache = temp_dir("corrupt");
        let mut src = fake_source(&cache);
        src.fetch.files.insert(
            format!("{BASE}/source2.msix"),
            make_zip("Public/index.db", b"garbage"),
        );
        assert!(src.index(false).is_err());
        assert!(!cache.join("index.db").exists());
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn versions_and_manifest() {
        let cache = temp_dir("versions");
        let src = fake_source(&cache);
        let (index, _) = src.index(false).unwrap();
        let pkg = index.get("7zip.7zip").unwrap();
        let versions = src.versions(pkg).unwrap();
        assert_eq!(versions[0].version, "26.06");
        let entry = Source::<FakeFetch>::pick_version(&versions, None).unwrap();
        let m = src.manifest(entry).unwrap();
        assert_eq!(m.id, "7zip.7zip");
        assert_eq!(m.version, "26.06");
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn tampered_manifest_is_rejected() {
        let cache = temp_dir("tamper");
        let mut src = fake_source(&cache);
        let mut m = fixture("7zip.7zip-26.06.yaml");
        m.extend_from_slice(b"\n# extra\n");
        src.fetch
            .files
            .insert(format!("{BASE}/manifests/7/7zip/7zip/26.06/67fd"), m);
        let (index, _) = src.index(false).unwrap();
        let versions = src.versions(index.get("7zip.7zip").unwrap()).unwrap();
        assert!(matches!(
            src.manifest(&versions[0]),
            Err(Error::HashMismatch { .. })
        ));
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn tampered_version_data_is_rejected() {
        let cache = temp_dir("tamper-vd");
        let mut src = fake_source(&cache);
        let mut vd = fixture("7zip.versionData.mszyml");
        vd[30] ^= 1;
        src.fetch.files.insert(
            format!("{BASE}/packages/7zip.7zip/a701c043/versionData.mszyml"),
            vd,
        );
        let (index, _) = src.index(false).unwrap();
        assert!(matches!(
            src.versions(index.get("7zip.7zip").unwrap()),
            Err(Error::HashMismatch { .. })
        ));
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn picking_versions() {
        let e = |v: &str| VersionEntry {
            version: v.into(),
            relative_path: format!("m/{v}"),
            sha256: String::new(),
        };
        let list = [e("2.0.0"), e("1.10"), e("1.9")];
        let pick = |w| Source::<FakeFetch>::pick_version(&list, w);
        assert_eq!(pick(None).unwrap().version, "2.0.0");
        assert_eq!(pick(Some("1.9")).unwrap().version, "1.9");
        assert_eq!(pick(Some("2")).unwrap().version, "2.0.0");
        let err = pick(Some("3")).unwrap_err().to_string();
        assert!(err.contains("2.0.0, 1.10, 1.9"), "{err}");
        assert!(Source::<FakeFetch>::pick_version(&[], None).is_err());
    }
}
