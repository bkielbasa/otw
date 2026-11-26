//! Windows shell links (`.LNK`), in the Win95 binary format: a
//! `ShellLinkHeader`, a `LinkInfo` naming the local target path, and ANSI
//! string data (description, working directory, arguments).

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

const HEADER_SIZE: u32 = 0x4C;
const LINK_CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46,
];
const HAS_ID_LIST: u32 = 0x01;
const HAS_LINK_INFO: u32 = 0x02;
const HAS_NAME: u32 = 0x04;
const HAS_RELATIVE_PATH: u32 = 0x08;
const HAS_WORKING_DIR: u32 = 0x10;
const HAS_ARGUMENTS: u32 = 0x20;
const HAS_ICON_LOCATION: u32 = 0x40;
const IS_UNICODE: u32 = 0x80;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellLink {
    /// Absolute target, e.g. `C:\WINDOWS\NOTEPAD.EXE`.
    pub target: String,
    pub arguments: String,
    pub working_dir: String,
    pub description: String,
}

fn put_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u16).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

pub fn write(link: &ShellLink) -> Vec<u8> {
    let mut flags = HAS_LINK_INFO;
    if !link.description.is_empty() {
        flags |= HAS_NAME;
    }
    if !link.working_dir.is_empty() {
        flags |= HAS_WORKING_DIR;
    }
    if !link.arguments.is_empty() {
        flags |= HAS_ARGUMENTS;
    }
    let mut out = Vec::new();
    out.extend_from_slice(&HEADER_SIZE.to_le_bytes());
    out.extend_from_slice(&LINK_CLSID);
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&0x20u32.to_le_bytes()); // FILE_ATTRIBUTE_ARCHIVE
    out.extend_from_slice(&[0u8; 24]); // creation, access, write times
    out.extend_from_slice(&0u32.to_le_bytes()); // file size
    out.extend_from_slice(&0u32.to_le_bytes()); // icon index
    out.extend_from_slice(&1u32.to_le_bytes()); // SW_SHOWNORMAL
    out.extend_from_slice(&[0u8; 12]); // hot key, reserved

    // LinkInfo with a VolumeID and LocalBasePath.
    let volume: Vec<u8> = {
        let mut v = Vec::new();
        v.extend_from_slice(&17u32.to_le_bytes()); // size
        v.extend_from_slice(&3u32.to_le_bytes()); // DRIVE_FIXED
        v.extend_from_slice(&0u32.to_le_bytes()); // serial number
        v.extend_from_slice(&0x10u32.to_le_bytes()); // label offset
        v.push(0); // empty label
        v
    };
    let base_offset = 0x1C + volume.len() as u32;
    let suffix_offset = base_offset + link.target.len() as u32 + 1;
    let size = suffix_offset + 1;
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&0x1Cu32.to_le_bytes()); // header size
    out.extend_from_slice(&1u32.to_le_bytes()); // VolumeIDAndLocalBasePath
    out.extend_from_slice(&0x1Cu32.to_le_bytes()); // VolumeID offset
    out.extend_from_slice(&base_offset.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // no network link
    out.extend_from_slice(&suffix_offset.to_le_bytes());
    out.extend_from_slice(&volume);
    out.extend_from_slice(link.target.as_bytes());
    out.push(0);
    out.push(0); // common path suffix

    if flags & HAS_NAME != 0 {
        put_string(&mut out, &link.description);
    }
    if flags & HAS_WORKING_DIR != 0 {
        put_string(&mut out, &link.working_dir);
    }
    if flags & HAS_ARGUMENTS != 0 {
        put_string(&mut out, &link.arguments);
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // terminal extra block
    out
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn cstr_at(bytes: &[u8], offset: usize) -> Option<String> {
    let rest = bytes.get(offset..)?;
    let end = rest.iter().position(|b| *b == 0)?;
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// Parse a shell link. `relative_base` is the folder holding the link, for
/// links that only carry a relative path.
pub fn read(bytes: &[u8]) -> Option<ShellLink> {
    if u32_at(bytes, 0)? != HEADER_SIZE || bytes.get(4..20)? != LINK_CLSID {
        return None;
    }
    let flags = u32_at(bytes, 20)?;
    let mut offset = HEADER_SIZE as usize;
    if flags & HAS_ID_LIST != 0 {
        offset += 2 + u16_at(bytes, offset)? as usize;
    }
    let mut link = ShellLink::default();
    if flags & HAS_LINK_INFO != 0 {
        let info = offset;
        let size = u32_at(bytes, info)? as usize;
        let info_flags = u32_at(bytes, info + 8)?;
        if info_flags & 1 != 0 {
            let base = cstr_at(bytes, info + u32_at(bytes, info + 16)? as usize)?;
            let suffix =
                cstr_at(bytes, info + u32_at(bytes, info + 24)? as usize).unwrap_or_default();
            link.target = if suffix.is_empty() || base.ends_with('\\') {
                base + &suffix
            } else {
                format!("{base}\\{suffix}")
            };
        }
        offset = info + size;
    }
    let unicode = flags & IS_UNICODE != 0;
    let mut next = |present: bool| -> Option<Option<String>> {
        if !present {
            return Some(None);
        }
        let count = u16_at(bytes, offset)? as usize;
        offset += 2;
        let text = if unicode {
            let units: Vec<u16> = bytes
                .get(offset..offset + count * 2)?
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            offset += count * 2;
            String::from_utf16_lossy(&units)
        } else {
            let text = String::from_utf8_lossy(bytes.get(offset..offset + count)?).into_owned();
            offset += count;
            text
        };
        Some(Some(text))
    };
    let description = next(flags & HAS_NAME != 0)?;
    let relative = next(flags & HAS_RELATIVE_PATH != 0)?;
    let working_dir = next(flags & HAS_WORKING_DIR != 0)?;
    let arguments = next(flags & HAS_ARGUMENTS != 0)?;
    let _icon = next(flags & HAS_ICON_LOCATION != 0)?;
    link.description = description.unwrap_or_default();
    link.working_dir = working_dir.unwrap_or_default();
    link.arguments = arguments.unwrap_or_default();
    if link.target.is_empty() {
        link.target = relative?;
    }
    Some(link)
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    #[test]
    fn links_round_trip() {
        let link = ShellLink {
            target: "C:\\WINDOWS\\NOTEPAD.EXE".into(),
            arguments: "C:\\README.TXT".into(),
            working_dir: "C:\\WINDOWS".into(),
            description: "Notepad".into(),
        };
        let bytes = write(&link);
        assert_eq!(&bytes[..4], &[0x4C, 0, 0, 0]);
        assert_eq!(read(&bytes), Some(link));
    }

    #[test]
    fn relative_unicode_links_are_read() {
        let mut bytes = write(&ShellLink {
            target: String::new(),
            ..Default::default()
        });
        // Rewrite as a link with only a Unicode relative path.
        bytes.truncate(HEADER_SIZE as usize);
        bytes[20..24].copy_from_slice(&(HAS_RELATIVE_PATH | IS_UNICODE).to_le_bytes());
        let text: Vec<u16> = "..\\APP.EXE".encode_utf16().collect();
        bytes.extend_from_slice(&(text.len() as u16).to_le_bytes());
        for unit in text {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(read(&bytes).unwrap().target, "..\\APP.EXE");
        assert_eq!(read(b"not a link"), None);
    }
}
