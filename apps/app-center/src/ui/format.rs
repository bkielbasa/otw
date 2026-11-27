//! Text shown to the user: sizes, progress, error messages, and fitting
//! text into a width.

use crate::cli::human_size;
use crate::error::Error;
use crate::select::NoChoice;
use crate::source::IndexProgress;

/// How far a download has got, 0.0 to 1.0, when the total is known.
pub fn fraction(done: u64, total: Option<u64>) -> Option<f32> {
    match total {
        Some(t) if t > 0 => Some((done as f64 / t as f64).clamp(0.0, 1.0) as f32),
        _ => None,
    }
}

/// "1.2 MiB of 4.0 MiB (30%)", or "1.2 MiB" when the total is unknown.
pub fn progress_label(done: u64, total: Option<u64>) -> String {
    match total {
        Some(t) if t > 0 => format!(
            "{} of {} ({}%)",
            human_size(done),
            human_size(t),
            (done.min(t) * 100 / t)
        ),
        _ => human_size(done),
    }
}

/// "2.5 MiB/s", from bytes and milliseconds; None until there is enough to
/// measure.
pub fn speed_label(bytes: u64, elapsed_ms: u64) -> Option<String> {
    if elapsed_ms < 500 || bytes == 0 {
        return None;
    }
    Some(format!("{}/s", human_size(bytes * 1000 / elapsed_ms)))
}

/// "12 s left", "3 min left".
pub fn eta_label(done: u64, total: Option<u64>, elapsed_ms: u64) -> Option<String> {
    let total = total?;
    if elapsed_ms < 1000 || done == 0 || done >= total {
        return None;
    }
    let left_ms = (total - done) as u128 * elapsed_ms as u128 / done as u128;
    let secs = (left_ms / 1000) as u64 + 1;
    Some(if secs < 60 {
        format!("{secs} s left")
    } else if secs < 3600 {
        format!("{} min left", secs.div_ceil(60))
    } else {
        format!("{} h left", secs / 3600)
    })
}

/// What the index loader is doing, for the status line.
pub fn index_progress_label(p: &IndexProgress) -> String {
    match p {
        IndexProgress::ReadingCache => "Reading the package index...".into(),
        IndexProgress::Downloading { done, total } => format!(
            "Downloading the package index: {}",
            progress_label(*done, *total)
        ),
        IndexProgress::Unpacking => "Unpacking the package index...".into(),
    }
}

/// A message for the user: a short title and a longer explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub detail: String,
}

fn msg(title: &str, detail: impl Into<String>) -> Message {
    Message {
        title: title.into(),
        detail: detail.into(),
    }
}

/// Turns a library error into something a user can act on.
pub fn friendly_error(e: &Error) -> Message {
    match e {
        Error::Http(d) => msg(
            "Can't reach the internet",
            format!("Check the network connection and try again. ({d})"),
        ),
        Error::HashMismatch { .. } => msg(
            "Download failed the integrity check",
            "The file did not match the SHA-256 checksum published in the winget \
             repository, so it was deleted. It may have been changed or corrupted \
             on the way. Try again later.",
        ),
        Error::NotFound(d) => msg("Not found", d.clone()),
        Error::Io(err) => msg("Couldn't save the file", err.to_string()),
        Error::Format(d) => msg("Unexpected data from the repository", d.clone()),
        Error::Cancelled => msg("Download cancelled", ""),
        Error::Usage(d) => msg("Something went wrong", d.clone()),
    }
}

/// Why a package has nothing to download.
pub fn no_choice_message(n: &NoChoice) -> Message {
    match n {
        NoChoice::NoInstallers => msg(
            "No installer available",
            "This package's manifest lists no installers.",
        ),
        NoChoice::NoArch { available } => msg(
            "No 64-bit or 32-bit build",
            format!(
                "There is no installer that runs on x64. Available: {}.",
                available.join(", ")
            ),
        ),
        NoChoice::OnlyPackaged { available } => msg(
            "Store/MSIX package only",
            format!(
                "This app is only published as a packaged app ({}), which OTW \
                 can't install yet.",
                available.join(", ")
            ),
        ),
    }
}

/// The label of the button that opens a downloaded file.
pub fn open_label(file_name: &str) -> &'static str {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".zip") {
        "Open archive"
    } else if lower.ends_with(".msi") {
        "Open installer"
    } else {
        "Run installer"
    }
}

/// "26.06, 25.01, 24.09 and 31 more"
pub fn versions_summary(versions: &[String], shown: usize) -> String {
    if versions.len() <= shown {
        return versions.join(", ");
    }
    format!(
        "{} and {} more",
        versions[..shown].join(", "),
        versions.len() - shown
    )
}

/// Cuts `text` so that it fits `max_width`, ending it with "...".
pub fn ellipsize(text: &str, max_width: i32, measure: impl Fn(&str) -> i32) -> String {
    if measure(text) <= max_width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    // Longest prefix p such that prefix + "..." fits: binary search.
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let candidate: String = chars[..mid].iter().collect::<String>() + "...";
        if measure(&candidate) <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let prefix: String = chars[..lo].iter().collect();
    format!("{}...", prefix.trim_end())
}

/// Breaks `text` into lines no wider than `max_width`, at spaces where
/// possible. Existing line breaks are kept; a word too long for a line is
/// split. At most `max_lines` lines are returned, the last one ellipsized.
pub fn wrap(
    text: &str,
    max_width: i32,
    max_lines: usize,
    measure: impl Fn(&str) -> i32,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut truncated = false;
    'paragraphs: for para in text.replace("\r\n", "\n").split('\n') {
        let para = para.trim_end();
        if para.trim().is_empty() {
            // Keep paragraph breaks, but never two blank lines in a row.
            if lines.last().is_some_and(|l| !l.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }
        let mut line = String::new();
        for word in para.split(' ').filter(|w| !w.is_empty()) {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if measure(&candidate) <= max_width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                if lines.len() >= max_lines {
                    truncated = true;
                    break 'paragraphs;
                }
            }
            // The word alone: split it if it does not fit.
            let mut rest: Vec<char> = word.chars().collect();
            while measure(&rest.iter().collect::<String>()) > max_width && rest.len() > 1 {
                let mut n = rest.len() - 1;
                while n > 1 && measure(&rest[..n].iter().collect::<String>()) > max_width {
                    n -= 1;
                }
                lines.push(rest[..n].iter().collect());
                rest.drain(..n);
                if lines.len() >= max_lines {
                    truncated = true;
                    break 'paragraphs;
                }
            }
            line = rest.into_iter().collect();
        }
        if !line.is_empty() {
            lines.push(line);
            if lines.len() >= max_lines {
                truncated = true;
                break;
            }
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if truncated {
        if let Some(last) = lines.last_mut() {
            *last = ellipsize(&format!("{last} ..."), max_width, &measure);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every character is 10 pixels wide.
    fn w(s: &str) -> i32 {
        s.chars().count() as i32 * 10
    }

    #[test]
    fn fractions_and_labels() {
        assert_eq!(fraction(50, Some(200)), Some(0.25));
        assert_eq!(fraction(500, Some(200)), Some(1.0));
        assert_eq!(fraction(5, None), None);
        assert_eq!(fraction(5, Some(0)), None);
        assert_eq!(
            progress_label(3 << 20, Some(4 << 20)),
            "3.0 MiB of 4.0 MiB (75%)"
        );
        assert_eq!(progress_label(512, None), "512 B");
    }

    #[test]
    fn speed_and_eta() {
        assert_eq!(speed_label(1 << 20, 100), None);
        assert_eq!(speed_label(2 << 20, 1000).as_deref(), Some("2.0 MiB/s"));
        assert_eq!(eta_label(10, Some(100), 2000).as_deref(), Some("19 s left"));
        assert_eq!(eta_label(1, Some(100), 2000).as_deref(), Some("4 min left"));
        assert_eq!(
            eta_label(1, Some(10_000), 2000).as_deref(),
            Some("5 h left")
        );
        assert_eq!(eta_label(100, Some(100), 2000), None);
        assert_eq!(eta_label(10, None, 2000), None);
        assert_eq!(eta_label(10, Some(100), 200), None);
    }

    #[test]
    fn index_progress() {
        assert!(index_progress_label(&IndexProgress::ReadingCache).contains("Reading"));
        assert_eq!(
            index_progress_label(&IndexProgress::Downloading {
                done: 1 << 20,
                total: Some(4 << 20)
            }),
            "Downloading the package index: 1.0 MiB of 4.0 MiB (25%)"
        );
    }

    #[test]
    fn errors_are_explained() {
        assert_eq!(
            friendly_error(&Error::Http("timed out".into())).title,
            "Can't reach the internet"
        );
        let m = friendly_error(&Error::HashMismatch {
            what: "x".into(),
            expected: "a".into(),
            actual: "b".into(),
        });
        assert!(m.detail.contains("SHA-256") && m.detail.contains("deleted"));
        let m = no_choice_message(&NoChoice::OnlyPackaged {
            available: vec!["x64 msix".into()],
        });
        assert!(m.title.contains("MSIX") && m.detail.contains("x64 msix"));
        let m = no_choice_message(&NoChoice::NoArch {
            available: vec!["arm64 exe".into()],
        });
        assert!(m.detail.contains("arm64 exe"));
        assert_eq!(
            friendly_error(&Error::Cancelled).title,
            "Download cancelled"
        );
    }

    #[test]
    fn open_labels() {
        assert_eq!(open_label("7z2606-x64.exe"), "Run installer");
        assert_eq!(open_label("tool.ZIP"), "Open archive");
        assert_eq!(open_label("x.msi"), "Open installer");
    }

    #[test]
    fn version_summaries() {
        let v: Vec<String> = ["3", "2", "1"].iter().map(|s| s.to_string()).collect();
        assert_eq!(versions_summary(&v, 5), "3, 2, 1");
        assert_eq!(versions_summary(&v, 2), "3, 2 and 1 more");
        assert_eq!(versions_summary(&[], 2), "");
    }

    #[test]
    fn ellipsizing() {
        assert_eq!(ellipsize("short", 100, w), "short");
        assert_eq!(ellipsize("a long name here", 60, w), "a l...");
        assert_eq!(ellipsize("a b", 10, w), "...");
        assert!(w(&ellipsize("Mozilla Firefox Developer Edition", 155, w)) <= 155);
    }

    #[test]
    fn wrapping_words() {
        assert_eq!(
            wrap("the quick brown fox jumps", 100, 10, w),
            vec!["the quick", "brown fox", "jumps"]
        );
        assert_eq!(wrap("one\n\n\n\ntwo\n", 100, 10, w), vec!["one", "", "two"]);
        assert_eq!(
            wrap("abcdefghijkl", 50, 10, w),
            vec!["abcde", "fghij", "kl"]
        );
        assert!(wrap("", 100, 10, w).is_empty());
        assert!(wrap("x y", 0, 10, w).iter().all(|l| !l.is_empty()));
    }

    #[test]
    fn wrapping_is_limited() {
        let lines = wrap("aa bb cc dd ee ff gg", 50, 2, w);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "aa bb");
        assert!(lines[1].ends_with("..."), "{lines:?}");
        assert!(lines.iter().all(|l| w(l) <= 50), "{lines:?}");
    }
}
