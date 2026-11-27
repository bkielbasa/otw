//! Small wrappers over classic GDI: solid fills, rounded rectangles,
//! ellipses and TextOutW in a handful of fonts. Text is measured with
//! GetTextExtentPoint32W; wrapping and ellipsis are done by
//! `app_center::ui::format`, so DrawText's fancier flags are not needed.

use app_center::ui::geom::Rect;
use app_center::ui::theme::{self, Font};
use std::cell::RefCell;
use std::collections::HashMap;
use windows_sys::Win32::Foundation::{RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, Ellipse, FillRect, GetStockObject,
    GetTextExtentPoint32W, GetTextMetricsW, RoundRect, SelectObject, SetBkMode, SetTextColor,
    TextOutW, CLEARTYPE_QUALITY, DEFAULT_CHARSET, HDC, HFONT, HGDIOBJ, NULL_BRUSH, NULL_PEN,
    PS_SOLID, TEXTMETRICW, TRANSPARENT,
};

/// The fonts of [`Font`], created once.
pub struct Fonts {
    handles: HashMap<Font, HFONT>,
}

impl Fonts {
    pub fn new() -> Fonts {
        let face: Vec<u16> = theme::FONT_FACE
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let handles = Font::ALL
            .iter()
            .map(|&f| {
                let (height, weight) = f.spec();
                // SAFETY: plain GDI object creation; `face` is NUL-terminated.
                let h = unsafe {
                    CreateFontW(
                        -height,
                        0,
                        0,
                        0,
                        weight,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET as u32,
                        0,
                        0,
                        CLEARTYPE_QUALITY as u32,
                        0x20, // DEFAULT_PITCH | FF_SWISS
                        face.as_ptr(),
                    )
                };
                (f, h)
            })
            .collect();
        Fonts { handles }
    }

    pub fn get(&self, f: Font) -> HFONT {
        self.handles.get(&f).copied().unwrap_or(0)
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for h in self.handles.values() {
            // SAFETY: the fonts were created by us and are not selected
            // into any DC any more.
            unsafe {
                DeleteObject(*h);
            }
        }
    }
}

fn rect(r: Rect) -> RECT {
    RECT {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Draws on one DC. Fonts are selected as needed and the DC's original
/// font is put back when the painter is dropped.
pub struct Painter<'a> {
    pub dc: HDC,
    fonts: &'a Fonts,
    original_font: HGDIOBJ,
    heights: RefCell<HashMap<Font, i32>>,
}

impl<'a> Painter<'a> {
    pub fn new(dc: HDC, fonts: &'a Fonts) -> Painter<'a> {
        // SAFETY: dc is a valid DC for the painter's lifetime.
        let original_font = unsafe {
            SetBkMode(dc, TRANSPARENT as i32);
            SelectObject(dc, fonts.get(Font::Body))
        };
        Painter {
            dc,
            fonts,
            original_font,
            heights: RefCell::new(HashMap::new()),
        }
    }

    fn select(&self, f: Font) {
        // SAFETY: selecting our own font into our DC.
        unsafe {
            SelectObject(self.dc, self.fonts.get(f));
        }
    }

    pub fn fill(&self, r: Rect, color: u32) {
        if r.is_empty() {
            return;
        }
        // SAFETY: the brush is created, used and deleted here.
        unsafe {
            let brush = CreateSolidBrush(color);
            FillRect(self.dc, &rect(r), brush);
            DeleteObject(brush);
        }
    }

    /// A filled rounded rectangle, optionally with a 1-pixel border.
    pub fn round_rect(&self, r: Rect, radius: i32, fill: u32, border: Option<u32>) {
        if r.is_empty() {
            return;
        }
        // SAFETY: pen and brush are created, selected, restored and deleted
        // here.
        unsafe {
            let brush = CreateSolidBrush(fill);
            let pen = match border {
                Some(c) => CreatePen(PS_SOLID, 1, c),
                None => 0,
            };
            let old_brush = SelectObject(self.dc, brush);
            let old_pen = SelectObject(
                self.dc,
                if pen != 0 {
                    pen
                } else {
                    GetStockObject(NULL_PEN)
                },
            );
            // With NULL_PEN, GDI leaves the right/bottom edge unpainted.
            let extra = if pen == 0 { 1 } else { 0 };
            RoundRect(
                self.dc,
                r.left,
                r.top,
                r.right + extra,
                r.bottom + extra,
                radius * 2,
                radius * 2,
            );
            SelectObject(self.dc, old_brush);
            SelectObject(self.dc, old_pen);
            DeleteObject(brush);
            if pen != 0 {
                DeleteObject(pen);
            }
        }
    }

    pub fn ellipse(&self, r: Rect, fill: u32) {
        // SAFETY: as in round_rect.
        unsafe {
            let brush = CreateSolidBrush(fill);
            let old_brush = SelectObject(self.dc, brush);
            let old_pen = SelectObject(self.dc, GetStockObject(NULL_PEN));
            Ellipse(self.dc, r.left, r.top, r.right + 1, r.bottom + 1);
            SelectObject(self.dc, old_brush);
            SelectObject(self.dc, old_pen);
            DeleteObject(brush);
        }
    }

    /// An unfilled ellipse outline `width` pixels thick.
    pub fn ring(&self, r: Rect, width: i32, color: u32) {
        // SAFETY: as in round_rect.
        unsafe {
            let pen = CreatePen(PS_SOLID, width, color);
            let old_pen = SelectObject(self.dc, pen);
            let old_brush = SelectObject(self.dc, GetStockObject(NULL_BRUSH));
            Ellipse(self.dc, r.left, r.top, r.right, r.bottom);
            SelectObject(self.dc, old_brush);
            SelectObject(self.dc, old_pen);
            DeleteObject(pen);
        }
    }

    pub fn text_width(&self, text: &str, f: Font) -> i32 {
        if text.is_empty() {
            return 0;
        }
        self.select(f);
        let w = utf16(text);
        let mut size = SIZE { cx: 0, cy: 0 };
        // SAFETY: `w` and `size` are valid for the call.
        unsafe {
            GetTextExtentPoint32W(self.dc, w.as_ptr(), w.len() as i32, &mut size);
        }
        size.cx
    }

    /// The height of a line of text in `f`.
    pub fn line_height(&self, f: Font) -> i32 {
        if let Some(h) = self.heights.borrow().get(&f) {
            return *h;
        }
        self.select(f);
        // SAFETY: TEXTMETRICW is plain data, filled in by GDI.
        let h = unsafe {
            let mut tm: TEXTMETRICW = std::mem::zeroed();
            if GetTextMetricsW(self.dc, &mut tm) != 0 && tm.tmHeight > 0 {
                tm.tmHeight
            } else {
                f.spec().0 + f.spec().0 / 3
            }
        };
        self.heights.borrow_mut().insert(f, h);
        h
    }

    pub fn text(&self, x: i32, y: i32, text: &str, f: Font, color: u32) {
        if text.is_empty() {
            return;
        }
        self.select(f);
        let w = utf16(text);
        // SAFETY: `w` is valid for the call.
        unsafe {
            SetTextColor(self.dc, color);
            TextOutW(self.dc, x, y, w.as_ptr(), w.len() as i32);
        }
    }

    /// One line in `r`, vertically centred, cut with "…" if too wide.
    pub fn text_in(&self, r: Rect, text: &str, f: Font, color: u32, align: Align) {
        let fitted = app_center::ui::format::ellipsize(text, r.width(), |s| self.text_width(s, f));
        let w = self.text_width(&fitted, f);
        let x = match align {
            Align::Left => r.left,
            Align::Center => r.left + (r.width() - w) / 2,
            Align::Right => r.right - w,
        };
        let y = r.top + (r.height() - self.line_height(f)) / 2;
        self.text(x, y, &fitted, f, color);
    }

    pub fn lines(&self, x: i32, y: i32, lines: &[String], f: Font, color: u32) -> i32 {
        let lh = self.line_height(f);
        for (i, line) in lines.iter().enumerate() {
            self.text(x, y + i as i32 * lh, line, f, color);
        }
        lh * lines.len() as i32
    }
}

impl Drop for Painter<'_> {
    fn drop(&mut self) {
        // SAFETY: restores the DC's original font.
        unsafe {
            SelectObject(self.dc, self.original_font);
        }
    }
}
