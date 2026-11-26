//! The stop screen, drawn straight to the framebuffer.


#[path = "../../src/host/font.rs"]
mod font;

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
