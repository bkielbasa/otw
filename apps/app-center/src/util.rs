//! Small string helpers for URLs.

/// Decodes `%XX` escapes; anything malformed is kept as it is.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escapes one URL path segment. Package identifiers contain characters
/// such as `+` ("Notepad++.Notepad++"), which the CDN wants as they are.
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~+".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Joins a base URL and a relative path with exactly one slash.
pub fn join(base: &str, rel: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        rel.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding() {
        assert_eq!(
            percent_decode("%5BContent_Types%5D.xml"),
            "[Content_Types].xml"
        );
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("caf%C3%A9"), "caf\u{e9}");
    }

    #[test]
    fn encoding() {
        assert_eq!(encode_segment("Notepad++.Notepad++"), "Notepad++.Notepad++");
        assert_eq!(encode_segment("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn joining() {
        assert_eq!(join("https://x/cache/", "/a/b"), "https://x/cache/a/b");
        assert_eq!(join("https://x/cache", "a"), "https://x/cache/a");
    }
}
