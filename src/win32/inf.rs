//! Setup information (INF) files: parsing, string substitution and the
//! registry directives (`AddReg`, `DelReg`), as SETUPAPI and ADVPACK
//! process them for installers and self-registering DLLs.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::collections::HashMap;

use crate::win32::registry;

/// A parsed INF file: its sections in order, each a list of lines.
#[derive(Clone, Debug, Default)]
pub struct Inf {
    sections: Vec<(String, Vec<String>)>,
    /// `[Strings]` and strings supplied by the caller, by lower-case name.
    strings: HashMap<String, String>,
    /// Where the INF is (`C:\\...`): files it copies come from here.
    pub dir: String,
}

impl Inf {
    pub fn parse(text: &str) -> Self {
        let mut inf = Self::default();
        let mut pending = String::new();
        for raw in text.lines() {
            let line = strip_comment(raw.trim_end_matches(['\r', '\x1A']));
            // A trailing backslash continues the line.
            if let Some(start) = line.trim_end().strip_suffix('\\') {
                pending.push_str(start);
                continue;
            }
            let line = std::mem::take(&mut pending) + line;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.split_once(']')).map(|(n, _)| n) {
                inf.sections.push((name.trim().to_owned(), Vec::new()));
                continue;
            }
            if let Some((_, lines)) = inf.sections.last_mut() {
                lines.push(line.to_owned());
            }
        }
        for line in inf.section("Strings").unwrap_or_default().to_vec() {
            if let Some((name, value)) = line.split_once('=') {
                let value = unquote(value.trim());
                inf.strings.insert(name.trim().to_ascii_lowercase(), value);
            }
        }
        inf
    }

    /// Add (or replace) a string for `%name%` substitution.
    pub fn define(&mut self, name: &str, value: &str) {
        self.strings.insert(name.to_ascii_lowercase(), value.to_owned());
    }

    pub fn section(&self, name: &str) -> Option<&[String]> {
        self.sections
            .iter()
            .find(|(section, _)| section.eq_ignore_ascii_case(name))
            .map(|(_, lines)| lines.as_slice())
    }

    pub fn has_section(&self, name: &str) -> bool {
        self.section(name).is_some()
    }

    /// The values of `key=` lines in a section, split at commas.
    pub fn directive(&self, section: &str, key: &str) -> Vec<String> {
        let mut values = Vec::new();
        for line in self.section(section).unwrap_or_default() {
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case(key) {
                values.extend(self.fields(value).into_iter().filter(|v| !v.is_empty()));
            }
        }
        values
    }

    /// The values of `key=` lines as whole lines (commands: commas kept),
    /// each naming a section whose lines are returned, substituted.
    pub fn directive_lines(&self, section: &str, key: &str) -> Vec<String> {
        let mut lines = Vec::new();
        for name in self.directive(section, key) {
            for line in self.section(&name).unwrap_or_default() {
                let fields = self.fields(line);
                lines.push(fields.join(","));
            }
        }
        lines
    }

    /// The first value of a `key=` line, substituted.
    pub fn value(&self, section: &str, key: &str) -> Option<String> {
        self.section(section)?.iter().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case(key)
                .then(|| unquote(&self.substitute(value.trim())))
        })
    }

    /// Replace `%name%` with strings (`%%` is a percent sign). Unknown
    /// names stay as they are.
    pub fn substitute(&self, text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(start) = rest.find('%') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let Some(end) = after.find('%') else {
                out.push_str(&rest[start..]);
                return out;
            };
            let name = &after[..end];
            if name.is_empty() {
                out.push('%');
            } else if let Some(value) = self.lookup(name) {
                out.push_str(&value);
            } else {
                out.push('%');
                out.push_str(name);
                out.push('%');
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }

    fn lookup(&self, name: &str) -> Option<String> {
        if let Some(value) = self.strings.get(&name.to_ascii_lowercase()) {
            return Some(value.clone());
        }
        // Directory IDs.
        Some(
            match name.parse::<u32>().ok()? {
                10 | 25 => "C:\\WINDOWS",
                11 => "C:\\WINDOWS\\SYSTEM",
                12 => "C:\\WINDOWS\\SYSTEM\\IOSUBSYS",
                17 => "C:\\WINDOWS\\INF",
                18 => "C:\\WINDOWS\\HELP",
                20 => "C:\\WINDOWS\\FONTS",
                24 | 30 => "C:\\",
                53 => "C:\\WINDOWS",
                _ => return None,
            }
            .to_owned(),
        )
    }

    /// A line's fields, each with its strings substituted.
    pub fn fields(&self, line: &str) -> Vec<String> {
        fields(line).iter().map(|field| self.substitute(field)).collect()
    }

    /// Define the custom directory IDs a section's `CustomDestination` names:
    /// lines `49000,49001=Section,flags` whose section reads each folder
    /// from the registry (`root,key,value,,default`).
    pub fn apply_custom_destinations(&mut self, section: &str) {
        for list in self.directive(section, "CustomDestination") {
            for line in self.section(&list).unwrap_or_default().to_vec() {
                let Some((ids, target)) = line.split_once('=') else { continue };
                let target_section = self.fields(target).into_iter().next().unwrap_or_default();
                let entries: Vec<Vec<String>> = self
                    .section(&target_section)
                    .unwrap_or_default()
                    .to_vec()
                    .iter()
                    .map(|l| self.fields(l))
                    .collect();
                for (index, id) in ids.split(',').map(str::trim).enumerate() {
                    let Some(entry) = entries.get(index).or(entries.last()) else { continue };
                    let value = key_path(entry)
                        .and_then(|path| registry::query(&path, entry.get(2).map(String::as_str).unwrap_or("")))
                        .map(|(_, data)| data.iter().map(|&b| b as char).collect::<String>())
                        .filter(|v| !v.is_empty())
                        .or_else(|| entry.get(4).cloned())
                        .unwrap_or_default();
                    self.define(id, &value);
                }
            }
        }
    }

    /// The folder a copy list goes to (`[DestinationDirs]`: `LDID[,subdir]`).
    pub fn destination(&self, list: &str) -> Option<String> {
        let entry = self.value_fields("DestinationDirs", list).or_else(|| self.value_fields("DestinationDirs", "DefaultDestDir"))?;
        let mut dir = self.substitute(&format!("%{}%", entry.first()?.trim()));
        if let Some(sub) = entry.get(1).filter(|s| !s.is_empty()) {
            dir = format!("{}\\{}", dir.trim_end_matches('\\'), sub);
        }
        Some(dir)
    }

    /// The fields of a `key=` line.
    fn value_fields(&self, section: &str, key: &str) -> Option<Vec<String>> {
        self.section(section)?.iter().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            name.trim().eq_ignore_ascii_case(key).then(|| self.fields(value))
        })
    }

    /// The files of a copy list: (target name, source name).
    pub fn copy_list(&self, list: &str) -> Vec<(String, String)> {
        if let Some(file) = list.strip_prefix('@') {
            return vec![(file.to_owned(), file.to_owned())];
        }
        self.section(list)
            .unwrap_or_default()
            .iter()
            .filter_map(|line| {
                let fields = self.fields(line);
                let target = fields.first().filter(|t| !t.is_empty())?.clone();
                let source = fields.get(1).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| target.clone());
                Some((target, source))
            })
            .collect()
    }

    /// Process a section's `DelReg` and then its `AddReg` sections.
    pub fn install_registry(&self, section: &str) {
        for name in self.directive(section, "DelReg") {
            for line in self.section(&name).unwrap_or_default() {
                del_reg(&self.fields(line));
            }
        }
        for name in self.directive(section, "AddReg") {
            for line in self.section(&name).unwrap_or_default() {
                add_reg(&self.fields(line));
            }
        }
    }
}

fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (index, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => return &line[..index],
            _ => {}
        }
    }
    line
}

fn unquote(text: &str) -> String {
    let text = text.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text[1..text.len() - 1].replace("\"\"", "\"")
    } else {
        text.to_owned()
    }
}

/// Split a line at commas outside quotes, unquoting each field.
pub fn fields(line: &str) -> Vec<String> {
    let mut list = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => list.push(std::mem::take(&mut field).trim().to_owned()),
            _ => field.push(c),
        }
    }
    list.push(field.trim().to_owned());
    list
}

fn root(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_uppercase().as_str() {
        "HKCR" | "HKEY_CLASSES_ROOT" => "HKEY_CLASSES_ROOT",
        "HKCU" | "HKEY_CURRENT_USER" => "HKEY_CURRENT_USER",
        "HKLM" | "HKEY_LOCAL_MACHINE" => "HKEY_LOCAL_MACHINE",
        "HKU" | "HKEY_USERS" => "HKEY_USERS",
        _ => return None,
    })
}

fn key_path(fields: &[String]) -> Option<String> {
    let root = root(fields.first()?)?;
    let sub = fields.get(1).map(String::as_str).unwrap_or("");
    Some(if sub.is_empty() { root.to_owned() } else { format!("{root}\\{sub}") })
}

fn number(text: &str) -> u32 {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).unwrap_or(0)
    } else {
        text.parse::<i64>().map(|v| v as u32).unwrap_or(0)
    }
}

const FLG_BINVALUETYPE: u32 = 0x1;
const FLG_NOCLOBBER: u32 = 0x2;
const FLG_DELVAL: u32 = 0x4;
const FLG_APPEND: u32 = 0x8;
const FLG_KEYONLY: u32 = 0x10;
const FLG_OVERWRITEONLY: u32 = 0x20;
const TYPE_MULTI_SZ: u32 = 0x10000;
const TYPE_EXPAND_SZ: u32 = 0x20000;
const TYPE_DWORD: u32 = 0x10001;
const TYPE_NONE: u32 = 0x20001;

/// `root, subkey, value name, flags, data...`
fn add_reg(fields: &[String]) {
    let Some(path) = key_path(fields) else {
        return;
    };
    let name = fields.get(2).map(String::as_str).unwrap_or("");
    let flags = fields.get(3).map(|f| number(f)).unwrap_or(0);
    let data = fields.get(4..).unwrap_or_default();
    if flags & FLG_DELVAL != 0 {
        registry::delete_value(&path, name);
        return;
    }
    let exists = registry::query(&path, name).is_some();
    if (flags & FLG_NOCLOBBER != 0 && exists) || (flags & FLG_OVERWRITEONLY != 0 && !exists) {
        return;
    }
    if flags & FLG_KEYONLY != 0 || (fields.len() <= 2) {
        registry::create_key(&path);
        return;
    }
    if fields.len() <= 3 && name.is_empty() {
        registry::create_key(&path);
        return;
    }
    let (kind, bytes): (u32, Vec<u8>) = match flags & 0xFFFF_0001 {
        TYPE_DWORD => {
            // A number, or four hex bytes.
            if data.len() >= 4 {
                (4, data.iter().take(4).map(|b| u8::from_str_radix(b.trim(), 16).unwrap_or(0)).collect())
            } else {
                (4, number(data.first().map(String::as_str).unwrap_or("0")).to_le_bytes().to_vec())
            }
        }
        TYPE_NONE => (0, hex_bytes(data)),
        TYPE_MULTI_SZ => {
            let mut bytes = Vec::new();
            if flags & FLG_APPEND != 0 {
                if let Some((_, old)) = registry::query(&path, name) {
                    bytes = old;
                    while bytes.last() == Some(&0) {
                        bytes.pop();
                    }
                    if !bytes.is_empty() {
                        bytes.push(0);
                    }
                }
            }
            for item in data {
                bytes.extend_from_slice(&ansi(item));
                bytes.push(0);
            }
            bytes.push(0);
            (7, bytes)
        }
        TYPE_EXPAND_SZ => (2, ansi(data.first().map(String::as_str).unwrap_or(""))),
        other if other & FLG_BINVALUETYPE != 0 => {
            // REG_BINARY, or the type in the high word.
            let kind = if flags >> 16 != 0 { flags >> 16 } else { 3 };
            (kind, hex_bytes(data))
        }
        _ => (1, ansi(data.first().map(String::as_str).unwrap_or(""))),
    };
    registry::set(&path, name, kind, &bytes);
}

fn hex_bytes(fields: &[String]) -> Vec<u8> {
    fields.iter().filter_map(|b| u8::from_str_radix(b.trim(), 16).ok()).collect()
}

fn ansi(text: &str) -> Vec<u8> {
    text.chars().map(|c| if (c as u32) < 0x100 { c as u32 as u8 } else { b'?' }).collect()
}

/// `root, subkey[, value name]`
fn del_reg(fields: &[String]) {
    let Some(path) = key_path(fields) else {
        return;
    };
    match fields.get(2) {
        Some(name) if fields.len() > 2 => registry::delete_value(&path, name),
        _ => registry::delete_key(&path),
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    #[test]
    fn sections_strings_and_fields() {
        let inf = Inf::parse(
            "[Version]\r\nSignature=\"$CHICAGO$\" ; comment\r\n[Install]\r\nAddReg=A, B\r\n[A]\r\n\
             HKCR,\"CLSID\\%CLSID%\",,,\"%DESC%\"\r\n[Strings]\r\nCLSID=\"{1}\"\r\nDESC=\"Say \"\"hi\"\"\"\r\n",
        );
        assert_eq!(inf.value("Version", "Signature").as_deref(), Some("$CHICAGO$"));
        assert_eq!(inf.directive("Install", "AddReg"), ["A", "B"]);
        assert_eq!(inf.fields(&inf.section("A").unwrap()[0]), ["HKCR", "CLSID\\{1}", "", "", "Say \"hi\""]);
        assert_eq!(inf.substitute("%11%\\X.DLL %% %UNKNOWN%"), "C:\\WINDOWS\\SYSTEM\\X.DLL % %UNKNOWN%");
    }
}
