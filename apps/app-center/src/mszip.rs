//! Decompression of `.mszyml` files.
//!
//! The winget source2 format stores each package's version list as YAML
//! compressed with the Windows Compression API's MSZIP algorithm in buffer
//! mode. The layout, as observed on cdn.winget.microsoft.com:
//!
//! ```text
//! 0   4  magic 0A 51 E5 C0
//! 4   1  header size (0x18)
//! 5   1  0
//! 6   1  checksum of the header
//! 7   1  algorithm (2 = MSZIP)
//! 8   8  total uncompressed size (little endian)
//! 16  8  uncompressed block size
//! 24     blocks: u32 compressed length, then "CK" and a raw deflate stream
//! ```
//!
//! As in MSZIP generally, a block's deflate stream may refer back into the
//! output of the blocks before it, so all blocks inflate into one buffer.

use crate::error::{Error, Result};
use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;

const MAGIC: [u8; 4] = [0x0a, 0x51, 0xe5, 0xc0];

/// Refuse to allocate more than this for one file.
const MAX_SIZE: u64 = 256 << 20;

pub fn decompress_buffer(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 24 || data[..4] != MAGIC {
        return Err(Error::format("not an MSZIP compressed buffer"));
    }
    if data[7] != 2 {
        return Err(Error::format(format!(
            "unsupported compression algorithm {}",
            data[7]
        )));
    }
    let header_size = data[4] as usize;
    let total = u64::from_le_bytes(data[8..16].try_into().unwrap());
    if total > MAX_SIZE || header_size < 24 {
        return Err(Error::format("bad MSZIP header"));
    }
    let total = total as usize;
    let mut out = vec![0u8; total];
    let mut out_pos = 0;
    let mut pos = header_size;
    while out_pos < total {
        let len = data
            .get(pos..pos + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()) as usize)
            .ok_or_else(|| Error::format("truncated MSZIP data"))?;
        pos += 4;
        let block = data
            .get(pos..pos + len)
            .filter(|b| b.starts_with(b"CK"))
            .ok_or_else(|| Error::format("bad MSZIP block"))?;
        pos += len;
        let mut state = DecompressorOxide::new();
        // A non-wrapping output buffer lets back-references reach the
        // output of earlier blocks.
        let flags = inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        let (status, _, written) = decompress(&mut state, &block[2..], &mut out, out_pos, flags);
        if status != TINFLStatus::Done || written == 0 {
            return Err(Error::format(format!("bad MSZIP block: {status:?}")));
        }
        out_pos += written;
    }
    if pos != data.len() {
        return Err(Error::format("trailing data after MSZIP blocks"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn decompresses_a_real_version_list() {
        let out = decompress_buffer(&fixture("7zip.versionData.mszyml")).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("sV: 1.0\n"));
        assert!(text.contains("rP: manifests/7/7zip/7zip/26.06/67fd"));
    }

    #[test]
    fn later_blocks_refer_back_to_earlier_ones() {
        let out = decompress_buffer(&fixture("twoblock.mszyml")).unwrap();
        assert_eq!(out, fixture("twoblock.txt"));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(decompress_buffer(b"plain yaml, not compressed").is_err());
        let mut data = fixture("7zip.versionData.mszyml");
        data.truncate(data.len() - 10);
        assert!(decompress_buffer(&data).is_err());
        let mut data = fixture("7zip.versionData.mszyml");
        data[7] = 3;
        assert!(decompress_buffer(&data).is_err());
    }
}
