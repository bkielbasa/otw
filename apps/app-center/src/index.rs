//! The package index: the `packages` table of winget's source2 `index.db`
//! plus each package's tags and commands.

use crate::error::{Error, Result};
use crate::hash::to_hex;
use crate::sqlite::{Database, Row};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub moniker: Option<String>,
    pub latest_version: String,
    /// SHA-256 of the package's version data, hex. Its first 8 digits are
    /// part of the version data's URL.
    pub hash: String,
    pub tags: Vec<String>,
    pub commands: Vec<String>,
}

impl Package {
    /// Path of the package's version list, relative to the source's base URL.
    pub fn version_data_path(&self) -> Result<String> {
        if self.hash.len() < 8 {
            return Err(Error::format(format!(
                "{} has no version data hash",
                self.id
            )));
        }
        Ok(format!(
            "packages/{}/{}/versionData.mszyml",
            crate::util::encode_segment(&self.id),
            &self.hash[..8]
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Index {
    pub packages: Vec<Package>,
}

/// Reads a lookup table such as `tags2` (rowid → text) and its map table
/// such as `tags2_map` ((text rowid, package rowid)) into package → texts.
fn many_to_many(db: &Database, values: &str, map: &str) -> Result<HashMap<i64, Vec<String>>> {
    let mut names = HashMap::new();
    for row in db.table(values)? {
        if let (Some(id), Some(text)) = (row.rowid, row.get(1).as_text()) {
            names.insert(id, text.to_string());
        }
    }
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    for row in db.table(map)? {
        let (Some(value), Some(pkg)) = (row.get(0).as_int(), row.get(1).as_int()) else {
            continue;
        };
        if let Some(name) = names.get(&value) {
            out.entry(pkg).or_default().push(name.clone());
        }
    }
    Ok(out)
}

fn text(row: &Row, i: usize) -> Option<String> {
    row.get(i)
        .as_text()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

impl Index {
    /// Loads the index from the bytes of a source2 `index.db`.
    pub fn from_sqlite(data: &[u8]) -> Result<Index> {
        let db = Database::open(data)?;
        let major = db
            .table("metadata")?
            .into_iter()
            .find(|r| r.get(0).as_text() == Some("majorVersion"))
            .and_then(|r| text(&r, 1));
        if major.as_deref() != Some("2") {
            return Err(Error::format(format!(
                "index schema version {} is not supported (expected 2)",
                major.unwrap_or_else(|| "unknown".into())
            )));
        }
        let mut tags = many_to_many(&db, "tags2", "tags2_map")?;
        let mut commands = many_to_many(&db, "commands2", "commands2_map")?;
        let mut packages = Vec::new();
        for row in db.table("packages")? {
            let Some(rowid) = row.rowid else { continue };
            let (Some(id), Some(name)) = (text(&row, 1), text(&row, 2)) else {
                continue;
            };
            packages.push(Package {
                id,
                name,
                moniker: text(&row, 3),
                latest_version: text(&row, 4).unwrap_or_default(),
                hash: row.get(7).as_blob().map(to_hex).unwrap_or_default(),
                tags: tags.remove(&rowid).unwrap_or_default(),
                commands: commands.remove(&rowid).unwrap_or_default(),
            });
        }
        Ok(Index { packages })
    }

    /// Finds a package by its identifier, ignoring case like winget does.
    pub fn get(&self, id: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.id.eq_ignore_ascii_case(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn fixture_index() -> Index {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/index.db"
        ))
        .unwrap();
        Index::from_sqlite(&data).unwrap()
    }

    #[test]
    fn loads_packages_with_tags_and_commands() {
        let index = fixture_index();
        assert_eq!(index.packages.len(), 611);
        let seven = index.get("7zip.7zip").unwrap();
        assert_eq!(seven.name, "7-Zip");
        assert_eq!(seven.moniker.as_deref(), Some("7zip"));
        assert_eq!(seven.latest_version, "26.06");
        assert!(seven.hash.starts_with("a701c043"));
        assert!(
            seven.tags.iter().any(|t| t == "archiver"),
            "{:?}",
            seven.tags
        );
        assert!(
            seven.commands.iter().any(|c| c == "7z"),
            "{:?}",
            seven.commands
        );
    }

    #[test]
    fn lookup_ignores_case() {
        let index = fixture_index();
        assert_eq!(
            index.get("notepad++.notepad++").unwrap().id,
            "Notepad++.Notepad++"
        );
        assert!(index.get("No.Such.Package").is_none());
    }

    #[test]
    fn version_data_path() {
        let index = fixture_index();
        assert_eq!(
            index.get("7zip.7zip").unwrap().version_data_path().unwrap(),
            "packages/7zip.7zip/a701c043/versionData.mszyml"
        );
        assert!(Package::default().version_data_path().is_err());
    }

    #[test]
    fn synthetic_tag_map_spans_many_pages() {
        let index = fixture_index();
        let tagged = index
            .packages
            .iter()
            .filter(|p| p.tags.iter().any(|t| t == "synthetic-tag"))
            .count();
        assert_eq!(tagged, 600);
    }

    #[test]
    fn rejects_other_databases() {
        assert!(Index::from_sqlite(b"nope").is_err());
    }
}
