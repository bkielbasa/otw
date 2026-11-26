//! VERSION.DLL: reading the version resource of a file on drive C:.
//!
//! The version block is copied as stored (32-bit files keep it in UTF-16);
//! `VerQueryValueA` answers `\` with the fixed file information, and
//! `\VarFileInfo\Translation` and `\StringFileInfo\<lang>\<name>` with the
//! value, strings converted to ANSI.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::win32::{crt, read_cstr, ApiContext, ApiOutcome, GuestMemory};

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "GetFileVersionInfoSizeA" | "GetFileVersionInfoSizeW" => 2,
        "GetFileVersionInfoA" | "GetFileVersionInfoW" | "VerQueryValueA" | "VerQueryValueW" => 4,
        "VerFindFileA" | "VerInstallFileA" => 8,
        _ => return None,
    })
}

/// "VS_VERSION_INFO" in UTF-16.
fn signature() -> Vec<u8> {
    "VS_VERSION_INFO".bytes().flat_map(|b| [b, 0]).collect()
}

/// The version block of a PE file.
fn version_block(bytes: &[u8]) -> Option<Vec<u8>> {
    let at = bytes.windows(30).position(|w| w == signature().as_slice())?;
    let start = at.checked_sub(6)?;
    let length = u16::from_le_bytes([bytes[start], bytes[start + 1]]) as usize;
    bytes.get(start..start + length).map(<[u8]>::to_vec)
}

fn path_arg<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, wide: bool) -> Vec<u8> {
    if wide {
        crate::win32::unicode::wide_to_ansi(&crate::win32::unicode::read_wstr(memory, va))
    } else {
        read_cstr(memory, va)
    }
}

fn file_block(ctx: &ApiContext, path: &[u8]) -> Option<Vec<u8>> {
    let host = crt::host_path_in(ctx, path)?;
    version_block(&std::fs::read(host).ok()?)
}

/// A child block of `block` (a version structure) by key, as (start, length,
/// value offset, value length, type).
fn children(block: &[u8]) -> Vec<(String, usize, usize)> {
    // Header: wLength, wValueLength, wType, key (UTF-16, NUL), padding, value.
    let mut list = Vec::new();
    let header = |at: usize| -> Option<(usize, usize, String, usize)> {
        let length = u16::from_le_bytes([*block.get(at)?, *block.get(at + 1)?]) as usize;
        let mut key = String::new();
        let mut cursor = at + 6;
        while cursor + 1 < block.len() {
            let unit = u16::from_le_bytes([block[cursor], block[cursor + 1]]);
            cursor += 2;
            if unit == 0 {
                break;
            }
            key.push(char::from_u32(unit as u32).unwrap_or('?'));
        }
        let value = (cursor + 3) & !3;
        Some((length, value, key, cursor))
    };
    let Some((length, value, _, _)) = header(0) else {
        return list;
    };
    let value_length = u16::from_le_bytes([block[2], block[3]]) as usize;
    let mut at = (value + value_length + 3) & !3;
    while at + 6 < length.min(block.len()) {
        let Some((child_length, _, key, _)) = header(at) else { break };
        if child_length == 0 {
            break;
        }
        list.push((key, at, child_length));
        at = (at + child_length + 3) & !3;
    }
    list
}

/// Find a value by path: (offset of the value in `block`, value length in
/// bytes, is text).
fn query(block: &[u8], path: &str) -> Option<(usize, usize, bool)> {
    let mut start = 0;
    let mut length = block.len();
    for part in path.split('\\').filter(|p| !p.is_empty()) {
        let sub = &block[start..start + length];
        let (_, at, child) = children(sub).into_iter().find(|(key, _, _)| key.eq_ignore_ascii_case(part))?;
        start += at;
        length = child;
    }
    let sub = &block[start..start + length];
    let value_length = u16::from_le_bytes([sub[2], sub[3]]) as usize;
    let text = u16::from_le_bytes([sub[4], sub[5]]) == 1;
    let mut cursor = 6;
    while cursor + 1 < sub.len() && (sub[cursor] != 0 || sub[cursor + 1] != 0) {
        cursor += 2;
    }
    let value = (cursor + 2 + 3) & !3;
    // Text lengths count characters.
    let bytes = if text { value_length * 2 } else { value_length };
    Some((start + value, bytes, text))
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    let wide = name.ends_with('W');
    let value = match name {
        "GetFileVersionInfoSizeA" | "GetFileVersionInfoSizeW" => {
            let path = path_arg(memory, args[0], wide);
            if std::env::var_os("OTW_TRACE_FILES").is_some() {
                eprintln!("[file] version of {:?}: {:?}", String::from_utf8_lossy(&path), file_block(ctx, &path).map(|b| b.len()));
            }
            if args[1] != 0 {
                let _ = memory.write_u32(args[1], 0);
            }
            match file_block(ctx, &path) {
                Some(block) => block.len() as u32,
                None => {
                    ctx.shared.borrow_mut().last_error = 1813; // ERROR_RESOURCE_TYPE_NOT_FOUND
                    0
                }
            }
        }
        "GetFileVersionInfoA" | "GetFileVersionInfoW" => {
            // (path, handle, size, buffer)
            let path = path_arg(memory, args[0], wide);
            match file_block(ctx, &path) {
                Some(block) => {
                    let length = block.len().min(args[2] as usize);
                    let _ = memory.write_bytes(args[3], &block[..length]);
                    1
                }
                None => 0,
            }
        }
        "VerQueryValueA" | "VerQueryValueW" => {
            // (block, sub-block, &pointer, &length)
            let total = memory.read_bytes(args[0], 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
            let block = memory.read_bytes(args[0], total as usize).unwrap_or_default();
            let path: String = path_arg(memory, args[1], wide).iter().map(|&b| b as char).collect();
            match query(&block, &path) {
                Some((offset, length, text)) if !block.is_empty() => {
                    let (pointer, size) = if text && !wide {
                        // An ANSI copy of the string.
                        let units: Vec<u16> = block[offset..(offset + length).min(block.len())]
                            .chunks_exact(2)
                            .map(|c| u16::from_le_bytes([c[0], c[1]]))
                            .take_while(|&u| u != 0)
                            .collect();
                        let ansi = crate::win32::unicode::wide_to_ansi(&units);
                        let copy = ctx.shared.borrow_mut().heap.alloc(ansi.len() as u32 + 1).unwrap_or(0);
                        let _ = memory.write_bytes(copy, &[ansi.as_slice(), &[0]].concat());
                        (copy, ansi.len() as u32 + 1)
                    } else {
                        (args[0] + offset as u32, if text { length as u32 / 2 } else { length as u32 })
                    };
                    let _ = memory.write_u32(args[2], pointer);
                    if args[3] != 0 {
                        let _ = memory.write_u32(args[3], size);
                    }
                    1
                }
                _ => 0,
            }
        }
        // VerInstallFileA(flags, source name, destination name, source
        // folder, destination folder, current folder, temporary name,
        // &temporary length): copy the file over.
        "VerInstallFileA" => {
            let name = |memory: &mut M, va: u32| String::from_utf8_lossy(&read_cstr(memory, va)).into_owned();
            let (source, target) = (name(memory, args[1]), name(memory, args[2]));
            let (from_dir, to_dir) = (name(memory, args[3]), name(memory, args[4]));
            let from = format!("{}\\{source}", from_dir.trim_end_matches('\\'));
            let to = format!("{}\\{target}", to_dir.trim_end_matches('\\'));
            let copied = match (crt::host_path_in(ctx, from.as_bytes()), crt::host_path_in(ctx, to.as_bytes())) {
                (Some(from), Some(to)) => std::fs::copy(from, to).is_ok(),
                _ => false,
            };
            if args[7] != 0 {
                let _ = memory.write_u32(args[7], 0);
            }
            if copied { 0 } else { 0x20 } // VIF_CANNOTREADSRC
        }
        "VerFindFileA" => {
            // (flags, file, Windows dir, application dir, &current dir,
            //  &length, &destination dir, &length): where a file is and
            //  where it belongs (the system folder when shared).
            const VFFF_ISSHAREDFILE: u32 = 1;
            let file = read_cstr(memory, args[1]);
            let app_dir = read_cstr(memory, args[3]);
            let system = b"C:\\WINDOWS\\SYSTEM".to_vec();
            let windows = b"C:\\WINDOWS".to_vec();
            let exists = |dir: &[u8]| {
                crt::host_path_in(ctx, &[dir, b"\\", &file].concat()).is_some_and(|p| p.exists())
            };
            let current = [system.clone(), windows.clone(), app_dir.clone()].into_iter().find(|dir| !dir.is_empty() && exists(dir)).unwrap_or_default();
            let destination = if args[0] & VFFF_ISSHAREDFILE != 0 || app_dir.is_empty() { system } else { app_dir };
            let write = |memory: &mut M, buffer: u32, length: u32, text: &[u8]| {
                let capacity = memory.read_u32(length).unwrap_or(0) as usize;
                if buffer != 0 && capacity > text.len() {
                    let _ = memory.write_bytes(buffer, &[text, &[0]].concat());
                }
                let _ = memory.write_u32(length, text.len() as u32 + 1);
            };
            write(memory, args[4], args[5], &current);
            write(memory, args[6], args[7], &destination);
            u32::from(!current.is_empty() && !current.eq_ignore_ascii_case(&destination)) // VFF_CURNEDEST
        }
        _ => return None,
    };
    Some(ApiOutcome::new(value, count))
}
