//! The screen: the firmware's framebuffer, drawn through a back buffer in
//! memory so frames appear whole.

use alloc::vec;
use alloc::vec::Vec;

#[path = "../../src/host/font.rs"]
mod font;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8);

pub const BLACK: Color = Color(0, 0, 0);
pub const WHITE: Color = Color(255, 255, 255);
pub const TEAL: Color = Color(0, 128, 128);
pub const FACE: Color = Color(192, 192, 192);
pub const SHADOW: Color = Color(128, 128, 128);
pub const NAVY: Color = Color(0, 0, 128);

pub struct Screen {
    pub width: usize,
    pub height: usize,
    stride: usize,
    /// Blue in the lowest byte (BGR) or red (RGB).
    bgr: bool,
    framebuffer: *mut u32,
    back: Vec<u32>,
}

impl Screen {
    pub fn new(framebuffer: *mut u32, width: usize, height: usize, stride: usize, bgr: bool) -> Self {
        Self { width, height, stride, bgr, framebuffer, back: vec![0; width * height] }
    }

    fn pixel(&self, color: Color) -> u32 {
        let Color(r, g, b) = color;
        if self.bgr {
            (r as u32) << 16 | (g as u32) << 8 | b as u32
        } else {
            (b as u32) << 16 | (g as u32) << 8 | r as u32
        }
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        let value = self.pixel(color);
        let x0 = x.clamp(0, self.width as i32) as usize;
        let x1 = (x + w).clamp(0, self.width as i32) as usize;
        let y0 = y.clamp(0, self.height as i32) as usize;
        let y1 = (y + h).clamp(0, self.height as i32) as usize;
        for row in y0..y1 {
            self.back[row * self.width + x0..row * self.width + x1].fill(value);
        }
    }

    pub fn put(&mut self, x: i32, y: i32, color: Color) {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
            let value = self.pixel(color);
            self.back[y as usize * self.width + x as usize] = value;
        }
    }

    /// A raised (or, pressed, sunken) 3D edge in the Windows 95 style.
    pub fn bevel(&mut self, x: i32, y: i32, w: i32, h: i32, pressed: bool) {
        let (light, dark) = if pressed { (BLACK, WHITE) } else { (WHITE, BLACK) };
        self.fill(x, y, w, 1, light);
        self.fill(x, y, 1, h, light);
        self.fill(x, y + h - 1, w, 1, dark);
        self.fill(x + w - 1, y, 1, h, dark);
        self.fill(x + 1, y + h - 2, w - 2, 1, SHADOW);
        self.fill(x + w - 2, y + 1, 1, h - 2, SHADOW);
    }

    /// Text in OTW's 5x7 font, `scale` pixels per font pixel; returns the
    /// width drawn.
    pub fn text(&mut self, x: i32, y: i32, text: &str, color: Color, scale: i32) -> i32 {
        let mut cx = x;
        for c in text.chars() {
            let code = c as u32;
            if (32..127).contains(&code) {
                let index = (code - 32) as usize;
                for row in 0..7 {
                    for column in 0..5 {
                        if glyph_pixel(index, row, column) {
                            self.fill(cx + column as i32 * scale, y + row as i32 * scale, scale, scale, color);
                        }
                    }
                }
            }
            cx += 6 * scale;
        }
        cx - x
    }

    /// The mouse pointer (an arrow) at `(x, y)`.
    pub fn pointer(&mut self, x: i32, y: i32) {
        const ARROW: [&str; 16] = [
            "X", "XX", "X.X", "X..X", "X...X", "X....X", "X.....X", "X......X", "X.......X", "X........X",
            "X.....XXXXX", "X..X..X", "X.X X..X", "XX  X..X", "X    X..X", "     XXX",
        ];
        for (row, line) in ARROW.iter().enumerate() {
            for (column, c) in line.chars().enumerate() {
                let color = match c {
                    'X' => BLACK,
                    '.' => WHITE,
                    _ => continue,
                };
                self.put(x + column as i32, y + row as i32, color);
            }
        }
    }

    /// Show the back buffer.
    pub fn present(&mut self) {
        for row in 0..self.height {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.back.as_ptr().add(row * self.width),
                    self.framebuffer.add(row * self.stride),
                    self.width,
                );
            }
        }
    }
}

/// The font's pixel at (row, column) of glyph `index` (character - 32).
/// FONT_DATA holds each glyph's 7 rows of 5 columns in turn ('#' is ink).
fn glyph_pixel(index: usize, row: usize, column: usize) -> bool {
    font::FONT_DATA.get(index * 35 + row * 5 + column).is_some_and(|&b| b == b'#')
}

/// Stop screen: the message in white on blue, drawn straight to the
/// framebuffer (no memory needed).
pub unsafe fn blue_screen(info: &crate::BootInfo, message: &str) {
    let (blue, white) = if info.bgr { (0x0000_00AAu32, 0x00FF_FFFFu32) } else { (0x00AA_0000u32, 0x00FF_FFFFu32) };
    for row in 0..info.height {
        for column in 0..info.width {
            *info.framebuffer.add(row * info.stride + column) = blue;
        }
    }
    let scale = 2;
    let put_text = |x0: usize, y0: usize, text: &str| {
        for (i, c) in text.chars().enumerate() {
            let code = c as u32;
            if !(32..127).contains(&code) {
                continue;
            }
            for row in 0..7 {
                for column in 0..5 {
                    if glyph_pixel((code - 32) as usize, row, column) {
                        for dy in 0..scale {
                            for dx in 0..scale {
                                let x = x0 + i * 6 * scale + column * scale + dx;
                                let y = y0 + row * scale + dy;
                                if x < info.width && y < info.height {
                                    *info.framebuffer.add(y * info.stride + x) = white;
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    put_text(40, 40, "OTW has stopped.");
    let per_line = (info.width.saturating_sub(80) / (6 * scale)).max(10);
    let chars: alloc::vec::Vec<char> = message.chars().collect();
    for (line, chunk) in chars.chunks(per_line).enumerate().take(20) {
        let text: alloc::string::String = chunk.iter().collect();
        put_text(40, 80 + line * 20, &text);
    }
    put_text(40, 80 + 22 * 20, "Restart the machine to continue.");
}
