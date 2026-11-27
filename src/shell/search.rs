//! Searching programs from the Menu button's menu: what can be found
//! (desktop shortcuts, the Start Menu's Programs, programs and shortcuts
//! anywhere on drive C:, and a few of the shell's own commands), and how
//! matches are ranked.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::collections::HashSet;

use super::{
    icons::{self, Builtin},
    popup::{Command, Item},
    setup,
    volume::{extension, join, parse_path, path_text, VolPath},
    stem, Shell, Target,
};

/// How many matches the menu shows.
pub const MAX_RESULTS: usize = 10;
/// How deep and how wide the scan of drive C: goes.
const SCAN_DEPTH: usize = 6;
const SCAN_DIRS: usize = 800;

/// Where a candidate comes from; earlier sources rank first among equally
/// good matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Desktop shortcuts and the Start Menu's Programs.
    Shortcut,
    /// The shell's own commands (Run..., Windows Explorer, ...).
    Shell,
    /// A program or shortcut found on drive C:.
    Drive,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub name: String,
    pub command: Command,
    /// The file it opens, for its icon.
    pub path: Option<VolPath>,
    pub icon: Option<Builtin>,
    pub source: Source,
}

/// How well `name` matches what was typed, lower is better: the whole
/// name, its start, the start of a later word, anywhere, every typed word
/// somewhere, the typed letters in order. `None` when it does not match.
pub fn match_rank(query: &str, name: &str) -> Option<u32> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return None;
    }
    let name = name.to_ascii_lowercase();
    if name == query {
        return Some(0);
    }
    if name.starts_with(&query) {
        return Some(1);
    }
    let bytes = name.as_bytes();
    if name
        .match_indices(&query)
        .any(|(at, _)| at > 0 && !bytes[at - 1].is_ascii_alphanumeric())
    {
        return Some(2);
    }
    if name.contains(&query) {
        return Some(3);
    }
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.len() > 1 && words.iter().all(|word| name.contains(word)) {
        return Some(4);
    }
    if query.len() >= 2 && !query.contains(' ') {
        let mut letters = name.chars();
        if query.chars().all(|c| letters.any(|n| n == c)) {
            return Some(5);
        }
    }
    None
}

/// The candidates matching `query`, best first, at most [`MAX_RESULTS`]:
/// by how well they match, then by source, then shorter names first.
pub fn find<'a>(candidates: &'a [Candidate], query: &str) -> Vec<&'a Candidate> {
    let mut found: Vec<(u32, &Candidate)> = candidates
        .iter()
        .filter_map(|c| match_rank(query, &c.name).map(|rank| (rank, c)))
        .collect();
    found.sort_by(|(ra, a), (rb, b)| {
        (ra, a.source, a.name.len(), a.name.to_ascii_lowercase()).cmp(&(
            rb,
            b.source,
            b.name.len(),
            b.name.to_ascii_lowercase(),
        ))
    });
    found
        .into_iter()
        .map(|(_, c)| c)
        .take(MAX_RESULTS)
        .collect()
}

/// A program's name as the menu shows it: without `.exe` or `.lnk`.
fn display_name(file: &str) -> String {
    if matches!(extension(file).as_str(), "LNK" | "EXE") {
        stem(file).to_string()
    } else {
        file.to_string()
    }
}

impl Shell {
    /// Everything the search can find, gathered once and kept until the
    /// files change (`refresh`).
    pub(super) fn search_candidates(&self) -> Vec<Candidate> {
        if let Some(cached) = self.search_index.borrow().as_ref() {
            return cached.clone();
        }
        let index = self.gather_candidates();
        *self.search_index.borrow_mut() = Some(index.clone());
        index
    }

    fn gather_candidates(&self) -> Vec<Candidate> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        // What a file starts: a shortcut's target, else the file itself.
        let mut add = |shell: &Shell, out: &mut Vec<Candidate>, path: VolPath, source: Source| {
            let name = path.last().cloned().unwrap_or_default();
            let key = match extension(&name).as_str() {
                "LNK" => shell
                    .resolve_link(&path)
                    .map(|(target, arguments)| {
                        format!("{} {arguments}", path_text(&target)).trim_end().to_string()
                    })
                    .unwrap_or_else(|| path_text(&path)),
                _ => path_text(&path),
            }
            .to_ascii_lowercase();
            if !seen.insert(key) {
                return;
            }
            out.push(Candidate {
                name: display_name(&name),
                command: Command::Open(path.clone()),
                path: Some(path),
                icon: None,
                source,
            });
        };
        for icon in &self.icons {
            if let (Target::Path(path), false) = (&icon.target, icon.is_dir) {
                add(self, &mut out, path.clone(), Source::Shortcut);
            }
        }
        for (_, path) in &self.programs {
            add(self, &mut out, path.clone(), Source::Shortcut);
        }
        if let Some(volume) = &self.volume {
            // The Start Menu's Programs, folders and all.
            let mut pending = vec![(parse_path(setup::PROGRAMS), 0)];
            while let Some((dir, depth)) = pending.pop() {
                for entry in volume.list(&dir).unwrap_or_default() {
                    let path = join(&dir, &entry.name);
                    if entry.is_dir {
                        if depth < SCAN_DEPTH {
                            pending.push((path, depth + 1));
                        }
                    } else {
                        add(self, &mut out, path, Source::Shortcut);
                    }
                }
            }
        }
        let mut builtins = vec![
            ("Run...", Command::Run, Builtin::Run),
            ("Shut Down...", Command::ShutDown, Builtin::ShutDown),
            ("Help", Command::About, Builtin::TextDocument),
        ];
        if self.volume.is_some() {
            builtins.insert(0, ("Windows Explorer", Command::Explore(Vec::new()), Builtin::Computer));
        }
        for (name, command, icon) in builtins {
            out.push(Candidate {
                name: name.into(),
                command,
                path: None,
                icon: Some(icon),
                source: Source::Shell,
            });
        }
        if let Some(volume) = &self.volume {
            // Programs and shortcuts anywhere on the drive, breadth first.
            let mut pending = std::collections::VecDeque::from([(Vec::new(), 0)]);
            let mut visited = 0;
            while let Some((dir, depth)) = pending.pop_front() {
                visited += 1;
                if visited > SCAN_DIRS {
                    break;
                }
                for entry in volume.list(&dir).unwrap_or_default() {
                    let path = join(&dir, &entry.name);
                    if entry.is_dir {
                        if depth < SCAN_DEPTH {
                            pending.push_back((path, depth + 1));
                        }
                    } else if matches!(extension(&entry.name).as_str(), "EXE" | "LNK") {
                        add(self, &mut out, path, Source::Drive);
                    }
                }
            }
        }
        out
    }

    /// The menu items for what was typed: the best matches, or a note that
    /// nothing matches.
    pub(super) fn search_items(&self, query: &str) -> Vec<Item> {
        let candidates = self.search_candidates();
        let found = find(&candidates, query);
        if found.is_empty() {
            return vec![Item::command("No programs found", Command::Refresh).enabled(false)];
        }
        found
            .into_iter()
            .map(|c| {
                let (icon, overlay) = match (&c.path, c.icon) {
                    (Some(path), _) => self.icon_for(path, false, 16, 0),
                    (None, Some(builtin)) => (icons::builtin(builtin), false),
                    (None, None) => (icons::builtin(Builtin::Program), false),
                };
                let item = Item::command(&c.name.replace('&', "&&"), c.command.clone())
                    .icon(icon, overlay);
                match (&c.path, c.source) {
                    // Where a program found on the drive lives.
                    (Some(path), Source::Drive) if path.len() > 1 => {
                        let folder = &path[path.len() - 2];
                        item.accel(&String::from_utf8_lossy(&super::fit(folder, 90)))
                    }
                    _ => item,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    fn candidate(name: &str, source: Source) -> Candidate {
        Candidate {
            name: name.into(),
            command: Command::Refresh,
            path: None,
            icon: None,
            source,
        }
    }

    #[test]
    fn ranks_whole_names_then_starts_then_words_then_anywhere() {
        assert_eq!(match_rank("notepad", "Notepad"), Some(0));
        assert_eq!(match_rank("note", "Notepad"), Some(1));
        assert_eq!(match_rank("pad", "Word Pad"), Some(2));
        assert_eq!(match_rank("explorer", "Internet-Explorer"), Some(2));
        assert_eq!(match_rank("pad", "Notepad"), Some(3));
        assert_eq!(match_rank("paint brush", "Brush Paint"), Some(4));
        assert_eq!(match_rank("ffx", "Firefox"), Some(5));
        assert_eq!(match_rank("zz", "Firefox"), None);
        assert_eq!(match_rank("  ", "Firefox"), None);
        assert_eq!(match_rank(" FIRE ", "firefox"), Some(1));
        // A single letter does not match letters scattered in a name.
        assert_eq!(match_rank("x", "Paint"), None);
    }

    #[test]
    fn best_matches_come_first() {
        let candidates = vec![
            candidate("Uninstall Firefox", Source::Drive),
            candidate("firefox", Source::Drive),
            candidate("Firefox", Source::Shortcut),
            candidate("Notepad", Source::Shortcut),
            candidate("Fire Starter", Source::Shortcut),
        ];
        let names: Vec<&str> = find(&candidates, "firef")
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        // Starts of names first (the shortcut before the drive's copy), then
        // a later word.
        assert_eq!(names, vec!["Firefox", "firefox", "Uninstall Firefox"]);
        assert!(find(&candidates, "").is_empty());
        let fire: Vec<&str> = find(&candidates, "fire")
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(fire[0], "Firefox", "shorter names first: {fire:?}");
        assert_eq!(fire[1], "Fire Starter");
    }

    #[test]
    fn results_are_capped() {
        let candidates: Vec<Candidate> = (0..30)
            .map(|i| candidate(&format!("Tool {i}"), Source::Drive))
            .collect();
        assert_eq!(find(&candidates, "tool").len(), MAX_RESULTS);
        assert_eq!(find(&candidates, "tool")[0].name, "Tool 0");
    }

    #[test]
    fn names_lose_program_extensions() {
        assert_eq!(display_name("Notepad.lnk"), "Notepad");
        assert_eq!(display_name("FIREFOX.EXE"), "FIREFOX");
        assert_eq!(display_name("README.TXT"), "README.TXT");
    }
}
