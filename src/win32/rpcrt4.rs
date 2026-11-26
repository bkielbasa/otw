//! RPCRT4: what COM proxy/stub DLLs (such as IE's ACTXPRXY) and UUID users
//! call. Every object lives in its program's single apartment, so nothing is
//! ever marshalled: proxies register successfully and are never created.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::win32::{ApiContext, ApiOutcome, GuestMemory};

const S_OK: u32 = 0;
const S_FALSE: u32 = 1;
const CLASS_E_CLASSNOTAVAILABLE: u32 = 0x8004_0111;

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "NdrDllCanUnloadNow" | "UuidCreate" | "RpcStringFreeA" | "RpcStringFreeW" => 1,
        "UuidToStringA" | "UuidToStringW" | "UuidFromStringA" | "UuidFromStringW" => 2,
        "NdrDllRegisterProxy" | "NdrDllUnregisterProxy" => 3,
        "NdrDllGetClassObject" => 6,
        _ => return None,
    })
}

fn uuid_text(bytes: &[u8]) -> String {
    let d1 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let d2 = u16::from_le_bytes([bytes[4], bytes[5]]);
    let d3 = u16::from_le_bytes([bytes[6], bytes[7]]);
    let tail: String = bytes[8..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{d1:08x}-{d2:04x}-{d3:04x}-{}-{}", &tail[..4], &tail[4..])
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    let value = match name {
        "NdrDllRegisterProxy" | "NdrDllUnregisterProxy" => S_OK,
        "NdrDllGetClassObject" => {
            let _ = memory.write_u32(args[2], 0);
            CLASS_E_CLASSNOTAVAILABLE
        }
        "NdrDllCanUnloadNow" => S_FALSE,
        "UuidCreate" => {
            // Version 4 (random) UUIDs.
            let mut bytes = [0u8; 16];
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut state = seed as u64 ^ 0x9E37_79B9_7F4A_7C15 ^ args[0] as u64;
            for byte in &mut bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            bytes[7] = (bytes[7] & 0x0F) | 0x40;
            bytes[8] = (bytes[8] & 0x3F) | 0x80;
            let _ = memory.write_bytes(args[0], &bytes);
            S_OK
        }
        "UuidToStringA" | "UuidToStringW" => {
            let bytes = memory.read_bytes(args[0], 16).unwrap_or_else(|| vec![0; 16]);
            let text = uuid_text(&bytes);
            let mut data: Vec<u8> = if name.ends_with('W') {
                text.bytes().flat_map(|b| [b, 0]).chain([0, 0]).collect()
            } else {
                text.bytes().chain([0]).collect()
            };
            data.truncate(data.len());
            let block = ctx.shared.borrow_mut().bump_alloc(data.len() as u32);
            let _ = memory.write_bytes(block, &data);
            let _ = memory.write_u32(args[1], block);
            S_OK
        }
        "UuidFromStringA" | "UuidFromStringW" => 1705, // RPC_S_INVALID_STRING_UUID
        _ => S_OK,
    };
    Some(ApiOutcome::new(value, count))
}
