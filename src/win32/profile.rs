//! Initialization files: the KERNEL32 profile functions read and write
//! `.ini` files on the guest drive, in both character sets. A file name
//! without a directory lives in the Windows directory; the plain `Profile`
//! functions use WIN.INI.

use crate::win32::{crt, read_cstr, unicode, ApiContext, ApiOutcome, GuestMemory};

pub fn arg_count(name: &str) -> Option<usize> {
    let base = name.strip_suffix('A').or_else(|| name.strip_suffix('W'))?;
    Some(match base {
        "GetProfileInt" | "WriteProfileString" | "GetPrivateProfileSectionNames" => 3,
        "GetPrivateProfileInt" | "GetPrivateProfileSection" | "WritePrivateProfileString" => 4,
        "GetProfileString" | "GetPrivateProfileStruct" | "WritePrivateProfileStruct" => 5,
        "GetPrivateProfileString" => 6,
        "WritePrivateProfileSection" | "WriteProfileSection" => 3,
        "GetProfileSection" => 3,
        _ => return None,
    })
}

/// One parsed file: its lines, kept verbatim so writes preserve the rest.
/// A file that starts with a UTF-16 byte-order mark stays UTF-16.
struct Ini {
    lines: Vec<Vec<u8>>,
    utf16: bool,
}

fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|b| !b.is_ascii_whitespace()).map(|i| i + 1).unwrap_or(start);
    &bytes[start..end.max(start)]
}

fn section_name(line: &[u8]) -> Option<&[u8]> {
    let line = trim(line);
    (line.first() == Some(&b'[')).then(|| {
        let end = line.iter().position(|&b| b == b']').unwrap_or(line.len());
        trim(&line[1..end])
    })
}

fn key_value(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let line = trim(line);
    if line.is_empty() || line[0] == b';' {
        return None;
    }
    // A line without `=` is a key with no value (INF file lists).
    match line.iter().position(|&b| b == b'=') {
        Some(eq) => Some((trim(&line[..eq]), trim(&line[eq + 1..]))),
        None => Some((line, &line[line.len()..])),
    }
}

impl Ini {
    fn parse(raw: &[u8]) -> Self {
        let utf16 = raw.starts_with(&[0xFF, 0xFE]);
        let decoded;
        let text = if utf16 {
            let units: Vec<u16> = raw[2..].chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
            decoded = unicode::wide_to_ansi(&units);
            decoded.as_slice()
        } else {
            raw
        };
        let lines = text
            .split(|&b| b == b'\n')
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line).to_vec())
            .collect::<Vec<_>>();
        let mut ini = Self { lines, utf16 };
        while ini.lines.last().is_some_and(|l| l.is_empty()) {
            ini.lines.pop();
        }
        ini
    }

    fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for line in &self.lines {
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
        if self.utf16 {
            let mut wide = vec![0xFF, 0xFE];
            wide.extend(unicode::ansi_to_wide(&out).iter().flat_map(|unit| unit.to_le_bytes()));
            return wide;
        }
        out
    }

    /// Line range of a section's body: (header line, end of body).
    fn section(&self, name: &[u8]) -> Option<(usize, usize)> {
        let start = self
            .lines
            .iter()
            .position(|line| section_name(line).is_some_and(|s| s.eq_ignore_ascii_case(name)))?;
        let end = self.lines[start + 1..]
            .iter()
            .position(|line| section_name(line).is_some())
            .map(|i| start + 1 + i)
            .unwrap_or(self.lines.len());
        Some((start, end))
    }

    fn sections(&self) -> Vec<Vec<u8>> {
        self.lines.iter().filter_map(|line| section_name(line).map(<[u8]>::to_vec)).collect()
    }

    fn entries(&self, section: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let Some((start, end)) = self.section(section) else {
            return Vec::new();
        };
        self.lines[start + 1..end]
            .iter()
            .filter_map(|line| key_value(line).map(|(k, v)| (k.to_vec(), v.to_vec())))
            .collect()
    }

    /// A section's lines as written (`key=value`, or a bare line), without
    /// comments and blank lines.
    fn section_lines(&self, section: &[u8]) -> Vec<Vec<u8>> {
        let Some((start, end)) = self.section(section) else {
            return Vec::new();
        };
        self.lines[start + 1..end]
            .iter()
            .filter_map(|line| {
                let (key, value) = key_value(line)?;
                Some(if trim(line).contains(&b'=') { [key, b"=", value].concat() } else { key.to_vec() })
            })
            .collect()
    }

    fn get(&self, section: &[u8], key: &[u8]) -> Option<Vec<u8>> {
        self.entries(section).into_iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
    }

    /// Set (`Some`) or delete (`None`) a key; deleting a missing key is a no-op.
    fn set(&mut self, section: &[u8], key: &[u8], value: Option<&[u8]>) {
        let (start, end) = match self.section(section) {
            Some(range) => range,
            None if value.is_none() => return,
            None => {
                if self.lines.last().is_some_and(|l| !l.is_empty()) {
                    self.lines.push(Vec::new());
                }
                self.lines.push([b"[".as_slice(), section, b"]"].concat());
                (self.lines.len() - 1, self.lines.len())
            }
        };
        let existing = (start + 1..end)
            .find(|&i| key_value(&self.lines[i]).is_some_and(|(k, _)| k.eq_ignore_ascii_case(key)));
        match (existing, value) {
            (Some(i), Some(value)) => self.lines[i] = [key, b"=", value].concat(),
            (Some(i), None) => {
                self.lines.remove(i);
            }
            (None, Some(value)) => {
                // After the section's last non-blank line.
                let mut at = end;
                while at > start + 1 && trim(&self.lines[at - 1]).is_empty() {
                    at -= 1;
                }
                self.lines.insert(at, [key, b"=", value].concat());
            }
            (None, None) => {}
        }
    }

    fn remove_section(&mut self, section: &[u8]) {
        if let Some((start, end)) = self.section(section) {
            self.lines.drain(start..end);
        }
    }
}

/// The guest path of a profile file name.
fn file_path(ctx: &ApiContext, name: &[u8]) -> Vec<u8> {
    let bare = !name.iter().any(|&b| matches!(b, b'\\' | b'/' | b':'));
    if bare {
        [b"C:\\WINDOWS\\".as_slice(), name].concat()
    } else {
        crt::absolute(ctx, name)
    }
}

fn load(ctx: &ApiContext, file: &[u8]) -> Ini {
    let text = crt::host_path_in(ctx, &file_path(ctx, file)).and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
    Ini::parse(&text)
}

fn save(ctx: &ApiContext, file: &[u8], ini: &Ini) -> bool {
    crt::host_path_in(ctx, &file_path(ctx, file)).is_some_and(|path| std::fs::write(path, ini.render()).is_ok())
}

/// Read a string argument (NULL stays `None`).
fn arg<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, wide: bool) -> Option<Vec<u8>> {
    (va != 0).then(|| {
        if wide {
            unicode::wide_to_ansi(&unicode::read_wstr(memory, va))
        } else {
            read_cstr(memory, va)
        }
    })
}

/// Copy `text` to a buffer of `capacity` characters with one NUL, truncating
/// like Win32; the result is the number of characters copied.
fn put_string<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, capacity: u32, text: &[u8], wide: bool) -> u32 {
    if va == 0 || capacity == 0 {
        return 0;
    }
    let count = text.len().min(capacity as usize - 1);
    put_raw(memory, va, &[&text[..count], b"\0"].concat(), wide);
    count as u32
}

/// Copy a list of strings as NUL-separated items ending with an extra NUL.
/// A list that does not fit is cut and the result is `capacity - 2`.
fn put_list<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, capacity: u32, items: &[Vec<u8>], wide: bool) -> u32 {
    if va == 0 || capacity < 2 {
        return 0;
    }
    let mut block = Vec::new();
    for item in items {
        block.extend_from_slice(item);
        block.push(0);
    }
    let capacity = capacity as usize;
    if block.len() + 1 > capacity {
        block.truncate(capacity - 2);
        block.extend_from_slice(b"\0\0");
        put_raw(memory, va, &block, wide);
        return capacity as u32 - 2;
    }
    let length = block.len();
    block.push(0);
    if items.is_empty() {
        block.push(0);
    }
    put_raw(memory, va, &block, wide);
    length as u32
}

fn put_raw<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, bytes: &[u8], wide: bool) {
    if wide {
        unicode::write_wide(memory, va, &unicode::ansi_to_wide(bytes), false);
    } else {
        let _ = memory.write_bytes(va, bytes);
    }
}

/// GetPrivateProfileInt's parse: optional sign, decimal or 0x hex digits.
fn parse_int(text: &[u8]) -> Option<u32> {
    let text = trim(text);
    let (negative, digits) = match text.first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (radix, digits) = match digits {
        [b'0', b'x' | b'X', rest @ ..] => (16, rest),
        _ => (10, digits),
    };
    let end = digits.iter().position(|b| !(*b as char).is_digit(radix)).unwrap_or(digits.len());
    if end == 0 {
        return Some(0);
    }
    let value = u32::from_str_radix(std::str::from_utf8(&digits[..end]).ok()?, radix).ok()?;
    Some(if negative { value.wrapping_neg() } else { value })
}

/// Values are returned without one pair of surrounding quotes.
fn unquote(value: Vec<u8>) -> Vec<u8> {
    match value.as_slice() {
        [q, inner @ .., e] if q == e && (*q == b'"' || *q == b'\'') => inner.to_vec(),
        _ => value,
    }
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let wide = name.ends_with('W');
    let base = &name[..name.len() - 1];
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    // The file argument's position; the plain Profile functions use WIN.INI.
    let file_arg = |index: usize, memory: &mut M| arg(memory, args[index], wide).unwrap_or_else(|| b"WIN.INI".to_vec());
    let win_ini = || b"WIN.INI".to_vec();
    match base {
        "GetPrivateProfileString" | "GetProfileString" => {
            let file = if base == "GetProfileString" { win_ini() } else { file_arg(5, memory) };
            let ini = load(ctx, &file);
            let section = arg(memory, args[0], wide);
            let key = arg(memory, args[1], wide);
            let (buffer, size) = (args[3], args[4]);
            match (section, key) {
                (None, _) => ok(put_list(memory, buffer, size, &ini.sections(), wide)),
                (Some(section), None) => {
                    let keys: Vec<_> = ini.entries(&section).into_iter().map(|(k, _)| k).collect();
                    ok(put_list(memory, buffer, size, &keys, wide))
                }
                (Some(section), Some(key)) => {
                    let value = match ini.get(&section, &key) {
                        Some(value) => unquote(value),
                        None => trim(&arg(memory, args[2], wide).unwrap_or_default()).to_vec(),
                    };
                    ok(put_string(memory, buffer, size, &value, wide))
                }
            }
        }
        "GetPrivateProfileInt" | "GetProfileInt" => {
            let file = if base == "GetProfileInt" { win_ini() } else { file_arg(3, memory) };
            let (Some(section), Some(key)) = (arg(memory, args[0], wide), arg(memory, args[1], wide)) else {
                return ok(args[2]);
            };
            let value = load(ctx, &file).get(&section, &key).and_then(|v| parse_int(&v));
            ok(value.unwrap_or(args[2]))
        }
        "GetPrivateProfileSectionNames" => {
            let ini = load(ctx, &file_arg(2, memory));
            ok(put_list(memory, args[0], args[1], &ini.sections(), wide))
        }
        "GetPrivateProfileSection" | "GetProfileSection" => {
            let file = if base == "GetProfileSection" { win_ini() } else { file_arg(3, memory) };
            let section = arg(memory, args[0], wide).unwrap_or_default();
            let lines = load(ctx, &file).section_lines(&section);
            ok(put_list(memory, args[1], args[2], &lines, wide))
        }
        "WritePrivateProfileString" | "WriteProfileString" => {
            let file = if base == "WriteProfileString" { win_ini() } else { file_arg(3, memory) };
            let Some(section) = arg(memory, args[0], wide) else {
                return ok(1); // flush the cache: nothing is cached
            };
            let mut ini = load(ctx, &file);
            match arg(memory, args[1], wide) {
                None => ini.remove_section(&section),
                Some(key) => {
                    let value = arg(memory, args[2], wide);
                    ini.set(&section, &key, value.as_deref());
                }
            }
            ok(u32::from(save(ctx, &file, &ini)))
        }
        "WritePrivateProfileSection" | "WriteProfileSection" => {
            let file = if base == "WriteProfileSection" { win_ini() } else { file_arg(2, memory) };
            let Some(section) = arg(memory, args[0], wide) else {
                return ok(0);
            };
            let mut ini = load(ctx, &file);
            ini.remove_section(&section);
            match args[1] {
                0 => {}
                mut va => loop {
                    let Some(line) = arg(memory, va, wide) else { break };
                    if line.is_empty() {
                        // An empty section is still written.
                        if ini.section(&section).is_none() {
                            ini.lines.push([b"[".as_slice(), &section, b"]"].concat());
                        }
                        break;
                    }
                    va += (line.len() as u32 + 1) * if wide { 2 } else { 1 };
                    if let Some((key, value)) = key_value(&line) {
                        ini.set(&section, key, Some(value));
                    }
                },
            }
            ok(u32::from(save(ctx, &file, &ini)))
        }
        "GetPrivateProfileStruct" => {
            // Stored as hex digits followed by a checksum byte (sum mod 256).
            let file = file_arg(4, memory);
            let (Some(section), Some(key)) = (arg(memory, args[0], wide), arg(memory, args[1], wide)) else {
                return ok(0);
            };
            let Some(hex) = load(ctx, &file).get(&section, &key) else {
                return ok(0);
            };
            let bytes: Option<Vec<u8>> = hex
                .chunks(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
                .collect();
            match bytes {
                Some(mut bytes) if bytes.len() == args[3] as usize + 1 => {
                    let checksum = bytes.pop().unwrap_or(0);
                    if bytes.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)) != checksum {
                        return ok(0);
                    }
                    let _ = memory.write_bytes(args[2], &bytes);
                    ok(1)
                }
                _ => ok(0),
            }
        }
        "WritePrivateProfileStruct" => {
            let file = file_arg(4, memory);
            let Some(section) = arg(memory, args[0], wide) else {
                return ok(0);
            };
            let mut ini = load(ctx, &file);
            match (arg(memory, args[1], wide), args[2]) {
                (Some(key), 0) => ini.set(&section, &key, None),
                (Some(key), data) => {
                    let bytes = memory.read_bytes(data, args[3] as usize).unwrap_or_default();
                    let checksum = bytes.iter().fold(0u8, |sum, b| sum.wrapping_add(*b));
                    let hex: String = bytes.iter().chain([&checksum]).map(|b| format!("{b:02X}")).collect();
                    ini.set(&section, &key, Some(hex.as_bytes()));
                }
                (None, _) => ini.remove_section(&section),
            }
            ok(u32::from(save(ctx, &file, &ini)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_the_rest_of_the_file() {
        let mut ini = Ini::parse(b"; comment\r\n[One]\r\na=1\r\n\r\n[Two]\r\nb = 2\r\n");
        assert_eq!(ini.get(b"two", b"B"), Some(b"2".to_vec()));
        ini.set(b"One", b"c", Some(b"3"));
        ini.set(b"Two", b"b", None);
        ini.set(b"Three", b"d", Some(b"4"));
        assert_eq!(
            ini.render(),
            b"; comment\r\n[One]\r\na=1\r\nc=3\r\n\r\n[Two]\r\n\r\n[Three]\r\nd=4\r\n".to_vec()
        );
        assert_eq!(ini.sections(), vec![b"One".to_vec(), b"Two".to_vec(), b"Three".to_vec()]);
        ini.remove_section(b"two");
        assert_eq!(ini.sections().len(), 2);
    }

    #[test]
    fn utf16_files_stay_utf16() {
        let text: Vec<u8> = [0xFF, 0xFE].into_iter().chain("[A]\r\nk=v\r\n".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        let mut ini = Ini::parse(&text);
        assert_eq!(ini.get(b"a", b"k"), Some(b"v".to_vec()));
        ini.set(b"A", b"n", Some(b"1"));
        let expected: Vec<u8> = [0xFF, 0xFE].into_iter().chain("[A]\r\nk=v\r\nn=1\r\n".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(ini.render(), expected);
    }

    #[test]
    fn integers_parse_like_windows() {
        assert_eq!(parse_int(b" 42abc"), Some(42));
        assert_eq!(parse_int(b"0x1F"), Some(31));
        assert_eq!(parse_int(b"-3"), Some(-3i32 as u32));
        assert_eq!(parse_int(b"x"), Some(0));
    }
}
