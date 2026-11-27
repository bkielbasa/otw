//! Package version ordering, following winget's rules.
//!
//! winget splits a version at dots. Each part is a leading number and an
//! optional rest ("2b" is 2 and "b"). Parts compare by number first; for
//! equal numbers a part with no rest is newer than one with a rest (so
//! "1.0" is newer than "1.0-beta"), otherwise the rests compare as text,
//! ignoring case. Missing trailing parts count as 0, so "1.0" equals "1".
//! A leading "v" is ignored.

use std::cmp::Ordering;

#[derive(Debug, Clone)]
pub struct Version {
    text: String,
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Part {
    number: u64,
    rest: String,
}

impl Part {
    fn parse(s: &str) -> Part {
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        // Numbers too big for u64 saturate; they still sort after the others.
        let number = if digits == 0 {
            0
        } else {
            s[..digits].parse().unwrap_or(u64::MAX)
        };
        Part {
            number,
            rest: s[digits..].to_ascii_lowercase(),
        }
    }

    fn zero() -> Part {
        Part {
            number: 0,
            rest: String::new(),
        }
    }

    fn is_zero(&self) -> bool {
        self.number == 0 && self.rest.is_empty()
    }
}

impl Ord for Part {
    fn cmp(&self, other: &Self) -> Ordering {
        self.number.cmp(&other.number).then_with(|| {
            match (self.rest.is_empty(), other.rest.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.rest.cmp(&other.rest),
            }
        })
    }
}

impl PartialOrd for Part {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Version {
    pub fn parse(text: &str) -> Version {
        let trimmed = text.trim();
        let body = match trimmed.as_bytes().first() {
            Some(b'v' | b'V') if trimmed[1..].starts_with(|c: char| c.is_ascii_digit()) => {
                &trimmed[1..]
            }
            _ => trimmed,
        };
        let mut parts: Vec<Part> = body.split('.').map(|p| Part::parse(p.trim())).collect();
        while parts.last().is_some_and(Part::is_zero) {
            parts.pop();
        }
        Version {
            text: trimmed.to_string(),
            parts,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.parts.len().max(other.parts.len());
        let zero = Part::zero();
        for i in 0..n {
            let a = self.parts.get(i).unwrap_or(&zero);
            let b = other.parts.get(i).unwrap_or(&zero);
            match a.cmp(b) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s)
    }

    #[test]
    fn numeric_parts_compare_as_numbers() {
        assert!(v("1.10") > v("1.9"));
        assert!(v("26.06") > v("26.05"));
        assert!(v("26.06") > v("9.20"));
        assert!(v("8.9.9.1") > v("8.9.9"));
        assert!(v("160.0.1") > v("16.0.1"));
    }

    #[test]
    fn trailing_zeros_do_not_matter() {
        assert_eq!(v("1.0"), v("1"));
        assert_eq!(v("1.0.0.0"), v("1"));
        assert!(v("1.0.1") > v("1"));
    }

    #[test]
    fn suffixes_sort_before_the_plain_number() {
        assert!(v("1.0") > v("1.0-beta"));
        assert!(v("1.0-rc1") > v("1.0-beta"));
        assert!(v("1.0b") < v("1.0"));
        assert!(v("2.0-alpha") > v("1.9"));
    }

    #[test]
    fn suffix_comparison_ignores_case() {
        assert_eq!(v("1.0-Beta"), v("1.0-beta"));
    }

    #[test]
    fn leading_v_is_ignored() {
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("V2"), v("2.0"));
        // "very" is not a version prefix.
        assert_eq!(v("very").as_str(), "very");
    }

    #[test]
    fn text_is_kept() {
        assert_eq!(v(" 26.06 ").to_string(), "26.06");
    }

    #[test]
    fn huge_numbers_do_not_panic() {
        assert!(v("99999999999999999999999") > v("1"));
    }

    #[test]
    fn sorting_a_list() {
        let mut list: Vec<Version> = ["1.2", "1.10", "1.2-rc", "0.9", "1.2.1"]
            .iter()
            .map(|s| v(s))
            .collect();
        list.sort();
        let sorted: Vec<&str> = list.iter().map(Version::as_str).collect();
        assert_eq!(sorted, ["0.9", "1.2-rc", "1.2", "1.2.1", "1.10"]);
    }
}
