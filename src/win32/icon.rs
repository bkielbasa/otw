//! Icons: decoding `RT_GROUP_ICON`/`RT_ICON` resources and the system
//! icons, for `LoadIconA`, window captions and the taskbar's notification
//! area.

use crate::win32::{
    gfx::Rgb,
    user::{self, ResName},
    ApiContext, GuestMemory,
};

/// A decoded icon; `None` pixels are transparent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconImage {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<Option<Rgb>>,
}

/// An icon handle's images: 32x32 for windows and dialogs, 16x16 for
/// captions and the notification area.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Icon {
    pub large: IconImage,
    pub small: IconImage,
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) as u32)
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// The `RT_ICON` id in an icon group (`GRPICONDIR`) closest to `size`,
/// preferring more colours up to 256.
pub fn pick(group: &[u8], size: i32) -> Option<u32> {
    let count = u16_at(group, 4)? as usize;
    let mut best: Option<(i32, u32)> = None;
    for index in 0..count.min(64) {
        let entry = 6 + index * 14;
        let width = match *group.get(entry)? {
            0 => 256,
            w => w as i32,
        };
        let bits = u16_at(group, entry + 6)?;
        let id = u16_at(group, entry + 12)?;
        let score = (width - size).abs() * 64 - if bits <= 8 { bits as i32 } else { 0 };
        if best.map_or(true, |(s, _)| score < s) {
            best = Some((score, id));
        }
    }
    best.map(|(_, id)| id)
}

/// Resize to `size` pixels square (nearest neighbour).
pub fn scale(bitmap: &IconImage, size: i32) -> IconImage {
    if bitmap.width == size && bitmap.height == size {
        return bitmap.clone();
    }
    let mut pixels = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            pixels.push(
                bitmap.pixels[((y * bitmap.height / size) * bitmap.width + x * bitmap.width / size)
                    as usize],
            );
        }
    }
    IconImage {
        width: size,
        height: size,
        pixels,
    }
}

/// Decode an icon image: BITMAPINFOHEADER, palette, XOR bits, AND mask.
pub fn decode_dib(dib: &[u8]) -> Option<IconImage> {
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
    Some(IconImage {
        width,
        height,
        pixels,
    })
}

/// Load icon `name` from a module's resources (or any loaded module when
/// `module` is 0) at `size`.
pub fn load<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    module: u32,
    name: &ResName,
    size: i32,
) -> Option<IconImage> {
    let (group_va, group_size) =
        user::find_resource_any(ctx, memory, module, &ResName::Id(14), name)?;
    let group = memory.read_bytes(group_va, group_size.min(4096) as usize)?;
    let id = pick(&group, size)?;
    let (icon_va, icon_size) =
        user::find_resource_any(ctx, memory, module, &ResName::Id(3), &ResName::Id(id))?;
    let dib = memory.read_bytes(icon_va, icon_size.min(0x40000) as usize)?;
    Some(scale(&decode_dib(&dib)?, size))
}

/// The icons `LoadIconA(NULL, IDI_*)` returns.
pub fn system_icon(id: u32) -> Option<Icon> {
    // 16x16 art, drawn at both sizes: k black, w white, b blue, y yellow,
    // r red, g grey.
    let art: &str = match id {
        // IDI_APPLICATION: a window.
        32512 | 32517 => concat!(
            "................",
            "kkkkkkkkkkkkkkkk",
            "kbbbbbbbbbbbbbbk",
            "kbwbbbbbbbbbbbbk",
            "kkkkkkkkkkkkkkkk",
            "kwwwwwwwwwwwwwwk",
            "kwwwwwwwwwwwwwwk",
            "kwwwwwwwwwwwwwwk",
            "kwwwwwwwwwwwwwwk",
            "kwwwwwwwwwwwwwwk",
            "kwwwwwwwwwwwwwwk",
            "kwwwwwwwwwwwwwwk",
            "kwwwwwwwwwwwwwwk",
            "kwwwwwwwwwwwwwwk",
            "kkkkkkkkkkkkkkkk",
            "................",
        ),
        // IDI_HAND: a red stop sign with a white cross.
        32513 => concat!(
            ".....kkkkkk.....",
            "...kkrrrrrrkk...",
            "..krrrrrrrrrrk..",
            ".krrrrrrrrrrrrk.",
            ".krrwrrrrrrwrrk.",
            "krrrrwrrrrwrrrrk",
            "krrrrrwrrwrrrrrk",
            "krrrrrrwwrrrrrrk",
            "krrrrrrwwrrrrrrk",
            "krrrrrwrrwrrrrrk",
            "krrrrwrrrrwrrrrk",
            ".krrwrrrrrrwrrk.",
            ".krrrrrrrrrrrrk.",
            "..krrrrrrrrrrk..",
            "...kkrrrrrrkk...",
            ".....kkkkkk.....",
        ),
        // IDI_QUESTION and IDI_ASTERISK: a white bubble with ? or i.
        32514 | 32516 => {
            if id == 32514 {
                concat!(
                    ".....kkkkkk.....",
                    "...kkwwwwwwkk...",
                    "..kwwwwwwwwwwk..",
                    ".kwwwwbbbbwwwwk.",
                    ".kwwwbbwwbbwwwk.",
                    "kwwwwwwwwbbwwwwk",
                    "kwwwwwwwbbwwwwwk",
                    "kwwwwwwbbwwwwwwk",
                    "kwwwwwwbbwwwwwwk",
                    "kwwwwwwwwwwwwwwk",
                    ".kwwwwwbbwwwwwk.",
                    ".kwwwwwbbwwwwwk.",
                    "..kwwwwwwwwwwk..",
                    "...kkwwwwwwkk...",
                    ".....kkkkkk.....",
                    "................",
                )
            } else {
                concat!(
                    ".....kkkkkk.....",
                    "...kkwwwwwwkk...",
                    "..kwwwwbbwwwwk..",
                    ".kwwwwwbbwwwwwk.",
                    ".kwwwwwwwwwwwwk.",
                    "kwwwwwbbbwwwwwwk",
                    "kwwwwwwbbwwwwwwk",
                    "kwwwwwwbbwwwwwwk",
                    "kwwwwwwbbwwwwwwk",
                    "kwwwwwwbbwwwwwwk",
                    ".kwwwwbbbbwwwwk.",
                    ".kwwwwwwwwwwwwk.",
                    "..kwwwwwwwwwwk..",
                    "...kkwwwwwwkk...",
                    ".....kkkkkk.....",
                    "................",
                )
            }
        }
        // IDI_EXCLAMATION: a yellow triangle with !.
        32515 => concat!(
            ".......kk.......",
            "......kyyk......",
            "......kyyk......",
            ".....kyyyyk.....",
            ".....kykkyk.....",
            "....kyykkyyk....",
            "....kyykkyyk....",
            "...kyyykkyyyk...",
            "...kyyykkyyyk...",
            "..kyyyykkyyyyk..",
            "..kyyyyyyyyyyk..",
            ".kyyyyykkyyyyyk.",
            ".kyyyyykkyyyyyk.",
            "kyyyyyyyyyyyyyyk",
            "kkkkkkkkkkkkkkkk",
            "................",
        ),
        _ => return None,
    };
    let pixels = art
        .bytes()
        .map(|code| match code {
            b'k' => Some(0x000000),
            b'w' => Some(0xFFFFFF),
            b'b' => Some(0x000080),
            b'y' => Some(0xFFFF00),
            b'r' => Some(0xFF0000),
            b'g' => Some(0x808080),
            _ => None,
        })
        .collect();
    let small = IconImage {
        width: 16,
        height: 16,
        pixels,
    };
    Some(Icon {
        large: scale(&small, 32),
        small,
    })
}
