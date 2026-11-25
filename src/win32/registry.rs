//! The registry (ADVAPI32 `Reg*`, ANSI and Unicode).
//!
//! One tree serves every program, as in Windows, and is kept on drive C: in
//! `C:\WINDOWS\REGISTRY.REG`, a REGEDIT4 file, so what an installer
//! registers is there the next time a program runs. `HKEY_CLASSES_ROOT` is
//! `HKEY_LOCAL_MACHINE\SOFTWARE\Classes` and `HKEY_CURRENT_USER` is
//! `HKEY_USERS\.DEFAULT`, as on Windows 95 without user profiles.
//!
//! Names are case-insensitive and keep the case they were created with.
//! String data is stored in the ANSI code page (as Windows 95 does) and
//! converted for the `W` functions.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::win32::{crt, read_cstr, ApiContext, ApiOutcome, GuestMemory};

const SUCCESS: u32 = 0;
const NOT_FOUND: u32 = 2;
const BAD_HANDLE: u32 = 6;
const INVALID_PARAMETER: u32 = 87;
const MORE_DATA: u32 = 234;
const NO_MORE_ITEMS: u32 = 259;

const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const REG_MULTI_SZ: u32 = 7;

#[derive(Clone, Debug, Default, PartialEq)]
struct Key {
    name: String,
    subkeys: Vec<Key>,
    values: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
struct Value {
    name: String,
    kind: u32,
    data: Vec<u8>,
}

impl Key {
    fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    fn child(&self, name: &str) -> Option<&Key> {
        self.subkeys.iter().find(|k| k.name.eq_ignore_ascii_case(name))
    }

    fn child_mut(&mut self, name: &str) -> Option<&mut Key> {
        self.subkeys
            .iter_mut()
            .find(|k| k.name.eq_ignore_ascii_case(name))
    }

    fn find(&self, path: &[String]) -> Option<&Key> {
        path.iter().try_fold(self, |key, part| key.child(part))
    }

    fn find_mut(&mut self, path: &[String]) -> Option<&mut Key> {
        path.iter().try_fold(self, |key, part| key.child_mut(part))
    }

    /// The key at `path`, created with its parents. Returns whether it existed.
    fn create(&mut self, path: &[String]) -> bool {
        let mut key = self;
        let mut existed = true;
        for part in path {
            if key.child(part).is_none() {
                key.subkeys.push(Key::named(part));
                existed = false;
            }
            key = key.child_mut(part).expect("just created");
        }
        existed
    }

    fn value(&self, name: &str) -> Option<&Value> {
        self.values.iter().find(|v| v.name.eq_ignore_ascii_case(name))
    }

    fn set_value(&mut self, name: &str, kind: u32, data: Vec<u8>) {
        match self.values.iter_mut().find(|v| v.name.eq_ignore_ascii_case(name)) {
            Some(value) => {
                value.kind = kind;
                value.data = data;
            }
            None => self.values.push(Value {
                name: name.to_owned(),
                kind,
                data,
            }),
        }
    }
}

/// The registry of one drive C:.
struct Registry {
    file: PathBuf,
    /// Holds `HKEY_LOCAL_MACHINE`, `HKEY_USERS` and `HKEY_DYN_DATA`.
    root: Key,
    handles: HashMap<u32, Vec<String>>,
    next_handle: u32,
    dirty: bool,
    saved: Instant,
}

thread_local! {
    static REGISTRY: RefCell<Option<Registry>> = const { RefCell::new(None) };
}

fn split(path: &str) -> Vec<String> {
    path.split('\\')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn root_path(key: u32) -> Option<Vec<String>> {
    Some(split(match key {
        0x8000_0000 => "HKEY_LOCAL_MACHINE\\SOFTWARE\\Classes",
        0x8000_0001 => "HKEY_USERS\\.DEFAULT",
        0x8000_0002 => "HKEY_LOCAL_MACHINE",
        0x8000_0003 => "HKEY_USERS",
        0x8000_0005 => "HKEY_LOCAL_MACHINE\\Config\\0001",
        0x8000_0006 => "HKEY_DYN_DATA",
        _ => return None,
    }))
}

impl Registry {
    fn load(file: PathBuf) -> Self {
        let mut registry = Self {
            file,
            root: Key::default(),
            handles: HashMap::new(),
            next_handle: 0x0C00_0010,
            dirty: false,
            saved: Instant::now(),
        };
        match std::fs::read(&registry.file) {
            Ok(text) => registry.root = parse(&String::from_utf8_lossy(&text)),
            Err(_) => {
                registry.seed();
                registry.dirty = true;
            }
        }
        for root in ["HKEY_LOCAL_MACHINE\\SOFTWARE\\Classes", "HKEY_USERS\\.DEFAULT"] {
            registry.root.create(&split(root));
        }
        registry
    }

    /// What Windows 95 Setup leaves for programs to find.
    fn seed(&mut self) {
        let current = split("HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion");
        self.root.create(&current);
        let key = self.root.find_mut(&current).expect("created");
        for (name, value) in [
            ("ProgramFilesDir", "C:\\Program Files"),
            ("CommonFilesDir", "C:\\Program Files\\Common Files"),
            ("SystemRoot", "C:\\WINDOWS"),
            ("MediaPath", "C:\\WINDOWS\\MEDIA"),
            ("Version", "Windows 95"),
            ("VersionNumber", "4.00.950"),
            ("SubVersionNumber", ""),
            ("ProductName", "Microsoft Windows 95"),
            ("RegisteredOwner", "OTW"),
        ] {
            key.set_value(name, REG_SZ, sz(value.as_bytes()));
        }
    }

    fn path(&self, handle: u32) -> Option<Vec<String>> {
        root_path(handle).or_else(|| self.handles.get(&handle).cloned())
    }

    fn open(&mut self, path: Vec<String>) -> u32 {
        let handle = self.next_handle;
        self.next_handle += 4;
        self.handles.insert(handle, path);
        handle
    }

    fn save(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(parent) = self.file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&self.file, format(&self.root)).is_ok() {
            self.dirty = false;
        }
        self.saved = Instant::now();
    }

    /// Save now and then while keys are being written.
    fn save_soon(&mut self) {
        if self.dirty && self.saved.elapsed() > Duration::from_secs(2) {
            self.save();
        }
    }
}

fn with<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let file = crt::drive_root().join("WINDOWS").join("REGISTRY.REG");
    REGISTRY.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().map(|r| &r.file) != Some(&file) {
            if let Some(previous) = slot.as_mut() {
                previous.save();
            }
            *slot = Some(Registry::load(file));
        }
        f(slot.as_mut().expect("loaded"))
    })
}

/// A value, for the subsystem's own lookups: `path` is a full key path
/// such as `HKEY_CLASSES_ROOT\\CLSID\\{...}\\InprocServer32`; an empty
/// `value` is the key's default value. Strings come without their
/// terminator.
pub fn query(path: &str, value: &str) -> Option<(u32, Vec<u8>)> {
    let path = canonical(path);
    with(|r| {
        let found = r.root.find(&path)?.value(value)?;
        let mut data = found.data.clone();
        if is_string(found.kind) && data.last() == Some(&0) {
            data.pop();
        }
        Some((found.kind, data))
    })
}

/// A key's values: (name, type, data, strings without their terminator).
pub fn values(path: &str) -> Vec<(String, u32, Vec<u8>)> {
    let path = canonical(path);
    with(|r| {
        r.root
            .find(&path)
            .map(|key| {
                key.values
                    .iter()
                    .map(|v| {
                        let mut data = v.data.clone();
                        if is_string(v.kind) && data.last() == Some(&0) {
                            data.pop();
                        }
                        (v.name.clone(), v.kind, data)
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// The names of a key's subkeys.
pub fn subkeys(path: &str) -> Vec<String> {
    let path = canonical(path);
    with(|r| r.root.find(&path).map(|key| key.subkeys.iter().map(|k| k.name.clone()).collect()).unwrap_or_default())
}

/// Set a string value (creating the key).
pub fn set_string(path: &str, value: &str, data: &[u8]) {
    let path = canonical(path);
    with(|r| {
        r.root.create(&path);
        r.root.find_mut(&path).expect("created").set_value(value, REG_SZ, sz(data));
        r.dirty = true;
    });
}

/// Set a value of any type (creating the key). String data is ANSI and
/// gets its terminator here.
pub fn set(path: &str, value: &str, kind: u32, data: &[u8]) {
    let path = canonical(path);
    let data = if is_string(kind) && data.last() != Some(&0) { sz(data) } else { data.to_vec() };
    with(|r| {
        r.root.create(&path);
        r.root.find_mut(&path).expect("created").set_value(value, kind, data);
        r.dirty = true;
    });
}

/// Create a key with no values.
pub fn create_key(path: &str) {
    let path = canonical(path);
    with(|r| r.dirty |= !r.root.create(&path));
}

/// Whether a key exists.
pub fn key_exists(path: &str) -> bool {
    let path = canonical(path);
    with(|r| r.root.find(&path).is_some())
}

/// Delete a key and everything under it.
pub fn delete_key(path: &str) {
    let path = canonical(path);
    let Some((last, parent)) = path.split_last() else {
        return;
    };
    with(|r| {
        if let Some(parent) = r.root.find_mut(parent) {
            let before = parent.subkeys.len();
            parent.subkeys.retain(|k| !k.name.eq_ignore_ascii_case(last));
            r.dirty |= parent.subkeys.len() != before;
        }
    });
}

/// Delete one value.
pub fn delete_value(path: &str, value: &str) {
    let path = canonical(path);
    with(|r| {
        if let Some(key) = r.root.find_mut(&path) {
            let before = key.values.len();
            key.values.retain(|v| !v.name.eq_ignore_ascii_case(value));
            r.dirty |= key.values.len() != before;
        }
    });
}

/// A key path with its root spelled out as stored.
fn canonical(path: &str) -> Vec<String> {
    let mut parts = split(path);
    if parts.is_empty() {
        return parts;
    }
    let root = match parts[0].to_ascii_uppercase().as_str() {
        "HKEY_CLASSES_ROOT" | "HKCR" => 0x8000_0000,
        "HKEY_CURRENT_USER" | "HKCU" => 0x8000_0001,
        "HKEY_LOCAL_MACHINE" | "HKLM" => 0x8000_0002,
        "HKEY_USERS" | "HKU" => 0x8000_0003,
        _ => return parts,
    };
    let mut full = root_path(root).unwrap_or_default();
    full.extend(parts.drain(1..));
    full
}

/// Write pending changes to drive C:.
pub fn flush() {
    REGISTRY.with(|cell| {
        if let Some(registry) = cell.borrow_mut().as_mut() {
            registry.save();
        }
    });
}

/// A string value as stored: the bytes and a terminator.
fn sz(text: &[u8]) -> Vec<u8> {
    let mut data = text.to_vec();
    data.push(0);
    data
}

fn is_string(kind: u32) -> bool {
    matches!(kind, REG_SZ | REG_EXPAND_SZ | REG_MULTI_SZ)
}

// ── REGEDIT4 files ─────────────────────────────────────────────────────────

fn format(root: &Key) -> String {
    let mut out = String::from("REGEDIT4\r\n");
    fn walk(key: &Key, path: &str, out: &mut String) {
        if !path.is_empty() {
            out.push_str(&format!("\r\n[{path}]\r\n"));
            for value in &key.values {
                let name = if value.name.is_empty() {
                    "@".to_owned()
                } else {
                    format!("\"{}\"", escape(value.name.as_bytes()))
                };
                out.push_str(&format!("{name}={}\r\n", format_data(value)));
            }
        }
        for child in &key.subkeys {
            let child_path = if path.is_empty() {
                child.name.clone()
            } else {
                format!("{path}\\{}", child.name)
            };
            walk(child, &child_path, out);
        }
    }
    walk(root, "", &mut out);
    out
}

fn escape(bytes: &[u8]) -> String {
    let mut text = String::new();
    for &byte in bytes {
        match byte {
            b'\\' => text.push_str("\\\\"),
            b'"' => text.push_str("\\\""),
            _ => text.push(byte as char),
        }
    }
    text
}

fn format_data(value: &Value) -> String {
    let data = &value.data;
    let plain = value.kind == REG_SZ
        && data.last() == Some(&0)
        && !data[..data.len() - 1].iter().any(|&b| b == 0 || b == b'\r' || b == b'\n');
    if plain {
        return format!("\"{}\"", escape(&data[..data.len() - 1]));
    }
    if value.kind == 4 && data.len() == 4 {
        return format!("dword:{:08x}", u32::from_le_bytes([data[0], data[1], data[2], data[3]]));
    }
    let hex: Vec<String> = data.iter().map(|b| format!("{b:02x}")).collect();
    let hex = hex.join(",");
    if value.kind == 3 {
        format!("hex:{hex}")
    } else {
        format!("hex({:x}):{hex}", value.kind)
    }
}

fn parse(text: &str) -> Key {
    let mut root = Key::default();
    let mut current: Option<Vec<String>> = None;
    let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>().into_iter();
    while let Some(mut line) = lines.next() {
        // Hex data continues on the next line after a trailing backslash.
        while line.trim_end().ends_with('\\') && line.contains("=hex") {
            line = line.trim_end().trim_end_matches('\\').to_owned();
            match lines.next() {
                Some(next) => line.push_str(next.trim()),
                None => break,
            }
        }
        let line = line.trim_end_matches('\r');
        if let Some(path) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let path = split(path);
            root.create(&path);
            current = Some(path);
            continue;
        }
        let Some(path) = &current else {
            continue;
        };
        let Some((name, data)) = parse_value(line) else {
            continue;
        };
        if let Some(key) = root.find_mut(path) {
            key.set_value(&name, data.0, data.1);
        }
    }
    root
}

/// One `name=data` line.
fn parse_value(line: &str) -> Option<(String, (u32, Vec<u8>))> {
    let (name, rest) = if let Some(rest) = line.strip_prefix('@') {
        (String::new(), rest.strip_prefix('=')?)
    } else {
        let (name, rest) = parse_quoted(line.strip_prefix('"')?)?;
        (String::from_utf8_lossy(&name).into_owned(), rest.strip_prefix('=')?)
    };
    let data = if let Some(text) = rest.strip_prefix('"') {
        (REG_SZ, sz(&parse_quoted(text)?.0))
    } else if let Some(hex) = rest.strip_prefix("dword:") {
        (4, u32::from_str_radix(hex.trim(), 16).ok()?.to_le_bytes().to_vec())
    } else if let Some(hex) = rest.strip_prefix("hex:") {
        (3, parse_hex(hex))
    } else if let Some(rest) = rest.strip_prefix("hex(") {
        let (kind, hex) = rest.split_once("):")?;
        (u32::from_str_radix(kind, 16).ok()?, parse_hex(hex))
    } else {
        return None;
    };
    Some((name, data))
}

/// The bytes of a quoted string (after its opening quote) and the rest.
fn parse_quoted(text: &str) -> Option<(Vec<u8>, &str)> {
    let mut bytes = Vec::new();
    let mut chars = text.char_indices();
    while let Some((index, c)) = chars.next() {
        match c {
            '\\' => {
                let (_, next) = chars.next()?;
                bytes.push(next as u32 as u8);
            }
            '"' => return Some((bytes, &text[index + 1..])),
            _ => bytes.push(c as u32 as u8),
        }
    }
    None
}

fn parse_hex(text: &str) -> Vec<u8> {
    text.split(',')
        .filter_map(|b| u8::from_str_radix(b.trim(), 16).ok())
        .collect()
}

// ── The API ────────────────────────────────────────────────────────────────

pub fn arg_count(name: &str) -> Option<usize> {
    let base = name.strip_suffix('A').or_else(|| name.strip_suffix('W')).unwrap_or(name);
    Some(match base {
        "RegCloseKey" | "RegFlushKey" => 1,
        "RegDeleteKey" | "RegDeleteValue" => 2,
        "RegOpenKey" | "RegCreateKey" => 3,
        "RegQueryValue" | "RegEnumKey" => 4,
        "RegOpenKeyEx" | "RegSetValue" | "RegNotifyChangeKeyValue" => 5,
        "RegQueryValueEx" | "RegSetValueEx" => 6,
        "RegEnumKeyEx" | "RegEnumValue" => 8,
        "RegCreateKeyEx" => 9,
        "RegQueryInfoKey" => 12,
        _ => return None,
    })
}

/// A string argument: ANSI, or UTF-16 for the `W` functions.
fn read_name<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, wide: bool) -> String {
    if va == 0 {
        return String::new();
    }
    let bytes = if wide { read_wide(memory, va) } else { read_cstr(memory, va) };
    bytes.iter().map(|&b| b as char).collect()
}

/// A UTF-16 string as ANSI bytes.
fn read_wide<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = va;
    while let Some(unit) = memory.read_bytes(at, 2) {
        let unit = u16::from_le_bytes([unit[0], unit[1]]);
        if unit == 0 || out.len() > 1 << 16 {
            break;
        }
        out.push(if unit < 0x100 { unit as u8 } else { b'?' });
        at += 2;
    }
    out
}

fn to_wide(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().flat_map(|&b| [b, 0]).collect()
}

fn from_wide(bytes: &[u8]) -> Vec<u8> {
    bytes
        .chunks_exact(2)
        .map(|c| {
            let unit = u16::from_le_bytes([c[0], c[1]]);
            if unit < 0x100 {
                unit as u8
            } else {
                b'?'
            }
        })
        .collect()
}

/// Write a name into a buffer of `capacity` characters (with terminator).
/// Returns false when it does not fit.
fn write_name<M: GuestMemory + ?Sized>(
    memory: &mut M,
    va: u32,
    capacity: u32,
    name: &str,
    wide: bool,
) -> bool {
    let bytes: Vec<u8> = name.chars().map(|c| c as u32 as u8).collect();
    if bytes.len() as u32 >= capacity {
        return false;
    }
    let mut data = if wide { to_wide(&bytes) } else { bytes };
    data.extend_from_slice(if wide { &[0, 0] } else { &[0] });
    memory.write_bytes(va, &data).is_some()
}

/// Copy value data to a caller's buffer (`size_va` holds its size in bytes
/// and receives the size needed).
fn write_data<M: GuestMemory + ?Sized>(
    memory: &mut M,
    data: &[u8],
    data_va: u32,
    size_va: u32,
) -> u32 {
    if size_va == 0 {
        return if data_va == 0 { SUCCESS } else { INVALID_PARAMETER };
    }
    let capacity = memory.read_u32(size_va).unwrap_or(0) as usize;
    let _ = memory.write_u32(size_va, data.len() as u32);
    if data_va == 0 {
        return SUCCESS;
    }
    if capacity < data.len() {
        return MORE_DATA;
    }
    let _ = memory.write_bytes(data_va, data);
    SUCCESS
}

/// Value data as a caller sees it: strings in UTF-16 for `W` functions.
fn visible(kind: u32, data: &[u8], wide: bool) -> Vec<u8> {
    if wide && is_string(kind) {
        to_wide(data)
    } else {
        data.to_vec()
    }
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    _ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let wide = name.ends_with('W');
    let base = name.strip_suffix('A').or_else(|| name.strip_suffix('W')).unwrap_or(name);
    let status = call(memory, base, wide, args);
    if std::env::var_os("OTW_TRACE_REGISTRY").is_some() {
        let sub = match base {
            "RegCloseKey" | "RegFlushKey" | "RegEnumKey" | "RegEnumKeyEx" | "RegEnumValue"
            | "RegQueryInfoKey" | "RegNotifyChangeKeyValue" => String::new(),
            _ => read_name(memory, args[1], wide),
        };
        let path = with(|r| r.path(args[0])).map(|p| p.join("\\")).unwrap_or_default();
        eprintln!("[reg] {name} {path} {sub:?} -> {status}");
    }
    Some(ApiOutcome::new(status, count))
}

fn call<M: GuestMemory + ?Sized>(memory: &mut M, name: &str, wide: bool, args: &[u32]) -> u32 {
    let Some(base) = with(|r| r.path(args[0])) else {
        return BAD_HANDLE;
    };
    let sub_path = |memory: &mut M, va: u32| {
        let mut path = base.clone();
        path.extend(split(&read_name(memory, va, wide).replace('/', "\\")));
        path
    };
    match name {
        "RegCloseKey" => with(|r| {
            r.handles.remove(&args[0]);
            r.save_soon();
            SUCCESS
        }),
        "RegFlushKey" => with(|r| {
            r.save();
            SUCCESS
        }),
        "RegNotifyChangeKeyValue" => SUCCESS,
        "RegOpenKey" | "RegOpenKeyEx" | "RegCreateKey" | "RegCreateKeyEx" => {
            let path = sub_path(memory, args[1]);
            let out = match name {
                "RegOpenKey" | "RegCreateKey" => args[2],
                "RegOpenKeyEx" => args[4],
                _ => args[7],
            };
            if out == 0 {
                return INVALID_PARAMETER;
            }
            let result = with(|r| {
                let existed = if name.starts_with("RegCreate") {
                    let existed = r.root.create(&path);
                    r.dirty |= !existed;
                    existed
                } else if r.root.find(&path).is_none() {
                    return None;
                } else {
                    true
                };
                Some((r.open(path), existed))
            });
            match result {
                Some((handle, existed)) => {
                    let _ = memory.write_u32(out, handle);
                    if name == "RegCreateKeyEx" && args[8] != 0 {
                        let _ = memory.write_u32(args[8], if existed { 2 } else { 1 });
                    }
                    SUCCESS
                }
                None => {
                    let _ = memory.write_u32(out, 0);
                    NOT_FOUND
                }
            }
        }
        "RegDeleteKey" => {
            let path = sub_path(memory, args[1]);
            let Some((last, parent)) = path.split_last() else {
                return INVALID_PARAMETER;
            };
            with(|r| {
                let Some(parent) = r.root.find_mut(parent) else {
                    return NOT_FOUND;
                };
                let before = parent.subkeys.len();
                parent.subkeys.retain(|k| !k.name.eq_ignore_ascii_case(last));
                if parent.subkeys.len() == before {
                    return NOT_FOUND;
                }
                r.dirty = true;
                SUCCESS
            })
        }
        "RegDeleteValue" => {
            let value = read_name(memory, args[1], wide);
            with(|r| {
                let Some(key) = r.root.find_mut(&base) else {
                    return NOT_FOUND;
                };
                let before = key.values.len();
                key.values.retain(|v| !v.name.eq_ignore_ascii_case(&value));
                if key.values.len() == before {
                    return NOT_FOUND;
                }
                r.dirty = true;
                SUCCESS
            })
        }
        "RegSetValue" => {
            // RegSetValue(key, subkey, REG_SZ, data, length): a subkey's default.
            let path = sub_path(memory, args[1]);
            let text = if wide { read_wide(memory, args[3]) } else { read_cstr(memory, args[3]) };
            with(|r| {
                r.root.create(&path);
                r.root.find_mut(&path).expect("created").set_value("", REG_SZ, sz(&text));
                r.dirty = true;
                SUCCESS
            })
        }
        "RegSetValueEx" => {
            let value = read_name(memory, args[1], wide);
            let kind = args[3];
            let mut data = memory.read_bytes(args[4], args[5] as usize).unwrap_or_default();
            if is_string(kind) {
                if wide {
                    data = from_wide(&data);
                }
                // A string's stored length covers its terminator.
                if data.last() != Some(&0) {
                    data.push(0);
                }
            }
            with(|r| {
                let Some(key) = r.root.find_mut(&base) else {
                    return BAD_HANDLE;
                };
                key.set_value(&value, kind, data);
                r.dirty = true;
                SUCCESS
            })
        }
        "RegQueryValue" => {
            // RegQueryValue(key, subkey, data, &size): a subkey's default, ""
            // when it has none.
            let path = sub_path(memory, args[1]);
            let Some(data) = with(|r| {
                let key = r.root.find(&path)?;
                Some(match key.value("") {
                    Some(value) if is_string(value.kind) => value.data.clone(),
                    _ => vec![0],
                })
            }) else {
                return NOT_FOUND;
            };
            write_data(memory, &visible(REG_SZ, &data, wide), args[2], args[3])
        }
        "RegQueryValueEx" => {
            let value = read_name(memory, args[1], wide);
            let Some((kind, data)) = with(|r| {
                let found = r.root.find(&base)?.value(&value)?;
                Some((found.kind, found.data.clone()))
            }) else {
                return NOT_FOUND;
            };
            if args[3] != 0 {
                let _ = memory.write_u32(args[3], kind);
            }
            write_data(memory, &visible(kind, &data, wide), args[4], args[5])
        }
        "RegEnumKey" | "RegEnumKeyEx" => {
            let Some(child) = with(|r| {
                r.root
                    .find(&base)
                    .and_then(|key| key.subkeys.get(args[1] as usize))
                    .map(|key| key.name.clone())
            }) else {
                return NO_MORE_ITEMS;
            };
            if name == "RegEnumKey" {
                return if write_name(memory, args[2], args[3], &child, wide) {
                    SUCCESS
                } else {
                    MORE_DATA
                };
            }
            let capacity = memory.read_u32(args[3]).unwrap_or(0);
            if !write_name(memory, args[2], capacity, &child, wide) {
                return MORE_DATA;
            }
            let _ = memory.write_u32(args[3], child.len() as u32);
            if args[5] != 0 && args[6] != 0 {
                let capacity = memory.read_u32(args[6]).unwrap_or(0);
                let _ = write_name(memory, args[5], capacity, "", wide);
                let _ = memory.write_u32(args[6], 0);
            }
            if args[7] != 0 {
                let _ = memory.write_bytes(args[7], &[0; 8]);
            }
            SUCCESS
        }
        "RegEnumValue" => {
            // (key, index, name, &name_len, reserved, &type, data, &data_size)
            let Some(value) = with(|r| r.root.find(&base)?.values.get(args[1] as usize).cloned())
            else {
                return NO_MORE_ITEMS;
            };
            let capacity = memory.read_u32(args[3]).unwrap_or(0);
            if !write_name(memory, args[2], capacity, &value.name, wide) {
                return MORE_DATA;
            }
            let _ = memory.write_u32(args[3], value.name.len() as u32);
            if args[5] != 0 {
                let _ = memory.write_u32(args[5], value.kind);
            }
            write_data(memory, &visible(value.kind, &value.data, wide), args[6], args[7])
        }
        "RegQueryInfoKey" => {
            // (key, class, &class_len, reserved, &subkeys, &max_subkey_len,
            //  &max_class_len, &values, &max_value_name_len, &max_value_len,
            //  &security_descriptor_size, &last_write_time)
            let Some(info) = with(|r| {
                let key = r.root.find(&base)?;
                let unit = if wide { 2 } else { 1 };
                Some([
                    key.subkeys.len() as u32,
                    key.subkeys.iter().map(|k| k.name.len() as u32).max().unwrap_or(0),
                    0,
                    key.values.len() as u32,
                    key.values.iter().map(|v| v.name.len() as u32).max().unwrap_or(0),
                    key.values
                        .iter()
                        .map(|v| v.data.len() as u32 * if is_string(v.kind) { unit } else { 1 })
                        .max()
                        .unwrap_or(0),
                    0,
                ])
            }) else {
                return BAD_HANDLE;
            };
            if args[1] != 0 && args[2] != 0 {
                let capacity = memory.read_u32(args[2]).unwrap_or(0);
                let _ = write_name(memory, args[1], capacity, "", wide);
                let _ = memory.write_u32(args[2], 0);
            }
            for (index, value) in (4..11).zip(info) {
                if args[index] != 0 {
                    let _ = memory.write_u32(args[index], value);
                }
            }
            if args[11] != 0 {
                let _ = memory.write_bytes(args[11], &[0; 8]);
            }
            SUCCESS
        }
        _ => NOT_FOUND,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regedit_files_round_trip() {
        let mut root = Key::default();
        let path = split("HKEY_LOCAL_MACHINE\\SOFTWARE\\Classes\\CLSID\\{1}\\InprocServer32");
        root.create(&path);
        let key = root.find_mut(&path).unwrap();
        key.set_value("", REG_SZ, sz(b"C:\\WINDOWS\\SYSTEM\\A \"B\".DLL"));
        key.set_value("ThreadingModel", REG_SZ, sz(b"Apartment"));
        key.set_value("Flags", 4, 0x1234u32.to_le_bytes().to_vec());
        key.set_value("Raw", 3, vec![1, 2, 0xFF]);
        key.set_value("Path", REG_EXPAND_SZ, sz(b"%windir%"));
        let text = format(&root);
        assert!(text.contains("@=\"C:\\\\WINDOWS\\\\SYSTEM\\\\A \\\"B\\\".DLL\""));
        assert!(text.contains("\"Flags\"=dword:00001234"));
        assert_eq!(parse(&text), root);
    }

    #[test]
    fn names_are_case_insensitive() {
        let mut root = Key::default();
        assert!(!root.create(&split("HKEY_USERS\\.Default\\Software")));
        assert!(root.create(&split("hkey_users\\.DEFAULT\\SOFTWARE")));
        assert_eq!(root.find(&split("HKEY_USERS\\.default")).unwrap().name, ".Default");
    }
}
