//! Colours and fonts of the dark, flat look that matches OTW's shell.
//!
//! Colours are Win32 `COLORREF`s (0x00BBGGRR).

/// A `COLORREF` from red, green and blue.
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Mixes two colours: `t` = 0 gives `a`, 255 gives `b`.
pub fn mix(a: u32, b: u32, t: u8) -> u32 {
    let ch = |c: u32, shift: u32| ((c >> shift) & 0xFF) as u32;
    let m = |shift: u32| {
        let (x, y) = (ch(a, shift), ch(b, shift));
        ((x * (255 - t as u32) + y * t as u32 + 127) / 255) << shift
    };
    m(0) | m(8) | m(16)
}

/// The window background.
pub const BACKGROUND: u32 = rgb(0x12, 0x12, 0x14);
/// The header bar and the results list.
pub const SURFACE: u32 = rgb(0x1A, 0x1A, 0x1D);
/// Raised things: the search box, secondary buttons, cards.
pub const RAISED: u32 = rgb(0x26, 0x26, 0x2B);
pub const HOVER: u32 = rgb(0x22, 0x22, 0x27);
pub const SELECTED: u32 = rgb(0x24, 0x2C, 0x3D);
pub const BORDER: u32 = rgb(0x2E, 0x2E, 0x34);
pub const TEXT: u32 = rgb(0xF2, 0xF2, 0xF5);
pub const TEXT_MUTED: u32 = rgb(0x9A, 0x9A, 0xA4);
pub const TEXT_FAINT: u32 = rgb(0x6A, 0x6A, 0x74);
pub const ACCENT: u32 = rgb(0x3B, 0x82, 0xF6);
pub const ACCENT_HOVER: u32 = rgb(0x5B, 0x96, 0xF8);
pub const ACCENT_TEXT: u32 = rgb(0xFF, 0xFF, 0xFF);
pub const SUCCESS: u32 = rgb(0x22, 0xC5, 0x5E);
pub const WARNING: u32 = rgb(0xF5, 0x9E, 0x0B);
pub const DANGER: u32 = rgb(0xEF, 0x44, 0x44);
pub const TRACK: u32 = rgb(0x2C, 0x2C, 0x33);
pub const SCROLL_THUMB: u32 = rgb(0x44, 0x44, 0x4C);

/// Background colours for the letter tiles that stand in for app icons.
pub const AVATARS: [u32; 8] = [
    rgb(0x3B, 0x82, 0xF6),
    rgb(0x8B, 0x5C, 0xF6),
    rgb(0xEC, 0x48, 0x99),
    rgb(0xF9, 0x73, 0x16),
    rgb(0x10, 0xB9, 0x81),
    rgb(0x06, 0xB6, 0xD4),
    rgb(0xEA, 0xB3, 0x08),
    rgb(0x64, 0x74, 0x8B),
];

/// The tile colour of a package: always the same for the same id.
pub fn avatar_color(id: &str) -> u32 {
    // FNV-1a, so the colour does not change between runs or builds.
    let mut h: u32 = 0x811C_9DC5;
    for b in id.to_ascii_lowercase().bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    AVATARS[(h % AVATARS.len() as u32) as usize]
}

/// The letter on a package's tile.
pub fn avatar_letter(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into())
}

/// The face asked for; OTW and Windows fall back to their UI font.
pub const FONT_FACE: &str = "Segoe UI";

/// The fonts the window uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Font {
    /// "App Center" in the header.
    Brand,
    /// The package name in the details panel.
    Title,
    /// Section headings and list row names.
    Strong,
    Body,
    Small,
    Button,
}

impl Font {
    pub const ALL: [Font; 6] = [
        Font::Brand,
        Font::Title,
        Font::Strong,
        Font::Body,
        Font::Small,
        Font::Button,
    ];

    /// Character height in pixels and weight (400 normal, 600 semibold).
    pub fn spec(self) -> (i32, i32) {
        match self {
            Font::Brand => (22, 600),
            Font::Title => (28, 600),
            Font::Strong => (16, 600),
            Font::Body => (15, 400),
            Font::Small => (13, 400),
            Font::Button => (15, 600),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorref_layout() {
        assert_eq!(rgb(0x11, 0x22, 0x33), 0x0033_2211);
    }

    #[test]
    fn avatars_are_stable() {
        assert_eq!(
            avatar_color("Mozilla.Firefox"),
            avatar_color("mozilla.firefox")
        );
        let distinct: std::collections::HashSet<u32> =
            ["a", "b", "c", "d", "e", "f", "g", "h", "i"]
                .iter()
                .map(|s| avatar_color(s))
                .collect();
        assert!(distinct.len() >= 3);
        assert_eq!(avatar_letter("7-Zip"), "7");
        assert_eq!(avatar_letter("éclair"), "É");
        assert_eq!(avatar_letter("++"), "?");
    }

    #[test]
    fn mixing() {
        let (a, b) = (rgb(0, 0, 0), rgb(255, 100, 50));
        assert_eq!(mix(a, b, 0), a);
        assert_eq!(mix(a, b, 255), b);
        assert_eq!(mix(a, b, 128), rgb(128, 50, 25));
    }

    #[test]
    fn text_is_readable_on_backgrounds() {
        // Rough luminance contrast check: text must be much brighter.
        let lum = |c: u32| (c & 0xFF) + ((c >> 8) & 0xFF) + ((c >> 16) & 0xFF);
        for bg in [BACKGROUND, SURFACE, RAISED, SELECTED, HOVER] {
            assert!(lum(TEXT) > lum(bg) + 400);
            assert!(lum(TEXT_MUTED) > lum(bg) + 200);
        }
    }
}
