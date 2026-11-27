//! OTW's look: dark, flat and minimal. The taskbar, the shell's own
//! windows, window frames, menus and scroll bars share these colours and
//! drawing helpers; the system colour table (`gdi::sys_color`) takes its
//! caption, menu and highlight colours from here too, so programs that ask
//! for system colours match. Programs' client areas stay light (window
//! and button faces), so text they draw in the default black stays
//! readable.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::win32::gfx::{Rect, Rgb};
use crate::win32::user::Painter;

/// The taskbar: near black.
pub const TASKBAR: Rgb = 0x0B0B0D;
/// A taskbar button for the window in use, or a pressed one.
pub const TASKBAR_ACTIVE: Rgb = 0x2A2A30;
/// The accent: the active window's outline, the task in use, focus.
pub const ACCENT: Rgb = 0x4C8DFF;
/// The accent as a fill under white text (a dialog's default button).
pub const ACCENT_STRONG: Rgb = 0x2F6AD0;
/// The desktop.
pub const DESKTOP: Rgb = 0x14161B;
/// Panels: the shell's windows, menus, dialogs.
pub const SURFACE: Rgb = 0x1F1F23;
/// Content wells: file lists, text fields' surroundings.
pub const WELL: Rgb = 0x17171A;
/// Raised things on a panel: fields, buttons, a hovered row.
pub const RAISED: Rgb = 0x2C2C31;
/// Lines between areas, outlines of panels and fields.
pub const BORDER: Rgb = 0x3A3A41;
pub const TEXT: Rgb = 0xE8E8EB;
pub const TEXT_DIM: Rgb = 0x9D9DA5;
pub const TEXT_DISABLED: Rgb = 0x66666D;
/// Selected rows and items.
pub const SELECTION: Rgb = 0x2D5AA0;
/// A selection in a window that is not in use.
pub const SELECTION_INACTIVE: Rgb = 0x3A3A41;
/// A highlighted menu item.
pub const MENU_HOT: Rgb = 0x3B3B42;
pub const CAPTION_ACTIVE: Rgb = 0x1B1B1F;
pub const CAPTION_INACTIVE: Rgb = 0x2B2B30;
pub const CAPTION_TEXT_ACTIVE: Rgb = 0xFFFFFF;
pub const CAPTION_TEXT_INACTIVE: Rgb = 0x8E8E96;
/// The outline of a window that is not in use.
pub const FRAME_INACTIVE: Rgb = 0x45454C;
/// A pressed caption button, and the close button pressed.
pub const BUTTON_PRESSED: Rgb = 0x45454C;
pub const CLOSE_PRESSED: Rgb = 0xD13438;
/// Scroll bars: track, thumb.
pub const SCROLL_TRACK: Rgb = 0x202024;
pub const SCROLL_THUMB: Rgb = 0x5C5C64;

/// The line along the top of the taskbar.
pub const TASKBAR_EDGE: Rgb = 0x26262B;
/// A selected desktop icon, with the desktop in use or not.
pub const DESKTOP_SELECTION: Rgb = 0x2B3E5E;
pub const DESKTOP_SELECTION_INACTIVE: Rgb = 0x262A33;

/// The desktop's colour at row `y` of a screen `height` rows tall: a quiet
/// gradient from a slate grey at the top to [`DESKTOP`] at the bottom.
pub fn desktop_shade(y: i32, height: i32) -> Rgb {
    const TOP: Rgb = 0x232733;
    let t = y.clamp(0, height.max(1)) as u32;
    let h = height.max(1) as u32;
    let channel = |shift: u32| {
        let a = (TOP >> shift) & 0xFF;
        let b = (DESKTOP >> shift) & 0xFF;
        (a * (h - t) + b * t) / h
    };
    channel(16) << 16 | channel(8) << 8 | channel(0)
}

/// Caption colour of an active or inactive window.
pub fn caption(active: bool) -> Rgb {
    if active {
        CAPTION_ACTIVE
    } else {
        CAPTION_INACTIVE
    }
}

pub fn caption_text(active: bool) -> Rgb {
    if active {
        CAPTION_TEXT_ACTIVE
    } else {
        CAPTION_TEXT_INACTIVE
    }
}

/// The one-pixel outline round a window: the accent while it is in use.
pub fn frame_outline(active: bool) -> Rgb {
    if active {
        ACCENT
    } else {
        FRAME_INACTIVE
    }
}

/// A one-pixel outline just inside `rect`.
pub fn outline(p: &mut Painter<'_>, rect: Rect, color: Rgb) {
    if rect.width() <= 0 || rect.height() <= 0 {
        return;
    }
    p.hline(rect.left, rect.right, rect.top, color);
    p.hline(rect.left, rect.right, rect.bottom - 1, color);
    p.vline(rect.left, rect.top, rect.bottom, color);
    p.vline(rect.right - 1, rect.top, rect.bottom, color);
}

/// Fill `rect` with its corners cut by `radius` pixels (a rounded box
/// without blending: the corners keep what was under them).
pub fn rounded(p: &mut Painter<'_>, rect: Rect, radius: i32, color: Rgb) {
    let radius = radius.min(rect.height() / 2).min(rect.width() / 2).max(0);
    for row in 0..rect.height() {
        let from_edge = row.min(rect.height() - 1 - row);
        let inset = corner_inset(radius, from_edge);
        p.hline(
            rect.left + inset,
            rect.right - inset,
            rect.top + row,
            color,
        );
    }
}

/// How far a row `from_edge` pixels from the top (or bottom) of a box with
/// rounded corners of `radius` starts inside its side.
pub fn corner_inset(radius: i32, from_edge: i32) -> i32 {
    if from_edge >= radius {
        return 0;
    }
    // Distance from the corner circle's centre to this row.
    let dy = radius - from_edge;
    let mut dx = 0;
    while dx * dx + dy * dy < radius * radius + radius / 2 {
        dx += 1;
    }
    (radius - dx + 1).clamp(0, radius)
}

/// A rounded outline: `rounded` in the border colour with the inside
/// filled again.
pub fn rounded_box(p: &mut Painter<'_>, rect: Rect, radius: i32, fill: Rgb, border: Rgb) {
    rounded(p, rect, radius, border);
    rounded(p, rect.inflate(-1, -1), (radius - 1).max(0), fill);
}

/// A small chevron centred on (cx, cy): 0 up, 1 down, 2 left, 3 right.
pub fn chevron(p: &mut Painter<'_>, cx: i32, cy: i32, direction: u8, color: Rgb) {
    for i in 0..4 {
        match direction {
            0 => {
                p.pixel(cx - i, cy - 1 + i, color);
                p.pixel(cx + i, cy - 1 + i, color);
            }
            1 => {
                p.pixel(cx - i, cy + 1 - i, color);
                p.pixel(cx + i, cy + 1 - i, color);
            }
            2 => {
                p.pixel(cx - 1 + i, cy - i, color);
                p.pixel(cx - 1 + i, cy + i, color);
            }
            _ => {
                p.pixel(cx + 1 - i, cy - i, color);
                p.pixel(cx + 1 - i, cy + i, color);
            }
        }
    }
}

/// Glyphs of caption buttons.
pub const GLYPH_CLOSE: u8 = 0;
pub const GLYPH_MAXIMIZE: u8 = 1;
pub const GLYPH_MINIMIZE: u8 = 2;
pub const GLYPH_RESTORE: u8 = 3;

/// A flat caption button: no face of its own, the glyph drawn in thin
/// lines; pressed, it lights up (red for close).
pub fn caption_button(p: &mut Painter<'_>, rect: Rect, glyph: u8, active: bool, pressed: bool) {
    let background = match (pressed, glyph) {
        (true, GLYPH_CLOSE) => CLOSE_PRESSED,
        (true, _) => BUTTON_PRESSED,
        _ => caption(active),
    };
    p.fill(rect, background);
    let color = if pressed {
        CAPTION_TEXT_ACTIVE
    } else {
        caption_text(active)
    };
    let size = (rect.height() - 8).clamp(5, 9) | 1;
    let half = size / 2;
    let cx = rect.left + rect.width() / 2;
    let cy = rect.top + rect.height() / 2;
    match glyph {
        GLYPH_CLOSE => {
            for i in 0..size {
                p.pixel(cx - half + i, cy - half + i, color);
                p.pixel(cx + half - i, cy - half + i, color);
            }
        }
        GLYPH_MAXIMIZE => outline(
            p,
            Rect::new(cx - half, cy - half, cx + half + 1, cy + half + 1),
            color,
        ),
        GLYPH_MINIMIZE => p.hline(cx - half, cx + half + 1, cy, color),
        _ => {
            // Restore: a square with another behind it.
            let front = Rect::new(cx - half, cy - half + 2, cx + half - 1, cy + half + 1);
            outline(p, front, color);
            p.hline(front.left + 2, front.right + 2, front.top - 2, color);
            p.vline(front.right + 1, front.top - 2, front.bottom - 2, color);
        }
    }
}

/// Scroll bar geometry along the axis is `user::scroll_thumb`'s: arrow
/// areas at both ends, the thumb between. Drawn flat: a dark track,
/// chevrons for the arrows and a slim rounded thumb.
pub fn scroll_bar(
    p: &mut Painter<'_>,
    rect: Rect,
    vertical: bool,
    arrow_size: i32,
    thumb: Option<(i32, i32)>,
    enabled: bool,
) {
    p.fill(rect, SCROLL_TRACK);
    let arrow_color = if enabled { TEXT_DIM } else { TEXT_DISABLED };
    let (cx, cy) = (rect.left + rect.width() / 2, rect.top + rect.height() / 2);
    if vertical {
        chevron(p, cx, rect.top + arrow_size / 2, 0, arrow_color);
        chevron(p, cx, rect.bottom - arrow_size / 2 - 1, 1, arrow_color);
    } else {
        chevron(p, rect.left + arrow_size / 2, cy, 2, arrow_color);
        chevron(p, rect.right - arrow_size / 2 - 1, cy, 3, arrow_color);
    }
    if let Some((start, size)) = thumb {
        let bar = if vertical {
            Rect::sized(rect.left + 4, rect.top + start + 1, rect.width() - 8, size - 2)
        } else {
            Rect::sized(rect.left + start + 1, rect.top + 4, size - 2, rect.height() - 8)
        };
        rounded(p, bar, 2, SCROLL_THUMB);
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;
    use crate::win32::gfx::{Region, Surface};

    fn painter(surface: &mut Surface) -> Painter<'_> {
        let clip = Region::from_rect(surface.bounds());
        Painter {
            surface,
            clip,
            ox: 0,
            oy: 0,
        }
    }

    #[test]
    fn rounded_boxes_cut_their_corners_only() {
        let mut surface = Surface::new(20, 20, 24);
        surface.pixels.fill(0);
        rounded(&mut painter(&mut surface), Rect::new(0, 0, 20, 10), 3, 0xFFFFFF);
        assert_eq!(surface.get(0, 0), Some(0), "corner stays");
        assert_eq!(surface.get(19, 9), Some(0), "corner stays");
        assert_eq!(surface.get(10, 0), Some(0xFFFFFF), "top edge");
        assert_eq!(surface.get(0, 5), Some(0xFFFFFF), "side");
        assert_eq!(surface.get(10, 10), Some(0), "below the box");
        assert_eq!(corner_inset(0, 0), 0);
        assert_eq!(corner_inset(3, 3), 0);
        assert!(corner_inset(3, 0) > corner_inset(3, 2));
    }

    #[test]
    fn caption_buttons_show_state() {
        let rect = Rect::new(0, 0, 26, 17);
        let mut surface = Surface::new(26, 17, 24);
        caption_button(&mut painter(&mut surface), rect, GLYPH_CLOSE, true, false);
        assert_eq!(surface.get(1, 1), Some(CAPTION_ACTIVE));
        assert_eq!(surface.get(13, 8), Some(CAPTION_TEXT_ACTIVE), "the cross");
        caption_button(&mut painter(&mut surface), rect, GLYPH_CLOSE, true, true);
        assert_eq!(surface.get(1, 1), Some(CLOSE_PRESSED));
        caption_button(&mut painter(&mut surface), rect, GLYPH_MINIMIZE, false, false);
        assert_eq!(surface.get(1, 1), Some(CAPTION_INACTIVE));
        assert_eq!(surface.get(13, 8), Some(CAPTION_TEXT_INACTIVE), "the bar");
        caption_button(&mut painter(&mut surface), rect, GLYPH_MAXIMIZE, true, true);
        assert_eq!(surface.get(1, 1), Some(BUTTON_PRESSED));
    }

    #[test]
    fn scroll_bars_are_a_dark_track_with_a_thumb() {
        let mut surface = Surface::new(16, 100, 24);
        scroll_bar(
            &mut painter(&mut surface),
            Rect::new(0, 0, 16, 100),
            true,
            16,
            Some((30, 20)),
            true,
        );
        assert_eq!(surface.get(1, 50), Some(SCROLL_TRACK));
        assert_eq!(surface.get(8, 40), Some(SCROLL_THUMB));
        assert_eq!(surface.get(8, 70), Some(SCROLL_TRACK));
        assert_eq!(surface.get(8, 7), Some(TEXT_DIM), "the up chevron");
    }

    /// WCAG contrast ratio of two colours.
    fn contrast(a: Rgb, b: Rgb) -> f64 {
        let luminance = |rgb: Rgb| {
            let channel = |shift: u32| {
                let c = ((rgb >> shift) & 0xFF) as f64 / 255.0;
                if c <= 0.03928 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
        };
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn system_colours_follow_the_theme_and_stay_readable() {
        use crate::win32::gdi::sys_color;
        assert_eq!(sys_color(2), CAPTION_ACTIVE);
        assert_eq!(sys_color(3), CAPTION_INACTIVE);
        assert_eq!(sys_color(4), SURFACE);
        assert_eq!(sys_color(0), SCROLL_TRACK);
        // Programs draw black on window and button faces: they stay light.
        assert_eq!(sys_color(5), 0xFFFFFF);
        assert!(contrast(sys_color(15), 0x000000) > 15.0);
        // Text and its background, as pairs.
        for (background, text) in [
            (5, 8),   // window
            (15, 18), // buttons
            (4, 7),   // menus
            (2, 9),   // active caption
            (3, 19),  // inactive caption
            (13, 14), // selection
            (24, 23), // tooltips
            (15, 17), // grey text on a button face
        ] {
            let ratio = contrast(sys_color(background), sys_color(text));
            assert!(ratio >= 4.0, "colours {background}/{text}: {ratio:.2}");
        }
        // Menus' disabled items and dim text on the dark panels.
        assert!(contrast(SURFACE, TEXT_DIM) >= 4.5);
        assert!(contrast(SURFACE, TEXT_DISABLED) >= 2.0);
        assert!(contrast(TASKBAR, TEXT) >= 15.0);
        assert!(contrast(SELECTION, TEXT) >= 4.5);
        assert!(contrast(ACCENT_STRONG, 0xFFFFFF) >= 4.5);
    }

    #[test]
    fn the_desktop_darkens_towards_the_taskbar() {
        assert_eq!(desktop_shade(700, 700), DESKTOP);
        let (top, bottom) = (desktop_shade(0, 700), desktop_shade(699, 700));
        assert!(top & 0xFF > bottom & 0xFF);
        assert_eq!(desktop_shade(-5, 700), desktop_shade(0, 700));
    }

    #[test]
    fn active_and_inactive_frames_differ() {
        assert_ne!(frame_outline(true), frame_outline(false));
        assert_ne!(caption(true), caption(false));
        assert_ne!(caption_text(true), caption_text(false));
    }
}
