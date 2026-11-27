//! A minimal ZIP reader, enough to take `Public/index.db` out of the
//! winget `source2.msix` package (an MSIX package is a ZIP64 archive).
//! Supports stored and deflated entries and ZIP64 sizes and offsets.

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    method: u16,
    compressed_size: u64,
    size: u64,
    local_offset: u64,
}

pub struct Archive<'a> {
    data: &'a [u8],
    entries: Vec<Entry>,
}

fn le16(b: &[u8], off: usize) -> Result<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| Error::format("truncated ZIP archive"))
}

fn le32(b: &[u8], off: usize) -> Result<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| Error::format("truncated ZIP archive"))
}

fn le64(b: &[u8], off: usize) -> Result<u64> {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| Error::format("truncated ZIP archive"))
}

const EOCD: u32 = 0x0605_4b50;
const EOCD64: u32 = 0x0606_4b50;
const EOCD64_LOCATOR: u32 = 0x0706_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const LOCAL: u32 = 0x0403_4b50;

impl<'a> Archive<'a> {
    pub fn open(data: &'a [u8]) -> Result<Archive<'a>> {
        // The end of central directory record is 22 bytes plus a comment of
        // up to 64 KiB, so search backwards from the end.
        let lowest = data.len().saturating_sub(22 + 65535);
        let eocd = (lowest..=data.len().saturating_sub(22))
            .rev()
            .find(|&i| le32(data, i).ok() == Some(EOCD))
            .ok_or_else(|| Error::format("not a ZIP archive"))?;

        let mut count = le16(data, eocd + 10)? as u64;
        let mut cd_offset = le32(data, eocd + 16)? as u64;
        if count == 0xffff || cd_offset == 0xffff_ffff {
            // ZIP64: the locator sits right before the end record.
            let loc = eocd
                .checked_sub(20)
                .filter(|&l| le32(data, l).ok() == Some(EOCD64_LOCATOR))
                .ok_or_else(|| Error::format("missing ZIP64 locator"))?;
            let rec = le64(data, loc + 8)? as usize;
            if le32(data, rec)? != EOCD64 {
                return Err(Error::format("bad ZIP64 end record"));
            }
            count = le64(data, rec + 32)?;
            cd_offset = le64(data, rec + 48)?;
        }

        let mut entries = Vec::new();
        let mut pos = usize::try_from(cd_offset).map_err(|_| Error::format("bad ZIP offset"))?;
        for _ in 0..count {
            if le32(data, pos)? != CENTRAL {
                return Err(Error::format("bad ZIP central directory"));
            }
            let method = le16(data, pos + 10)?;
            let mut compressed_size = le32(data, pos + 20)? as u64;
            let mut size = le32(data, pos + 24)? as u64;
            let name_len = le16(data, pos + 28)? as usize;
            let extra_len = le16(data, pos + 30)? as usize;
            let comment_len = le16(data, pos + 32)? as usize;
            let mut local_offset = le32(data, pos + 42)? as u64;
            let name_bytes = data
                .get(pos + 46..pos + 46 + name_len)
                .ok_or_else(|| Error::format("truncated ZIP archive"))?;
            let name = String::from_utf8_lossy(name_bytes).into_owned();

            // A ZIP64 extra field holds the real values of the fields that
            // are 0xffffffff, in this order.
            let mut extra = pos + 46 + name_len;
            let extra_end = extra + extra_len;
            while extra + 4 <= extra_end {
                let id = le16(data, extra)?;
                let len = le16(data, extra + 2)? as usize;
                if id == 0x0001 {
                    let mut f = extra + 4;
                    for field in [&mut size, &mut compressed_size, &mut local_offset] {
                        if *field == 0xffff_ffff && f + 8 <= extra + 4 + len {
                            *field = le64(data, f)?;
                            f += 8;
                        }
                    }
                }
                extra += 4 + len;
            }

            entries.push(Entry {
                name,
                method,
                compressed_size,
                size,
                local_offset,
            });
            pos = extra_end + comment_len;
        }
        Ok(Archive { data, entries })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The uncompressed contents of the entry called `name`. MSIX packages
    /// percent-encode some characters in names, so a name matches in either form.
    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.name == name || crate::util::percent_decode(&e.name) == name)
            .ok_or_else(|| Error::format(format!("{name} is not in the archive")))?;
        let off =
            usize::try_from(entry.local_offset).map_err(|_| Error::format("bad ZIP offset"))?;
        if le32(self.data, off)? != LOCAL {
            return Err(Error::format("bad ZIP local header"));
        }
        let start =
            off + 30 + le16(self.data, off + 26)? as usize + le16(self.data, off + 28)? as usize;
        let raw = usize::try_from(entry.compressed_size)
            .ok()
            .and_then(|len| self.data.get(start..start.checked_add(len)?))
            .ok_or_else(|| Error::format("truncated ZIP entry"))?;
        let out = match entry.method {
            0 => raw.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec(raw)
                .map_err(|e| Error::format(format!("bad deflate data in {name}: {e:?}")))?,
            m => {
                return Err(Error::format(format!(
                    "unsupported ZIP method {m} for {name}"
                )))
            }
        };
        if out.len() as u64 != entry.size {
            return Err(Error::format(format!("{name} has the wrong size")));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample.zip"
        ))
        .unwrap()
    }

    #[test]
    fn lists_entries() {
        let data = fixture();
        let zip = Archive::open(&data).unwrap();
        let names: Vec<&str> = zip.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Public/index.db", "AppxManifest.xml"]);
    }

    #[test]
    fn reads_deflated_and_stored_entries() {
        let data = fixture();
        let zip = Archive::open(&data).unwrap();
        assert_eq!(
            zip.read("Public/index.db").unwrap(),
            b"pretend database ".repeat(200)
        );
        assert_eq!(zip.read("AppxManifest.xml").unwrap(), b"<Package/>");
        assert!(zip.read("missing").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(Archive::open(b"definitely not a zip file at all, no").is_err());
        let data = fixture();
        assert!(Archive::open(&data[..data.len() - 30]).is_err());
    }
}
