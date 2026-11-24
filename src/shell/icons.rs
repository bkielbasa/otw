//! Shell icons: a small built-in set in the Win95 style, and the genuine
//! icons stored in program files (`RT_GROUP_ICON` / `RT_ICON` resources).

use crate::{win32::gfx::Rgb, win32::user::Painter};

/// A decoded icon; `None` pixels are transparent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<Option<Rgb>>,
}

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
    let count = u16_at(group, 4)? as usize;
    // Prefer the requested size with the most colours up to 256.
    let mut best: Option<(i32, u32, u32)> = None;
    for index in 0..count.min(64) {
        let entry = 6 + index * 14;
        let width = match *group.get(entry)? {
            0 => 256,
            w => w as i32,
        };
        let bits = u16_at(group, entry + 6)?;
        let id = u16_at(group, entry + 12)?;
        let score = (width - size).abs() * 64 - if bits <= 8 { bits as i32 } else { 0 };
        if best.map_or(true, |(s, _, _)| score < s) {
            best = Some((score, id, bits));
        }
    }
    let (_, id, _) = best?;
    let bitmap = decode_dib(resources.find(3, Some(id))?)?;
    Some(if bitmap.width == size {
        bitmap
    } else {
        scale(&bitmap, size)
    })
}

fn scale(bitmap: &Bitmap, size: i32) -> Bitmap {
    let mut pixels = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            pixels.push(
                bitmap.pixels[((y * bitmap.height / size) * bitmap.width + x * bitmap.width / size)
                    as usize],
            );
        }
    }
    Bitmap {
        width: size,
        height: size,
        pixels,
    }
}

/// Decode an icon image: BITMAPINFOHEADER, palette, XOR bits, AND mask.
fn decode_dib(dib: &[u8]) -> Option<Bitmap> {
    let header = u32_at(dib, 0)? as usize;
    let width = u32_at(dib, 4)? as i32;
    let height = u32_at(dib, 8)? as i32 / 2;
    let bits = u16_at(dib, 14)?;
    if !(1..=256).contains(&width) || !(1..=256).contains(&height) {
        return None;
    }
    let colors = if bits <= 8 {
        match u32_at(dib, 32)? {
            0 => 1 << bits,
            n => n as usize,
        }
    } else {
        0
    };
    let palette = header;
    let xor = palette + colors * 4;
    let xor_stride = ((width as usize * bits as usize + 31) / 32) * 4;
    let and = xor + xor_stride * height as usize;
    let and_stride = ((width as usize + 31) / 32) * 4;
    let mut pixels = vec![None; (width * height) as usize];
    for y in 0..height as usize {
        let row = height as usize - 1 - y;
        for x in 0..width as usize {
            let mask = dib.get(and + row * and_stride + x / 8)? >> (7 - x % 8) & 1;
            if mask != 0 {
                continue;
            }
            let line = xor + row * xor_stride;
            let rgb = match bits {
                1 | 4 | 8 => {
                    let bit = x * bits as usize;
                    let byte = *dib.get(line + bit / 8)?;
                    let index =
                        (byte >> (8 - bits as usize - bit % 8)) & ((1u16 << bits) - 1) as u8;
                    let entry = palette + index as usize * 4;
                    (*dib.get(entry + 2)? as u32) << 16
                        | (*dib.get(entry + 1)? as u32) << 8
                        | *dib.get(entry)? as u32
                }
                24 | 32 => {
                    let at = line + x * (bits as usize / 8);
                    (*dib.get(at + 2)? as u32) << 16
                        | (*dib.get(at + 1)? as u32) << 8
                        | *dib.get(at)? as u32
                }
                _ => return None,
            };
            pixels[y * width as usize + x] = Some(rgb);
        }
    }
    Some(Bitmap {
        width,
        height,
        pixels,
    })
}

#[cfg(test)]
mod tests {
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
