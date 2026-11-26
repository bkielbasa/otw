//! Shell icons: a small built-in set in the Win95 style, and the genuine
//! icons stored in program files (`RT_GROUP_ICON` / `RT_ICON` resources).

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::{win32::gfx::Rgb, win32::icon, win32::user::Painter};

/// A decoded icon; `None` pixels are transparent.
/// A decoded icon; `None` pixels are transparent.
pub use crate::win32::icon::IconImage as Bitmap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Folder,
    Document,
    TextDocument,
    Picture,
    Program,
    Computer,
    Drive,
    Run,
    ShutDown,
    Shortcut,
}

const FOLDER: &str = "\
................\
................\
..ooooo.........\
.oyyyyyo........\
oyyyyyyyoooooo..\
owwwwwwwwwwwwyok\
owyyyyyyyyyyyyok\
owyyyyyyyyyyyyok\
owyyyyyyyyyyyyok\
owyyyyyyyyyyyyok\
owyyyyyyyyyyyyok\
owyyyyyyyyyyyyok\
owyyyyyyyyyyyyok\
okkkkkkkkkkkkkkk\
................\
................";

const DOCUMENT: &str = "\
..kkkkkkkkk.....\
..kwwwwwwwkk....\
..kwwwwwwwkwk...\
..kwwwwwwwkkkk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kkkkkkkkkkkk..\
................";

const TEXT_DOCUMENT: &str = "\
..kkkkkkkkk.....\
..kwwwwwwwkk....\
..kwwwwwwwkwk...\
..kwkkkkwwkkkk..\
..kwwwwwwwwwwk..\
..kwkkkkkkkkwk..\
..kwwwwwwwwwwk..\
..kwkkkkkkkkwk..\
..kwwwwwwwwwwk..\
..kwkkkkkkkkwk..\
..kwwwwwwwwwwk..\
..kwkkkkkkwwwk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kkkkkkkkkkkk..\
................";

const PICTURE: &str = "\
..kkkkkkkkk.....\
..kwwwwwwwkk....\
..kwwwwwwwkwk...\
..kwwwwwwwkkkk..\
..kwccccccccwk..\
..kwccyycccBwk..\
..kwccyyccBBwk..\
..kwcccccBBBwk..\
..kwGGcccBBBwk..\
..kwGGGrrrBBwk..\
..kwGGGrrrrrwk..\
..kwccccccccwk..\
..kwwwwwwwwwwk..\
..kwwwwwwwwwwk..\
..kkkkkkkkkkkk..\
................";

const PROGRAM: &str = "\
................\
................\
kkkkkkkkkkkkkkkk\
kbbbbbbbbbbbbbbk\
kbwbbbbbbbbbbbbk\
kkkkkkkkkkkkkkkk\
kwwwwwwwwwwwwwwk\
kwwwwwwwwwwwwwwk\
kwwwwwwwwwwwwwwk\
kwwwwwwwwwwwwwwk\
kwwwwwwwwwwwwwwk\
kwwwwwwwwwwwwwwk\
kwwwwwwwwwwwwwwk\
kkkkkkkkkkkkkkkk\
................\
................";

const COMPUTER: &str = "\
..kkkkkkkkkkkk..\
..klllllllllllk.\
..klkkkkkkkkglk.\
..klkccccccwglk.\
..klkccccccwglk.\
..klkccccccwglk.\
..klwwwwwwwwglk.\
..kllllllllllgk.\
..kgggggggggggk.\
...kkkkkkkkkkk..\
....kllllllgk...\
..kkkkkkkkkkkkk.\
.kwlllllllllllgk\
.klllllllkkkllgk\
.kgggggggggggggk\
.kkkkkkkkkkkkkkk";

const DRIVE: &str = "\
................\
................\
................\
................\
................\
................\
..kkkkkkkkkkkkk.\
.kwwwwwwwwwwwwgk\
kwllllllllllllgk\
kwllllllllllllgk\
klllllllllllGGgk\
kggggggggggggggk\
.kkkkkkkkkkkkkk.\
................\
................\
................";

const RUN: &str = "\
................\
...kkkkkkkkkkk..\
...kbbbbbbbbbk..\
...kwwwwwwwwwk..\
...kwwwwwwwwwk..\
...kwwwwwwwwwk..\
...kwwwwwwwwwk..\
...kkkkkkkkkkk..\
.......kk.......\
......kkkk......\
.....kkkkkk.....\
.......kk.......\
.......kk.......\
....kkkkkkkk....\
....kllllllk....\
....kkkkkkkk....";

const SHUT_DOWN: &str = "\
................\
..kkkkkkkkkkkk..\
..klllllllllllk.\
..klkkkkkkkkglk.\
..klkbbbbbbwglk.\
..klkbbyybbwglk.\
..klkbbyybbwglk.\
..klkbbbbbbwglk.\
..klwwwwwwwwglk.\
..kllllllllllgk.\
..kgggggggggggk.\
...kkkkkkkkkkk..\
....kllllllgk...\
..kkkkkkkkkkkkk.\
..kgggggggggggk.\
..kkkkkkkkkkkkk.";

/// The shortcut overlay arrow, drawn at the icon's bottom-left.
const SHORTCUT: &str = "\
kkkkkkkk\
kwwwwwwk\
kwwkkkwk\
kwwwkkwk\
kwwkwkwk\
kwkwwwwk\
kwwwwwwk\
kkkkkkkk";

fn color(code: u8) -> Option<Rgb> {
    Some(match code {
        b'k' => 0x000000,
        b'w' => 0xFFFFFF,
        b'g' => 0x808080,
        b'l' => 0xC0C0C0,
        b'y' => 0xFFFF00,
        b'o' => 0x808000,
        b'b' => 0x000080,
        b'c' => 0x00FFFF,
        b'B' => 0x0000FF,
        b'r' => 0xFF0000,
        b'G' => 0x008000,
        _ => return None,
    })
}

fn from_art(art: &str) -> Bitmap {
    let side = (art.len() as f64).sqrt() as i32;
    Bitmap {
        width: side,
        height: side,
        pixels: art.bytes().map(color).collect(),
    }
}

pub fn builtin(icon: Builtin) -> Bitmap {
    from_art(match icon {
        Builtin::Folder => FOLDER,
        Builtin::Document => DOCUMENT,
        Builtin::TextDocument => TEXT_DOCUMENT,
        Builtin::Picture => PICTURE,
        Builtin::Program => PROGRAM,
        Builtin::Computer => COMPUTER,
        Builtin::Drive => DRIVE,
        Builtin::Run => RUN,
        Builtin::ShutDown => SHUT_DOWN,
        Builtin::Shortcut => SHORTCUT,
    })
}

/// Draw `bitmap` scaled to `size` pixels square with its top-left at (x, y).
pub fn draw(p: &mut Painter<'_>, bitmap: &Bitmap, x: i32, y: i32, size: i32, selected: bool) {
    for dy in 0..size {
        let sy = dy * bitmap.height / size;
        for dx in 0..size {
            let sx = dx * bitmap.width / size;
            let Some(mut rgb) = bitmap.pixels[(sy * bitmap.width + sx) as usize] else {
                continue;
            };
            if selected && (dx + dy) % 2 == 0 {
                // Win95 dithers selected icons with the highlight colour.
                rgb = 0x000080;
            }
            p.pixel(x + dx, y + dy, rgb);
        }
    }
}

/// Draw the shortcut arrow over an icon drawn at (x, y) with `size`.
pub fn draw_shortcut_overlay(p: &mut Painter<'_>, x: i32, y: i32, size: i32) {
    let overlay = builtin(Builtin::Shortcut);
    let side = if size >= 32 { 10 } else { 8 };
    draw(p, &overlay, x, y + size - side, side, false);
}

// ── Icons inside PE files ────────────────────────────────────────────────

fn u16_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) as u32)
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

struct Resources<'a> {
    bytes: &'a [u8],
    image: &'a crate::kernel::pe::PeImage,
    base: usize,
}

impl Resources<'_> {
    /// Entries of the directory at `offset` (relative to the section) as
    /// (name or id, offset to data, is subdirectory).
    fn entries(&self, offset: usize) -> Vec<(u32, usize, bool)> {
        let at = self.base + offset;
        let count =
            u16_at(self.bytes, at + 12).unwrap_or(0) + u16_at(self.bytes, at + 14).unwrap_or(0);
        (0..count.min(512) as usize)
            .filter_map(|index| {
                let entry = at + 16 + index * 8;
                let name = u32_at(self.bytes, entry)?;
                let data = u32_at(self.bytes, entry + 4)?;
                Some((name, (data & 0x7FFF_FFFF) as usize, data & 0x8000_0000 != 0))
            })
            .collect()
    }

    /// Data of the first language of resource (`kind`, `name`); `None`
    /// name takes the first resource of the type.
    fn find(&self, kind: u32, name: Option<u32>) -> Option<&[u8]> {
        let (_, types, _) = self
            .entries(0)
            .into_iter()
            .find(|(id, _, dir)| *id == kind && *dir)?;
        let (_, names, _) = self
            .entries(types)
            .into_iter()
            .find(|(id, _, dir)| *dir && name.map_or(true, |n| *id == n))?;
        let (_, leaf, _) = *self.entries(names).first()?;
        let rva = u32_at(self.bytes, self.base + leaf)?;
        let size = u32_at(self.bytes, self.base + leaf + 4)? as usize;
        let offset = self.image.rva_to_offset(rva)?;
        self.bytes.get(offset..offset + size)
    }
}

/// The program's first icon at `size` (16 or 32), decoded.
pub fn from_pe(bytes: &[u8], size: i32) -> Option<Bitmap> {
    let image = crate::kernel::pe::PeImage::parse(bytes.to_vec()).ok()?;
    let pe = u32_at(bytes, 0x3C)? as usize;
    let optional = pe + 24;
    if u16_at(bytes, optional)? != 0x10B {
        return None;
    }
    let rsrc_rva = u32_at(bytes, optional + 96 + 2 * 8)?;
    if rsrc_rva == 0 {
        return None;
    }
    let resources = Resources {
        bytes,
        image: &image,
        base: image.rva_to_offset(rsrc_rva)?,
    };
    let group = resources.find(14, None)?;
    let id = icon::pick(group, size)?;
    let bitmap = icon::decode_dib(resources.find(3, Some(id))?)?;
    Some(icon::scale(&bitmap, size))
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    #[test]
    fn builtin_icons_are_square() {
        for icon in [
            Builtin::Folder,
            Builtin::Computer,
            Builtin::Picture,
            Builtin::Shortcut,
        ] {
            let bitmap = builtin(icon);
            assert_eq!(bitmap.pixels.len() as i32, bitmap.width * bitmap.height);
        }
    }

    #[test]
    fn program_icons_are_read_from_pe_resources() {
        let Some(bytes) = crate::test_binary("win95/NOTEPAD.EXE") else {
            return;
        };
        let large = from_pe(&bytes, 32).expect("Notepad has an icon");
        assert_eq!((large.width, large.height), (32, 32));
        let opaque = large.pixels.iter().filter(|p| p.is_some()).count();
        assert!(
            opaque > 200 && opaque < 1024,
            "icon has a transparent outline, {opaque} opaque pixels"
        );
        let small = from_pe(&bytes, 16).unwrap();
        assert_eq!(small.width, 16);
    }
}
