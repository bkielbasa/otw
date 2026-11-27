//! Searching the index.
//!
//! Every package gets a score for how well the query matches it; the best
//! match field wins. From best to worst: exact identifier, exact moniker,
//! exact name, a match after removing punctuation ("7zip" = "7-Zip"),
//! a prefix of the name, identifier part or moniker, a substring, a tag or
//! command, and finally (for queries of several words) every word found
//! somewhere. Ties go to the shorter name, then the identifier.

use crate::index::{Index, Package};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchKind {
    AllWords = 1,
    TagSubstring = 2,
    Command = 3,
    Tag = 4,
    Substring = 5,
    Prefix = 6,
    Normalized = 7,
    Name = 8,
    Moniker = 9,
    Id = 10,
}

impl MatchKind {
    pub fn label(self) -> &'static str {
        match self {
            MatchKind::Id => "id",
            MatchKind::Moniker => "moniker",
            MatchKind::Name => "name",
            MatchKind::Normalized => "name",
            MatchKind::Prefix => "prefix",
            MatchKind::Substring => "substring",
            MatchKind::Tag => "tag",
            MatchKind::Command => "command",
            MatchKind::TagSubstring => "tag",
            MatchKind::AllWords => "words",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hit<'a> {
    pub package: &'a Package,
    pub kind: MatchKind,
}

/// Lower case, letters and digits only.
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn score(p: &Package, query: &str) -> Option<MatchKind> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    let nq = normalize(&q);
    let id = p.id.to_lowercase();
    let name = p.name.to_lowercase();
    let moniker = p.moniker.as_deref().unwrap_or("").to_lowercase();

    if id == q {
        return Some(MatchKind::Id);
    }
    if !moniker.is_empty() && moniker == q {
        return Some(MatchKind::Moniker);
    }
    if name == q {
        return Some(MatchKind::Name);
    }
    // The last part of the identifier is often the product ("7zip.7zip").
    let id_last = id.rsplit('.').next().unwrap_or("");
    if !nq.is_empty()
        && [&name, &moniker, &id_last.to_string()]
            .iter()
            .any(|s| normalize(s) == nq)
    {
        return Some(MatchKind::Normalized);
    }
    let prefix = |s: &str| {
        !s.is_empty() && (s.starts_with(&q) || (!nq.is_empty() && normalize(s).starts_with(&nq)))
    };
    if prefix(&name) || prefix(&moniker) || id.split('.').any(prefix) {
        return Some(MatchKind::Prefix);
    }
    let contains = |s: &str| s.contains(&q) || (!nq.is_empty() && normalize(s).contains(&nq));
    if contains(&name) || contains(&id) || (!moniker.is_empty() && contains(&moniker)) {
        return Some(MatchKind::Substring);
    }
    let tags: Vec<String> = p.tags.iter().map(|t| t.to_lowercase()).collect();
    if tags.iter().any(|t| *t == q) {
        return Some(MatchKind::Tag);
    }
    if p.commands.iter().any(|c| c.to_lowercase() == q) {
        return Some(MatchKind::Command);
    }
    if q.len() >= 3 && tags.iter().any(|t| t.contains(&q)) {
        return Some(MatchKind::TagSubstring);
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    if words.len() > 1 {
        let hay = format!("{id} {name} {moniker} {}", tags.join(" "));
        if words.iter().all(|w| hay.contains(w)) {
            return Some(MatchKind::AllWords);
        }
    }
    None
}

/// The packages matching `query`, best first.
pub fn search<'a>(index: &'a Index, query: &str) -> Vec<Hit<'a>> {
    let mut hits: Vec<Hit> = index
        .packages
        .iter()
        .filter_map(|p| score(p, query).map(|kind| Hit { package: p, kind }))
        .collect();
    hits.sort_by(|a, b| {
        b.kind
            .cmp(&a.kind)
            .then_with(|| a.package.name.len().cmp(&b.package.name.len()))
            .then_with(|| {
                a.package
                    .id
                    .to_lowercase()
                    .cmp(&b.package.id.to_lowercase())
            })
    });
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(
        id: &str,
        name: &str,
        moniker: Option<&str>,
        tags: &[&str],
        commands: &[&str],
    ) -> Package {
        Package {
            id: id.into(),
            name: name.into(),
            moniker: moniker.map(Into::into),
            latest_version: "1".into(),
            hash: "0".repeat(64),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            commands: commands.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn sample() -> Index {
        Index {
            packages: vec![
                pkg(
                    "7zip.7zip",
                    "7-Zip",
                    Some("7zip"),
                    &["archiver", "zip"],
                    &["7z"],
                ),
                pkg("7zip.7zr", "7zr", Some("7zr"), &[], &[]),
                pkg(
                    "M2Team.NanaZip",
                    "NanaZip",
                    None,
                    &["7zip", "archiver"],
                    &[],
                ),
                pkg(
                    "Notepad++.Notepad++",
                    "Notepad++",
                    Some("notepad++"),
                    &["editor"],
                    &[],
                ),
                pkg(
                    "dail8859.NotepadNext",
                    "Notepad Next",
                    None,
                    &["notepad++"],
                    &[],
                ),
                pkg(
                    "Some.Archiver",
                    "Big Archiver Suite",
                    None,
                    &["compression-tools"],
                    &[],
                ),
            ],
        }
    }

    fn ids<'a>(hits: &[Hit<'a>]) -> Vec<&'a str> {
        hits.iter().map(|h| h.package.id.as_str()).collect()
    }

    #[test]
    fn exact_matches() {
        let idx = sample();
        let p = &idx.packages[0];
        assert_eq!(score(p, "7zip.7zip"), Some(MatchKind::Id));
        assert_eq!(score(p, "7ZIP"), Some(MatchKind::Moniker));
        assert_eq!(score(p, "7-zip"), Some(MatchKind::Name));
        assert_eq!(score(p, "7 zip"), Some(MatchKind::Normalized));
    }

    #[test]
    fn seven_zip_ranks_first() {
        let idx = sample();
        let hits = search(&idx, "7zip");
        assert_eq!(ids(&hits), ["7zip.7zip", "7zip.7zr", "M2Team.NanaZip"]);
        assert_eq!(hits[1].kind, MatchKind::Prefix);
        assert_eq!(hits[2].kind, MatchKind::Tag);
    }

    #[test]
    fn notepad_plus_plus_ranks_first() {
        let idx = sample();
        let hits = search(&idx, "notepad++");
        assert_eq!(ids(&hits)[0], "Notepad++.Notepad++");
        assert!(ids(&hits).contains(&"dail8859.NotepadNext"));
    }

    #[test]
    fn substring_tag_command_and_words() {
        let idx = sample();
        assert_eq!(score(&idx.packages[2], "nana"), Some(MatchKind::Prefix));
        assert_eq!(score(&idx.packages[2], "team"), Some(MatchKind::Substring));
        assert_eq!(score(&idx.packages[0], "7z"), Some(MatchKind::Prefix));
        assert_eq!(
            score(&idx.packages[5], "compression"),
            Some(MatchKind::TagSubstring)
        );
        assert_eq!(
            score(&idx.packages[5], "suite archiver"),
            Some(MatchKind::AllWords)
        );
        assert_eq!(score(&idx.packages[5], "suite banana"), None);
    }

    #[test]
    fn commands_match_exactly() {
        let idx = Index {
            packages: vec![pkg("A.Tool", "Tool", None, &[], &["frob"])],
        };
        assert_eq!(score(&idx.packages[0], "frob"), Some(MatchKind::Command));
        assert_eq!(score(&idx.packages[0], "fro"), None);
    }

    #[test]
    fn empty_query_matches_nothing() {
        let idx = sample();
        assert!(search(&idx, "  ").is_empty());
        assert!(search(&idx, "zzzzzz").is_empty());
    }

    #[test]
    fn ties_prefer_shorter_names() {
        let idx = Index {
            packages: vec![
                pkg("B.LongerEditor", "Editor Deluxe", None, &[], &[]),
                pkg("A.Editor", "Editor X", None, &[], &[]),
            ],
        };
        assert_eq!(ids(&search(&idx, "edit")), ["A.Editor", "B.LongerEditor"]);
    }

    #[test]
    fn search_the_fixture_index() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/index.db"
        ))
        .unwrap();
        let idx = Index::from_sqlite(&data).unwrap();
        assert_eq!(ids(&search(&idx, "7zip"))[0], "7zip.7zip");
        let np = search(&idx, "notepad++");
        assert_eq!(np[0].package.id, "Notepad++.Notepad++");
        let firefox = search(&idx, "firefox");
        assert_eq!(firefox[0].package.id, "Mozilla.Firefox");
    }
}
