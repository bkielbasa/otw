//! MSVCRT20 runtime pieces backed by the guest heap and host files, plus the
//! host file layer shared with KERNEL32's file APIs.
//!
//! Guest paths on drive C: map to a host directory (`OTW_DRIVE_C`, default
//! `~/.otw/C`). Components are matched case-insensitively, like FAT.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

use crate::win32::{read_cstr, write_cstr, ApiContext, ApiOutcome, GuestMemory};

/// Host directory backing guest drive C:.
pub fn drive_root() -> PathBuf {
    if let Some(root) = std::env::var_os("OTW_DRIVE_C") {
        return PathBuf::from(root);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".otw").join("C")
}

/// The guest path (`C:\\...`) of a host file inside drive C:'s directory.
/// Make sure drive C: has the folders every Windows installation has.
pub fn prepare_drive() {
    let root = drive_root();
    for folder in ["WINDOWS/SYSTEM", "WINDOWS/TEMP", "WINDOWS/Desktop", "WINDOWS/Start Menu/Programs"] {
        let _ = std::fs::create_dir_all(root.join(folder));
    }
    // Windows programs that programs run: OTW carries them out itself
    // (see CreateProcess); installers check that the files exist.
    let rundll32 = root.join("WINDOWS/RUNDLL32.EXE");
    if !rundll32.exists() {
        let _ = std::fs::write(rundll32, b"MZ OTW: rundll32 runs built in\r\n");
    }
}

pub fn guest_path_of(host: &std::path::Path) -> Option<Vec<u8>> {
    let host = host.canonicalize().ok()?;
    let (letter, relative) = [(b'C', Some(drive_root())), (b'D', drive_d_root())]
        .into_iter()
        .find_map(|(letter, root)| {
            let root = root?.canonicalize().ok()?;
            host.strip_prefix(&root).ok().map(|r| (letter, r.to_path_buf()))
        })?;
    let mut path = vec![letter, b':'];
    for component in relative.components() {
        path.push(b'\\');
        path.extend_from_slice(component.as_os_str().to_string_lossy().as_bytes());
    }
    if path.len() == 2 {
        path.push(b'\\');
    }
    Some(path)
}

/// Host directory backing drive D:, if any: the folder of a program
/// started from outside drive C: (`OTW_DRIVE_D`), as if from a CD.
pub fn drive_d_root() -> Option<PathBuf> {
    std::env::var_os("OTW_DRIVE_D").map(PathBuf::from)
}

/// The host directory behind a drive letter.
fn root_of(drive: u8) -> Option<PathBuf> {
    match drive.to_ascii_uppercase() {
        b'C' => Some(drive_root()),
        b'D' => drive_d_root(),
        _ => None,
    }
}

/// Translate a guest path to a host path. Existing components are matched
/// case-insensitively; missing ones are kept as given.
pub fn host_path(guest: &[u8]) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(guest).replace('/', "\\");
    // Win32 file namespace paths (\\?\C:\...) are the plain paths.
    let text = text.strip_prefix("\\\\?\\").map(str::to_string).unwrap_or(text);
    // The fonts folder shows the host's fonts (see `win32::fonts`).
    let upper = text.to_ascii_uppercase();
    if let Some(name) = upper.strip_prefix("C:\\WINDOWS\\FONTS\\") {
        if !name.contains('\\') && std::env::var_os("OTW_NO_HOST_FONTS").is_none() {
            if let Some(path) = crate::win32::fonts::db().host_path(name) {
                return Some(path);
            }
        }
    }
    let mut rest = text.as_str();
    let mut drive = b'C';
    if rest.len() >= 2 && rest.as_bytes()[1] == b':' {
        drive = rest.as_bytes()[0];
        rest = &rest[2..];
    }
    let root = root_of(drive)?;
    let _ = std::fs::create_dir_all(&root);
    let mut path = root.clone();
    let relative = !rest.starts_with('\\');
    if relative {
        // The current directory is the root of C:.
    }
    for component in rest.split('\\').filter(|c| !c.is_empty() && *c != ".") {
        if component == ".." {
            if path != root {
                path.pop();
            }
            continue;
        }
        let matched = std::fs::read_dir(&path).ok().and_then(|entries| {
            entries
                .filter_map(Result::ok)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(component)
                })
                .map(|entry| entry.file_name())
        });
        match matched {
            Some(name) => path.push(name),
            None => path.push(component),
        }
    }
    Some(path)
}

/// A guest path made absolute against the process's current directory
/// (`C:\` unless SetCurrentDirectory changed it).
pub fn absolute(ctx: &ApiContext, guest: &[u8]) -> Vec<u8> {
    let guest = guest.strip_prefix(b"\\\\?\\").unwrap_or(guest);
    let has_drive = guest.len() >= 2 && guest[1] == b':';
    if has_drive {
        return guest.to_vec();
    }
    if guest.first().is_some_and(|b| *b == b'\\' || *b == b'/') {
        // The root of the current drive.
        let current = ctx.shared.borrow().current_dir.clone();
        let drive = if current.len() >= 2 && current[1] == b':' { current[..2].to_vec() } else { b"C:".to_vec() };
        return [drive.as_slice(), guest].concat();
    }
    let mut path = ctx.shared.borrow().current_dir.clone();
    if !path.ends_with(b"\\") {
        path.push(b'\\');
    }
    path.extend_from_slice(guest);
    path
}

/// [`host_path`] of a guest path, relative ones resolved against the
/// current directory.
pub fn host_path_in(ctx: &ApiContext, guest: &[u8]) -> Option<PathBuf> {
    host_path(&absolute(ctx, guest))
}

/// Open a host file and register it under a new guest handle.
pub fn open_file(
    ctx: &ApiContext,
    path: &[u8],
    read: bool,
    write: bool,
    create: bool,
    truncate: bool,
    must_not_exist: bool,
) -> Result<u32, u32> {
    let Some(host) = host_path_in(ctx, path) else {
        return Err(3); // ERROR_PATH_NOT_FOUND
    };
    if must_not_exist && host.exists() {
        return Err(80); // ERROR_FILE_EXISTS
    }
    let mut options = std::fs::OpenOptions::new();
    options
        .read(read || !write)
        .write(write)
        .create(create && write)
        .truncate(truncate && write);
    match options.open(&host) {
        Ok(file) => {
            let mut shared = ctx.shared.borrow_mut();
            let handle = shared.alloc_handle() | 0x0070_0000;
            shared.files.insert(handle, file);
            Ok(handle)
        }
        Err(error) => Err(match error.kind() {
            std::io::ErrorKind::NotFound => 2,
            std::io::ErrorKind::PermissionDenied => 5,
            _ => 31,
        }),
    }
}

pub fn read_file(ctx: &ApiContext, handle: u32, length: usize) -> Option<Vec<u8>> {
    let mut shared = ctx.shared.borrow_mut();
    let file = shared.files.get_mut(&handle)?;
    let mut buffer = vec![0u8; length.min(64 << 20)];
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    buffer.truncate(filled);
    Some(buffer)
}

pub fn write_file(ctx: &ApiContext, handle: u32, bytes: &[u8]) -> Option<usize> {
    let mut shared = ctx.shared.borrow_mut();
    let file = shared.files.get_mut(&handle)?;
    file.write_all(bytes).ok()?;
    Some(bytes.len())
}

pub fn seek_file(ctx: &ApiContext, handle: u32, offset: i64, whence: u32) -> Option<u64> {
    let mut shared = ctx.shared.borrow_mut();
    let file = shared.files.get_mut(&handle)?;
    let from = match whence {
        0 => SeekFrom::Start(offset.max(0) as u64),
        1 => SeekFrom::Current(offset),
        _ => SeekFrom::End(offset),
    };
    file.seek(from).ok()
}

pub fn file_size(ctx: &ApiContext, handle: u32) -> Option<u64> {
    let shared = ctx.shared.borrow();
    shared.files.get(&handle)?.metadata().ok().map(|m| m.len())
}

pub fn close_file(ctx: &ApiContext, handle: u32) -> bool {
    ctx.shared.borrow_mut().files.remove(&handle).is_some()
}

// ── C runtime ────────────────────────────────────────────────────────────

/// Guest addresses of CRT data tables, created on first use.
#[derive(Clone, Copy, Debug, Default)]
pub struct CrtTables {
    pub pctype: u32,
    pub pctype_slot: u32,
    pub mbctype: u32,
    pub mb_cur_max: u32,
    pub errno: u32,
    pub doserrno: u32,
    pub tm: u32,
    pub strtok: u32,
}

fn tables<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) -> CrtTables {
    let existing = ctx.shared.borrow().crt_tables;
    if existing.pctype != 0 {
        return existing;
    }
    let mut shared = ctx.shared.borrow_mut();
    let base = shared.heap.alloc(0x800).unwrap_or(0);
    drop(shared);
    let t = CrtTables {
        pctype: base + 2, // index -1 is valid (EOF)
        pctype_slot: base + 0x400,
        mbctype: base + 0x410,
        mb_cur_max: base + 0x520,
        errno: base + 0x524,
        doserrno: base + 0x528,
        tm: base + 0x530,
        strtok: base + 0x560,
    };
    let mut table = vec![0u8; 2 + 512];
    for c in 0..256usize {
        let b = c as u8;
        let mut flags: u16 = 0;
        if b.is_ascii_uppercase() {
            flags |= 0x1 | 0x100;
        }
        if b.is_ascii_lowercase() {
            flags |= 0x2 | 0x100;
        }
        if b.is_ascii_digit() {
            flags |= 0x4;
        }
        if matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r') {
            flags |= 0x8;
        }
        if b.is_ascii_punctuation() {
            flags |= 0x10;
        }
        if b < 0x20 || b == 0x7F {
            flags |= 0x20;
        }
        if b == b' ' {
            flags |= 0x40;
        }
        if b.is_ascii_hexdigit() {
            flags |= 0x80;
        }
        table[2 + c * 2..4 + c * 2].copy_from_slice(&flags.to_le_bytes());
    }
    let _ = memory.write_bytes(base, &table);
    let _ = memory.write_u32(t.pctype_slot, t.pctype);
    let _ = memory.write_bytes(t.mbctype, &[0u8; 260]);
    let _ = memory.write_u32(t.mb_cur_max, 1);
    ctx.shared.borrow_mut().crt_tables = t;
    t
}

fn ctype_flags(c: u32) -> u32 {
    let b = (c & 0xFF) as u8;
    let mut flags = 0;
    if b.is_ascii_uppercase() {
        flags |= 0x1;
    }
    if b.is_ascii_lowercase() {
        flags |= 0x2;
    }
    if b.is_ascii_digit() {
        flags |= 0x4;
    }
    if matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r') {
        flags |= 0x8;
    }
    if b.is_ascii_punctuation() {
        flags |= 0x10;
    }
    if b < 0x20 || b == 0x7F {
        flags |= 0x20;
    }
    if b == b' ' {
        flags |= 0x40;
    }
    if b.is_ascii_hexdigit() {
        flags |= 0x80;
    }
    if b.is_ascii_alphabetic() {
        flags |= 0x100;
    }
    flags
}

fn parse_long(text: &[u8], base: u32) -> (i64, usize) {
    let mut i = 0;
    while i < text.len() && text[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut negative = false;
    if i < text.len() && (text[i] == b'+' || text[i] == b'-') {
        negative = text[i] == b'-';
        i += 1;
    }
    let mut base = base;
    if (base == 0 || base == 16)
        && i + 1 < text.len()
        && text[i] == b'0'
        && (text[i + 1] | 0x20) == b'x'
    {
        base = 16;
        i += 2;
    } else if base == 0 && i < text.len() && text[i] == b'0' {
        base = 8;
    } else if base == 0 {
        base = 10;
    }
    let start = i;
    let mut value: i64 = 0;
    while i < text.len() {
        let digit = match text[i] {
            b'0'..=b'9' => (text[i] - b'0') as u32,
            b'a'..=b'z' => (text[i] - b'a' + 10) as u32,
            b'A'..=b'Z' => (text[i] - b'A' + 10) as u32,
            _ => break,
        };
        if digit >= base {
            break;
        }
        value = value
            .saturating_mul(base as i64)
            .saturating_add(digit as i64);
        i += 1;
    }
    if i == start {
        return (0, 0);
    }
    (if negative { -value } else { value }, i)
}

fn parse_double(text: &[u8]) -> (f64, usize) {
    let mut i = 0;
    while i < text.len() && text[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    if i < text.len() && (text[i] == b'+' || text[i] == b'-') {
        i += 1;
    }
    while i < text.len() && text[i].is_ascii_digit() {
        i += 1;
    }
    if i < text.len() && text[i] == b'.' {
        i += 1;
        while i < text.len() && text[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < text.len() && (text[i] | 0x20) == b'e' {
        let mark = i;
        i += 1;
        if i < text.len() && (text[i] == b'+' || text[i] == b'-') {
            i += 1;
        }
        let digits = i;
        while i < text.len() && text[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits {
            i = mark;
        }
    }
    let value = std::str::from_utf8(&text[start..i])
        .ok()
        .and_then(|s| s.parse::<f64>().ok());
    match value {
        Some(v) => (v, i),
        None => (0.0, 0),
    }
}

/// Seconds since 1970 → broken-down UTC time fields.
fn civil(secs: i64) -> [i32; 9] {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (hour, min, sec) = (
        (rem / 3600) as i32,
        ((rem % 3600) / 60) as i32,
        (rem % 60) as i32,
    );
    // Howard Hinnant's days-from-civil inverse.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as i32;
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;
    let wday = ((days + 4).rem_euclid(7)) as i32;
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let cumulative = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let yday = cumulative[(m - 1) as usize] + d - 1 + if leap && m > 2 { 1 } else { 0 };
    [sec, min, hour, d, m - 1, year - 1900, wday, yday, 0]
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "__p___mb_cur_max" | "__p__mbctype" | "__p__pctype" | "__doserrno" | "_errno" | "abort"
        | "_endthreadex" => 0,
        "malloc"
        | "_strdup"
        | "_msize"
        | "strlen"
        | "wcslen"
        | "time"
        | "localtime"
        | "gmtime"
        | "mktime"
        | "fclose"
        | "fflush"
        | "clearerr"
        | "_get_osfhandle"
        | "_mbclen"
        | "_ismbcdigit"
        | "_ismbcspace"
        | "_setmbcp"
        | "?set_terminate@@YAP6AXXZP6AXXZ@Z"
        | "?_set_new_handler@@YAP6AHI@ZP6AHI@Z@Z"
        | "toupper"
        | "tolower"
        | "isalpha"
        | "isdigit"
        | "isspace"
        | "isalnum"
        | "isupper"
        | "islower"
        | "isxdigit"
        | "atol"
        | "atof"
        | "_mbsinc"
        | "_strupr"
        | "_strlwr"
        | "_mbsupr"
        | "_mbslwr"
        | "srand"
        | "_close"
        | "fgetc"
        | "_isatty"
        | "_fileno"
        | "ftell"
        | "rewind"
        | "_mbsdup" => 1,
        "realloc" | "calloc" | "_mbschr" | "_mbsrchr" | "_mbsstr" | "_mbspbrk" | "_mbscspn"
        | "_mbsspn" | "_mbsdec" | "_isctype" | "_open_osfhandle" | "_fdopen" | "fgetpos"
        | "fputs" | "strtod" | "_expand" | "strcmp" | "strcpy" | "strcat" | "strchr"
        | "strrchr" | "strstr" | "_stricmp" | "_strcmpi" | "strspn" | "strcspn" | "strpbrk"
        | "fopen" | "_mbscpy" | "_mbscat" | "fsetpos" | "fputc" | "strtok" | "_mbstok" => 2,
        "strtol" | "strtoul" | "fgets" | "fseek" | "_gcvt" | "vsprintf" | "memmove" | "memchr"
        | "strncmp" | "_strnicmp" | "strncat" | "_mbsncpy" | "_mbsnbcpy" | "_mbsncmp"
        | "_mbsnicmp" | "_mbsnbicmp" | "_read" | "_write" | "_lseek" | "_vsnprintf" => 3,
        "fread" | "fwrite" | "strftime" | "_snprintf" => 4,
        "sprintf" | "_beginthread" => 8,
        "_beginthreadex" => 6,
        _ => return None,
    })
}

fn cdecl(value: u32) -> Option<ApiOutcome> {
    Some(ApiOutcome::new(value, 0))
}

/// FILE* layout: we keep the host handle in `_file` (offset 16) and an
/// error/eof flag word in `_flag` (offset 12).
fn file_handle<M: GuestMemory + ?Sized>(memory: &mut M, stream: u32) -> u32 {
    memory.read_u32(stream + 16).unwrap_or(0)
}

fn new_stream<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, handle: u32) -> u32 {
    let stream = ctx.shared.borrow_mut().heap.alloc(32).unwrap_or(0);
    if stream != 0 {
        let _ = memory.write_bytes(stream, &[0u8; 32]);
        let _ = memory.write_u32(stream + 16, handle);
    }
    stream
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let t = tables(ctx, memory);
    match name {
        "malloc" => cdecl(ctx.shared.borrow_mut().heap.alloc(args[0]).unwrap_or(0)),
        "calloc" => {
            let size = args[0].saturating_mul(args[1]);
            let address = ctx.shared.borrow_mut().heap.alloc(size).unwrap_or(0);
            if address != 0 {
                let _ = memory.write_bytes(address, &vec![0u8; size as usize]);
            }
            cdecl(address)
        }
        "realloc" => {
            if args[0] == 0 {
                return cdecl(ctx.shared.borrow_mut().heap.alloc(args[1]).unwrap_or(0));
            }
            if args[1] == 0 {
                ctx.shared.borrow_mut().heap.free(args[0]);
                return cdecl(0);
            }
            let old_size = ctx.shared.borrow().heap.size_of(args[0]).unwrap_or(0);
            if old_size >= args[1] {
                return cdecl(args[0]);
            }
            let address = ctx.shared.borrow_mut().heap.alloc(args[1]).unwrap_or(0);
            if address != 0 {
                if let Some(bytes) = memory.read_bytes(args[0], old_size.min(args[1]) as usize) {
                    let _ = memory.write_bytes(address, &bytes);
                }
                ctx.shared.borrow_mut().heap.free(args[0]);
            }
            cdecl(address)
        }
        "_expand" => {
            let size = ctx.shared.borrow().heap.size_of(args[0]).unwrap_or(0);
            cdecl(if size >= args[1] { args[0] } else { 0 })
        }
        "_msize" => cdecl(ctx.shared.borrow().heap.size_of(args[0]).unwrap_or(0)),
        "_strdup" | "_mbsdup" => {
            if args[0] == 0 {
                return cdecl(0);
            }
            let text = read_cstr(memory, args[0]);
            let address = ctx
                .shared
                .borrow_mut()
                .heap
                .alloc(text.len() as u32 + 1)
                .unwrap_or(0);
            let _ = write_cstr(memory, address, &text);
            cdecl(address)
        }
        "strlen" => cdecl(read_cstr(memory, args[0]).len() as u32),
        "wcslen" => {
            let mut length = 0u32;
            while let Some(bytes) = memory.read_bytes(args[0] + length * 2, 2) {
                if bytes == [0, 0] || length > 1 << 20 {
                    break;
                }
                length += 1;
            }
            cdecl(length)
        }
        "memmove" => {
            if let Some(bytes) = memory.read_bytes(args[1], (args[2] as usize).min(64 << 20)) {
                let _ = memory.write_bytes(args[0], &bytes);
            }
            cdecl(args[0])
        }
        "memchr" => {
            let bytes = memory
                .read_bytes(args[0], args[2] as usize)
                .unwrap_or_default();
            cdecl(
                bytes
                    .iter()
                    .position(|b| *b == args[1] as u8)
                    .map(|p| args[0] + p as u32)
                    .unwrap_or(0),
            )
        }
        "strcmp" | "_stricmp" | "_strcmpi" => {
            let mut a = read_cstr(memory, args[0]);
            let mut b = read_cstr(memory, args[1]);
            if name != "strcmp" {
                a.make_ascii_lowercase();
                b.make_ascii_lowercase();
            }
            cdecl(crate::win32::cstr_cmp(&a, &b).signum() as u32)
        }
        "strncmp" | "_strnicmp" | "_mbsncmp" | "_mbsnicmp" | "_mbsnbicmp" => {
            let mut a = read_cstr(memory, args[0]);
            let mut b = read_cstr(memory, args[1]);
            a.truncate(args[2] as usize);
            b.truncate(args[2] as usize);
            if name.contains('i') && name != "strncmp" {
                a.make_ascii_lowercase();
                b.make_ascii_lowercase();
            }
            cdecl(crate::win32::cstr_cmp(&a, &b).signum() as u32)
        }
        "strcpy" | "_mbscpy" => {
            let text = read_cstr(memory, args[1]);
            let _ = write_cstr(memory, args[0], &text);
            cdecl(args[0])
        }
        "strcat" | "_mbscat" | "strncat" => {
            let mut text = read_cstr(memory, args[0]);
            let mut tail = read_cstr(memory, args[1]);
            if name == "strncat" {
                tail.truncate(args[2] as usize);
            }
            text.extend_from_slice(&tail);
            let _ = write_cstr(memory, args[0], &text);
            cdecl(args[0])
        }
        "_mbsncpy" | "_mbsnbcpy" => {
            let mut text = read_cstr(memory, args[1]);
            text.truncate(args[2] as usize);
            let mut padded = text.clone();
            padded.resize(args[2] as usize, 0);
            let _ = memory.write_bytes(args[0], &padded);
            cdecl(args[0])
        }
        "strchr" | "_mbschr" | "strrchr" | "_mbsrchr" => {
            let text = read_cstr(memory, args[0]);
            let ch = args[1] as u8;
            if ch == 0 {
                return cdecl(args[0] + text.len() as u32);
            }
            let found = if name.contains('r') && name != "strchr" && name != "_mbschr" {
                text.iter().rposition(|b| *b == ch)
            } else {
                text.iter().position(|b| *b == ch)
            };
            cdecl(found.map(|p| args[0] + p as u32).unwrap_or(0))
        }
        "strstr" | "_mbsstr" => {
            let hay = read_cstr(memory, args[0]);
            let needle = read_cstr(memory, args[1]);
            if needle.is_empty() {
                return cdecl(args[0]);
            }
            cdecl(
                hay.windows(needle.len())
                    .position(|w| w == needle.as_slice())
                    .map(|p| args[0] + p as u32)
                    .unwrap_or(0),
            )
        }
        "strspn" | "_mbsspn" | "strcspn" | "_mbscspn" | "strpbrk" | "_mbspbrk" => {
            let text = read_cstr(memory, args[0]);
            let set = read_cstr(memory, args[1]);
            let accept = name.ends_with("spn") && !name.contains('c');
            let position = text.iter().position(|b| set.contains(b) != accept);
            if name.ends_with("pbrk") {
                let p = text.iter().position(|b| set.contains(b));
                return cdecl(p.map(|p| args[0] + p as u32).unwrap_or(0));
            }
            cdecl(position.unwrap_or(text.len()) as u32)
        }
        "strtok" | "_mbstok" => {
            let saved = memory.read_u32(t.strtok).unwrap_or(0);
            let start = if args[0] != 0 { args[0] } else { saved };
            if start == 0 {
                return cdecl(0);
            }
            let text = read_cstr(memory, start);
            let delims = read_cstr(memory, args[1]);
            let Some(begin) = text.iter().position(|b| !delims.contains(b)) else {
                let _ = memory.write_u32(t.strtok, 0);
                return cdecl(0);
            };
            let end = text[begin..]
                .iter()
                .position(|b| delims.contains(b))
                .map(|p| p + begin);
            match end {
                Some(end) => {
                    let _ = memory.write_bytes(start + end as u32, &[0]);
                    let _ = memory.write_u32(t.strtok, start + end as u32 + 1);
                }
                None => {
                    let _ = memory.write_u32(t.strtok, 0);
                }
            }
            cdecl(start + begin as u32)
        }
        "_mbsinc" => cdecl(args[0] + 1),
        "_mbsdec" => cdecl(if args[1] > args[0] { args[1] - 1 } else { 0 }),
        "_mbclen" => cdecl(1),
        "_ismbcdigit" => cdecl((args[0] < 0x100 && (args[0] as u8).is_ascii_digit()) as u32),
        "_ismbcspace" => cdecl((args[0] < 0x100 && (args[0] as u8).is_ascii_whitespace()) as u32),
        "_setmbcp" => cdecl(0),
        "_isctype" => cdecl(ctype_flags(args[0]) & args[1]),
        "toupper" => cdecl((args[0] as u8).to_ascii_uppercase() as u32),
        "tolower" => cdecl((args[0] as u8).to_ascii_lowercase() as u32),
        "isalpha" => cdecl(ctype_flags(args[0]) & 0x103),
        "isdigit" => cdecl(ctype_flags(args[0]) & 0x4),
        "isspace" => cdecl(ctype_flags(args[0]) & 0x8),
        "isalnum" => cdecl(ctype_flags(args[0]) & 0x107),
        "isupper" => cdecl(ctype_flags(args[0]) & 0x1),
        "islower" => cdecl(ctype_flags(args[0]) & 0x2),
        "isxdigit" => cdecl(ctype_flags(args[0]) & 0x80),
        "_strupr" | "_mbsupr" | "_strlwr" | "_mbslwr" => {
            let text = read_cstr(memory, args[0]);
            let converted = if name.contains("upr") {
                text.to_ascii_uppercase()
            } else {
                text.to_ascii_lowercase()
            };
            let _ = memory.write_bytes(args[0], &converted);
            cdecl(args[0])
        }
        "__p___mb_cur_max" => cdecl(t.mb_cur_max),
        "__p__mbctype" => cdecl(t.mbctype),
        "__p__pctype" => cdecl(t.pctype_slot),
        "__doserrno" => cdecl(t.doserrno),
        "_errno" => cdecl(t.errno),
        "strtol" | "strtoul" | "atol" => {
            let text = read_cstr(memory, args[0]);
            let base = if name == "atol" { 10 } else { args[2] };
            let (value, used) = parse_long(&text, base);
            if name != "atol" && args[1] != 0 {
                let _ = memory.write_u32(args[1], args[0] + used as u32);
            }
            cdecl(if name == "strtoul" {
                value as u32
            } else {
                value.clamp(i32::MIN as i64, i32::MAX as i64) as i32 as u32
            })
        }
        "strtod" | "atof" => {
            let text = read_cstr(memory, args[0]);
            let (value, used) = parse_double(&text);
            if name == "strtod" && args[1] != 0 {
                let _ = memory.write_u32(args[1], args[0] + used as u32);
            }
            // Doubles return in ST(0); the CPU backend loads `fpu_result`.
            ctx.shared.borrow_mut().fpu_result = Some(value);
            cdecl(0)
        }
        "_gcvt" => {
            let value = f64::from_bits((args[0] as u64) | ((args[1] as u64) << 32));
            let digits = args[2] as usize;
            let mut text = format!("{value:.*}", digits.saturating_sub(1).min(17));
            if text.contains('.') {
                while text.ends_with('0') {
                    text.pop();
                }
            }
            let _ = write_cstr(memory, args[3.min(args.len() - 1)], text.as_bytes());
            cdecl(args[2])
        }
        "vsprintf" | "_vsnprintf" => {
            let (buffer, limit, format_va, va_list) = if name == "vsprintf" {
                (args[0], usize::MAX, args[1], args[2])
            } else {
                (
                    args[0],
                    args[1] as usize,
                    args[2],
                    args.get(3).copied().unwrap_or(0),
                )
            };
            let format = read_cstr(memory, format_va);
            let mut text =
                crate::win32::controls::format_printf(memory, &format, &[], Some(va_list));
            if text.len() > limit {
                text.truncate(limit);
            }
            let _ = write_cstr(memory, buffer, &text);
            cdecl(text.len() as u32)
        }
        "sprintf" | "_snprintf" => {
            let (buffer, format_va, rest) = if name == "sprintf" {
                (args[0], args[1], &args[2..])
            } else {
                (args[0], args[2], &args[3..])
            };
            let format = read_cstr(memory, format_va);
            let text = crate::win32::controls::format_printf(memory, &format, rest, None);
            let _ = write_cstr(memory, buffer, &text);
            cdecl(text.len() as u32)
        }
        "time" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0) as u32;
            if args[0] != 0 {
                let _ = memory.write_u32(args[0], now);
            }
            cdecl(now)
        }
        "localtime" | "gmtime" => {
            let secs = memory.read_u32(args[0]).unwrap_or(0) as i64;
            let fields = civil(secs);
            let mut bytes = Vec::new();
            for field in fields {
                bytes.extend_from_slice(&field.to_le_bytes());
            }
            let _ = memory.write_bytes(t.tm, &bytes);
            cdecl(t.tm)
        }
        "mktime" => {
            let bytes = memory
                .read_bytes(args[0], 36)
                .unwrap_or_else(|| vec![0; 36]);
            let v = |i: usize| {
                i32::from_le_bytes([
                    bytes[i * 4],
                    bytes[i * 4 + 1],
                    bytes[i * 4 + 2],
                    bytes[i * 4 + 3],
                ]) as i64
            };
            let days = days_from_civil(v(5) + 1900, v(4) + 1, v(3));
            cdecl((days * 86400 + v(2) * 3600 + v(1) * 60 + v(0)) as u32)
        }
        "strftime" => {
            let format = read_cstr(memory, args[2]);
            let bytes = memory
                .read_bytes(args[3], 36)
                .unwrap_or_else(|| vec![0; 36]);
            let v = |i: usize| {
                i32::from_le_bytes([
                    bytes[i * 4],
                    bytes[i * 4 + 1],
                    bytes[i * 4 + 2],
                    bytes[i * 4 + 3],
                ])
            };
            let months = [
                "January",
                "February",
                "March",
                "April",
                "May",
                "June",
                "July",
                "August",
                "September",
                "October",
                "November",
                "December",
            ];
            let days = [
                "Sunday",
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
            ];
            let mut out = String::new();
            let mut chars = format.iter().copied().peekable();
            while let Some(c) = chars.next() {
                if c != b'%' {
                    out.push(c as char);
                    continue;
                }
                let mut spec = chars.next().unwrap_or(b'%');
                if spec == b'#' {
                    spec = chars.next().unwrap_or(b'%');
                }
                match spec {
                    b'Y' => out += &(v(5) + 1900).to_string(),
                    b'y' => out += &format!("{:02}", (v(5) + 1900) % 100),
                    b'm' => out += &format!("{:02}", v(4) + 1),
                    b'd' => out += &format!("{:02}", v(3)),
                    b'H' => out += &format!("{:02}", v(2)),
                    b'I' => out += &format!("{:02}", (v(2) + 11) % 12 + 1),
                    b'M' => out += &format!("{:02}", v(1)),
                    b'S' => out += &format!("{:02}", v(0)),
                    b'p' => out += if v(2) < 12 { "AM" } else { "PM" },
                    b'B' => out += months[(v(4).clamp(0, 11)) as usize],
                    b'b' => out += &months[(v(4).clamp(0, 11)) as usize][..3],
                    b'A' => out += days[(v(6).clamp(0, 6)) as usize],
                    b'a' => out += &days[(v(6).clamp(0, 6)) as usize][..3],
                    b'j' => out += &format!("{:03}", v(7) + 1),
                    b'x' => {
                        out += &format!("{:02}/{:02}/{:02}", v(4) + 1, v(3), (v(5) + 1900) % 100)
                    }
                    b'X' => out += &format!("{:02}:{:02}:{:02}", v(2), v(1), v(0)),
                    b'c' => {
                        out += &format!(
                            "{:02}/{:02}/{:02} {:02}:{:02}:{:02}",
                            v(4) + 1,
                            v(3),
                            (v(5) + 1900) % 100,
                            v(2),
                            v(1),
                            v(0)
                        )
                    }
                    b'%' => out.push('%'),
                    _ => {}
                }
            }
            let mut bytes = out.into_bytes();
            if bytes.len() + 1 > args[1] as usize {
                return cdecl(0);
            }
            bytes.truncate(args[1] as usize - 1);
            let _ = write_cstr(memory, args[0], &bytes);
            cdecl(bytes.len() as u32)
        }
        "srand" => {
            ctx.shared.borrow_mut().crt.rand_state = args[0];
            cdecl(0)
        }
        "?set_terminate@@YAP6AXXZP6AXXZ@Z" | "?_set_new_handler@@YAP6AHI@ZP6AHI@Z@Z" => cdecl(0),
        "abort" => Some(ApiOutcome {
            wide: None,
            value: 0,
            arg_count: 0,
            exit_code: Some(3),
        }),
        "_beginthreadex" | "_beginthread" => {
            eprintln!("[crt] {name}: threads are not supported; reporting failure");
            cdecl(0)
        }
        "_endthreadex" => Some(ApiOutcome {
            wide: None,
            value: 0,
            arg_count: 0,
            exit_code: Some(0),
        }),

        // ── stdio over host files ──
        "_open_osfhandle" => cdecl(args[0]),
        "_get_osfhandle" | "_fileno" => cdecl(if name == "_fileno" {
            file_handle(memory, args[0])
        } else {
            args[0]
        }),
        "_fdopen" => cdecl(new_stream(ctx, memory, args[0])),
        "fopen" => {
            let path = read_cstr(memory, args[0]);
            let mode = read_cstr(memory, args[1]);
            let write = mode.contains(&b'w') || mode.contains(&b'a') || mode.contains(&b'+');
            let read = mode.contains(&b'r') || mode.contains(&b'+');
            let truncate = mode.contains(&b'w');
            match open_file(
                ctx,
                &path,
                read,
                write,
                write && !mode.starts_with(b"r"),
                truncate,
                false,
            ) {
                Ok(handle) => {
                    if mode.contains(&b'a') {
                        seek_file(ctx, handle, 0, 2);
                    }
                    cdecl(new_stream(ctx, memory, handle))
                }
                Err(_) => cdecl(0),
            }
        }
        "fclose" => {
            let handle = file_handle(memory, args[0]);
            close_file(ctx, handle);
            ctx.shared.borrow_mut().heap.free(args[0]);
            cdecl(0)
        }
        "fflush" => cdecl(0),
        "clearerr" => {
            let _ = memory.write_u32(args[0] + 12, 0);
            cdecl(0)
        }
        "fseek" => {
            let handle = file_handle(memory, args[0]);
            cdecl(
                if seek_file(ctx, handle, args[1] as i32 as i64, args[2]).is_some() {
                    0
                } else {
                    u32::MAX
                },
            )
        }
        "ftell" => {
            let handle = file_handle(memory, args[0]);
            cdecl(
                seek_file(ctx, handle, 0, 1)
                    .map(|p| p as u32)
                    .unwrap_or(u32::MAX),
            )
        }
        "rewind" => {
            let handle = file_handle(memory, args[0]);
            seek_file(ctx, handle, 0, 0);
            cdecl(0)
        }
        "fgetpos" => {
            let handle = file_handle(memory, args[0]);
            let position = seek_file(ctx, handle, 0, 1).unwrap_or(0);
            let _ = memory.write_u32(args[1], position as u32);
            let _ = memory.write_u32(args[1] + 4, (position >> 32) as u32);
            cdecl(0)
        }
        "fsetpos" => {
            let handle = file_handle(memory, args[0]);
            let position = memory.read_u32(args[1]).unwrap_or(0);
            seek_file(ctx, handle, position as i64, 0);
            cdecl(0)
        }
        "fread" => {
            let handle = file_handle(memory, args[3]);
            let size = args[1].max(1) as usize;
            let bytes = read_file(ctx, handle, size * args[2] as usize).unwrap_or_default();
            let _ = memory.write_bytes(args[0], &bytes);
            let items = bytes.len() / size;
            if items < args[2] as usize {
                let _ = memory.write_u32(args[3] + 12, 0x10);
            }
            cdecl(items as u32)
        }
        "fwrite" => {
            let handle = file_handle(memory, args[3]);
            let size = args[1].max(1) as usize;
            let bytes = memory
                .read_bytes(args[0], size * args[2] as usize)
                .unwrap_or_default();
            let written = if handle == 0 {
                let _ = std::io::stdout().write_all(&bytes);
                bytes.len()
            } else {
                write_file(ctx, handle, &bytes).unwrap_or(0)
            };
            cdecl((written / size) as u32)
        }
        "fgets" => {
            let handle = file_handle(memory, args[2]);
            let mut line = Vec::new();
            while line.len() + 1 < args[1] as usize {
                let Some(byte) = read_file(ctx, handle, 1) else {
                    break;
                };
                let Some(&byte) = byte.first() else { break };
                line.push(byte);
                if byte == b'\n' {
                    break;
                }
            }
            if line.is_empty() {
                return cdecl(0);
            }
            let _ = write_cstr(memory, args[0], &line);
            cdecl(args[0])
        }
        "fgetc" => {
            let handle = file_handle(memory, args[0]);
            cdecl(
                read_file(ctx, handle, 1)
                    .and_then(|b| b.first().copied())
                    .map(|b| b as u32)
                    .unwrap_or(u32::MAX),
            )
        }
        "fputs" | "fputc" => {
            let handle = file_handle(memory, args[1]);
            let bytes = if name == "fputs" {
                read_cstr(memory, args[0])
            } else {
                vec![args[0] as u8]
            };
            write_file(ctx, handle, &bytes);
            cdecl(if name == "fputc" { args[0] & 0xFF } else { 0 })
        }
        "_isatty" => cdecl(0),
        "_close" => {
            close_file(ctx, args[0]);
            cdecl(0)
        }
        "_read" => {
            let bytes = read_file(ctx, args[0], args[2] as usize).unwrap_or_default();
            let _ = memory.write_bytes(args[1], &bytes);
            cdecl(bytes.len() as u32)
        }
        "_write" => {
            let bytes = memory
                .read_bytes(args[1], args[2] as usize)
                .unwrap_or_default();
            cdecl(write_file(ctx, args[0], &bytes).unwrap_or(0) as u32)
        }
        "_lseek" => cdecl(
            seek_file(ctx, args[0], args[1] as i32 as i64, args[2])
                .map(|p| p as u32)
                .unwrap_or(u32::MAX),
        ),
        _ => None,
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
    fn civil_time_round_trips() {
        let secs = 1_700_000_000i64;
        let fields = civil(secs);
        let days = days_from_civil(
            fields[5] as i64 + 1900,
            fields[4] as i64 + 1,
            fields[3] as i64,
        );
        assert_eq!(
            days * 86400 + fields[2] as i64 * 3600 + fields[1] as i64 * 60 + fields[0] as i64,
            secs
        );
    }

    #[test]
    fn strtol_handles_bases() {
        assert_eq!(parse_long(b"  -42xyz", 10), (-42, 5));
        assert_eq!(parse_long(b"0x1F", 0), (31, 4));
    }
}
