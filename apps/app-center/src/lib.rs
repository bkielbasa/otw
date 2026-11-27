//! App Center: find and download Windows applications listed in the winget
//! community repository.
//!
//! The library reads the repository's "source2" files from
//! `cdn.winget.microsoft.com` ([`source`]): the SQLite package index inside
//! `source2.msix` ([`zip`], [`sqlite`], [`index`]), each package's
//! MSZIP-compressed version list ([`mszip`]) and the YAML manifests
//! ([`manifest`]). It searches the index ([`search`]), picks the installer
//! that suits OTW best ([`select`]) and downloads it, checking its SHA-256
//! ([`download`], [`hash`]).

pub mod cli;
pub mod download;
pub mod error;
pub mod hash;
pub mod index;
pub mod manifest;
pub mod mszip;
pub mod search;
pub mod select;
pub mod source;
pub mod sqlite;
pub mod util;
pub mod version;
pub mod zip;

pub use error::{Error, Result};

use index::{Index, Package};
use search::MatchKind;

/// Finds the package a user means by `query`: an identifier (any case), or
/// a moniker or name that only one package has.
pub fn resolve<'a>(index: &'a Index, query: &str) -> Result<&'a Package> {
    if let Some(p) = index.get(query) {
        return Ok(p);
    }
    let hits = search::search(index, query);
    let Some(best) = hits.first() else {
        return Err(Error::NotFound(format!("no package matches {query:?}")));
    };
    let strong = |k: MatchKind| k >= MatchKind::Name;
    let tied = hits.iter().take_while(|h| h.kind == best.kind).count();
    if strong(best.kind) && tied == 1 {
        return Ok(best.package);
    }
    let candidates: Vec<String> = hits
        .iter()
        .take(8)
        .map(|h| format!("  {}  ({})", h.package.id, h.package.name))
        .collect();
    Err(Error::NotFound(format!(
        "{query:?} is not a package identifier; did you mean one of these?\n{}",
        candidates.join("\n")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/index.db"
        ))
        .unwrap();
        Index::from_sqlite(&data).unwrap()
    }

    #[test]
    fn resolves_ids_monikers_and_names() {
        let idx = index();
        assert_eq!(resolve(&idx, "7ZIP.7ZIP").unwrap().id, "7zip.7zip");
        assert_eq!(resolve(&idx, "7zip").unwrap().id, "7zip.7zip");
        assert_eq!(
            resolve(&idx, "notepad++").unwrap().id,
            "Notepad++.Notepad++"
        );
        assert_eq!(
            resolve(&idx, "Notepad Next").unwrap().id,
            "dail8859.NotepadNext"
        );
    }

    #[test]
    fn ambiguous_or_weak_matches_list_candidates() {
        let idx = index();
        let err = resolve(&idx, "notepad").unwrap_err().to_string();
        assert!(err.contains("did you mean"), "{err}");
        assert!(err.contains("Notepad++.Notepad++"), "{err}");
        assert!(matches!(resolve(&idx, "qqqqqq"), Err(Error::NotFound(_))));
    }
}
