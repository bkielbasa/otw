//! Software raster engine behind the GDI32 layer.
//!
//! Everything here is host-side and independent of the CPU backend: surfaces
//! of `0x00RRGGBB` pixels, rectangle-list clip regions, Win32 binary (ROP2)
//! and ternary (ROP3) raster operations, Bresenham lines, scanline polygon and
//! ellipse fills, flood fill, bit-block transfers and a proportional bitmap
//! font. The Win32 layer translates logical coordinates, `COLORREF`s and
//! object handles into calls on these primitives.

use std::sync::OnceLock;

/// A pixel in `0x00RRGGBB` form.
pub type Rgb = u32;

/// Convert a Win32 `COLORREF` (`0x00BBGGRR`) into an [`Rgb`] pixel.
pub fn colorref_to_rgb(color: u32) -> Rgb {
    let r = color & 0xFF;
    let g = (color >> 8) & 0xFF;
    let b = (color >> 16) & 0xFF;
    (r << 16) | (g << 8) | b
}

/// Convert an [`Rgb`] pixel into a Win32 `COLORREF`.
pub fn rgb_to_colorref(rgb: Rgb) -> u32 {
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    r | (g << 8) | (b << 16)
}

/// Half-open rectangle `[left, right) x [top, bottom)`, like a Win32 `RECT`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub const fn sized(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self::new(x, y, x + width, y + height)
    }

    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    /// Swap reversed edges so `left <= right` and `top <= bottom`.
    pub fn normalized(&self) -> Self {
        Self::new(
            self.left.min(self.right),
            self.top.min(self.bottom),
            self.left.max(self.right),
            self.top.max(self.bottom),
        )
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub fn intersect(&self, other: &Rect) -> Rect {
        let rect = Rect::new(
            self.left.max(other.left),
            self.top.max(other.top),
            self.right.min(other.right),
            self.bottom.min(other.bottom),
        );
        if rect.is_empty() {
            Rect::default()
        } else {
            rect
        }
    }

    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        Rect::new(
            self.left.min(other.left),
            self.top.min(other.top),
            self.right.max(other.right),
            self.bottom.max(other.bottom),
        )
    }

    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(
            self.left + dx,
            self.top + dy,
            self.right + dx,
            self.bottom + dy,
        )
    }

    pub fn inflate(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(
            self.left - dx,
            self.top - dy,
            self.right + dx,
            self.bottom + dy,
        )
    }
}

/// A region stored as a list of non-overlapping rectangles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Region {
    pub rects: Vec<Rect>,
}

impl Region {
    pub fn empty() -> Self {
        Self { rects: Vec::new() }
    }

    pub fn from_rect(rect: Rect) -> Self {
        if rect.is_empty() {
            Self::empty()
        } else {
            Self { rects: vec![rect] }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub fn bounds(&self) -> Rect {
        self.rects
            .iter()
            .fold(Rect::default(), |acc, rect| acc.union(rect))
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.rects.iter().any(|rect| rect.contains(x, y))
    }

    pub fn intersects_rect(&self, rect: &Rect) -> bool {
        self.rects.iter().any(|r| !r.intersect(rect).is_empty())
    }

    pub fn offset(&mut self, dx: i32, dy: i32) {
        for rect in &mut self.rects {
            *rect = rect.offset(dx, dy);
        }
    }

    pub fn intersect_rect(&self, clip: &Rect) -> Region {
        Region {
            rects: self
                .rects
                .iter()
                .map(|rect| rect.intersect(clip))
                .filter(|rect| !rect.is_empty())
                .collect(),
        }
    }

    pub fn intersect(&self, other: &Region) -> Region {
        let mut rects = Vec::new();
        for a in &self.rects {
            for b in &other.rects {
                let rect = a.intersect(b);
                if !rect.is_empty() {
                    rects.push(rect);
                }
            }
        }
        Region { rects }
    }

    /// Remove `hole` from every rectangle, splitting into up to four bands.
    pub fn subtract_rect(&self, hole: &Rect) -> Region {
        if hole.is_empty() {
            return self.clone();
        }
        let mut rects = Vec::with_capacity(self.rects.len() + 4);
        for rect in &self.rects {
            let overlap = rect.intersect(hole);
            if overlap.is_empty() {
                rects.push(*rect);
                continue;
            }
            if rect.top < overlap.top {
                rects.push(Rect::new(rect.left, rect.top, rect.right, overlap.top));
            }
            if overlap.bottom < rect.bottom {
                rects.push(Rect::new(
                    rect.left,
                    overlap.bottom,
                    rect.right,
                    rect.bottom,
                ));
            }
            if rect.left < overlap.left {
                rects.push(Rect::new(
                    rect.left,
                    overlap.top,
                    overlap.left,
                    overlap.bottom,
                ));
            }
            if overlap.right < rect.right {
                rects.push(Rect::new(
                    overlap.right,
                    overlap.top,
                    rect.right,
                    overlap.bottom,
                ));
            }
        }
        Region { rects }
    }

    pub fn subtract(&self, other: &Region) -> Region {
        let mut result = self.clone();
        for rect in &other.rects {
            result = result.subtract_rect(rect);
        }
        result
    }

    pub fn union(&self, other: &Region) -> Region {
        let mut result = self.subtract(other);
        result.rects.extend(other.rects.iter().copied());
        result
    }

    pub fn union_rect(&self, rect: &Rect) -> Region {
        self.union(&Region::from_rect(*rect))
    }

    pub fn xor(&self, other: &Region) -> Region {
        self.subtract(other).union(&other.subtract(self))
    }

    /// Build a region from horizontal spans `(y, x0, x1)`; adjacent rows with
    /// identical spans are merged into taller rectangles.
    pub fn from_spans(spans: &[(i32, i32, i32)]) -> Region {
        let mut rects: Vec<Rect> = Vec::new();
        for &(y, x0, x1) in spans {
            if x1 <= x0 {
                continue;
            }
            if let Some(last) = rects
                .iter_mut()
                .rev()
                .take(8)
                .find(|r| r.bottom == y && r.left == x0 && r.right == x1)
            {
                last.bottom = y + 1;
                continue;
            }
            rects.push(Rect::new(x0, y, x1, y + 1));
        }
        Region { rects }
    }
}

/// A raster surface. Monochrome bitmaps use the same storage with only black
/// and white pixels; `bits_per_pixel` records the format the guest asked for.
#[derive(Clone, Debug)]
pub struct Surface {
    pub width: i32,
    pub height: i32,
    pub bits_per_pixel: u16,
    pub pixels: Vec<Rgb>,
}

impl Surface {
    pub fn new(width: i32, height: i32, bits_per_pixel: u16) -> Self {
        let width = width.max(0);
        let height = height.max(0);
        let fill = if bits_per_pixel == 1 {
            0x000000
        } else {
            0x000000
        };
        Self {
            width,
            height,
            bits_per_pixel,
            pixels: vec![fill; (width as usize) * (height as usize)],
        }
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    pub fn is_mono(&self) -> bool {
        self.bits_per_pixel == 1
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32) -> Option<Rgb> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        Some(self.pixels[(y * self.width + x) as usize])
    }

    #[inline]
    pub fn put(&mut self, x: i32, y: i32, value: Rgb) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let value = if self.bits_per_pixel == 1 {
            mono_of(value)
        } else {
            value & 0xFF_FFFF
        };
        self.pixels[(y * self.width + x) as usize] = value;
    }

    /// Resize, keeping the overlapping top-left content.
    pub fn resize(&mut self, width: i32, height: i32, fill: Rgb) {
        let width = width.max(0);
        let height = height.max(0);
        if width == self.width && height == self.height {
            return;
        }
        let mut pixels = vec![fill; (width as usize) * (height as usize)];
        for y in 0..height.min(self.height) {
            for x in 0..width.min(self.width) {
                pixels[(y * width + x) as usize] = self.pixels[(y * self.width + x) as usize];
            }
        }
        self.width = width;
        self.height = height;
        self.pixels = pixels;
    }
}

/// Monochrome conversion used for colour-to-mono writes: white stays white,
/// everything else becomes black. GDI's colour-to-mono blits compare against
/// the background colour instead; see [`blit`].
fn mono_of(value: Rgb) -> Rgb {
    let r = (value >> 16) & 0xFF;
    let g = (value >> 8) & 0xFF;
    let b = value & 0xFF;
    if r * 30 + g * 59 + b * 11 >= 128 * 100 {
        0xFF_FFFF
    } else {
        0
    }
}

/// Evaluate a ternary raster operation on 24-bit pixels.
#[inline]
pub fn rop3(code: u8, p: u32, s: u32, d: u32) -> u32 {
    match code {
        0x00 => 0,
        0xFF => 0xFF_FFFF,
        0xCC => s,
        0xF0 => p,
        0xAA => d,
        0x55 => !d & 0xFF_FFFF,
        0x33 => !s & 0xFF_FFFF,
        0x66 => s ^ d,
        0x88 => s & d,
        0xEE => s | d,
        0x5A => p ^ d,
        0xC0 => p & s,
        0x44 => s & !d & 0xFF_FFFF,
        0xBB => (!s | d) & 0xFF_FFFF,
        0x11 => !(s | d) & 0xFF_FFFF,
        0xB8 => ((p ^ d) & s) ^ p,
        0xE2 => ((p ^ d) & s) ^ d,
        _ => {
            let mut result = 0u32;
            for index in 0..8 {
                if code >> index & 1 == 1 {
                    let pm = if index & 4 != 0 { p } else { !p };
                    let sm = if index & 2 != 0 { s } else { !s };
                    let dm = if index & 1 != 0 { d } else { !d };
                    result |= pm & sm & dm;
                }
            }
            result & 0xFF_FFFF
        }
    }
}

/// Evaluate a binary raster operation (`R2_BLACK` = 1 .. `R2_WHITE` = 16).
#[inline]
pub fn rop2(mode: u32, p: u32, d: u32) -> u32 {
    let code = (mode.clamp(1, 16) - 1) as u8;
    let mut result = 0u32;
    for index in 0..4 {
        if code >> index & 1 == 1 {
            let pm = if index & 2 != 0 { p } else { !p };
            let dm = if index & 1 != 0 { d } else { !d };
            result |= pm & dm;
        }
    }
    result & 0xFF_FFFF
}

pub const R2_COPYPEN: u32 = 13;

/// A GDI brush resolved to pixels.
#[derive(Clone, Debug)]
pub enum Fill {
    Null,
    Solid(Rgb),
    /// An 8x8 (or larger) pattern tiled from the brush origin. Monochrome
    /// patterns are stored with `None` for "background" pixels, which take
    /// the DC background colour, and `Some(rgb)` for foreground pixels.
    Pattern {
        width: i32,
        height: i32,
        pixels: Vec<PatternPixel>,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum PatternPixel {
    Color(Rgb),
    /// Monochrome pattern bit: `true` = 1 (background colour), `false` = 0
    /// (text colour), per Win32 mono-to-colour expansion.
    Mono(bool),
}

/// Colours a monochrome pattern or source expands to.
#[derive(Clone, Copy, Debug)]
pub struct MonoColors {
    /// Colour for 0 bits (the DC text colour).
    pub foreground: Rgb,
    /// Colour for 1 bits (the DC background colour).
    pub background: Rgb,
}

impl Fill {
    pub fn is_null(&self) -> bool {
        matches!(self, Fill::Null)
    }

    #[inline]
    pub fn sample(&self, x: i32, y: i32, origin: (i32, i32), mono: MonoColors) -> Option<Rgb> {
        match self {
            Fill::Null => None,
            Fill::Solid(rgb) => Some(*rgb),
            Fill::Pattern {
                width,
                height,
                pixels,
            } => {
                if *width <= 0 || *height <= 0 {
                    return None;
                }
                let px = (x - origin.0).rem_euclid(*width);
                let py = (y - origin.1).rem_euclid(*height);
                Some(match pixels[(py * width + px) as usize] {
                    PatternPixel::Color(rgb) => rgb,
                    PatternPixel::Mono(true) => mono.background,
                    PatternPixel::Mono(false) => mono.foreground,
                })
            }
        }
    }

    /// Standard hatch brush (`HS_*`) with transparent gaps filled by the
    /// background colour when the DC is opaque.
    pub fn hatch(style: u32, color: Rgb, background: Option<Rgb>) -> Fill {
        let mut pixels = Vec::with_capacity(64);
        for y in 0..8 {
            for x in 0..8 {
                let on = match style {
                    0 => y == 7,               // HS_HORIZONTAL
                    1 => x == 7,               // HS_VERTICAL
                    2 => x == y,               // HS_FDIAGONAL
                    3 => x == 7 - y,           // HS_BDIAGONAL
                    4 => x == 7 || y == 7,     // HS_CROSS
                    _ => x == y || x == 7 - y, // HS_DIAGCROSS
                };
                pixels.push(PatternPixel::Color(if on {
                    color
                } else {
                    background.unwrap_or(0xFF_FFFF)
                }));
            }
        }
        Fill::Pattern {
            width: 8,
            height: 8,
            pixels,
        }
    }
}

/// Raster target: a surface plus a device-space clip region and settings.
pub struct Target<'a> {
    pub surface: &'a mut Surface,
    pub clip: &'a Region,
}

impl Target<'_> {
    #[inline]
    fn visible(&self, x: i32, y: i32) -> bool {
        self.clip.contains(x, y)
    }

    #[inline]
    pub fn plot(&mut self, x: i32, y: i32, color: Rgb, mode: u32) {
        if !self.visible(x, y) {
            return;
        }
        let Some(dest) = self.surface.get(x, y) else {
            return;
        };
        let value = if mode == R2_COPYPEN {
            color
        } else {
            rop2(mode, color, dest)
        };
        self.surface.put(x, y, value);
    }

    /// Fill a device rectangle with a brush through a ROP2 mix mode.
    pub fn fill_rect(
        &mut self,
        rect: Rect,
        fill: &Fill,
        mode: u32,
        origin: (i32, i32),
        mono: MonoColors,
    ) {
        if fill.is_null() {
            return;
        }
        let rect = rect.normalized().intersect(&self.surface.bounds());
        let clip = self.clip.intersect_rect(&rect);
        for band in &clip.rects {
            for y in band.top..band.bottom {
                for x in band.left..band.right {
                    if let Some(color) = fill.sample(x, y, origin, mono) {
                        let index = (y * self.surface.width + x) as usize;
                        let dest = self.surface.pixels[index];
                        let value = if mode == R2_COPYPEN {
                            color
                        } else {
                            rop2(mode, color, dest)
                        };
                        self.surface.put(x, y, value);
                    }
                }
            }
        }
    }

    /// `PatBlt`: combine a brush and the destination through a ROP3.
    pub fn pat_blt(
        &mut self,
        rect: Rect,
        fill: &Fill,
        rop: u8,
        origin: (i32, i32),
        mono: MonoColors,
    ) {
        let rect = rect.normalized().intersect(&self.surface.bounds());
        let clip = self.clip.intersect_rect(&rect);
        for band in &clip.rects {
            for y in band.top..band.bottom {
                for x in band.left..band.right {
                    let p = fill.sample(x, y, origin, mono).unwrap_or(0);
                    let index = (y * self.surface.width + x) as usize;
                    let d = self.surface.pixels[index];
                    let value = rop3(rop, p, 0, d);
                    self.surface.put(x, y, value);
                }
            }
        }
    }

    /// Fill horizontal spans `(y, x0, x1)` (half-open) with a brush.
    pub fn fill_spans(
        &mut self,
        spans: &[(i32, i32, i32)],
        fill: &Fill,
        mode: u32,
        origin: (i32, i32),
        mono: MonoColors,
    ) {
        if fill.is_null() {
            return;
        }
        for &(y, x0, x1) in spans {
            for x in x0..x1 {
                if !self.visible(x, y) {
                    continue;
                }
                if let Some(color) = fill.sample(x, y, origin, mono) {
                    self.plot(x, y, color, mode);
                }
            }
        }
    }
}

/// Pen description resolved to device units.
#[derive(Clone, Copy, Debug)]
pub struct Stroke {
    pub color: Rgb,
    pub width: i32,
    /// `PS_SOLID` 0, `PS_DASH` 1, `PS_DOT` 2, `PS_DASHDOT` 3,
    /// `PS_DASHDOTDOT` 4, `PS_NULL` 5, `PS_INSIDEFRAME` 6.
    pub style: u32,
    pub mode: u32,
}

impl Stroke {
    pub fn is_null(&self) -> bool {
        self.style == 5
    }

    fn pattern_on(&self, step: u32) -> bool {
        match self.style {
            1 => step % 24 < 18,
            2 => step % 6 < 3,
            3 => matches!(step % 30, 0..=17 | 21..=26),
            4 => matches!(step % 36, 0..=17 | 21..=23 | 27..=29),
            _ => true,
        }
    }
}

/// Collect the pixels of a Bresenham line from `(x0, y0)` to `(x1, y1)`,
/// excluding the final point like Win32 `LineTo`.
pub fn line_points(x0: i32, y0: i32, x1: i32, y1: i32, include_last: bool) -> Vec<(i32, i32)> {
    let mut points = Vec::new();
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        if x == x1 && y == y1 {
            if include_last {
                points.push((x, y));
            }
            break;
        }
        points.push((x, y));
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
        if points.len() > 1 << 22 {
            break;
        }
    }
    points
}

/// Draw a pen stroke along a line. Wide pens stamp a round nib; pixels are
/// de-duplicated so XOR modes stay reversible.
pub fn stroke_line(
    target: &mut Target<'_>,
    stroke: &Stroke,
    from: (i32, i32),
    to: (i32, i32),
    include_last: bool,
) {
    if stroke.is_null() {
        return;
    }
    let points = line_points(from.0, from.1, to.0, to.1, include_last);
    if stroke.width <= 1 {
        for (step, (x, y)) in points.into_iter().enumerate() {
            if stroke.pattern_on(step as u32) {
                target.plot(x, y, stroke.color, stroke.mode);
            }
        }
        return;
    }
    let nib = nib_offsets(stroke.width);
    let mut seen = std::collections::HashSet::new();
    let mut ends = points;
    if !include_last {
        ends.push(to);
    }
    for (x, y) in ends {
        for (dx, dy) in &nib {
            if seen.insert((x + dx, y + dy)) {
                target.plot(x + dx, y + dy, stroke.color, stroke.mode);
            }
        }
    }
}

/// Offsets covered by a round pen nib of the given width.
pub fn nib_offsets(width: i32) -> Vec<(i32, i32)> {
    let width = width.max(1);
    let r = width as f32 / 2.0;
    let lo = -(width / 2);
    let hi = lo + width;
    let mut offsets = Vec::new();
    for dy in lo..hi {
        for dx in lo..hi {
            let fx = dx as f32 + 0.5 - (lo + hi) as f32 / 2.0;
            let fy = dy as f32 + 0.5 - (lo + hi) as f32 / 2.0;
            if width <= 3 || fx * fx + fy * fy <= r * r + 0.25 {
                offsets.push((dx, dy));
            }
        }
    }
    offsets
}

/// Scanline spans covering a polygon. `winding` selects `WINDING` instead of
/// `ALTERNATE` fill. Spans are half-open and follow GDI's convention of not
/// filling the right and bottom edges.
pub fn polygon_spans(points: &[(i32, i32)], winding: bool) -> Vec<(i32, i32, i32)> {
    let mut spans = Vec::new();
    if points.len() < 3 {
        return spans;
    }
    let min_y = points.iter().map(|p| p.1).min().unwrap_or(0);
    let max_y = points.iter().map(|p| p.1).max().unwrap_or(0);
    let n = points.len();
    for y in min_y..max_y {
        let sample = y as f64 + 0.5;
        let mut crossings: Vec<(f64, i32)> = Vec::new();
        for i in 0..n {
            let (x0, y0) = points[i];
            let (x1, y1) = points[(i + 1) % n];
            if y0 == y1 {
                continue;
            }
            let (fy0, fy1) = (y0 as f64, y1 as f64);
            if (sample >= fy0 && sample < fy1) || (sample >= fy1 && sample < fy0) {
                let t = (sample - fy0) / (fy1 - fy0);
                let x = x0 as f64 + t * (x1 - x0) as f64;
                crossings.push((x, if y1 > y0 { 1 } else { -1 }));
            }
        }
        crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        if winding {
            let mut count = 0;
            for window in 0..crossings.len() {
                let before = count;
                count += crossings[window].1;
                if before == 0 && count != 0 {
                    let start = crossings[window].0;
                    // find where winding returns to zero
                    let mut c = count;
                    let mut end = start;
                    for next in crossings.iter().skip(window + 1) {
                        c += next.1;
                        if c == 0 {
                            end = next.0;
                            break;
                        }
                    }
                    let x0 = (start + 0.5).floor() as i32;
                    let x1 = (end + 0.5).floor() as i32;
                    spans.push((y, x0, x1));
                }
            }
        } else {
            for pair in crossings.chunks(2) {
                if pair.len() == 2 {
                    let x0 = (pair[0].0 + 0.5).floor() as i32;
                    let x1 = (pair[1].0 + 0.5).floor() as i32;
                    spans.push((y, x0, x1));
                }
            }
        }
    }
    spans
}

/// Interior spans and outline pixels of an ellipse inscribed in `rect`
/// (exclusive of the right and bottom edges, like Win32 `Ellipse`).
pub fn ellipse_spans(rect: Rect) -> Vec<(i32, i32, i32)> {
    let rect = rect.normalized();
    let mut spans = Vec::new();
    let w = rect.width();
    let h = rect.height();
    if w <= 0 || h <= 0 {
        return spans;
    }
    let a = w as f64 / 2.0;
    let b = h as f64 / 2.0;
    let cx = rect.left as f64 + a;
    let cy = rect.top as f64 + b;
    for y in rect.top..rect.bottom {
        let dy = (y as f64 + 0.5 - cy) / b;
        let inside = 1.0 - dy * dy;
        if inside < 0.0 {
            continue;
        }
        let half = a * inside.sqrt();
        let x0 = (cx - half + 0.5).floor() as i32;
        let x1 = (cx + half + 0.5).floor() as i32;
        if x1 > x0 {
            spans.push((y, x0.max(rect.left), x1.min(rect.right)));
        }
    }
    spans
}

/// Outline points of an ellipse inscribed in `rect`, one pixel wide and
/// connected, without duplicates.
pub fn ellipse_outline(rect: Rect) -> Vec<(i32, i32)> {
    let spans = ellipse_spans(rect);
    let mut points = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |p: (i32, i32), points: &mut Vec<(i32, i32)>| {
        if seen.insert(p) {
            points.push(p);
        }
    };
    for (index, &(y, x0, x1)) in spans.iter().enumerate() {
        let prev = if index > 0 {
            Some(spans[index - 1])
        } else {
            None
        };
        let next = spans.get(index + 1).copied();
        // Left edge: cover from this row's start to the neighbour rows' start.
        let mut left_end = x0 + 1;
        let mut right_start = x1 - 1;
        for neighbour in [prev, next].into_iter().flatten() {
            if neighbour.0 != y {
                left_end = left_end.max(neighbour.1);
                right_start = right_start.min(neighbour.2);
            }
        }
        if prev.is_none() || next.is_none() {
            left_end = x1;
            right_start = x0;
        }
        for x in x0..left_end.min(x1) {
            push((x, y), &mut points);
        }
        for x in right_start.max(x0)..x1 {
            push((x, y), &mut points);
        }
    }
    points
}

/// Flatten a cubic Bezier into line points.
pub fn bezier_points(
    p0: (i32, i32),
    p1: (i32, i32),
    p2: (i32, i32),
    p3: (i32, i32),
) -> Vec<(i32, i32)> {
    let length = ((p1.0 - p0.0).abs()
        + (p1.1 - p0.1).abs()
        + (p2.0 - p1.0).abs()
        + (p2.1 - p1.1).abs()
        + (p3.0 - p2.0).abs()
        + (p3.1 - p2.1).abs()) as usize;
    let steps = (length / 2).clamp(4, 512);
    let mut points = Vec::with_capacity(steps + 1);
    for step in 0..=steps {
        let t = step as f64 / steps as f64;
        let u = 1.0 - t;
        let x = u * u * u * p0.0 as f64
            + 3.0 * u * u * t * p1.0 as f64
            + 3.0 * u * t * t * p2.0 as f64
            + t * t * t * p3.0 as f64;
        let y = u * u * u * p0.1 as f64
            + 3.0 * u * u * t * p1.1 as f64
            + 3.0 * u * t * t * p2.1 as f64
            + t * t * t * p3.1 as f64;
        let point = (x.round() as i32, y.round() as i32);
        if points.last() != Some(&point) {
            points.push(point);
        }
    }
    points
}

/// Flood fill from `(x, y)`. With `surface_mode` the fill replaces the
/// connected area whose colour equals `color` (`FLOODFILLSURFACE`); otherwise
/// it spreads until it reaches `color` (`FLOODFILLBORDER`). Returns the spans
/// to paint.
pub fn flood_spans(
    surface: &Surface,
    clip: &Region,
    x: i32,
    y: i32,
    color: Rgb,
    surface_mode: bool,
) -> Vec<(i32, i32, i32)> {
    let inside = |px: i32, py: i32| -> bool {
        if !clip.contains(px, py) {
            return false;
        }
        match surface.get(px, py) {
            Some(value) => {
                if surface_mode {
                    value == color
                } else {
                    value != color
                }
            }
            None => false,
        }
    };
    let mut spans = Vec::new();
    if !inside(x, y) {
        return spans;
    }
    let width = surface.width.max(0) as usize;
    let mut visited = vec![false; width * surface.height.max(0) as usize];
    let mut stack = vec![(x, y)];
    while let Some((sx, sy)) = stack.pop() {
        if visited[(sy as usize) * width + sx as usize] || !inside(sx, sy) {
            continue;
        }
        let mut x0 = sx;
        while x0 > 0 && inside(x0 - 1, sy) && !visited[(sy as usize) * width + (x0 - 1) as usize] {
            x0 -= 1;
        }
        let mut x1 = sx;
        while x1 + 1 < surface.width
            && inside(x1 + 1, sy)
            && !visited[(sy as usize) * width + (x1 + 1) as usize]
        {
            x1 += 1;
        }
        for px in x0..=x1 {
            visited[(sy as usize) * width + px as usize] = true;
        }
        spans.push((sy, x0, x1 + 1));
        for ny in [sy - 1, sy + 1] {
            if ny < 0 || ny >= surface.height {
                continue;
            }
            let mut px = x0;
            while px <= x1 {
                if inside(px, ny) && !visited[(ny as usize) * width + px as usize] {
                    stack.push((px, ny));
                    while px <= x1 && inside(px, ny) {
                        px += 1;
                    }
                }
                px += 1;
            }
        }
    }
    spans
}

/// Parameters of a bit-block transfer between two surfaces.
pub struct BlitParams<'a> {
    pub dest: Rect,
    pub src_x: i32,
    pub src_y: i32,
    pub src_width: i32,
    pub src_height: i32,
    pub rop: u8,
    pub fill: &'a Fill,
    pub origin: (i32, i32),
    /// Destination DC colours used to expand monochrome sources.
    pub dest_colors: MonoColors,
    /// Source DC background colour used for colour-to-mono conversion.
    pub src_background: Rgb,
}

/// `BitBlt`/`StretchBlt` with nearest-neighbour scaling. A `None` source is a
/// pattern-only operation. Negative destination extents mirror the image.
pub fn blit(
    dest_surface: &mut Surface,
    clip: &Region,
    source: Option<&Surface>,
    params: &BlitParams<'_>,
) {
    let BlitParams {
        dest,
        src_x,
        src_y,
        src_width,
        src_height,
        rop,
        fill,
        origin,
        dest_colors,
        src_background,
    } = *params;
    let mirror_x = (dest.width() < 0) != (src_width < 0);
    let mirror_y = (dest.height() < 0) != (src_height < 0);
    let dest_n = dest.normalized();
    let dw = dest_n.width();
    let dh = dest_n.height();
    if dw == 0 || dh == 0 {
        return;
    }
    let (sx0, sw) = if src_width < 0 {
        (src_x + src_width, -src_width)
    } else {
        (src_x, src_width)
    };
    let (sy0, sh) = if src_height < 0 {
        (src_y + src_height, -src_height)
    } else {
        (src_y, src_height)
    };
    let dest_mono = dest_surface.is_mono();
    let region = clip.intersect_rect(&dest_n.intersect(&dest_surface.bounds()));
    let uses_source = rop_uses_source(rop);
    let uses_pattern = rop_uses_pattern(rop);
    for band in &region.rects {
        for y in band.top..band.bottom {
            let mut fy = y - dest_n.top;
            if mirror_y {
                fy = dh - 1 - fy;
            }
            let sy = sy0
                + if sh == dh {
                    fy
                } else {
                    ((fy as i64 * sh as i64) / dh as i64) as i32
                };
            for x in band.left..band.right {
                let mut fx = x - dest_n.left;
                if mirror_x {
                    fx = dw - 1 - fx;
                }
                let s = if uses_source {
                    let sx = sx0
                        + if sw == dw {
                            fx
                        } else {
                            ((fx as i64 * sw as i64) / dw as i64) as i32
                        };
                    // GDI clips the transfer to the source bitmap: pixels
                    // with no source leave the destination unchanged.
                    let Some(raw) = source.and_then(|src| src.get(sx, sy)) else {
                        continue;
                    };
                    let src_mono = source.map(|src| src.is_mono()).unwrap_or(false);
                    if src_mono && !dest_mono {
                        // Mono to colour: 0 -> text colour, 1 -> background.
                        if raw & 0xFF_FFFF == 0xFF_FFFF {
                            dest_colors.background
                        } else {
                            dest_colors.foreground
                        }
                    } else if !src_mono && dest_mono {
                        // Colour to mono: background colour -> 1, else 0.
                        if raw & 0xFF_FFFF == src_background & 0xFF_FFFF {
                            0xFF_FFFF
                        } else {
                            0
                        }
                    } else {
                        raw
                    }
                } else {
                    0
                };
                let p = if uses_pattern {
                    fill.sample(x, y, origin, dest_colors).unwrap_or(0)
                } else {
                    0
                };
                let index = (y * dest_surface.width + x) as usize;
                let d = dest_surface.pixels[index];
                let value = rop3(rop, p, s, d);
                dest_surface.put(x, y, value);
            }
        }
    }
}

pub fn rop_uses_source(rop: u8) -> bool {
    ((rop >> 2) ^ rop) & 0x33 != 0
}

pub fn rop_uses_pattern(rop: u8) -> bool {
    ((rop >> 4) ^ rop) & 0x0F != 0
}

// ── Fonts ─────────────────────────────────────────────────────────────────

/// One rasterised face of the built-in proportional font.
pub struct FontFace {
    pub bold: bool,
    pub height: i32,
    pub ascent: i32,
    pub glyphs: Vec<Glyph>,
}

pub struct Glyph {
    pub advance: i32,
    pub stride: usize,
    pub bits: Vec<u8>,
}

impl FontFace {
    pub fn glyph(&self, byte: u8) -> &Glyph {
        let index = if byte < 32 { 0 } else { (byte - 32) as usize };
        &self.glyphs[index.min(self.glyphs.len() - 1)]
    }

    pub fn advance(&self, byte: u8) -> i32 {
        self.glyph(byte).advance
    }

    pub fn text_width(&self, text: &[u8]) -> i32 {
        text.iter().map(|byte| self.advance(*byte)).sum()
    }

    pub fn average_width(&self) -> i32 {
        // Win32 tmAveCharWidth is the weighted average of lowercase letters;
        // the width of 'x' is the conventional approximation.
        self.advance(b'x').max(1)
    }

    pub fn max_width(&self) -> i32 {
        self.glyphs
            .iter()
            .map(|glyph| glyph.advance)
            .max()
            .unwrap_or(1)
    }

    #[inline]
    pub fn pixel(&self, byte: u8, x: i32, y: i32) -> bool {
        let glyph = self.glyph(byte);
        if x < 0 || y < 0 || x >= glyph.advance || y >= self.height {
            return false;
        }
        let row = y as usize * glyph.stride;
        glyph.bits[row + x as usize / 8] & (0x80 >> (x % 8)) != 0
    }
}

static FONT_BLOB: &[u8] = include_bytes!("../../binaries/fonts/OTWSANS.FNT");

/// All faces of the built-in font, parsed once.
pub fn font_faces() -> &'static [FontFace] {
    static FACES: OnceLock<Vec<FontFace>> = OnceLock::new();
    FACES.get_or_init(|| parse_font(FONT_BLOB))
}

fn parse_font(blob: &[u8]) -> Vec<FontFace> {
    let mut faces = Vec::new();
    if blob.len() < 5 || &blob[..4] != b"OTWF" {
        return faces;
    }
    let count = blob[4] as usize;
    let mut offset = 5;
    for _ in 0..count {
        if offset + 4 > blob.len() {
            break;
        }
        let bold = blob[offset] != 0;
        let height = blob[offset + 1] as i32;
        let ascent = blob[offset + 2] as i32;
        offset += 4;
        let mut glyphs = Vec::with_capacity(224);
        for _ in 32..=255 {
            let advance = blob[offset] as i32;
            offset += 1;
            let stride = (advance as usize).div_ceil(8);
            let size = stride * height as usize;
            let bits = blob[offset..offset + size].to_vec();
            offset += size;
            glyphs.push(Glyph {
                advance,
                stride,
                bits,
            });
        }
        faces.push(FontFace {
            bold,
            height,
            ascent,
            glyphs,
        });
    }
    faces
}

/// Choose the face closest to a requested cell height and weight.
pub fn pick_face(height: i32, bold: bool) -> &'static FontFace {
    let faces = font_faces();
    let height = height.max(1);
    faces
        .iter()
        .filter(|face| face.bold == bold)
        .min_by_key(|face| (face.height - height).abs() * 4 + i32::from(face.height > height))
        .unwrap_or(&faces[0])
}

/// Draw text with its top-left cell corner at `(x, y)`. Returns the advance.
pub fn draw_text(
    target: &mut Target<'_>,
    face: &FontFace,
    x: i32,
    y: i32,
    text: &[u8],
    color: Rgb,
    underline: bool,
) -> i32 {
    let mut pen_x = x;
    for &byte in text {
        let glyph = face.glyph(byte);
        for gy in 0..face.height {
            for gx in 0..glyph.advance {
                if face.pixel(byte, gx, gy) {
                    target.plot(pen_x + gx, y + gy, color, R2_COPYPEN);
                }
            }
        }
        if face.bold && byte != b' ' {
            // Built-in bold faces are already emboldened.
        }
        pen_x += glyph.advance;
    }
    if underline {
        let uy = y + face.ascent + 1;
        for px in x..pen_x {
            target.plot(px, uy, color, R2_COPYPEN);
        }
    }
    pen_x - x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rop3_matches_named_codes() {
        assert_eq!(rop3(0xCC, 1, 0x123456, 9), 0x123456);
        assert_eq!(rop3(0x66, 0, 0x0F0F0F, 0xFFFFFF), 0xF0F0F0);
        // Generic path agrees with a fast path.
        let generic = {
            let code = 0x88u8;
            let mut result = 0u32;
            for index in 0..8 {
                if code >> index & 1 == 1 {
                    let pm = if index & 4 != 0 { 7 } else { !7u32 };
                    let sm = if index & 2 != 0 { 0xAB } else { !0xABu32 };
                    let dm = if index & 1 != 0 { 0x0F } else { !0x0Fu32 };
                    result |= pm & sm & dm;
                }
            }
            result & 0xFFFFFF
        };
        assert_eq!(generic, rop3(0x88, 7, 0xAB, 0x0F));
    }

    #[test]
    fn rop2_copy_and_xor() {
        assert_eq!(rop2(R2_COPYPEN, 0x123456, 0), 0x123456);
        assert_eq!(rop2(7, 0xFFFFFF, 0x00FF00), 0xFF00FF);
        assert_eq!(rop2(6, 0, 0x00FF00), 0xFF00FF);
    }

    #[test]
    fn region_subtraction_splits_bands() {
        let region =
            Region::from_rect(Rect::new(0, 0, 10, 10)).subtract_rect(&Rect::new(2, 2, 8, 8));
        let area: i32 = region.rects.iter().map(|r| r.width() * r.height()).sum();
        assert_eq!(area, 100 - 36);
        assert!(!region.contains(5, 5));
        assert!(region.contains(1, 5));
    }

    #[test]
    fn line_excludes_last_point() {
        let points = line_points(0, 0, 3, 0, false);
        assert_eq!(points, vec![(0, 0), (1, 0), (2, 0)]);
    }

    #[test]
    fn rectangle_polygon_fill_excludes_right_bottom() {
        let spans = polygon_spans(&[(0, 0), (4, 0), (4, 3), (0, 3)], false);
        assert_eq!(spans, vec![(0, 0, 4), (1, 0, 4), (2, 0, 4)]);
    }

    #[test]
    fn flood_fill_stays_inside_border() {
        let mut surface = Surface::new(5, 5, 24);
        for y in 0..5 {
            surface.put(2, y, 0xFF0000);
        }
        let clip = Region::from_rect(surface.bounds());
        let spans = flood_spans(&surface, &clip, 0, 0, 0, true);
        let covered: i32 = spans.iter().map(|s| s.2 - s.1).sum();
        assert_eq!(covered, 10);
    }

    #[test]
    fn built_in_font_has_both_weights() {
        let regular = pick_face(13, false);
        let bold = pick_face(13, true);
        assert_eq!(regular.height, 13);
        assert!(bold.bold);
        assert!(regular.text_width(b"Paint") > 10);
    }
}
