//! GDI32: device contexts, drawing objects and bitmaps.
//!
//! Every handle the guest sees is backed by host state here. Window DCs draw
//! into the backing surface of their top-level window (see `user.rs` for the
//! window geometry and visible regions); memory DCs draw into the bitmap they
//! have selected. DIB sections keep their pixels in guest memory and are
//! synchronised with the host surface around every operation that touches
//! them, so the guest can read and write the bits directly.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::collections::HashMap;

use crate::{
    kernel::pe::ImportName,
    win32::gfx::{
        self, colorref_to_rgb, rgb_to_colorref, BlitParams, Fill, FontFace, MonoColors,
        PatternPixel, Rect, Region, Rgb, Stroke, Surface, Target,
    },
    win32::{name_of, read_cstr, write_cstr, ApiContext, ApiOutcome, GuestMemory, SharedState},
};

/// Handles of the stock objects returned by `GetStockObject(index)`.
pub const STOCK_BASE: u32 = 0x0001_0000;
/// Brushes for system colours (`COLOR_* + 1` class backgrounds).
pub const SYSBRUSH_BASE: u32 = 0x0001_0100;
/// First dynamically allocated GDI handle.
const DYNAMIC_BASE: u32 = 0x0002_0000;
/// The 1x1 monochrome bitmap selected into new memory DCs.
pub const DEFAULT_BITMAP: u32 = STOCK_BASE + 0x80;

const WHITE_BRUSH: u32 = 0;
const BLACK_PEN: u32 = 7;
const SYSTEM_FONT: u32 = 13;
const DEFAULT_PALETTE: u32 = 15;

/// Windows 95 default system colours, as `0x00RRGGBB`.
pub fn sys_color(index: u32) -> Rgb {
    use crate::win32::theme;
    // OTW's dark theme for what the system draws (captions, frames, menus,
    // scroll bars, tooltips); window and button faces stay light, as
    // programs draw on them in black by default.
    match index {
        0 => theme::SCROLL_TRACK,           // COLOR_SCROLLBAR
        1 => theme::DESKTOP,                // COLOR_BACKGROUND
        2 => theme::CAPTION_ACTIVE,         // COLOR_ACTIVECAPTION
        3 => theme::CAPTION_INACTIVE,       // COLOR_INACTIVECAPTION
        4 => theme::SURFACE,                // COLOR_MENU
        5 => 0xFFFFFF,                      // COLOR_WINDOW
        6 => 0x646464,                      // COLOR_WINDOWFRAME
        7 => theme::TEXT,                   // COLOR_MENUTEXT
        8 => 0x000000,                      // COLOR_WINDOWTEXT
        9 => theme::CAPTION_TEXT_ACTIVE,    // COLOR_CAPTIONTEXT
        10 => theme::CAPTION_ACTIVE,        // COLOR_ACTIVEBORDER
        11 => theme::CAPTION_INACTIVE,      // COLOR_INACTIVEBORDER
        12 => theme::RAISED,                // COLOR_APPWORKSPACE
        13 => 0x0078D7,                     // COLOR_HIGHLIGHT
        14 => 0xFFFFFF,                     // COLOR_HIGHLIGHTTEXT
        15 => 0xF0F0F0,                     // COLOR_BTNFACE
        16 => 0xA0A0A0,                     // COLOR_BTNSHADOW
        17 => 0x6D6D6D,                     // COLOR_GRAYTEXT
        18 => 0x000000,                     // COLOR_BTNTEXT
        19 => theme::CAPTION_TEXT_INACTIVE, // COLOR_INACTIVECAPTIONTEXT
        20 => 0xFFFFFF,                     // COLOR_BTNHIGHLIGHT
        21 => 0x696969,                     // COLOR_3DDKSHADOW
        22 => 0xE3E3E3,                     // COLOR_3DLIGHT
        23 => theme::TEXT,                  // COLOR_INFOTEXT
        24 => theme::RAISED,                // COLOR_INFOBK
        26 => 0x0066CC,                     // COLOR_HOTLIGHT
        27 => theme::CAPTION_ACTIVE,        // COLOR_GRADIENTACTIVECAPTION
        28 => theme::CAPTION_INACTIVE,      // COLOR_GRADIENTINACTIVECAPTION
        29 => theme::MENU_HOT,              // COLOR_MENUHILIGHT
        30 => theme::SURFACE,               // COLOR_MENUBAR
        _ => 0xF0F0F0,
    }
}

/// The twenty static colours of the Windows default palette.
pub fn default_palette() -> Vec<Rgb> {
    vec![
        0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xC0C0C0, 0xC0DCC0,
        0xA6CAF0, 0xFFFBF0, 0xA0A0A4, 0x808080, 0xFF0000, 0x00FF00, 0xFFFF00, 0x0000FF, 0xFF00FF,
        0x00FFFF, 0xFFFFFF,
    ]
}

/// 256-entry palette used when the guest asks for 8-bit DIBs of a true-colour
/// bitmap: the static colours, a 6x6x6 cube and a grey ramp.
fn halftone_palette() -> Vec<Rgb> {
    let statics = default_palette();
    let mut palette = Vec::with_capacity(256);
    palette.extend_from_slice(&statics[..10]);
    for r in 0..6u32 {
        for g in 0..6u32 {
            for b in 0..6u32 {
                palette.push((r * 51) << 16 | (g * 51) << 8 | b * 51);
            }
        }
    }
    while palette.len() < 246 {
        let level = ((palette.len() - 226) as u32 * 255 / 20) & 0xFF;
        palette.push(level << 16 | level << 8 | level);
    }
    palette.extend_from_slice(&statics[10..]);
    palette
}

/// The sixteen VGA colours used for 4-bit DIBs.
fn vga_palette() -> Vec<Rgb> {
    vec![
        0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0x808080, 0xC0C0C0,
        0xFF0000, 0x00FF00, 0xFFFF00, 0x0000FF, 0xFF00FF, 0x00FFFF, 0xFFFFFF,
    ]
}

fn nearest_index(palette: &[Rgb], color: Rgb) -> usize {
    let (r, g, b) = (
        (color >> 16) as i32 & 0xFF,
        (color >> 8) as i32 & 0xFF,
        color as i32 & 0xFF,
    );
    let mut best = 0;
    let mut best_distance = i32::MAX;
    for (index, entry) in palette.iter().enumerate() {
        let (er, eg, eb) = (
            (*entry >> 16) as i32 & 0xFF,
            (*entry >> 8) as i32 & 0xFF,
            *entry as i32 & 0xFF,
        );
        let distance = (r - er) * (r - er) + (g - eg) * (g - eg) + (b - eb) * (b - eb);
        if distance < best_distance {
            best_distance = distance;
            best = index;
            if distance == 0 {
                break;
            }
        }
    }
    best
}

#[derive(Clone, Debug)]
pub struct Pen {
    pub style: u32,
    pub width: i32,
    pub color: u32,
}

#[derive(Clone, Debug)]
pub enum Brush {
    Null,
    Solid(u32),
    Hatch(u32, u32),
    Pattern(Surface),
}

#[derive(Clone, Debug)]
pub struct LogFont {
    pub height: i32,
    pub width: i32,
    pub escapement: i32,
    pub weight: i32,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub charset: u8,
    pub pitch: u8,
    pub face: Vec<u8>,
}

impl LogFont {
    fn named(face: &[u8], height: i32, weight: i32) -> Self {
        Self {
            height,
            width: 0,
            escapement: 0,
            weight,
            italic: false,
            underline: false,
            strike: false,
            charset: 0,
            pitch: 0x22,
            face: face.to_vec(),
        }
    }

    /// Cell height in pixels, following the Win32 sign convention: negative
    /// heights are character heights (excluding internal leading).
    pub fn cell_height(&self) -> i32 {
        match self.height {
            0 => 13,
            h if h < 0 => ((-h) as f32 * 1.2).round() as i32,
            h => h,
        }
    }

    pub fn face(&self) -> &'static FontFace {
        gfx::pick_face(self.cell_height(), self.weight >= 600)
    }
}

#[derive(Clone, Debug)]
pub struct Bitmap {
    pub surface: Surface,
    /// Guest address of DIB section bits, with the DIB layout.
    pub dib: Option<DibSection>,
}

#[derive(Clone, Debug)]
pub struct DibSection {
    pub bits: u32,
    pub bpp: u16,
    pub width: i32,
    /// Signed DIB height: positive is bottom-up.
    pub height: i32,
    pub colors: Vec<Rgb>,
    pub masks: Option<(u32, u32, u32)>,
}

#[derive(Clone, Debug)]
pub enum GdiObject {
    Pen(Pen),
    Brush(Brush),
    Font(LogFont),
    Bitmap(Bitmap),
    Region(Region),
    Palette(Vec<Rgb>),
    MetaFile,
}

/// Where a DC's drawing lands.
#[derive(Clone, Debug)]
pub enum DcTarget {
    /// A window DC. `client` DCs start at the client origin; `paint` is the
    /// BeginPaint clip in top-level surface coordinates.
    Window {
        hwnd: u32,
        client: bool,
        paint: Option<Region>,
    },
    /// A memory DC drawing into its selected bitmap.
    Memory,
    /// The whole screen (GetDC(NULL)). Drawing is discarded.
    Screen,
    /// An information context or printer DC with no surface.
    Info,
}

#[derive(Clone, Debug)]
pub struct DcAttrs {
    pub pen: u32,
    pub brush: u32,
    pub font: u32,
    pub bitmap: u32,
    pub palette: u32,
    pub region: u32,
    pub text_color: u32,
    pub bk_color: u32,
    pub bk_mode: u32,
    pub rop2: u32,
    pub poly_fill: u32,
    pub stretch_mode: u32,
    pub text_align: u32,
    pub map_mode: u32,
    pub window_org: (i32, i32),
    pub window_ext: (i32, i32),
    pub viewport_org: (i32, i32),
    pub viewport_ext: (i32, i32),
    pub pos: (i32, i32),
    pub brush_org: (i32, i32),
    /// Clip region in device coordinates relative to the DC origin.
    pub clip: Option<Region>,
    pub char_extra: i32,
    pub arc_direction: u32,
}

impl Default for DcAttrs {
    fn default() -> Self {
        Self {
            pen: STOCK_BASE + BLACK_PEN * 4,
            brush: STOCK_BASE + WHITE_BRUSH * 4,
            font: STOCK_BASE + SYSTEM_FONT * 4,
            bitmap: DEFAULT_BITMAP,
            palette: STOCK_BASE + DEFAULT_PALETTE * 4,
            region: 0,
            text_color: 0x000000,
            bk_color: 0xFFFFFF,
            bk_mode: 2,
            rop2: gfx::R2_COPYPEN,
            poly_fill: 1,
            stretch_mode: 1,
            text_align: 0,
            map_mode: 1,
            window_org: (0, 0),
            window_ext: (1, 1),
            viewport_org: (0, 0),
            viewport_ext: (1, 1),
            pos: (0, 0),
            brush_org: (0, 0),
            clip: None,
            char_extra: 0,
            arc_direction: 1,
        }
    }
}

impl DcAttrs {
    /// Logical to device-relative coordinates.
    pub fn lp_to_dp(&self, x: i32, y: i32) -> (i32, i32) {
        if self.map_mode == 1 {
            return (
                x - self.window_org.0 + self.viewport_org.0,
                y - self.window_org.1 + self.viewport_org.1,
            );
        }
        let wx = if self.window_ext.0 == 0 {
            1
        } else {
            self.window_ext.0
        };
        let wy = if self.window_ext.1 == 0 {
            1
        } else {
            self.window_ext.1
        };
        let dx = ((x - self.window_org.0) as i64 * self.viewport_ext.0 as i64 / wx as i64) as i32;
        let dy = ((y - self.window_org.1) as i64 * self.viewport_ext.1 as i64 / wy as i64) as i32;
        (dx + self.viewport_org.0, dy + self.viewport_org.1)
    }

    pub fn dp_to_lp(&self, x: i32, y: i32) -> (i32, i32) {
        if self.map_mode == 1 {
            return (
                x - self.viewport_org.0 + self.window_org.0,
                y - self.viewport_org.1 + self.window_org.1,
            );
        }
        let vx = if self.viewport_ext.0 == 0 {
            1
        } else {
            self.viewport_ext.0
        };
        let vy = if self.viewport_ext.1 == 0 {
            1
        } else {
            self.viewport_ext.1
        };
        let lx = ((x - self.viewport_org.0) as i64 * self.window_ext.0 as i64 / vx as i64) as i32;
        let ly = ((y - self.viewport_org.1) as i64 * self.window_ext.1 as i64 / vy as i64) as i32;
        (lx + self.window_org.0, ly + self.window_org.1)
    }

    /// Scale a logical width to device units (absolute).
    pub fn scale_x(&self, length: i32) -> i32 {
        if self.map_mode == 1 || self.window_ext.0 == 0 {
            return length;
        }
        ((length as i64 * self.viewport_ext.0.abs() as i64) / self.window_ext.0.abs().max(1) as i64)
            as i32
    }

    /// Fix up extents for MM_ISOTROPIC so both axes use the same scale.
    fn fix_isotropic(&mut self) {
        if self.map_mode != 7 {
            return;
        }
        let (wx, wy) = (
            self.window_ext.0.abs().max(1) as i64,
            self.window_ext.1.abs().max(1) as i64,
        );
        let (vx, vy) = (
            self.viewport_ext.0.abs() as i64,
            self.viewport_ext.1.abs() as i64,
        );
        let sx = vx * 1000 / wx;
        let sy = vy * 1000 / wy;
        if sx < sy {
            let value = (wy * sx / 1000) as i32;
            self.viewport_ext.1 = if self.viewport_ext.1 < 0 {
                -value
            } else {
                value
            };
        } else if sy < sx {
            let value = (wx * sy / 1000) as i32;
            self.viewport_ext.0 = if self.viewport_ext.0 < 0 {
                -value
            } else {
                value
            };
        }
    }
}

#[derive(Clone, Debug)]
pub struct Dc {
    pub attrs: DcAttrs,
    pub saved: Vec<DcAttrs>,
    pub target: DcTarget,
}

/// Host state behind every GDI handle.
#[derive(Debug)]
pub struct GdiState {
    pub objects: HashMap<u32, GdiObject>,
    pub dcs: HashMap<u32, Dc>,
    next: u32,
    /// Backing surfaces of top-level windows, keyed by HWND.
    pub surfaces: HashMap<u32, Surface>,
    /// Off-screen surface for GetDC(NULL) drawing.
    pub screen: Surface,
}

impl Default for GdiState {
    fn default() -> Self {
        let mut objects = HashMap::new();
        let stock: [(u32, GdiObject); 17] = [
            (0, GdiObject::Brush(Brush::Solid(0xFFFFFF))),
            (1, GdiObject::Brush(Brush::Solid(0xC0C0C0))),
            (2, GdiObject::Brush(Brush::Solid(0x808080))),
            (3, GdiObject::Brush(Brush::Solid(0x404040))),
            (4, GdiObject::Brush(Brush::Solid(0x000000))),
            (5, GdiObject::Brush(Brush::Null)),
            (
                6,
                GdiObject::Pen(Pen {
                    style: 0,
                    width: 1,
                    color: 0xFFFFFF,
                }),
            ),
            (
                7,
                GdiObject::Pen(Pen {
                    style: 0,
                    width: 1,
                    color: 0x000000,
                }),
            ),
            (
                8,
                GdiObject::Pen(Pen {
                    style: 5,
                    width: 1,
                    color: 0,
                }),
            ),
            (10, GdiObject::Font(LogFont::named(b"Terminal", 12, 400))),
            (11, GdiObject::Font(LogFont::named(b"Courier", 13, 400))),
            (
                12,
                GdiObject::Font(LogFont::named(b"MS Sans Serif", 13, 400)),
            ),
            (13, GdiObject::Font(LogFont::named(b"System", 16, 700))),
            (14, GdiObject::Font(LogFont::named(b"System", 16, 700))),
            (15, GdiObject::Palette(default_palette())),
            (16, GdiObject::Font(LogFont::named(b"Fixedsys", 15, 400))),
            (
                17,
                GdiObject::Font(LogFont::named(b"MS Sans Serif", -11, 400)),
            ),
        ];
        for (index, object) in stock {
            objects.insert(STOCK_BASE + index * 4, object);
        }
        for index in 0..31 {
            objects.insert(
                SYSBRUSH_BASE + index * 4,
                GdiObject::Brush(Brush::Solid(rgb_to_colorref(sys_color(index)))),
            );
        }
        let mut default_bitmap = Surface::new(1, 1, 1);
        default_bitmap.put(0, 0, 0);
        objects.insert(
            DEFAULT_BITMAP,
            GdiObject::Bitmap(Bitmap {
                surface: default_bitmap,
                dib: None,
            }),
        );
        Self {
            objects,
            dcs: HashMap::new(),
            next: DYNAMIC_BASE,
            surfaces: HashMap::new(),
            screen: Surface::new(1, 1, 24),
        }
    }
}

impl GdiState {
    fn alloc(&mut self) -> u32 {
        self.next = self.next.wrapping_add(4);
        if self.next < DYNAMIC_BASE {
            self.next = DYNAMIC_BASE;
        }
        self.next
    }

    pub fn insert(&mut self, object: GdiObject) -> u32 {
        let handle = self.alloc();
        self.objects.insert(handle, object);
        handle
    }

    pub fn new_dc(&mut self, target: DcTarget) -> u32 {
        let handle = self.alloc();
        let mut attrs = DcAttrs::default();
        if matches!(target, DcTarget::Window { .. }) {
            attrs.bitmap = 0;
        }
        self.dcs.insert(
            handle,
            Dc {
                attrs,
                saved: Vec::new(),
                target,
            },
        );
        handle
    }

    pub fn is_stock(handle: u32) -> bool {
        (STOCK_BASE..DYNAMIC_BASE).contains(&handle)
    }

    pub fn font(&self, handle: u32) -> LogFont {
        match self.objects.get(&handle) {
            Some(GdiObject::Font(font)) => font.clone(),
            _ => LogFont::named(b"MS Sans Serif", -11, 400),
        }
    }

    fn palette_of(&self, dc: &DcAttrs) -> Vec<Rgb> {
        match self.objects.get(&dc.palette) {
            Some(GdiObject::Palette(entries)) => entries.clone(),
            _ => default_palette(),
        }
    }

    /// Resolve a COLORREF, including PALETTEINDEX and PALETTERGB forms.
    pub fn color(&self, dc: &DcAttrs, color: u32) -> Rgb {
        match color >> 24 {
            1 => {
                let palette = self.palette_of(dc);
                palette.get((color & 0xFFFF) as usize).copied().unwrap_or(0)
            }
            _ => colorref_to_rgb(color & 0xFF_FFFF),
        }
    }

    pub fn bitmap_surface(&self, handle: u32) -> Option<&Surface> {
        match self.objects.get(&handle) {
            Some(GdiObject::Bitmap(bitmap)) => Some(&bitmap.surface),
            _ => None,
        }
    }

    /// Bits per pixel of the surface a DC draws on.
    pub fn dc_bpp(&self, hdc: u32) -> u16 {
        match self.dcs.get(&hdc) {
            Some(Dc {
                target: DcTarget::Memory,
                attrs,
                ..
            }) => self
                .bitmap_surface(attrs.bitmap)
                .map(|surface| surface.bits_per_pixel)
                .unwrap_or(1),
            _ => 24,
        }
    }

    fn stroke(&self, dc: &DcAttrs) -> Stroke {
        let pen = match self.objects.get(&dc.pen) {
            Some(GdiObject::Pen(pen)) => pen.clone(),
            _ => Pen {
                style: 0,
                width: 1,
                color: 0,
            },
        };
        Stroke {
            color: self.color(dc, pen.color),
            width: dc.scale_x(pen.width.max(1)).max(1),
            style: pen.style & 0xF,
            mode: dc.rop2,
        }
    }

    pub fn fill_of(&self, dc: &DcAttrs, brush: u32) -> Fill {
        match self.objects.get(&brush) {
            Some(GdiObject::Brush(Brush::Solid(color))) => Fill::Solid(self.color(dc, *color)),
            Some(GdiObject::Brush(Brush::Hatch(style, color))) => Fill::hatch(
                *style,
                self.color(dc, *color),
                Some(self.color(dc, dc.bk_color)),
            ),
            Some(GdiObject::Brush(Brush::Pattern(surface))) => {
                let mono = surface.is_mono();
                Fill::Pattern {
                    width: surface.width,
                    height: surface.height,
                    pixels: surface
                        .pixels
                        .iter()
                        .map(|pixel| {
                            if mono {
                                PatternPixel::Mono(*pixel & 0xFF_FFFF == 0xFF_FFFF)
                            } else {
                                PatternPixel::Color(*pixel)
                            }
                        })
                        .collect(),
                }
            }
            _ => Fill::Null,
        }
    }

    fn mono_colors(&self, dc: &DcAttrs) -> MonoColors {
        MonoColors {
            foreground: self.color(dc, dc.text_color),
            background: self.color(dc, dc.bk_color),
        }
    }
}

/// Which surface an operation draws on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceKey {
    Window(u32),
    Bitmap(u32),
    Screen,
}

/// Everything needed to execute one drawing call on a DC.
pub struct DrawCtx {
    pub attrs: DcAttrs,
    /// Device origin of the DC within its surface.
    pub origin: (i32, i32),
    pub stroke: Stroke,
    pub fill: Fill,
    pub mono: MonoColors,
    pub face: &'static FontFace,
    pub font: LogFont,
}

impl DrawCtx {
    /// Logical point to surface coordinates.
    pub fn pt(&self, x: i32, y: i32) -> (i32, i32) {
        let (dx, dy) = self.attrs.lp_to_dp(x, y);
        (dx + self.origin.0, dy + self.origin.1)
    }

    pub fn rect(&self, left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        let (l, t) = self.pt(left, top);
        let (r, b) = self.pt(right, bottom);
        Rect::new(l, t, r, b).normalized()
    }

    pub fn brush_origin(&self) -> (i32, i32) {
        (
            self.attrs.brush_org.0 + self.origin.0,
            self.attrs.brush_org.1 + self.origin.1,
        )
    }

    pub fn text_color(&self) -> Rgb {
        self.mono.foreground
    }

    pub fn bk_color(&self) -> Rgb {
        self.mono.background
    }
}

struct Resolved {
    key: SurfaceKey,
    origin: (i32, i32),
    clip: Region,
    ctx: DrawCtx,
    dib: Option<u32>,
}

fn resolve(shared: &SharedState, hdc: u32) -> Option<Resolved> {
    let gdi = &shared.gdi;
    let dc = gdi.dcs.get(&hdc)?;
    let (key, origin, base) = match &dc.target {
        DcTarget::Window {
            hwnd,
            client,
            paint,
        } => {
            let (top, ox, oy, mut clip) =
                crate::win32::user::dc_geometry(&shared.gui, *hwnd, *client)?;
            if let Some(paint) = paint {
                clip = clip.intersect(paint);
            }
            (SurfaceKey::Window(top), (ox, oy), clip)
        }
        DcTarget::Memory => {
            let surface = gdi.bitmap_surface(dc.attrs.bitmap)?;
            (
                SurfaceKey::Bitmap(dc.attrs.bitmap),
                (0, 0),
                Region::from_rect(surface.bounds()),
            )
        }
        DcTarget::Screen => (
            SurfaceKey::Screen,
            (0, 0),
            Region::from_rect(gdi.screen.bounds()),
        ),
        DcTarget::Info => return None,
    };
    let mut clip = base;
    if let Some(dc_clip) = &dc.attrs.clip {
        let mut dc_clip = dc_clip.clone();
        dc_clip.offset(origin.0, origin.1);
        clip = clip.intersect(&dc_clip);
    }
    let dib = match key {
        SurfaceKey::Bitmap(handle) => match gdi.objects.get(&handle) {
            Some(GdiObject::Bitmap(Bitmap { dib: Some(_), .. })) => Some(handle),
            _ => None,
        },
        _ => None,
    };
    let font = gdi.font(dc.attrs.font);
    let ctx = DrawCtx {
        stroke: gdi.stroke(&dc.attrs),
        fill: gdi.fill_of(&dc.attrs, dc.attrs.brush),
        mono: gdi.mono_colors(&dc.attrs),
        face: font.face(),
        font,
        attrs: dc.attrs.clone(),
        origin,
    };
    Some(Resolved {
        key,
        origin,
        clip,
        ctx,
        dib,
    })
}

fn take_surface(shared: &mut SharedState, key: SurfaceKey) -> Option<Surface> {
    match key {
        SurfaceKey::Window(hwnd) => shared.gdi.surfaces.remove(&hwnd),
        SurfaceKey::Bitmap(handle) => match shared.gdi.objects.get_mut(&handle) {
            Some(GdiObject::Bitmap(bitmap)) => Some(std::mem::replace(
                &mut bitmap.surface,
                Surface::new(0, 0, 24),
            )),
            _ => None,
        },
        SurfaceKey::Screen => Some(std::mem::replace(
            &mut shared.gdi.screen,
            Surface::new(0, 0, 24),
        )),
    }
}

fn put_surface(shared: &mut SharedState, key: SurfaceKey, surface: Surface) {
    match key {
        SurfaceKey::Window(hwnd) => {
            shared.gdi.surfaces.insert(hwnd, surface);
            shared.gui.dirty = true;
        }
        SurfaceKey::Bitmap(handle) => {
            if let Some(GdiObject::Bitmap(bitmap)) = shared.gdi.objects.get_mut(&handle) {
                bitmap.surface = surface;
            }
        }
        SurfaceKey::Screen => shared.gdi.screen = surface,
    }
}

// ── DIB encoding ───────────────────────────────────────────────────────────

/// A parsed BITMAPINFO.
#[derive(Clone, Debug)]
pub struct DibInfo {
    pub width: i32,
    pub height: i32,
    pub bpp: u16,
    pub compression: u32,
    pub size_image: u32,
    pub header_size: u32,
    pub colors: Vec<Rgb>,
    pub color_count: usize,
    pub masks: Option<(u32, u32, u32)>,
    /// Address of the colour table in guest memory.
    pub table_va: u32,
}

impl DibInfo {
    pub fn stride(&self) -> usize {
        ((self.width.unsigned_abs() as usize)
            .saturating_mul(self.bpp as usize)
            .saturating_add(31)
            / 32)
            .saturating_mul(4)
    }

    /// The size of the bits: run-length encoded bitmaps give theirs in the
    /// header (and may be larger than the decoded rows).
    pub fn image_size(&self) -> usize {
        if (self.compression == 1 || self.compression == 2) && self.size_image != 0 {
            return self.size_image as usize;
        }
        self.stride()
            .saturating_mul(self.height.unsigned_abs() as usize)
    }
}

fn read_u16<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> u16 {
    memory
        .read_bytes(va, 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0)
}

fn read_i32<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> i32 {
    memory.read_u32(va).unwrap_or(0) as i32
}

/// Read a BITMAPINFO (or BITMAPCOREINFO) and its colour table. `usage` 1
/// (`DIB_PAL_COLORS`) maps 16-bit palette indexes through `palette`.
pub fn read_dib_info<M: GuestMemory + ?Sized>(
    memory: &mut M,
    va: u32,
    usage: u32,
    palette: &[Rgb],
) -> Option<DibInfo> {
    if va == 0 {
        return None;
    }
    let header_size = memory.read_u32(va)?;
    let (width, height, bpp, compression, size_image, clr_used, table_va, quad) =
        if header_size == 12 {
            (
                read_u16(memory, va + 4) as i16 as i32,
                read_u16(memory, va + 6) as i16 as i32,
                read_u16(memory, va + 10),
                0,
                0,
                0,
                va + 12,
                false,
            )
        } else {
            (
                read_i32(memory, va + 4),
                read_i32(memory, va + 8),
                read_u16(memory, va + 14),
                memory.read_u32(va + 16)?,
                memory.read_u32(va + 20)?,
                memory.read_u32(va + 32)?,
                va + header_size,
                true,
            )
        };
    // GDI rejects headers with impossible dimensions or depths.
    if width <= 0
        || width > 32768
        || height == 0
        || height.unsigned_abs() > 32768
        || !matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32)
    {
        return None;
    }
    let mut masks = None;
    let mut table_va = table_va;
    if compression == 3 && (bpp == 16 || bpp == 32) {
        let r = memory.read_u32(table_va)?;
        let g = memory.read_u32(table_va + 4)?;
        let b = memory.read_u32(table_va + 8)?;
        masks = Some((r, g, b));
        table_va += 12;
    }
    let color_count = if bpp <= 8 {
        if clr_used == 0 || clr_used > (1 << bpp) {
            1usize << bpp
        } else {
            clr_used as usize
        }
    } else {
        clr_used as usize
    };
    let mut colors = Vec::with_capacity(color_count);
    if bpp <= 8 {
        if usage == 1 {
            for index in 0..color_count {
                let entry = read_u16(memory, table_va + index as u32 * 2) as usize;
                colors.push(palette.get(entry).copied().unwrap_or(0));
            }
        } else {
            let entry_size = if quad { 4 } else { 3 };
            let bytes = memory.read_bytes(table_va, color_count * entry_size)?;
            for chunk in bytes.chunks(entry_size) {
                colors.push((chunk[2] as u32) << 16 | (chunk[1] as u32) << 8 | chunk[0] as u32);
            }
        }
    }
    Some(DibInfo {
        width,
        height,
        bpp,
        compression,
        size_image,
        header_size,
        colors,
        color_count,
        masks,
        table_va,
    })
}

fn mask_shift(mask: u32) -> (u32, u32) {
    if mask == 0 {
        return (0, 0);
    }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).count_ones();
    (shift, bits)
}

fn expand(value: u32, mask: u32) -> u32 {
    let (shift, bits) = mask_shift(mask);
    if bits == 0 {
        return 0;
    }
    let v = (value & mask) >> shift;
    if bits >= 8 {
        v >> (bits - 8)
    } else {
        (v << (8 - bits)) | (v >> (2 * bits).saturating_sub(8).min(7))
    }
}

/// Decode DIB rows `start..start+lines` (bottom-up numbering for positive
/// heights) into `surface`.
pub fn decode_dib(info: &DibInfo, bits: &[u8], surface: &mut Surface, start: usize, lines: usize) {
    let width = info.width.unsigned_abs() as usize;
    let height = info.height.unsigned_abs() as usize;
    let bottom_up = info.height > 0;
    if info.compression == 1 || info.compression == 2 {
        decode_rle(info, bits, surface);
        return;
    }
    let stride = info.stride();
    for line in start..(start + lines).min(height) {
        let row_offset = (line - start) * stride;
        let y = if bottom_up { height - 1 - line } else { line };
        if row_offset + stride > bits.len() {
            break;
        }
        let row = &bits[row_offset..row_offset + stride];
        for x in 0..width {
            let color = match info.bpp {
                1 => {
                    let index = (row[x / 8] >> (7 - x % 8)) & 1;
                    info.colors
                        .get(index as usize)
                        .copied()
                        .unwrap_or(if index == 0 { 0 } else { 0xFFFFFF })
                }
                4 => {
                    let byte = row[x / 2];
                    let index = if x % 2 == 0 { byte >> 4 } else { byte & 0xF };
                    info.colors.get(index as usize).copied().unwrap_or(0)
                }
                8 => info.colors.get(row[x] as usize).copied().unwrap_or(0),
                16 => {
                    let value = u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]) as u32;
                    let (rm, gm, bm) = info.masks.unwrap_or((0x7C00, 0x03E0, 0x001F));
                    expand(value, rm) << 16 | expand(value, gm) << 8 | expand(value, bm)
                }
                24 => {
                    (row[x * 3 + 2] as u32) << 16 | (row[x * 3 + 1] as u32) << 8 | row[x * 3] as u32
                }
                32 => {
                    let value = u32::from_le_bytes([
                        row[x * 4],
                        row[x * 4 + 1],
                        row[x * 4 + 2],
                        row[x * 4 + 3],
                    ]);
                    match info.masks {
                        Some((rm, gm, bm)) => {
                            expand(value, rm) << 16 | expand(value, gm) << 8 | expand(value, bm)
                        }
                        None => value & 0xFF_FFFF,
                    }
                }
                _ => 0,
            };
            surface.put(x as i32, y as i32, color);
        }
    }
}

fn decode_rle(info: &DibInfo, bits: &[u8], surface: &mut Surface) {
    let height = info.height.unsigned_abs() as i32;
    let four = info.compression == 2;
    let (mut x, mut line) = (0i32, 0i32);
    let mut index = 0usize;
    let put = |x: i32, line: i32, value: u8, surface: &mut Surface| {
        let y = height - 1 - line;
        let color = info.colors.get(value as usize).copied().unwrap_or(0);
        surface.put(x, y, color);
    };
    while index + 1 < bits.len() {
        let count = bits[index];
        let value = bits[index + 1];
        index += 2;
        if count > 0 {
            for n in 0..count {
                let v = if four {
                    if n % 2 == 0 {
                        value >> 4
                    } else {
                        value & 0xF
                    }
                } else {
                    value
                };
                put(x, line, v, surface);
                x += 1;
            }
            continue;
        }
        match value {
            0 => {
                x = 0;
                line += 1;
            }
            1 => break,
            2 => {
                if index + 1 < bits.len() {
                    x += bits[index] as i32;
                    line += bits[index + 1] as i32;
                    index += 2;
                }
            }
            absolute => {
                let n = absolute as usize;
                let bytes = if four { n.div_ceil(2) } else { n };
                for k in 0..n {
                    let v = if four {
                        let byte = *bits.get(index + k / 2).unwrap_or(&0);
                        if k % 2 == 0 {
                            byte >> 4
                        } else {
                            byte & 0xF
                        }
                    } else {
                        *bits.get(index + k).unwrap_or(&0)
                    };
                    put(x, line, v, surface);
                    x += 1;
                }
                index += bytes + (bytes & 1);
            }
        }
    }
}

/// Encode surface rows into DIB bits for `info`'s format.
pub fn encode_dib(info: &DibInfo, surface: &Surface, start: usize, lines: usize) -> Vec<u8> {
    let width = info.width.unsigned_abs() as usize;
    let height = info.height.unsigned_abs() as usize;
    let bottom_up = info.height > 0;
    let stride = info.stride();
    let lines = lines.min(height.saturating_sub(start));
    let mut out = vec![0u8; stride * lines];
    let mut cache: HashMap<Rgb, u8> = HashMap::new();
    for line in start..start + lines {
        let y = if bottom_up { height - 1 - line } else { line };
        let row = &mut out[(line - start) * stride..(line - start + 1) * stride];
        for x in 0..width {
            let color = surface.get(x as i32, y as i32).unwrap_or(0);
            match info.bpp {
                1 => {
                    let index = if info.colors.len() >= 2 {
                        *cache
                            .entry(color)
                            .or_insert_with(|| nearest_index(&info.colors, color) as u8)
                    } else {
                        u8::from(color == 0xFFFFFF)
                    };
                    if index != 0 {
                        row[x / 8] |= 0x80 >> (x % 8);
                    }
                }
                4 => {
                    let index = *cache
                        .entry(color)
                        .or_insert_with(|| nearest_index(&info.colors, color) as u8);
                    if x % 2 == 0 {
                        row[x / 2] |= index << 4;
                    } else {
                        row[x / 2] |= index & 0xF;
                    }
                }
                8 => {
                    row[x] = *cache
                        .entry(color)
                        .or_insert_with(|| nearest_index(&info.colors, color) as u8);
                }
                16 => {
                    let (r, g, b) = (
                        (color >> 19) & 0x1F,
                        (color >> 11) & 0x1F,
                        (color >> 3) & 0x1F,
                    );
                    let value = (r << 10 | g << 5 | b) as u16;
                    row[x * 2..x * 2 + 2].copy_from_slice(&value.to_le_bytes());
                }
                24 => {
                    row[x * 3] = color as u8;
                    row[x * 3 + 1] = (color >> 8) as u8;
                    row[x * 3 + 2] = (color >> 16) as u8;
                }
                32 => {
                    row[x * 4..x * 4 + 4].copy_from_slice(&(color & 0xFF_FFFF).to_le_bytes());
                }
                _ => {}
            }
        }
    }
    out
}

fn dib_section_info(section: &DibSection) -> DibInfo {
    DibInfo {
        width: section.width,
        height: section.height,
        bpp: section.bpp,
        compression: if section.masks.is_some() { 3 } else { 0 },
        size_image: 0,
        header_size: 40,
        colors: section.colors.clone(),
        color_count: section.colors.len(),
        masks: section.masks,
        table_va: 0,
    }
}

/// Refresh a DIB section's host surface from its guest bits.
fn sync_in<M: GuestMemory + ?Sized>(memory: &mut M, shared: &mut SharedState, handle: u32) {
    if let Some(GdiObject::Bitmap(bitmap)) = shared.gdi.objects.get_mut(&handle) {
        if let Some(section) = &bitmap.dib {
            let info = dib_section_info(section);
            if let Some(bits) = memory.read_bytes(section.bits, info.image_size()) {
                let lines = info.height.unsigned_abs() as usize;
                decode_dib(&info, &bits, &mut bitmap.surface, 0, lines);
            }
        }
    }
}

/// A copy of a bitmap's pixels (DIB sections read from guest memory).
pub fn bitmap_pixels<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, handle: u32) -> Option<Surface> {
    let mut shared = ctx.shared.borrow_mut();
    sync_in(memory, &mut shared, handle);
    if let Some(surface) = shared.gdi.bitmap_surface(handle) {
        return Some(surface.clone());
    }
    None
}

/// A new device-dependent bitmap object holding `surface`.
pub fn insert_bitmap(ctx: &ApiContext, surface: Surface) -> u32 {
    ctx.shared.borrow_mut().gdi.insert(GdiObject::Bitmap(Bitmap { surface, dib: None }))
}

/// Write a DIB section's host surface back to its guest bits.
fn sync_out<M: GuestMemory + ?Sized>(memory: &mut M, shared: &SharedState, handle: u32) {
    if let Some(GdiObject::Bitmap(bitmap)) = shared.gdi.objects.get(&handle) {
        if let Some(section) = &bitmap.dib {
            let info = dib_section_info(section);
            let lines = info.height.unsigned_abs() as usize;
            let bits = encode_dib(&info, &bitmap.surface, 0, lines);
            let _ = memory.write_bytes(section.bits, &bits);
        }
    }
}

/// Run `draw` on the surface behind `hdc`. Returns `None` for DCs without a
/// surface (information contexts) or invalid handles.
/// EnumFontFamilies(Ex): call `callback(ENUMLOGFONTEX*, NEWTEXTMETRICEX*,
/// type, lparam)` for each font family (matching `filter`, a face name, if
/// given), with Unicode structures when `wide`.
pub fn enum_font_families<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    filter: Option<Vec<u8>>,
    callback: u32,
    lparam: u32,
    wide: bool,
) -> u32 {
    let filter = filter.filter(|face| !face.is_empty()).map(|face| face.to_ascii_lowercase());
    // (face, pitch-and-family, type) — TRUETYPE_FONTTYPE 4, RASTER 1.
    let families: [(&[u8], u8, u32); 6] = [
        (b"Arial", 0x22, 4),
        (b"Courier New", 0x31, 4),
        (b"MS Sans Serif", 0x22, 1),
        (b"Times New Roman", 0x12, 4),
        (b"System", 0x22, 1),
        (b"Wingdings", 0x02, 4),
    ];
    let buffer = ctx.shared.borrow_mut().heap.alloc(512).unwrap_or(0);
    if buffer == 0 {
        return 0;
    }
    let widen = |bytes: &[u8]| -> Vec<u8> { bytes.iter().flat_map(|&b| [b, 0]).collect() };
    let mut result = 1;
    for (face, pitch, kind) in families {
        if filter.as_ref().is_some_and(|filter| face.to_ascii_lowercase() != *filter) {
            continue;
        }
        let font = LogFont {
            height: 16,
            width: 7,
            escapement: 0,
            weight: 400,
            italic: false,
            underline: false,
            strike: false,
            charset: 0,
            pitch,
            face: face.to_vec(),
        };
        let logfont = logfont_bytes(&font);
        let mut full = face.to_vec();
        full.resize(64, 0);
        let mut style = b"Regular".to_vec();
        style.resize(32, 0);
        let mut tm = textmetric_bytes(&font);
        let (elf, tm, tm_offset) = if wide {
            // ENUMLOGFONTEXW: LOGFONTW, then full name, style and script.
            let mut elf = logfont[..28].to_vec();
            elf.extend(widen(&logfont[28..60]));
            elf.extend(widen(&full));
            elf.extend(widen(&style));
            elf.resize(348, 0);
            // TEXTMETRICW has WCHAR first/last/default/break characters.
            let mut wide_tm = tm[..44].to_vec();
            wide_tm.extend(widen(&tm[44..48]));
            wide_tm.extend_from_slice(&tm[48..53]);
            wide_tm.resize(100, 0);
            (elf, wide_tm, 352)
        } else {
            let mut elf = logfont;
            elf.extend_from_slice(&full);
            elf.extend_from_slice(&style);
            elf.resize(156, 0);
            tm.extend_from_slice(&[0u8; 24]);
            tm.truncate(80);
            (elf, tm, 160)
        };
        let _ = memory.write_bytes(buffer, &elf);
        let _ = memory.write_bytes(buffer + tm_offset, &tm);
        match memory.call_guest(callback, &[buffer, buffer + tm_offset, kind, lparam]) {
            Some(value) => {
                result = value;
                if value == 0 {
                    break;
                }
            }
            None => break,
        }
    }
    ctx.shared.borrow_mut().heap.free(buffer);
    result
}

pub fn with_dc<M: GuestMemory + ?Sized, R>(
    ctx: &ApiContext,
    memory: &mut M,
    hdc: u32,
    draw: impl FnOnce(&mut Target<'_>, &DrawCtx) -> R,
) -> Option<R> {
    let mut shared = ctx.shared.borrow_mut();
    let shared = &mut *shared;
    let resolved = resolve(shared, hdc)?;
    if let Some(handle) = resolved.dib {
        sync_in(memory, shared, handle);
    }
    let mut surface = take_surface(shared, resolved.key)?;
    let result = {
        let mut target = Target {
            surface: &mut surface,
            clip: &resolved.clip,
        };
        draw(&mut target, &resolved.ctx)
    };
    put_surface(shared, resolved.key, surface);
    if let Some(handle) = resolved.dib {
        sync_out(memory, shared, handle);
    }
    let _ = resolved.origin;
    Some(result)
}

fn update_pos(ctx: &ApiContext, hdc: u32, pos: (i32, i32)) {
    if let Some(dc) = ctx.shared.borrow_mut().gdi.dcs.get_mut(&hdc) {
        dc.attrs.pos = pos;
    }
}

fn read_points<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, count: u32) -> Vec<(i32, i32)> {
    let count = count.min(1 << 16) as usize;
    let bytes = memory.read_bytes(va, count * 8).unwrap_or_default();
    bytes
        .chunks_exact(8)
        .map(|c| {
            (
                i32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                i32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            )
        })
        .collect()
}

pub fn read_rect<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> Option<Rect> {
    if va == 0 {
        return None;
    }
    let bytes = memory.read_bytes(va, 16)?;
    let v = |i: usize| i32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    Some(Rect::new(v(0), v(4), v(8), v(12)))
}

pub fn write_rect<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, rect: Rect) -> Option<()> {
    if va == 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(16);
    for value in [rect.left, rect.top, rect.right, rect.bottom] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    memory.write_bytes(va, &bytes)
}

fn write_point<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, point: (i32, i32)) {
    if va != 0 {
        let _ = memory.write_u32(va, point.0 as u32);
        let _ = memory.write_u32(va + 4, point.1 as u32);
    }
}

/// Stroke a polyline of surface points with the DC pen.
fn stroke_polyline(target: &mut Target<'_>, ctx: &DrawCtx, points: &[(i32, i32)], closed: bool) {
    if ctx.stroke.is_null() || points.is_empty() {
        return;
    }
    if points.len() == 1 {
        return;
    }
    for pair in points.windows(2) {
        gfx::stroke_line(target, &ctx.stroke, pair[0], pair[1], false);
    }
    if closed {
        gfx::stroke_line(
            target,
            &ctx.stroke,
            *points.last().unwrap(),
            points[0],
            false,
        );
    }
}

fn draw_rect_shape(target: &mut Target<'_>, ctx: &DrawCtx, rect: Rect) {
    let mut rect = rect.normalized();
    let inside = ctx.stroke.style == 6;
    let w = if ctx.stroke.is_null() {
        0
    } else {
        ctx.stroke.width
    };
    if !ctx.fill.is_null() {
        let inner = if w > 0 {
            rect.inflate(
                -(w / 2).max(if w == 1 { 1 } else { 0 }),
                -(w / 2).max(if w == 1 { 1 } else { 0 }),
            )
        } else {
            rect
        };
        let inner = if w == 0 {
            Rect::new(rect.left, rect.top, rect.right - 1, rect.bottom - 1)
        } else {
            inner
        };
        target.fill_rect(
            inner,
            &ctx.fill,
            ctx.attrs.rop2,
            ctx.brush_origin(),
            ctx.mono,
        );
    }
    if w == 0 {
        return;
    }
    if inside && w > 1 {
        rect = rect.inflate(-(w / 2), -(w / 2));
    }
    let (l, t, r, b) = (rect.left, rect.top, rect.right - 1, rect.bottom - 1);
    if w == 1 {
        let points = [(l, t), (r, t), (r, b), (l, b)];
        stroke_polyline(target, ctx, &points, true);
    } else {
        let mut stroke = ctx.stroke;
        stroke.style = 0;
        // Wide frames are filled bands so corners stay square.
        let half = w / 2;
        let outer = Rect::new(l - half, t - half, r + w - half, b + w - half);
        let inner = Rect::new(l + w - half, t + w - half, r - half, b - half);
        let band = Region::from_rect(outer).subtract_rect(&inner);
        for piece in band.rects {
            target.fill_rect(
                piece,
                &Fill::Solid(stroke.color),
                stroke.mode,
                (0, 0),
                ctx.mono,
            );
        }
    }
}

fn draw_ellipse(target: &mut Target<'_>, ctx: &DrawCtx, rect: Rect) {
    let rect = rect.normalized();
    let w = if ctx.stroke.is_null() {
        0
    } else {
        ctx.stroke.width
    };
    let spans = gfx::ellipse_spans(rect);
    if !ctx.fill.is_null() {
        let inner = if w > 0 {
            gfx::ellipse_spans(rect.inflate(-w.max(1), -w.max(1)).normalized())
        } else {
            spans.clone()
        };
        let interior = if w == 1 {
            // Interior is everything not on the one-pixel outline.
            let outline: std::collections::HashSet<(i32, i32)> =
                gfx::ellipse_outline(rect).into_iter().collect();
            let mut result = Vec::new();
            for &(y, x0, x1) in &spans {
                let mut start = None;
                for x in x0..=x1 {
                    let inside = x < x1 && !outline.contains(&(x, y));
                    match (inside, start) {
                        (true, None) => start = Some(x),
                        (false, Some(s)) => {
                            result.push((y, s, x));
                            start = None;
                        }
                        _ => {}
                    }
                }
            }
            result
        } else {
            inner
        };
        target.fill_spans(
            &interior,
            &ctx.fill,
            ctx.attrs.rop2,
            ctx.brush_origin(),
            ctx.mono,
        );
    }
    if w == 0 {
        return;
    }
    if w == 1 {
        for (x, y) in gfx::ellipse_outline(rect) {
            target.plot(x, y, ctx.stroke.color, ctx.stroke.mode);
        }
    } else {
        let outer = gfx::ellipse_spans(rect.inflate(w / 2, w / 2));
        let inner_rect = rect.inflate(-(w - w / 2), -(w - w / 2));
        let inner: std::collections::HashSet<(i32, i32)> = gfx::ellipse_spans(inner_rect)
            .into_iter()
            .flat_map(|(y, x0, x1)| (x0..x1).map(move |x| (x, y)))
            .collect();
        for (y, x0, x1) in outer {
            for x in x0..x1 {
                if !inner.contains(&(x, y)) {
                    target.plot(x, y, ctx.stroke.color, ctx.stroke.mode);
                }
            }
        }
    }
}

fn draw_round_rect(target: &mut Target<'_>, ctx: &DrawCtx, rect: Rect, ew: i32, eh: i32) {
    let rect = rect.normalized();
    let ew = ew.abs().min(rect.width());
    let eh = eh.abs().min(rect.height());
    if ew < 2 || eh < 2 {
        draw_rect_shape(target, ctx, rect);
        return;
    }
    // Build the rounded outline as a polygon from the four corner arcs.
    let mut points = Vec::new();
    let (rx, ry) = (ew as f64 / 2.0, eh as f64 / 2.0);
    let corners = [
        (rect.right as f64 - 1.0 - rx, rect.top as f64 + ry, -90.0f64),
        (rect.left as f64 + rx, rect.top as f64 + ry, 180.0),
        (rect.left as f64 + rx, rect.bottom as f64 - 1.0 - ry, 90.0),
        (
            rect.right as f64 - 1.0 - rx,
            rect.bottom as f64 - 1.0 - ry,
            0.0,
        ),
    ];
    let order = [
        (0usize, 0.0f64, -90.0f64),
        (1, -90.0, -180.0),
        (2, 180.0, 90.0),
        (3, 90.0, 0.0),
    ];
    let _ = corners;
    let centers = [
        (rect.right as f64 - 1.0 - rx, rect.top as f64 + ry),
        (rect.left as f64 + rx, rect.top as f64 + ry),
        (rect.left as f64 + rx, rect.bottom as f64 - 1.0 - ry),
        (rect.right as f64 - 1.0 - rx, rect.bottom as f64 - 1.0 - ry),
    ];
    for (corner, start, end) in order {
        let (cx, cy) = centers[corner];
        let steps = 8;
        for step in 0..=steps {
            let angle = (start + (end - start) * step as f64 / steps as f64).to_radians();
            let point = (
                (cx + rx * angle.cos()).round() as i32,
                (cy + ry * angle.sin()).round() as i32,
            );
            if points.last() != Some(&point) {
                points.push(point);
            }
        }
    }
    if !ctx.fill.is_null() {
        let spans = gfx::polygon_spans(&points, false);
        target.fill_spans(
            &spans,
            &ctx.fill,
            ctx.attrs.rop2,
            ctx.brush_origin(),
            ctx.mono,
        );
    }
    stroke_polyline(target, ctx, &points, true);
}

/// Draw a run of text at surface position `(x, y)` (top-left of the cell)
/// honouring the background mode. Returns the width drawn.
pub fn text_run(
    target: &mut Target<'_>,
    ctx: &DrawCtx,
    x: i32,
    y: i32,
    text: &[u8],
    dx: Option<&[i32]>,
    opaque_rect: Option<Rect>,
) -> i32 {
    let face = ctx.face;
    let width = match dx {
        Some(widths) => widths.iter().take(text.len()).sum(),
        None => face.text_width(text) + ctx.attrs.char_extra * text.len() as i32,
    };
    if ctx.attrs.bk_mode == 2 && opaque_rect.is_none() {
        target.fill_rect(
            Rect::new(x, y, x + width, y + face.height),
            &Fill::Solid(ctx.bk_color()),
            gfx::R2_COPYPEN,
            (0, 0),
            ctx.mono,
        );
    }
    let mut pen_x = x;
    for (index, byte) in text.iter().enumerate() {
        gfx::draw_text(target, face, pen_x, y, &[*byte], ctx.text_color(), false);
        if ctx.font.italic {
            // Built-in faces are upright; italic requests render upright.
        }
        pen_x += match dx {
            Some(widths) => widths.get(index).copied().unwrap_or(face.advance(*byte)),
            None => face.advance(*byte) + ctx.attrs.char_extra,
        };
    }
    if ctx.font.underline {
        let uy = y + face.ascent + 1;
        for px in x..x + width {
            target.plot(px, uy, ctx.text_color(), gfx::R2_COPYPEN);
        }
    }
    if ctx.font.strike {
        let sy = y + face.ascent * 2 / 3;
        for px in x..x + width {
            target.plot(px, sy, ctx.text_color(), gfx::R2_COPYPEN);
        }
    }
    width
}

/// Text extent of `text` in the DC's font.
pub fn text_extent(ctx: &ApiContext, hdc: u32, text: &[u8]) -> (i32, i32) {
    let shared = ctx.shared.borrow();
    let attrs = shared
        .gdi
        .dcs
        .get(&hdc)
        .map(|dc| dc.attrs.clone())
        .unwrap_or_default();
    let face = shared.gdi.font(attrs.font).face();
    (
        face.text_width(text) + attrs.char_extra * text.len() as i32,
        face.height,
    )
}

/// Win32 `DrawTextA`, shared with USER32. Returns the text height.
pub fn draw_text_api<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hdc: u32,
    text: &[u8],
    rect_va: u32,
    format: u32,
) -> i32 {
    const DT_CENTER: u32 = 0x1;
    const DT_RIGHT: u32 = 0x2;
    const DT_VCENTER: u32 = 0x4;
    const DT_BOTTOM: u32 = 0x8;
    const DT_WORDBREAK: u32 = 0x10;
    const DT_SINGLELINE: u32 = 0x20;
    const DT_EXPANDTABS: u32 = 0x40;
    const DT_NOCLIP: u32 = 0x100;
    const DT_CALCRECT: u32 = 0x400;
    const DT_NOPREFIX: u32 = 0x800;
    let Some(rect) = read_rect(memory, rect_va) else {
        return 0;
    };
    let (face, attrs) = {
        let shared = ctx.shared.borrow();
        let attrs = shared
            .gdi
            .dcs
            .get(&hdc)
            .map(|dc| dc.attrs.clone())
            .unwrap_or_default();
        (shared.gdi.font(attrs.font).face(), attrs)
    };
    // Split into lines, honouring prefixes and word breaks.
    let mut lines: Vec<(Vec<u8>, Option<usize>)> = Vec::new();
    let raw_lines: Vec<&[u8]> = if format & DT_SINGLELINE != 0 {
        vec![text]
    } else {
        text.split(|byte| *byte == b'\n').collect()
    };
    let max_width = rect.width().max(1);
    for raw in raw_lines {
        let raw: Vec<u8> = raw.iter().copied().filter(|byte| *byte != b'\r').collect();
        let mut clean = Vec::new();
        let mut underline = None;
        let mut index = 0;
        while index < raw.len() {
            let byte = raw[index];
            if byte == b'&' && format & DT_NOPREFIX == 0 {
                if raw.get(index + 1) == Some(&b'&') {
                    clean.push(b'&');
                    index += 2;
                    continue;
                }
                underline = Some(clean.len());
                index += 1;
                continue;
            }
            if byte == b'\t' && format & DT_EXPANDTABS != 0 {
                clean.extend_from_slice(b"        ");
            } else {
                clean.push(byte);
            }
            index += 1;
        }
        if format & DT_WORDBREAK != 0 && format & DT_SINGLELINE == 0 {
            let mut current = Vec::new();
            for word in clean.split(|byte| *byte == b' ') {
                let candidate = if current.is_empty() {
                    word.to_vec()
                } else {
                    [current.as_slice(), b" ", word].concat()
                };
                if face.text_width(&candidate) > max_width && !current.is_empty() {
                    lines.push((std::mem::take(&mut current), None));
                    current = word.to_vec();
                } else {
                    current = candidate;
                }
            }
            lines.push((current, underline));
        } else {
            lines.push((clean, underline));
        }
    }
    let line_height = face.height;
    let total_height = line_height * lines.len().max(1) as i32;
    if format & DT_CALCRECT != 0 {
        let width = lines
            .iter()
            .map(|(line, _)| face.text_width(line))
            .max()
            .unwrap_or(0);
        let mut out = rect;
        out.right = out.left + width;
        out.bottom = out.top + total_height;
        let _ = write_rect(memory, rect_va, out);
        return total_height;
    }
    let mut y = rect.top;
    if format & DT_SINGLELINE != 0 {
        if format & DT_VCENTER != 0 {
            y = rect.top + (rect.height() - line_height) / 2;
        } else if format & DT_BOTTOM != 0 {
            y = rect.bottom - line_height;
        }
    }
    let _ = attrs;
    let clip_rect = rect;
    with_dc(ctx, memory, hdc, |target, dctx| {
        let device_clip = dctx.rect(
            clip_rect.left,
            clip_rect.top,
            clip_rect.right,
            clip_rect.bottom,
        );
        let clip = if format & DT_NOCLIP != 0 {
            target.clip.clone()
        } else {
            target.clip.intersect_rect(&device_clip)
        };
        let mut sub = Target {
            surface: target.surface,
            clip: &clip,
        };
        let mut line_y = y;
        for (line, underline) in &lines {
            let width = face.text_width(line);
            let x = if format & DT_CENTER != 0 {
                rect.left + (rect.width() - width) / 2
            } else if format & DT_RIGHT != 0 {
                rect.right - width
            } else {
                rect.left
            };
            let (sx, sy) = dctx.pt(x, line_y);
            text_run(&mut sub, dctx, sx, sy, line, None, None);
            if let Some(position) = underline {
                if *position < line.len() {
                    let ux = sx + face.text_width(&line[..*position]);
                    let uw = face.advance(line[*position]);
                    for px in ux..ux + uw {
                        sub.plot(px, sy + face.ascent + 1, dctx.text_color(), gfx::R2_COPYPEN);
                    }
                }
            }
            line_y += line_height;
        }
    });
    total_height
}

// ── API surface ────────────────────────────────────────────────────────────

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "AbortDoc"
        | "CloseMetaFile"
        | "CloseEnhMetaFile"
        | "CreateCompatibleDC"
        | "CreateFontIndirectA"
        | "CreateHalftonePalette"
        | "CreateMetaFileA"
        | "CreatePalette"
        | "CreatePatternBrush"
        | "CreateRectRgnIndirect"
        | "CreateSolidBrush"
        | "DeleteDC"
        | "DeleteMetaFile"
        | "DeleteObject"
        | "EndDoc"
        | "EndPage"
        | "GetBkColor"
        | "GetBkMode"
        | "GetMapMode"
        | "GetObjectType"
        | "GetPolyFillMode"
        | "GetROP2"
        | "GetStockObject"
        | "GetStretchBltMode"
        | "GetTextAlign"
        | "GetTextCharset"
        | "GetTextColor"
        | "RealizePalette"
        | "SaveDC"
        | "StartPage"
        | "UnrealizeObject"
        | "CreateBrushIndirect"
        | "CreatePenIndirect" => 1,
        "CopyMetaFileA"
        | "CreateHatchBrush"
        | "GetClipBox"
        | "GetClipRgn"
        | "GetCurrentPositionEx"
        | "GetDeviceCaps"
        | "GetNearestColor"
        | "GetNearestPaletteIndex"
        | "GetTextMetricsA"
        | "GetViewportExtEx"
        | "GetViewportOrgEx"
        | "GetWindowExtEx"
        | "GetWindowOrgEx"
        | "PlayMetaFile"
        | "RectVisible"
        | "ResizePalette"
        | "RestoreDC"
        | "SelectClipPath"
        | "SelectClipRgn"
        | "SelectObject"
        | "SetAbortProc"
        | "SetArcDirection"
        | "SetBkColor"
        | "SetBkMode"
        | "SetColorAdjustment"
        | "SetMapMode"
        | "SetMapperFlags"
        | "SetPolyFillMode"
        | "SetROP2"
        | "SetStretchBltMode"
        | "SetTextAlign"
        | "SetTextCharacterExtra"
        | "SetTextColor"
        | "StartDocA"
        | "GetDCOrgEx"
        | "GetRgnBox"
        | "GetCurrentObject"
        | "CreateDIBPatternBrushPt" => 2,
        "CreateCompatibleBitmap"
        | "CreatePen"
        | "CreatePolygonRgn"
        | "DPtoLP"
        | "ExtSelectClipRgn"
        | "FillRgn"
        | "GetObjectA"
        | "GetPixel"
        | "GetTextFaceA"
        | "LPtoDP"
        | "LineTo"
        | "OffsetClipRgn"
        | "OffsetRgn"
        | "PolyBezier"
        | "PolyBezierTo"
        | "Polygon"
        | "Polyline"
        | "PolylineTo"
        | "PtVisible"
        | "SelectPalette"
        | "SetTextJustification"
        | "PtInRegion" => 3,
        "CombineRgn"
        | "CreateDCA"
        | "CreateICA"
        | "CreateRectRgn"
        | "EnumFontFamiliesA"
        | "EnumMetaFile"
        | "GetCharWidthA"
        | "GetDIBColorTable"
        | "GetPaletteEntries"
        | "GetTextExtentPointA"
        | "GetTextExtentPoint32A"
        | "MoveToEx"
        | "OffsetViewportOrgEx"
        | "OffsetWindowOrgEx"
        | "PlayMetaFileRecord"
        | "PolyDraw"
        | "SetBrushOrgEx"
        | "SetDIBColorTable"
        | "SetPaletteEntries"
        | "SetPixel"
        | "SetPixelV"
        | "SetViewportExtEx"
        | "SetViewportOrgEx"
        | "SetWindowExtEx"
        | "SetWindowOrgEx"
        | "CreateEnhMetaFileA"
        | "CreateEllipticRgn"
        | "PolyPolygon" => 4,
        "CreateBitmap"
        | "Ellipse"
        | "EnumFontFamiliesExA"
        | "Escape"
        | "ExcludeClipRect"
        | "ExtCreatePen"
        | "ExtFloodFill"
        | "IntersectClipRect"
        | "Rectangle"
        | "SetRectRgn"
        | "TextOutA"
        | "FloodFill" => {
            if name == "FloodFill" {
                4
            } else {
                5
            }
        }
        "CreateDIBSection" | "CreateDIBitmap" | "PatBlt" | "ScaleViewportExtEx"
        | "ScaleWindowExtEx" => 6,
        "GetDIBits" | "RoundRect" | "SetDIBits" => 7,
        "ExtTextOutA" | "ExtTextOutW" => 8,
        "ArcTo" | "BitBlt" | "Arc" | "Pie" | "Chord" => 9,
        "StretchBlt" => 11,
        "SetDIBitsToDevice" => 12,
        "StretchDIBits" => 13,
        "CreateFontA" => 14,
        _ => return None,
    })
}

/// `GetObjectType` values.
fn object_type(shared: &SharedState, handle: u32) -> u32 {
    if let Some(dc) = shared.gdi.dcs.get(&handle) {
        return if matches!(dc.target, DcTarget::Memory) {
            10
        } else {
            3
        };
    }
    match shared.gdi.objects.get(&handle) {
        Some(GdiObject::Pen(_)) => 1,
        Some(GdiObject::Brush(_)) => 2,
        Some(GdiObject::Palette(_)) => 5,
        Some(GdiObject::Font(_)) => 6,
        Some(GdiObject::Bitmap(_)) => 7,
        Some(GdiObject::Region(_)) => 8,
        Some(GdiObject::MetaFile) => 9,
        None => 0,
    }
}

fn region_kind(region: &Region) -> u32 {
    match region.rects.len() {
        0 => 1,
        1 => 2,
        _ => 3,
    }
}

fn read_logfont<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> Option<LogFont> {
    let bytes = memory.read_bytes(va, 60)?;
    let v = |i: usize| i32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let face_bytes = &bytes[28..60];
    let end = face_bytes.iter().position(|b| *b == 0).unwrap_or(32);
    Some(LogFont {
        height: v(0),
        width: v(4),
        escapement: v(8),
        weight: v(16),
        italic: bytes[20] != 0,
        underline: bytes[21] != 0,
        strike: bytes[22] != 0,
        charset: bytes[23],
        pitch: bytes[27],
        face: face_bytes[..end].to_vec(),
    })
}

fn logfont_bytes(font: &LogFont) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(60);
    for value in [
        font.height,
        font.width,
        font.escapement,
        font.escapement,
        font.weight,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&[
        font.italic as u8,
        font.underline as u8,
        font.strike as u8,
        font.charset,
        0,
        0,
        0,
        font.pitch,
    ]);
    let mut face = font.face.clone();
    face.truncate(31);
    face.resize(32, 0);
    bytes.extend_from_slice(&face);
    bytes
}

fn textmetric_bytes(font: &LogFont) -> Vec<u8> {
    let face = font.face();
    let descent = face.height - face.ascent;
    let internal = (face.height - (font.cell_height() as f32 / 1.2) as i32).max(0);
    let mut bytes = Vec::with_capacity(56);
    for value in [
        face.height,
        face.ascent,
        descent,
        internal,
        0,
        face.average_width(),
        face.max_width(),
        if face.bold { 700 } else { 400 },
        0,
        96,
        96,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&[0x20, 0xFF, 0x1F, 0x20]);
    bytes.extend_from_slice(&[
        font.italic as u8,
        font.underline as u8,
        font.strike as u8,
        0x22,
        font.charset,
    ]);
    bytes.resize(56, 0);
    bytes
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &ImportName,
    args: &[u32],
) -> Option<ApiOutcome> {
    let name = name_of(name)?;
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    let a = |i: usize| args[i] as i32;

    match name {
        // ── DC lifetime ──
        "CreateCompatibleDC" => {
            let handle = ctx.shared.borrow_mut().gdi.new_dc(DcTarget::Memory);
            ok(handle)
        }
        "CreateDCA" | "CreateICA" => {
            let driver = read_cstr(memory, args[0]).to_ascii_uppercase();
            let device = read_cstr(memory, args[1]).to_ascii_uppercase();
            // The display by driver name, or a monitor by device name
            // (\\.\DISPLAY1, as GetMonitorInfo gives it).
            let target =
                if driver == b"DISPLAY" || args[0] == 0 && (device.is_empty() || device.starts_with(b"\\\\.\\DISPLAY")) {
                    DcTarget::Screen
                } else {
                    // No printers are installed.
                    return ok(0);
                };
            let handle = ctx.shared.borrow_mut().gdi.new_dc(target);
            ok(handle)
        }
        "DeleteDC" => {
            let removed = ctx.shared.borrow_mut().gdi.dcs.remove(&args[0]).is_some();
            ok(removed as u32)
        }
        "SaveDC" => {
            let mut shared = ctx.shared.borrow_mut();
            match shared.gdi.dcs.get_mut(&args[0]) {
                Some(dc) => {
                    dc.saved.push(dc.attrs.clone());
                    let depth = dc.saved.len() as u32;
                    drop(shared);
                    ok(depth)
                }
                None => ok(0),
            }
        }
        "RestoreDC" => {
            let mut shared = ctx.shared.borrow_mut();
            match shared.gdi.dcs.get_mut(&args[0]) {
                Some(dc) => {
                    let level = a(1);
                    let target_len = if level < 0 {
                        dc.saved.len() as i32 + level
                    } else {
                        level - 1
                    };
                    if target_len < 0 || target_len as usize >= dc.saved.len() {
                        return ok(0);
                    }
                    dc.saved.truncate(target_len as usize + 1);
                    if let Some(attrs) = dc.saved.pop() {
                        dc.attrs = attrs;
                    }
                    ok(1)
                }
                None => ok(0),
            }
        }

        // ── Objects ──
        "GetStockObject" => {
            let index = args[0];
            if index <= 17 && index != 9 {
                ok(STOCK_BASE + index * 4)
            } else {
                ok(0)
            }
        }
        "CreatePen" => {
            let pen = Pen {
                style: args[0],
                width: a(1),
                color: args[2],
            };
            let handle = ctx.shared.borrow_mut().gdi.insert(GdiObject::Pen(pen));
            ok(handle)
        }
        "CreatePenIndirect" => {
            let style = memory.read_u32(args[0]).unwrap_or(0);
            let width = memory.read_u32(args[0] + 4).unwrap_or(1) as i32;
            let color = memory.read_u32(args[0] + 12).unwrap_or(0);
            let handle = ctx.shared.borrow_mut().gdi.insert(GdiObject::Pen(Pen {
                style,
                width,
                color,
            }));
            ok(handle)
        }
        "ExtCreatePen" => {
            let color = memory.read_u32(args[2] + 4).unwrap_or(0);
            let pen = Pen {
                style: args[0] & 0xF,
                width: a(1),
                color,
            };
            let handle = ctx.shared.borrow_mut().gdi.insert(GdiObject::Pen(pen));
            ok(handle)
        }
        "CreateSolidBrush" => {
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Brush(Brush::Solid(args[0])));
            ok(handle)
        }
        "CreateHatchBrush" => {
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Brush(Brush::Hatch(args[0], args[1])));
            ok(handle)
        }
        "CreateBrushIndirect" => {
            let style = memory.read_u32(args[0]).unwrap_or(0);
            let color = memory.read_u32(args[0] + 4).unwrap_or(0);
            let hatch = memory.read_u32(args[0] + 8).unwrap_or(0);
            let mut shared = ctx.shared.borrow_mut();
            let brush = match style {
                0 => Brush::Solid(color),
                1 => Brush::Null,
                2 => Brush::Hatch(hatch, color),
                3 => match shared.gdi.bitmap_surface(hatch) {
                    Some(surface) => Brush::Pattern(surface.clone()),
                    None => Brush::Null,
                },
                _ => Brush::Solid(color),
            };
            let handle = shared.gdi.insert(GdiObject::Brush(brush));
            ok(handle)
        }
        "CreatePatternBrush" => {
            let mut shared = ctx.shared.borrow_mut();
            let surface = shared.gdi.bitmap_surface(args[0]).cloned();
            match surface {
                Some(surface) => {
                    let handle = shared.gdi.insert(GdiObject::Brush(Brush::Pattern(surface)));
                    ok(handle)
                }
                None => ok(0),
            }
        }
        "CreateDIBPatternBrushPt" => {
            let palette = default_palette();
            let Some(info) = read_dib_info(memory, args[0], args[1], &palette) else {
                return ok(0);
            };
            let table = if info.bpp <= 8 {
                info.color_count as u32 * if args[1] == 1 { 2 } else { 4 }
            } else {
                0
            };
            let bits_va = info.table_va + table;
            let bits = memory
                .read_bytes(bits_va, info.image_size())
                .unwrap_or_default();
            let mut surface = Surface::new(info.width.abs(), info.height.abs(), 24);
            decode_dib(
                &info,
                &bits,
                &mut surface,
                0,
                info.height.unsigned_abs() as usize,
            );
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Brush(Brush::Pattern(surface)));
            ok(handle)
        }
        "CreateFontIndirectA" => match read_logfont(memory, args[0]) {
            Some(font) => {
                let handle = ctx.shared.borrow_mut().gdi.insert(GdiObject::Font(font));
                ok(handle)
            }
            None => ok(0),
        },
        "CreateFontA" => {
            let face = read_cstr(memory, args[13]);
            let font = LogFont {
                height: a(0),
                width: a(1),
                escapement: a(2),
                weight: a(4),
                italic: args[5] != 0,
                underline: args[6] != 0,
                strike: args[7] != 0,
                charset: args[8] as u8,
                pitch: args[12] as u8,
                face,
            };
            let handle = ctx.shared.borrow_mut().gdi.insert(GdiObject::Font(font));
            ok(handle)
        }
        "CreateBitmap" => {
            let (width, height, bpp) = (a(0).max(0), a(1).max(0), args[3] as u16);
            let mono = bpp <= 1 && args[2] <= 1;
            let mut surface = Surface::new(width, height, if mono { 1 } else { 24 });
            if args[4] != 0 {
                let bits_per_pixel = if mono { 1 } else { bpp.max(1) };
                let stride = ((width as usize * bits_per_pixel as usize + 15) / 16) * 2;
                if let Some(bytes) = memory.read_bytes(args[4], stride * height as usize) {
                    for y in 0..height as usize {
                        for x in 0..width as usize {
                            let row = &bytes[y * stride..];
                            let color = match bits_per_pixel {
                                1 => {
                                    if row[x / 8] & (0x80 >> (x % 8)) != 0 {
                                        0xFFFFFF
                                    } else {
                                        0
                                    }
                                }
                                8 => {
                                    let palette = halftone_palette();
                                    palette[row[x] as usize]
                                }
                                16 => {
                                    let v = u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]) as u32;
                                    expand(v, 0x7C00) << 16
                                        | expand(v, 0x03E0) << 8
                                        | expand(v, 0x1F)
                                }
                                24 => {
                                    (row[x * 3 + 2] as u32) << 16
                                        | (row[x * 3 + 1] as u32) << 8
                                        | row[x * 3] as u32
                                }
                                32 => {
                                    (row[x * 4 + 2] as u32) << 16
                                        | (row[x * 4 + 1] as u32) << 8
                                        | row[x * 4] as u32
                                }
                                _ => 0,
                            };
                            surface.put(x as i32, y as i32, color);
                        }
                    }
                }
            }
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Bitmap(Bitmap { surface, dib: None }));
            ok(handle)
        }
        "CreateCompatibleBitmap" => {
            let mut shared = ctx.shared.borrow_mut();
            let bpp = if shared.gdi.dc_bpp(args[0]) == 1 {
                1
            } else {
                24
            };
            let mut surface = Surface::new(a(1).max(0), a(2).max(0), bpp);
            if bpp != 1 {
                surface.pixels.fill(0);
            }
            let handle = shared
                .gdi
                .insert(GdiObject::Bitmap(Bitmap { surface, dib: None }));
            ok(handle)
        }
        "CreateDIBitmap" => {
            let palette = {
                let shared = ctx.shared.borrow();
                let attrs = shared
                    .gdi
                    .dcs
                    .get(&args[0])
                    .map(|dc| dc.attrs.clone())
                    .unwrap_or_default();
                shared.gdi.palette_of(&attrs)
            };
            let Some(header) = read_dib_info(memory, args[1], 0, &palette) else {
                return ok(0);
            };
            let bpp = if args[0] != 0 && ctx.shared.borrow().gdi.dc_bpp(args[0]) == 1 {
                1
            } else {
                24
            };
            let mut surface = Surface::new(header.width.abs(), header.height.abs(), bpp);
            if args[2] & 4 != 0 && args[3] != 0 {
                if let Some(info) = read_dib_info(memory, args[4], args[5], &palette) {
                    let size = if info.compression == 1 || info.compression == 2 {
                        info.size_image as usize
                    } else {
                        info.image_size()
                    };
                    let bits = memory.read_bytes(args[3], size).unwrap_or_default();
                    decode_dib(
                        &info,
                        &bits,
                        &mut surface,
                        0,
                        info.height.unsigned_abs() as usize,
                    );
                }
            }
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Bitmap(Bitmap { surface, dib: None }));
            ok(handle)
        }
        "CreateDIBSection" => {
            let palette = default_palette();
            let Some(info) = read_dib_info(memory, args[1], args[2], &palette) else {
                return ok(0);
            };
            let size = info.image_size() as u32;
            let bits = {
                let mut shared = ctx.shared.borrow_mut();
                shared.heap.alloc(size.max(4)).unwrap_or(0)
            };
            if bits == 0 {
                return ok(0);
            }
            let _ = memory.write_bytes(bits, &vec![0u8; size as usize]);
            if args[3] != 0 {
                let _ = memory.write_ptr(args[3], bits as u64);
            }
            let colors = if info.bpp <= 8 {
                info.colors.clone()
            } else {
                Vec::new()
            };
            let mut surface = Surface::new(
                info.width.abs(),
                info.height.abs(),
                if info.bpp == 1 && colors.first() == Some(&0) && colors.get(1) == Some(&0xFFFFFF) {
                    1
                } else {
                    24
                },
            );
            let zero = colors.first().copied().unwrap_or(0);
            surface.pixels.fill(zero);
            let section = DibSection {
                bits,
                bpp: info.bpp,
                width: info.width,
                height: info.height,
                colors,
                masks: info.masks,
            };
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Bitmap(Bitmap {
                    surface,
                    dib: Some(section),
                }));
            ok(handle)
        }
        "CreateRectRgn" => {
            let region = Region::from_rect(Rect::new(a(0), a(1), a(2), a(3)).normalized());
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Region(region));
            ok(handle)
        }
        "CreateRectRgnIndirect" => {
            let rect = read_rect(memory, args[0]).unwrap_or_default().normalized();
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Region(Region::from_rect(rect)));
            ok(handle)
        }
        "CreateEllipticRgn" => {
            let spans = gfx::ellipse_spans(Rect::new(a(0), a(1), a(2), a(3)));
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Region(Region::from_spans(&spans)));
            ok(handle)
        }
        "CreatePolygonRgn" => {
            let points = read_points(memory, args[0], args[1]);
            let spans = gfx::polygon_spans(&points, args[2] == 2);
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Region(Region::from_spans(&spans)));
            ok(handle)
        }
        "SetRectRgn" => {
            let mut shared = ctx.shared.borrow_mut();
            if let Some(GdiObject::Region(region)) = shared.gdi.objects.get_mut(&args[0]) {
                *region = Region::from_rect(Rect::new(a(1), a(2), a(3), a(4)).normalized());
                return ok(1);
            }
            ok(0)
        }
        "CombineRgn" => {
            let mut shared = ctx.shared.borrow_mut();
            let get = |handle: u32, shared: &SharedState| match shared.gdi.objects.get(&handle) {
                Some(GdiObject::Region(region)) => Some(region.clone()),
                _ => None,
            };
            let Some(src1) = get(args[1], &shared) else {
                return ok(0);
            };
            let src2 = get(args[2], &shared).unwrap_or_default();
            let result = match args[3] {
                1 => src1.intersect(&src2),
                2 => src1.union(&src2),
                3 => src1.xor(&src2),
                4 => src1.subtract(&src2),
                _ => src1,
            };
            let kind = region_kind(&result);
            if let Some(GdiObject::Region(region)) = shared.gdi.objects.get_mut(&args[0]) {
                *region = result;
                return ok(kind);
            }
            ok(0)
        }
        "OffsetRgn" => {
            let mut shared = ctx.shared.borrow_mut();
            if let Some(GdiObject::Region(region)) = shared.gdi.objects.get_mut(&args[0]) {
                region.offset(a(1), a(2));
                let kind = region_kind(region);
                return ok(kind);
            }
            ok(0)
        }
        "GetRgnBox" => {
            let region = match ctx.shared.borrow().gdi.objects.get(&args[0]) {
                Some(GdiObject::Region(region)) => region.clone(),
                _ => return ok(0),
            };
            let _ = write_rect(memory, args[1], region.bounds());
            ok(region_kind(&region))
        }
        "PtInRegion" => {
            let inside = match ctx.shared.borrow().gdi.objects.get(&args[0]) {
                Some(GdiObject::Region(region)) => region.contains(a(1), a(2)),
                _ => false,
            };
            ok(inside as u32)
        }
        "CreatePalette" => {
            let count = read_u16(memory, args[0] + 2) as usize;
            let bytes = memory
                .read_bytes(args[0] + 4, count * 4)
                .unwrap_or_default();
            let entries = bytes
                .chunks_exact(4)
                .map(|c| (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32)
                .collect();
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Palette(entries));
            ok(handle)
        }
        "CreateHalftonePalette" => {
            let handle = ctx
                .shared
                .borrow_mut()
                .gdi
                .insert(GdiObject::Palette(halftone_palette()));
            ok(handle)
        }
        "GetPaletteEntries" => {
            let entries = match ctx.shared.borrow().gdi.objects.get(&args[0]) {
                Some(GdiObject::Palette(entries)) => entries.clone(),
                _ => return ok(0),
            };
            if args[3] == 0 {
                return ok(entries.len() as u32);
            }
            let start = args[1] as usize;
            let end = (start + args[2] as usize).min(entries.len());
            let mut bytes = Vec::new();
            for entry in entries.iter().take(end).skip(start) {
                bytes.extend_from_slice(&[
                    (entry >> 16) as u8,
                    (entry >> 8) as u8,
                    *entry as u8,
                    0,
                ]);
            }
            let _ = memory.write_bytes(args[3], &bytes);
            ok(end.saturating_sub(start) as u32)
        }
        "SetPaletteEntries" => {
            let bytes = memory
                .read_bytes(args[3], args[2] as usize * 4)
                .unwrap_or_default();
            let mut shared = ctx.shared.borrow_mut();
            if let Some(GdiObject::Palette(entries)) = shared.gdi.objects.get_mut(&args[0]) {
                for (index, chunk) in bytes.chunks_exact(4).enumerate() {
                    let slot = args[1] as usize + index;
                    if slot < entries.len() {
                        entries[slot] =
                            (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8 | chunk[2] as u32;
                    }
                }
                return ok(args[2]);
            }
            ok(0)
        }
        "ResizePalette" => {
            let mut shared = ctx.shared.borrow_mut();
            if let Some(GdiObject::Palette(entries)) = shared.gdi.objects.get_mut(&args[0]) {
                entries.resize(args[1] as usize, 0);
                return ok(1);
            }
            ok(0)
        }
        "GetNearestPaletteIndex" => {
            let entries = match ctx.shared.borrow().gdi.objects.get(&args[0]) {
                Some(GdiObject::Palette(entries)) => entries.clone(),
                _ => return ok(u32::MAX),
            };
            if entries.is_empty() {
                return ok(u32::MAX);
            }
            ok(nearest_index(&entries, colorref_to_rgb(args[1] & 0xFF_FFFF)) as u32)
        }
        "GetNearestColor" => {
            let shared = ctx.shared.borrow();
            let attrs = shared
                .gdi
                .dcs
                .get(&args[0])
                .map(|dc| dc.attrs.clone())
                .unwrap_or_default();
            let rgb = shared.gdi.color(&attrs, args[1]);
            let rgb = if shared.gdi.dc_bpp(args[0]) == 1 {
                if rgb == 0xFFFFFF {
                    rgb
                } else {
                    0
                }
            } else {
                rgb
            };
            ok(rgb_to_colorref(rgb))
        }
        "SelectPalette" => {
            let mut shared = ctx.shared.borrow_mut();
            match shared.gdi.dcs.get_mut(&args[0]) {
                Some(dc) => {
                    let previous = dc.attrs.palette;
                    dc.attrs.palette = args[1];
                    ok(previous)
                }
                None => ok(0),
            }
        }
        "RealizePalette" => ok(0),
        "UnrealizeObject" => ok(1),
        "CreateMetaFileA" | "CreateEnhMetaFileA" => {
            // Metafile recording is accepted as an information DC; OLE uses it
            // only to build presentation caches.
            let handle = ctx.shared.borrow_mut().gdi.new_dc(DcTarget::Info);
            ok(handle)
        }
        "CloseMetaFile" | "CloseEnhMetaFile" => {
            let mut shared = ctx.shared.borrow_mut();
            shared.gdi.dcs.remove(&args[0]);
            let handle = shared.gdi.insert(GdiObject::MetaFile);
            ok(handle)
        }
        "CopyMetaFileA" => {
            let handle = ctx.shared.borrow_mut().gdi.insert(GdiObject::MetaFile);
            ok(handle)
        }
        "DeleteMetaFile" => {
            ctx.shared.borrow_mut().gdi.objects.remove(&args[0]);
            ok(1)
        }
        "PlayMetaFile" | "EnumMetaFile" | "PlayMetaFileRecord" => ok(1),
        "DeleteObject" => {
            let handle = args[0];
            if GdiState::is_stock(handle) {
                return ok(1);
            }
            let removed = ctx
                .shared
                .borrow_mut()
                .gdi
                .objects
                .remove(&handle)
                .is_some();
            ok(removed as u32)
        }
        "SelectObject" => {
            let (hdc, handle) = (args[0], args[1]);
            let mut shared = ctx.shared.borrow_mut();
            let shared = &mut *shared;
            let kind = match shared.gdi.objects.get(&handle) {
                Some(GdiObject::Pen(_)) => 1,
                Some(GdiObject::Brush(_)) => 2,
                Some(GdiObject::Font(_)) => 6,
                Some(GdiObject::Bitmap(_)) => 7,
                Some(GdiObject::Region(region)) => {
                    let region = region.clone();
                    if let Some(dc) = shared.gdi.dcs.get_mut(&hdc) {
                        let kind = region_kind(&region);
                        dc.attrs.clip = Some(region);
                        return ok(kind);
                    }
                    return ok(0);
                }
                _ => return ok(0),
            };
            let Some(dc) = shared.gdi.dcs.get_mut(&hdc) else {
                return ok(0);
            };
            let previous = match kind {
                1 => std::mem::replace(&mut dc.attrs.pen, handle),
                2 => std::mem::replace(&mut dc.attrs.brush, handle),
                6 => std::mem::replace(&mut dc.attrs.font, handle),
                _ => {
                    if !matches!(dc.target, DcTarget::Memory) {
                        return ok(0);
                    }
                    let previous = std::mem::replace(&mut dc.attrs.bitmap, handle);
                    dc.attrs.clip = None;
                    previous
                }
            };
            ok(previous)
        }
        "GetCurrentObject" => {
            let shared = ctx.shared.borrow();
            let Some(dc) = shared.gdi.dcs.get(&args[0]) else {
                return ok(0);
            };
            ok(match args[1] {
                1 => dc.attrs.pen,
                2 => dc.attrs.brush,
                5 => dc.attrs.palette,
                6 => dc.attrs.font,
                7 => dc.attrs.bitmap,
                _ => 0,
            })
        }
        "GetObjectType" => {
            let kind = object_type(&ctx.shared.borrow(), args[0]);
            ok(kind)
        }
        "GetObjectA" => {
            let (handle, size, out) = (args[0], args[1] as usize, args[2]);
            let object = ctx.shared.borrow().gdi.objects.get(&handle).cloned();
            let bytes: Vec<u8> = match object {
                Some(GdiObject::Pen(pen)) => {
                    let mut b = Vec::new();
                    for v in [pen.style, pen.width as u32, 0, pen.color] {
                        b.extend_from_slice(&v.to_le_bytes());
                    }
                    b
                }
                Some(GdiObject::Brush(brush)) => {
                    let (style, color, hatch) = match brush {
                        Brush::Null => (1, 0, 0),
                        Brush::Solid(color) => (0, color, 0),
                        Brush::Hatch(style, color) => (2, color, style),
                        Brush::Pattern(_) => (3, 0, 0),
                    };
                    let mut b = Vec::new();
                    for v in [style, color, hatch] {
                        b.extend_from_slice(&v.to_le_bytes());
                    }
                    b
                }
                Some(GdiObject::Font(font)) => logfont_bytes(&font),
                Some(GdiObject::Bitmap(bitmap)) => {
                    let surface = &bitmap.surface;
                    let bpp = match &bitmap.dib {
                        Some(section) => section.bpp,
                        None => {
                            if surface.is_mono() {
                                1
                            } else {
                                24
                            }
                        }
                    };
                    let width_bytes = match &bitmap.dib {
                        Some(_) => ((surface.width as u32 * bpp as u32 + 31) / 32) * 4,
                        None => ((surface.width as u32 * bpp as u32 + 15) / 16) * 2,
                    };
                    let mut b = Vec::new();
                    for v in [
                        0u32,
                        surface.width as u32,
                        surface.height as u32,
                        width_bytes,
                    ] {
                        b.extend_from_slice(&v.to_le_bytes());
                    }
                    b.extend_from_slice(&1u16.to_le_bytes());
                    b.extend_from_slice(&bpp.to_le_bytes());
                    b.extend_from_slice(
                        &bitmap
                            .dib
                            .as_ref()
                            .map(|d| d.bits)
                            .unwrap_or(0)
                            .to_le_bytes(),
                    );
                    // In a 64-bit process bmBits is 8 bytes (BITMAP is 32), and
                    // DIBSECTION's dshSection is too (aligned: 104 in all).
                    let x64 = memory.ptr_size() == 8;
                    if x64 {
                        b.extend_from_slice(&[0; 4]);
                    }
                    if let Some(section) = &bitmap.dib {
                        if size >= if x64 { 104 } else { 84 } {
                            // DIBSECTION: BITMAP + BITMAPINFOHEADER + masks + section.
                            for v in [40u32, section.width as u32, section.height as u32] {
                                b.extend_from_slice(&v.to_le_bytes());
                            }
                            b.extend_from_slice(&1u16.to_le_bytes());
                            b.extend_from_slice(&section.bpp.to_le_bytes());
                            let image = ((section.width.unsigned_abs() * section.bpp as u32 + 31)
                                / 32)
                                * 4
                                * section.height.unsigned_abs();
                            for v in [
                                if section.masks.is_some() { 3u32 } else { 0 },
                                image,
                                0,
                                0,
                                section.colors.len() as u32,
                                0,
                            ] {
                                b.extend_from_slice(&v.to_le_bytes());
                            }
                            let (r, g, bl) = section.masks.unwrap_or((0, 0, 0));
                            for v in [r, g, bl] {
                                b.extend_from_slice(&v.to_le_bytes());
                            }
                            // dshSection (pointer-sized, aligned), dsOffset.
                            if x64 {
                                b.extend_from_slice(&[0; 4 + 8 + 4 + 4]);
                            } else {
                                b.extend_from_slice(&[0; 8]);
                            }
                        }
                    }
                    b
                }
                Some(GdiObject::Palette(entries)) => (entries.len() as u16).to_le_bytes().to_vec(),
                _ => return ok(0),
            };
            if out == 0 {
                return ok(bytes.len() as u32);
            }
            let length = bytes.len().min(size);
            let _ = memory.write_bytes(out, &bytes[..length]);
            ok(length as u32)
        }

        // ── DC attributes ──
        "SetBkColor"
        | "SetTextColor"
        | "SetBkMode"
        | "SetROP2"
        | "SetPolyFillMode"
        | "SetStretchBltMode"
        | "SetTextAlign"
        | "SetMapMode"
        | "SetTextCharacterExtra"
        | "SetArcDirection" => {
            let mut shared = ctx.shared.borrow_mut();
            let Some(dc) = shared.gdi.dcs.get_mut(&args[0]) else {
                return ok(if name == "SetBkColor" || name == "SetTextColor" {
                    u32::MAX
                } else {
                    0
                });
            };
            let value = args[1];
            let slot = match name {
                "SetBkColor" => &mut dc.attrs.bk_color,
                "SetTextColor" => &mut dc.attrs.text_color,
                "SetBkMode" => &mut dc.attrs.bk_mode,
                "SetROP2" => &mut dc.attrs.rop2,
                "SetPolyFillMode" => &mut dc.attrs.poly_fill,
                "SetStretchBltMode" => &mut dc.attrs.stretch_mode,
                "SetTextAlign" => &mut dc.attrs.text_align,
                "SetArcDirection" => &mut dc.attrs.arc_direction,
                "SetTextCharacterExtra" => {
                    let previous = dc.attrs.char_extra;
                    dc.attrs.char_extra = value as i32;
                    return ok(previous as u32);
                }
                _ => {
                    let previous = dc.attrs.map_mode;
                    dc.attrs.map_mode = value;
                    match value {
                        1 => {
                            dc.attrs.window_ext = (1, 1);
                            dc.attrs.viewport_ext = (1, 1);
                        }
                        2..=6 => {
                            // Metric modes at 96 DPI with y increasing upwards.
                            let units = match value {
                                2 => 254,
                                3 => 2540,
                                4 => 100,
                                5 => 1000,
                                _ => 1440,
                            };
                            dc.attrs.window_ext = (units, units);
                            dc.attrs.viewport_ext = (96, -96);
                        }
                        _ => {}
                    }
                    return ok(previous);
                }
            };
            let previous = std::mem::replace(slot, value);
            ok(previous)
        }
        "GetBkColor" | "GetTextColor" | "GetBkMode" | "GetROP2" | "GetPolyFillMode"
        | "GetStretchBltMode" | "GetTextAlign" | "GetMapMode" => {
            let shared = ctx.shared.borrow();
            let Some(dc) = shared.gdi.dcs.get(&args[0]) else {
                return ok(0);
            };
            ok(match name {
                "GetBkColor" => dc.attrs.bk_color,
                "GetTextColor" => dc.attrs.text_color,
                "GetBkMode" => dc.attrs.bk_mode,
                "GetROP2" => dc.attrs.rop2,
                "GetPolyFillMode" => dc.attrs.poly_fill,
                "GetStretchBltMode" => dc.attrs.stretch_mode,
                "GetTextAlign" => dc.attrs.text_align,
                _ => dc.attrs.map_mode,
            })
        }
        "SetMapperFlags" => ok(0),
        "SetColorAdjustment" => ok(1),
        "SetTextJustification" => ok(1),
        "SetBrushOrgEx"
        | "SetWindowOrgEx"
        | "SetViewportOrgEx"
        | "SetWindowExtEx"
        | "SetViewportExtEx"
        | "OffsetWindowOrgEx"
        | "OffsetViewportOrgEx" => {
            let mut shared = ctx.shared.borrow_mut();
            let Some(dc) = shared.gdi.dcs.get_mut(&args[0]) else {
                return ok(0);
            };
            let fixed_ext = matches!(name, "SetWindowExtEx" | "SetViewportExtEx")
                && !matches!(dc.attrs.map_mode, 7 | 8);
            let slot = match name {
                "SetBrushOrgEx" => &mut dc.attrs.brush_org,
                "SetWindowOrgEx" | "OffsetWindowOrgEx" => &mut dc.attrs.window_org,
                "SetViewportOrgEx" | "OffsetViewportOrgEx" => &mut dc.attrs.viewport_org,
                "SetWindowExtEx" => &mut dc.attrs.window_ext,
                _ => &mut dc.attrs.viewport_ext,
            };
            let previous = *slot;
            if !fixed_ext {
                if name.starts_with("Offset") {
                    *slot = (slot.0 + a(1), slot.1 + a(2));
                } else if !(name.ends_with("ExtEx") && (a(1) == 0 || a(2) == 0)) {
                    *slot = (a(1), a(2));
                }
            }
            if name.ends_with("ExtEx") {
                dc.attrs.fix_isotropic();
            }
            drop(shared);
            write_point(memory, args[3], previous);
            ok(1)
        }
        "ScaleWindowExtEx" | "ScaleViewportExtEx" => {
            let mut shared = ctx.shared.borrow_mut();
            let Some(dc) = shared.gdi.dcs.get_mut(&args[0]) else {
                return ok(0);
            };
            let slot = if name == "ScaleWindowExtEx" {
                &mut dc.attrs.window_ext
            } else {
                &mut dc.attrs.viewport_ext
            };
            let previous = *slot;
            if matches!(dc.attrs.map_mode, 7 | 8) && a(2) != 0 && a(4) != 0 {
                *slot = (slot.0 * a(1) / a(2), slot.1 * a(3) / a(4));
                dc.attrs.fix_isotropic();
            }
            drop(shared);
            write_point(memory, args[5], previous);
            ok(1)
        }
        "GetWindowOrgEx"
        | "GetViewportOrgEx"
        | "GetWindowExtEx"
        | "GetViewportExtEx"
        | "GetCurrentPositionEx" => {
            let value = {
                let shared = ctx.shared.borrow();
                let Some(dc) = shared.gdi.dcs.get(&args[0]) else {
                    return ok(0);
                };
                match name {
                    "GetWindowOrgEx" => dc.attrs.window_org,
                    "GetViewportOrgEx" => dc.attrs.viewport_org,
                    "GetWindowExtEx" => dc.attrs.window_ext,
                    "GetViewportExtEx" => dc.attrs.viewport_ext,
                    _ => dc.attrs.pos,
                }
            };
            write_point(memory, args[1], value);
            ok(1)
        }
        "GetDCOrgEx" => {
            let origin = {
                let shared = ctx.shared.borrow();
                resolve(&shared, args[0])
                    .map(|resolved| {
                        let (ox, oy) = resolved.origin;
                        match resolved.key {
                            SurfaceKey::Window(top) => shared
                                .gui
                                .window(top)
                                .map(|w| (w.x + ox, w.y + oy))
                                .unwrap_or((ox, oy)),
                            _ => (ox, oy),
                        }
                    })
                    .unwrap_or((0, 0))
            };
            write_point(memory, args[1], origin);
            ok(1)
        }
        "LPtoDP" | "DPtoLP" => {
            let attrs = ctx
                .shared
                .borrow()
                .gdi
                .dcs
                .get(&args[0])
                .map(|dc| dc.attrs.clone());
            let Some(attrs) = attrs else {
                return ok(0);
            };
            let points = read_points(memory, args[1], args[2]);
            for (index, (x, y)) in points.into_iter().enumerate() {
                let point = if name == "LPtoDP" {
                    attrs.lp_to_dp(x, y)
                } else {
                    attrs.dp_to_lp(x, y)
                };
                write_point(memory, args[1] + index as u32 * 8, point);
            }
            ok(1)
        }
        "GetDeviceCaps" => {
            let bpp = ctx.shared.borrow().gdi.dc_bpp(args[0]);
            let (screen_w, screen_h) = ctx.shared.borrow().gui.screen;
            let value: i32 = match args[1] {
                0 => 0x400,                // DRIVERVERSION
                2 => 1,                    // TECHNOLOGY: DT_RASDISPLAY
                4 => screen_w * 254 / 960, // HORZSIZE (mm)
                6 => screen_h * 254 / 960, // VERTSIZE
                8 => screen_w,
                10 => screen_h,
                12 => {
                    if bpp == 1 {
                        1
                    } else {
                        24
                    }
                } // BITSPIXEL
                14 => 1,      // PLANES
                16 => -1,     // NUMBRUSHES
                18 => -1,     // NUMPENS
                20 => 0,      // NUMMARKERS
                22 => 0,      // NUMFONTS
                24 => -1,     // NUMCOLORS
                26 => 0,      // PDEVICESIZE
                28 => 0xFF,   // CURVECAPS
                30 => 0xFE,   // LINECAPS
                32 => 0xFF,   // POLYGONALCAPS
                34 => 0x7807, // TEXTCAPS
                36 => 1,      // CLIPCAPS
                38 => 0x7E99, // RASTERCAPS without RC_PALETTE
                40 => 36,
                42 => 36,
                44 => 51,
                88 | 90 => 96,  // LOGPIXELSX/Y
                104 => 0,       // SIZEPALETTE
                106 => 20,      // NUMRESERVED
                108 => 24,      // COLORRES
                110..=115 => 0, // printer physical metrics
                _ => 0,
            };
            ok(value as u32)
        }

        // ── Clipping ──
        "SelectClipRgn" | "ExtSelectClipRgn" => {
            let mode = if name == "SelectClipRgn" { 5 } else { args[2] };
            let mut shared = ctx.shared.borrow_mut();
            let region = match shared.gdi.objects.get(&args[1]) {
                Some(GdiObject::Region(region)) => Some(region.clone()),
                _ => None,
            };
            let Some(dc) = shared.gdi.dcs.get_mut(&args[0]) else {
                return ok(0);
            };
            let current = dc.attrs.clip.clone();
            let next = match (mode, region) {
                (5, region) => region,
                (_, None) => current,
                (1, Some(r)) => Some(current.map(|c| c.intersect(&r)).unwrap_or(r)),
                (2, Some(r)) => current.map(|c| c.union(&r)),
                (3, Some(r)) => current.map(|c| c.xor(&r)),
                (4, Some(r)) => current.map(|c| c.subtract(&r)),
                (_, Some(r)) => Some(r),
            };
            let kind = next.as_ref().map(region_kind).unwrap_or(2);
            dc.attrs.clip = next;
            ok(kind)
        }
        "IntersectClipRect" | "ExcludeClipRect" => {
            let mut shared = ctx.shared.borrow_mut();
            let shared = &mut *shared;
            let resolved_bounds = resolve(shared, args[0]).map(|r| {
                let mut bounds = r.clip.clone();
                bounds.offset(-r.origin.0, -r.origin.1);
                bounds
            });
            let Some(dc) = shared.gdi.dcs.get_mut(&args[0]) else {
                return ok(0);
            };
            let (l, t) = dc.attrs.lp_to_dp(a(1), a(2));
            let (r, b) = dc.attrs.lp_to_dp(a(3), a(4));
            let rect = Rect::new(l, t, r, b).normalized();
            let base = dc
                .attrs
                .clip
                .clone()
                .or(resolved_bounds)
                .unwrap_or_else(|| Region::from_rect(Rect::new(-0x4000, -0x4000, 0x4000, 0x4000)));
            let next = if name == "IntersectClipRect" {
                base.intersect_rect(&rect)
            } else {
                base.subtract_rect(&rect)
            };
            let kind = region_kind(&next);
            dc.attrs.clip = Some(next);
            ok(kind)
        }
        "OffsetClipRgn" => {
            let mut shared = ctx.shared.borrow_mut();
            let Some(dc) = shared.gdi.dcs.get_mut(&args[0]) else {
                return ok(0);
            };
            let (dx, dy) = (dc.attrs.scale_x(a(1)), dc.attrs.scale_x(a(2)));
            if let Some(clip) = &mut dc.attrs.clip {
                clip.offset(dx, dy);
                return ok(region_kind(clip));
            }
            ok(1)
        }
        "GetClipBox" => {
            let (bounds, attrs) = {
                let shared = ctx.shared.borrow();
                let attrs = shared
                    .gdi
                    .dcs
                    .get(&args[0])
                    .map(|dc| dc.attrs.clone())
                    .unwrap_or_default();
                let bounds = match resolve(&shared, args[0]) {
                    Some(resolved) => {
                        let mut clip = resolved.clip.clone();
                        clip.offset(-resolved.origin.0, -resolved.origin.1);
                        clip
                    }
                    None => Region::from_rect(Rect::new(0, 0, 1, 1)),
                };
                (bounds, attrs)
            };
            let rect = bounds.bounds();
            let (l, t) = attrs.dp_to_lp(rect.left, rect.top);
            let (r, b) = attrs.dp_to_lp(rect.right, rect.bottom);
            let _ = write_rect(memory, args[1], Rect::new(l, t, r, b).normalized());
            ok(region_kind(&bounds))
        }
        "GetClipRgn" => {
            let clip = ctx
                .shared
                .borrow()
                .gdi
                .dcs
                .get(&args[0])
                .and_then(|dc| dc.attrs.clip.clone());
            match clip {
                Some(region) => {
                    let mut shared = ctx.shared.borrow_mut();
                    if let Some(GdiObject::Region(target)) = shared.gdi.objects.get_mut(&args[1]) {
                        *target = region;
                        return ok(1);
                    }
                    ok(u32::MAX)
                }
                None => ok(0),
            }
        }
        "SelectClipPath" => ok(0),
        "PtVisible" | "RectVisible" => {
            let shared = ctx.shared.borrow();
            let Some(resolved) = resolve(&shared, args[0]) else {
                return ok(0);
            };
            let visible = if name == "PtVisible" {
                let (x, y) = resolved.ctx.pt(a(1), a(2));
                resolved.clip.contains(x, y)
            } else {
                drop(shared);
                let rect = read_rect(memory, args[1]).unwrap_or_default();
                let shared = ctx.shared.borrow();
                let Some(resolved) = resolve(&shared, args[0]) else {
                    return ok(0);
                };
                let device = resolved
                    .ctx
                    .rect(rect.left, rect.top, rect.right, rect.bottom);
                resolved.clip.intersects_rect(&device)
            };
            ok(visible as u32)
        }

        // ── Drawing ──
        "MoveToEx" => {
            let previous = ctx
                .shared
                .borrow()
                .gdi
                .dcs
                .get(&args[0])
                .map(|dc| dc.attrs.pos);
            let Some(previous) = previous else {
                return ok(0);
            };
            write_point(memory, args[3], previous);
            update_pos(ctx, args[0], (a(1), a(2)));
            ok(1)
        }
        "LineTo" => {
            let (x, y) = (a(1), a(2));
            with_dc(ctx, memory, args[0], |target, dctx| {
                let from = dctx.pt(dctx.attrs.pos.0, dctx.attrs.pos.1);
                let to = dctx.pt(x, y);
                gfx::stroke_line(target, &dctx.stroke, from, to, false);
            });
            update_pos(ctx, args[0], (x, y));
            ok(1)
        }
        "Polyline" | "PolylineTo" | "PolyBezier" | "PolyBezierTo" | "PolyDraw" => {
            let points = read_points(memory, args[1], args[2]);
            let mut last = None;
            with_dc(ctx, memory, args[0], |target, dctx| {
                let mut logical = Vec::new();
                if name.ends_with("To") || name == "PolyDraw" {
                    logical.push(dctx.attrs.pos);
                }
                logical.extend_from_slice(&points);
                last = logical.last().copied();
                let device: Vec<(i32, i32)> = logical.iter().map(|p| dctx.pt(p.0, p.1)).collect();
                if name.starts_with("PolyBezier") {
                    let mut flat = Vec::new();
                    let mut index = 0;
                    if let Some(first) = device.first() {
                        flat.push(*first);
                    }
                    while index + 3 < device.len() {
                        let curve = gfx::bezier_points(
                            device[index],
                            device[index + 1],
                            device[index + 2],
                            device[index + 3],
                        );
                        flat.extend(curve.into_iter().skip(1));
                        index += 3;
                    }
                    stroke_polyline(target, dctx, &flat, false);
                    if let Some(end) = flat.last() {
                        if flat.len() >= 2 {
                            gfx::stroke_line(target, &dctx.stroke, *end, *end, true);
                        }
                    }
                } else {
                    stroke_polyline(target, dctx, &device, false);
                }
            });
            if name.ends_with("To") || name == "PolyDraw" {
                if let Some(point) = last {
                    update_pos(ctx, args[0], point);
                }
            }
            ok(1)
        }
        "Polygon" => {
            let points = read_points(memory, args[1], args[2]);
            with_dc(ctx, memory, args[0], |target, dctx| {
                let device: Vec<(i32, i32)> = points.iter().map(|p| dctx.pt(p.0, p.1)).collect();
                if !dctx.fill.is_null() {
                    let spans = gfx::polygon_spans(&device, dctx.attrs.poly_fill == 2);
                    target.fill_spans(
                        &spans,
                        &dctx.fill,
                        dctx.attrs.rop2,
                        dctx.brush_origin(),
                        dctx.mono,
                    );
                }
                stroke_polyline(target, dctx, &device, true);
            });
            ok(1)
        }
        "PolyPolygon" => {
            let counts_bytes = memory
                .read_bytes(args[2], args[3] as usize * 4)
                .unwrap_or_default();
            let counts: Vec<u32> = counts_bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let total: u32 = counts.iter().sum();
            let points = read_points(memory, args[1], total);
            with_dc(ctx, memory, args[0], |target, dctx| {
                let mut offset = 0usize;
                for count in &counts {
                    let polygon: Vec<(i32, i32)> = points
                        [offset..(offset + *count as usize).min(points.len())]
                        .iter()
                        .map(|p| dctx.pt(p.0, p.1))
                        .collect();
                    offset += *count as usize;
                    if !dctx.fill.is_null() {
                        let spans = gfx::polygon_spans(&polygon, dctx.attrs.poly_fill == 2);
                        target.fill_spans(
                            &spans,
                            &dctx.fill,
                            dctx.attrs.rop2,
                            dctx.brush_origin(),
                            dctx.mono,
                        );
                    }
                    stroke_polyline(target, dctx, &polygon, true);
                }
            });
            ok(1)
        }
        "Rectangle" => {
            with_dc(ctx, memory, args[0], |target, dctx| {
                let rect = dctx.rect(a(1), a(2), a(3), a(4));
                draw_rect_shape(target, dctx, rect);
            });
            ok(1)
        }
        "Ellipse" => {
            with_dc(ctx, memory, args[0], |target, dctx| {
                let rect = dctx.rect(a(1), a(2), a(3), a(4));
                draw_ellipse(target, dctx, rect);
            });
            ok(1)
        }
        "RoundRect" => {
            with_dc(ctx, memory, args[0], |target, dctx| {
                let rect = dctx.rect(a(1), a(2), a(3), a(4));
                let (ew, eh) = (dctx.attrs.scale_x(a(5)), dctx.attrs.scale_x(a(6)));
                draw_round_rect(target, dctx, rect, ew, eh);
            });
            ok(1)
        }
        "Arc" | "ArcTo" | "Pie" | "Chord" => {
            let mut end_point = (0, 0);
            with_dc(ctx, memory, args[0], |target, dctx| {
                let rect = dctx.rect(a(1), a(2), a(3), a(4));
                let (sx, sy) = dctx.pt(a(5), a(6));
                let (ex, ey) = dctx.pt(a(7), a(8));
                let cx = (rect.left + rect.right) as f64 / 2.0;
                let cy = (rect.top + rect.bottom) as f64 / 2.0;
                let rx = rect.width() as f64 / 2.0;
                let ry = rect.height() as f64 / 2.0;
                let start = (cy - sy as f64).atan2(sx as f64 - cx);
                let mut end = (cy - ey as f64).atan2(ex as f64 - cx);
                if end <= start {
                    end += std::f64::consts::TAU;
                }
                let steps = 48;
                let mut points = Vec::new();
                for step in 0..=steps {
                    let angle = start + (end - start) * step as f64 / steps as f64;
                    points.push((
                        (cx + rx * angle.cos()).round() as i32,
                        (cy - ry * angle.sin()).round() as i32,
                    ));
                }
                if let Some(last) = points.last() {
                    end_point = *last;
                }
                match name {
                    "Pie" | "Chord" => {
                        let mut polygon = points.clone();
                        if name == "Pie" {
                            polygon.push((cx.round() as i32, cy.round() as i32));
                        }
                        if !dctx.fill.is_null() {
                            let spans = gfx::polygon_spans(&polygon, false);
                            target.fill_spans(
                                &spans,
                                &dctx.fill,
                                dctx.attrs.rop2,
                                dctx.brush_origin(),
                                dctx.mono,
                            );
                        }
                        stroke_polyline(target, dctx, &polygon, true);
                    }
                    "ArcTo" => {
                        let from = dctx.pt(dctx.attrs.pos.0, dctx.attrs.pos.1);
                        if let Some(first) = points.first() {
                            gfx::stroke_line(target, &dctx.stroke, from, *first, false);
                        }
                        stroke_polyline(target, dctx, &points, false);
                    }
                    _ => stroke_polyline(target, dctx, &points, false),
                }
            });
            if name == "ArcTo" {
                let attrs = ctx
                    .shared
                    .borrow()
                    .gdi
                    .dcs
                    .get(&args[0])
                    .map(|dc| dc.attrs.clone())
                    .unwrap_or_default();
                let origin = {
                    let shared = ctx.shared.borrow();
                    resolve(&shared, args[0])
                        .map(|r| r.origin)
                        .unwrap_or((0, 0))
                };
                update_pos(
                    ctx,
                    args[0],
                    attrs.dp_to_lp(end_point.0 - origin.0, end_point.1 - origin.1),
                );
            }
            ok(1)
        }
        "SetPixel" | "SetPixelV" => {
            let color = args[3];
            let mut result = u32::MAX;
            with_dc(ctx, memory, args[0], |target, dctx| {
                let (x, y) = dctx.pt(a(1), a(2));
                let rgb = {
                    if color >> 24 == 0 {
                        colorref_to_rgb(color)
                    } else {
                        dctx.mono.foreground
                    }
                };
                target.plot(x, y, rgb, gfx::R2_COPYPEN);
                result = rgb_to_colorref(target.surface.get(x, y).unwrap_or(rgb));
            });
            ok(if name == "SetPixelV" { 1 } else { result })
        }
        "GetPixel" => {
            let mut result = u32::MAX;
            with_dc(ctx, memory, args[0], |target, dctx| {
                let (x, y) = dctx.pt(a(1), a(2));
                if target.clip.contains(x, y) {
                    if let Some(pixel) = target.surface.get(x, y) {
                        result = rgb_to_colorref(pixel);
                    }
                }
            });
            ok(result)
        }
        "ExtFloodFill" | "FloodFill" => {
            let surface_mode = name == "ExtFloodFill" && args[4] == 1;
            let color = args[3];
            with_dc(ctx, memory, args[0], |target, dctx| {
                let (x, y) = dctx.pt(a(1), a(2));
                let rgb = colorref_to_rgb(color & 0xFF_FFFF);
                let rgb = if target.surface.is_mono() {
                    if rgb == 0xFFFFFF {
                        rgb
                    } else {
                        0
                    }
                } else {
                    rgb
                };
                let spans = gfx::flood_spans(target.surface, target.clip, x, y, rgb, surface_mode);
                target.fill_spans(
                    &spans,
                    &dctx.fill,
                    gfx::R2_COPYPEN,
                    dctx.brush_origin(),
                    dctx.mono,
                );
            });
            ok(1)
        }
        "FillRgn" => {
            let region = match ctx.shared.borrow().gdi.objects.get(&args[1]) {
                Some(GdiObject::Region(region)) => region.clone(),
                _ => return ok(0),
            };
            let brush = args[2];
            let fill = {
                let shared = ctx.shared.borrow();
                let attrs = shared
                    .gdi
                    .dcs
                    .get(&args[0])
                    .map(|dc| dc.attrs.clone())
                    .unwrap_or_default();
                shared.gdi.fill_of(&attrs, brush)
            };
            with_dc(ctx, memory, args[0], |target, dctx| {
                for rect in &region.rects {
                    let device = dctx.rect(rect.left, rect.top, rect.right, rect.bottom);
                    target.fill_rect(
                        device,
                        &fill,
                        gfx::R2_COPYPEN,
                        dctx.brush_origin(),
                        dctx.mono,
                    );
                }
            });
            ok(1)
        }
        "PatBlt" => {
            let rop = (args[5] >> 16) as u8;
            with_dc(ctx, memory, args[0], |target, dctx| {
                if std::env::var_os("OTW_TRACE_GDI").is_some() {
                    eprintln!(
                        "[gdi] PatBlt {:?} rop={rop:#x} fill={:?} origin={:?}",
                        (a(1), a(2), a(3), a(4)),
                        dctx.fill,
                        dctx.origin
                    );
                }
                let (x, y) = dctx.pt(a(1), a(2));
                let (w, h) = (dctx.attrs.scale_x(a(3)), dctx.attrs.scale_x(a(4)));
                let rect = Rect::new(x, y, x + w, y + h).normalized();
                target.pat_blt(rect, &dctx.fill, rop, dctx.brush_origin(), dctx.mono);
            });
            ok(1)
        }
        "BitBlt" | "StretchBlt" => {
            let (hdc, x, y, w, h, src_dc) = (args[0], a(1), a(2), a(3), a(4), args[5]);
            let (sx, sy, sw, sh, rop) = if name == "BitBlt" {
                (a(6), a(7), w, h, args[8])
            } else {
                (a(6), a(7), a(8), a(9), args[10])
            };
            blit_dcs(
                ctx,
                memory,
                hdc,
                (x, y, w, h),
                src_dc,
                (sx, sy, sw, sh),
                (rop >> 16) as u8,
            );
            ok(1)
        }
        "StretchDIBits" | "SetDIBitsToDevice" => {
            let (hdc, x, y, w, h) = (args[0], a(1), a(2), a(3), a(4));
            let (sx, sy, sw, sh, bits_va, info_va, usage, rop) = if name == "StretchDIBits" {
                (
                    a(5),
                    a(6),
                    a(7),
                    a(8),
                    args[9],
                    args[10],
                    args[11],
                    args[12],
                )
            } else {
                (a(5), a(6), w, h, args[9], args[10], args[11], 0x00CC_0020)
            };
            let palette = {
                let shared = ctx.shared.borrow();
                let attrs = shared
                    .gdi
                    .dcs
                    .get(&hdc)
                    .map(|dc| dc.attrs.clone())
                    .unwrap_or_default();
                shared.gdi.palette_of(&attrs)
            };
            let Some(info) = read_dib_info(memory, info_va, usage, &palette) else {
                return ok(0);
            };
            let size = if info.compression == 1 || info.compression == 2 {
                info.size_image as usize
            } else {
                info.image_size()
            };
            let bits = memory.read_bytes(bits_va, size).unwrap_or_default();
            let mut source = Surface::new(info.width.abs(), info.height.abs(), 24);
            decode_dib(
                &info,
                &bits,
                &mut source,
                0,
                info.height.unsigned_abs() as usize,
            );
            if let Some(dir) = std::env::var_os("OTW_DUMP_DIBS") {
                // Debugging: each picture drawn, as a PPM file.
                static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!("[gdi] {name} #{n}: dest ({x},{y} {w}x{h}) src ({sx},{sy} {sw}x{sh}) dib {}x{} {}bpp", info.width, info.height, info.bpp);
                let (width, height) = (info.width.abs(), info.height.abs());
                let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
                for row in 0..height {
                    for column in 0..width {
                        let c = source.get(column, row).unwrap_or_default();
                        ppm.extend_from_slice(&[(c >> 16) as u8, (c >> 8) as u8, c as u8]);
                    }
                }
                let _ = std::fs::write(std::path::Path::new(&dir).join(format!("dib{n:04}.ppm")), ppm);
            }
            // DIB source coordinates are bottom-up for positive heights.
            let sy = if info.height > 0 && name == "StretchDIBits" {
                info.height - sy - sh
            } else if info.height > 0 {
                info.height - sy - sh
            } else {
                sy
            };
            let lines = h.abs();
            with_dc(ctx, memory, hdc, |target, dctx| {
                let (dx, dy) = dctx.pt(x, y);
                let (dw, dh) = (dctx.attrs.scale_x(w), dctx.attrs.scale_x(h));
                let fill = dctx.fill.clone();
                let params = BlitParams {
                    dest: Rect::new(dx, dy, dx + dw, dy + dh),
                    src_x: sx,
                    src_y: sy,
                    src_width: sw,
                    src_height: sh,
                    rop: (rop >> 16) as u8,
                    fill: &fill,
                    origin: dctx.brush_origin(),
                    dest_colors: dctx.mono,
                    src_background: 0xFFFFFF,
                };
                gfx::blit(target.surface, target.clip, Some(&source), &params);
            });
            ok(lines as u32)
        }
        "SetDIBits" => {
            let (hdc, hbm, start, lines, bits_va, info_va, usage) = (
                args[0], args[1], args[2], args[3], args[4], args[5], args[6],
            );
            let palette = {
                let shared = ctx.shared.borrow();
                let attrs = shared
                    .gdi
                    .dcs
                    .get(&hdc)
                    .map(|dc| dc.attrs.clone())
                    .unwrap_or_default();
                shared.gdi.palette_of(&attrs)
            };
            let Some(info) = read_dib_info(memory, info_va, usage, &palette) else {
                return ok(0);
            };
            let size = if info.compression == 1 || info.compression == 2 {
                info.size_image as usize
            } else {
                info.stride() * lines as usize
            };
            let bits = memory.read_bytes(bits_va, size).unwrap_or_default();
            let mut shared = ctx.shared.borrow_mut();
            let shared = &mut *shared;
            sync_in(memory, shared, hbm);
            if let Some(GdiObject::Bitmap(bitmap)) = shared.gdi.objects.get_mut(&hbm) {
                decode_dib(
                    &info,
                    &bits,
                    &mut bitmap.surface,
                    start as usize,
                    lines as usize,
                );
            } else {
                return ok(0);
            }
            sync_out(memory, shared, hbm);
            ok(lines)
        }
        "GetDIBits" => {
            let (hdc, hbm, start, lines, bits_va, info_va, usage) = (
                args[0], args[1], args[2], args[3], args[4], args[5], args[6],
            );
            {
                let mut shared = ctx.shared.borrow_mut();
                sync_in(memory, &mut shared, hbm);
            }
            let surface = match ctx.shared.borrow().gdi.objects.get(&hbm) {
                Some(GdiObject::Bitmap(bitmap)) => bitmap.surface.clone(),
                _ => return ok(0),
            };
            let _ = hdc;
            let header_size = memory.read_u32(info_va).unwrap_or(40);
            let requested_bpp = read_u16(memory, info_va + 14);
            if bits_va == 0 && requested_bpp == 0 {
                // Fill in the header describing the bitmap.
                let bpp: u16 = if surface.is_mono() { 1 } else { 24 };
                let stride = ((surface.width as u32 * bpp as u32 + 31) / 32) * 4;
                let mut header = Vec::with_capacity(40);
                for v in [header_size, surface.width as u32, surface.height as u32] {
                    header.extend_from_slice(&v.to_le_bytes());
                }
                header.extend_from_slice(&1u16.to_le_bytes());
                header.extend_from_slice(&bpp.to_le_bytes());
                for v in [0u32, stride * surface.height as u32, 0, 0, 0, 0] {
                    header.extend_from_slice(&v.to_le_bytes());
                }
                let _ = memory.write_bytes(info_va, &header[..(header_size as usize).min(40)]);
                return ok(1);
            }
            let bpp = if requested_bpp == 0 {
                24
            } else {
                requested_bpp
            };
            let width = read_i32(memory, info_va + 4);
            let height = read_i32(memory, info_va + 8);
            let compression = memory.read_u32(info_va + 16).unwrap_or(0);
            let colors = match bpp {
                1 => {
                    if surface.is_mono() {
                        vec![0x000000, 0xFFFFFF]
                    } else {
                        // Existing table supplied by caller is honoured when non-trivial.
                        vec![0x000000, 0xFFFFFF]
                    }
                }
                4 => vga_palette(),
                8 => halftone_palette(),
                _ => Vec::new(),
            };
            let table_va = info_va + header_size;
            // Write the colour table.
            if bpp <= 8 {
                let mut table = Vec::new();
                for (index, color) in colors.iter().enumerate() {
                    if usage == 1 {
                        table.extend_from_slice(&(index as u16).to_le_bytes());
                    } else {
                        table.extend_from_slice(&[
                            *color as u8,
                            (color >> 8) as u8,
                            (color >> 16) as u8,
                            0,
                        ]);
                    }
                }
                let _ = memory.write_bytes(table_va, &table);
            }
            let info = DibInfo {
                width: if width == 0 { surface.width } else { width },
                height: if height == 0 { surface.height } else { height },
                bpp,
                compression: if compression == 3 { 3 } else { 0 },
                size_image: 0,
                header_size,
                colors: colors.clone(),
                color_count: colors.len(),
                masks: None,
                table_va,
            };
            // Update biSizeImage / biClrUsed.
            let _ = memory.write_u32(info_va + 20, info.image_size() as u32);
            if bits_va == 0 {
                return ok(lines);
            }
            let bits = encode_dib(&info, &surface, start as usize, lines as usize);
            let _ = memory.write_bytes(bits_va, &bits);
            ok(lines.min(info.height.unsigned_abs().saturating_sub(start)))
        }
        "GetDIBColorTable" | "SetDIBColorTable" => {
            let (hdc, start, entries, table) =
                (args[0], args[1] as usize, args[2] as usize, args[3]);
            let bitmap = ctx
                .shared
                .borrow()
                .gdi
                .dcs
                .get(&hdc)
                .map(|dc| dc.attrs.bitmap);
            let Some(bitmap) = bitmap else {
                return ok(0);
            };
            if name == "GetDIBColorTable" {
                let colors = match ctx.shared.borrow().gdi.objects.get(&bitmap) {
                    Some(GdiObject::Bitmap(Bitmap {
                        dib: Some(section), ..
                    })) => section.colors.clone(),
                    _ => return ok(0),
                };
                let end = (start + entries).min(colors.len());
                let mut bytes = Vec::new();
                for color in colors.iter().take(end).skip(start) {
                    bytes.extend_from_slice(&[
                        *color as u8,
                        (color >> 8) as u8,
                        (color >> 16) as u8,
                        0,
                    ]);
                }
                let _ = memory.write_bytes(table, &bytes);
                ok(end.saturating_sub(start) as u32)
            } else {
                let bytes = memory.read_bytes(table, entries * 4).unwrap_or_default();
                let mut shared = ctx.shared.borrow_mut();
                let shared = &mut *shared;
                let mut changed = 0;
                if let Some(GdiObject::Bitmap(Bitmap {
                    dib: Some(section), ..
                })) = shared.gdi.objects.get_mut(&bitmap)
                {
                    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
                        if let Some(slot) = section.colors.get_mut(start + index) {
                            *slot =
                                (chunk[2] as u32) << 16 | (chunk[1] as u32) << 8 | chunk[0] as u32;
                            changed += 1;
                        }
                    }
                }
                sync_in(memory, shared, bitmap);
                ok(changed)
            }
        }

        // ── Text ──
        "TextOutA" | "ExtTextOutA" | "ExtTextOutW" => {
            let (hdc, x, y) = (args[0], a(1), a(2));
            let (options, rect_va, text_va, length, dx_va) = if name == "TextOutA" {
                (0, 0, args[3], args[4], 0)
            } else {
                (args[3], args[4], args[5], args[6], args[7])
            };
            let length = length.min(4096) as usize;
            let text: Vec<u8> = if name == "ExtTextOutW" {
                memory
                    .read_bytes(text_va, length * 2)
                    .unwrap_or_default()
                    .chunks_exact(2)
                    .map(|c| if c[1] == 0 { c[0] } else { b'?' })
                    .collect()
            } else {
                memory.read_bytes(text_va, length).unwrap_or_default()
            };
            let rect = read_rect(memory, rect_va);
            let dx: Option<Vec<i32>> = if dx_va != 0 {
                memory.read_bytes(dx_va, length * 4).map(|bytes| {
                    bytes
                        .chunks_exact(4)
                        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect()
                })
            } else {
                None
            };
            let mut new_pos = None;
            if std::env::var_os("OTW_TRACE_GDI").is_some() {
                eprintln!("[gdi] {name} hdc={hdc:#x} at ({x},{y}) opts={options:#x} rect={rect:?} text={:?}", String::from_utf8_lossy(&text));
            }
            with_dc(ctx, memory, hdc, |target, dctx| {
                if std::env::var_os("OTW_TRACE_GDI").is_some() {
                    eprintln!(
                        "[gdi]   origin={:?} bk={:06x} fg={:06x} mode={} clip={:?}",
                        dctx.origin,
                        dctx.bk_color(),
                        dctx.text_color(),
                        dctx.attrs.bk_mode,
                        target.clip.bounds()
                    );
                }
                let align = dctx.attrs.text_align;
                let (lx, ly) = if align & 1 != 0 {
                    dctx.attrs.pos
                } else {
                    (x, y)
                };
                let (mut sx, mut sy) = dctx.pt(lx, ly);
                let face = dctx.face;
                let width = match &dx {
                    Some(widths) => widths.iter().sum(),
                    None => face.text_width(&text) + dctx.attrs.char_extra * text.len() as i32,
                };
                match align & 6 {
                    2 => sx -= width,
                    6 => sx -= width / 2,
                    _ => {}
                }
                match align & 24 {
                    8 => sy -= face.height,
                    24 => sy -= face.ascent,
                    _ => {}
                }
                let opaque = rect.filter(|_| options & 2 != 0);
                let clip_rect = rect.filter(|_| options & 4 != 0);
                if let Some(r) = opaque {
                    let device = dctx.rect(r.left, r.top, r.right, r.bottom);
                    target.fill_rect(
                        device,
                        &Fill::Solid(dctx.bk_color()),
                        gfx::R2_COPYPEN,
                        (0, 0),
                        dctx.mono,
                    );
                }
                let clip = match clip_rect {
                    Some(r) => target
                        .clip
                        .intersect_rect(&dctx.rect(r.left, r.top, r.right, r.bottom)),
                    None => target.clip.clone(),
                };
                let mut sub = Target {
                    surface: target.surface,
                    clip: &clip,
                };
                text_run(&mut sub, dctx, sx, sy, &text, dx.as_deref(), opaque);
                if align & 1 != 0 {
                    new_pos = Some((lx + width, ly));
                }
            });
            if let Some(pos) = new_pos {
                update_pos(ctx, hdc, pos);
            }
            ok(1)
        }
        "GetTextExtentPointA" | "GetTextExtentPoint32A" => {
            let text = memory
                .read_bytes(args[1], (args[2] as usize).min(4096))
                .unwrap_or_default();
            let (w, h) = text_extent(ctx, args[0], &text);
            write_point(memory, args[3], (w, h));
            ok(1)
        }
        "GetTextMetricsA" => {
            let font = {
                let shared = ctx.shared.borrow();
                let attrs = shared
                    .gdi
                    .dcs
                    .get(&args[0])
                    .map(|dc| dc.attrs.clone())
                    .unwrap_or_default();
                shared.gdi.font(attrs.font)
            };
            let metrics = textmetric_bytes(&font);
            if std::env::var_os("OTW_TRACE_GDI").is_some() {
                let height = i32::from_le_bytes([metrics[0], metrics[1], metrics[2], metrics[3]]);
                eprintln!("[gdi] GetTextMetrics dc={:#x} face={:?} height={height}", args[0], String::from_utf8_lossy(&font.face));
            }
            let _ = memory.write_bytes(args[1], &metrics);
            ok(1)
        }
        "GetTextFaceA" => {
            let font = {
                let shared = ctx.shared.borrow();
                let attrs = shared
                    .gdi
                    .dcs
                    .get(&args[0])
                    .map(|dc| dc.attrs.clone())
                    .unwrap_or_default();
                shared.gdi.font(attrs.font)
            };
            let mut face = font.face.clone();
            if args[2] != 0 {
                face.truncate((args[1] as usize).saturating_sub(1));
                let _ = write_cstr(memory, args[2], &face);
            }
            ok(face.len() as u32 + 1)
        }
        "GetCharWidthA" => {
            let face = {
                let shared = ctx.shared.borrow();
                let attrs = shared
                    .gdi
                    .dcs
                    .get(&args[0])
                    .map(|dc| dc.attrs.clone())
                    .unwrap_or_default();
                shared.gdi.font(attrs.font).face()
            };
            let mut bytes = Vec::new();
            for code in args[1]..=args[2].min(args[1] + 1024) {
                bytes.extend_from_slice(&face.advance(code.min(255) as u8).to_le_bytes());
            }
            let _ = memory.write_bytes(args[3], &bytes);
            ok(1)
        }
        "GetTextCharset" => ok(0),
        "EnumFontFamiliesA" | "EnumFontFamiliesExA" => {
            let filter = if name == "EnumFontFamiliesA" && args[1] != 0 {
                Some(read_cstr(memory, args[1]))
            } else if name == "EnumFontFamiliesExA" && args[1] != 0 {
                let face = memory.read_bytes(args[1] + 28, 32).unwrap_or_default();
                let end = face.iter().position(|b| *b == 0).unwrap_or(face.len());
                Some(face[..end].to_vec())
            } else {
                None
            };
            ok(enum_font_families(ctx, memory, filter, args[2], args[3], false))
        }

        // ── Printing ──
        "StartDocA" => ok(u32::MAX),
        "EndDoc" | "StartPage" | "EndPage" | "AbortDoc" | "SetAbortProc" => ok(0),
        "Escape" => ok(0),
        _ => None,
    }
}

/// `BitBlt`/`StretchBlt` between two DCs.
fn blit_dcs<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    hdc: u32,
    dest: (i32, i32, i32, i32),
    src_dc: u32,
    src: (i32, i32, i32, i32),
    rop: u8,
) {
    let mut shared = ctx.shared.borrow_mut();
    let shared = &mut *shared;
    let Some(dst) = resolve(shared, hdc) else {
        return;
    };
    let source_info = if gfx::rop_uses_source(rop) {
        resolve(shared, src_dc)
    } else {
        None
    };
    if gfx::rop_uses_source(rop) && source_info.is_none() {
        return;
    }
    for handle in [dst.dib, source_info.as_ref().and_then(|s| s.dib)]
        .into_iter()
        .flatten()
    {
        sync_in(memory, shared, handle);
    }
    let (x, y, w, h) = dest;
    let (dx, dy) = dst.ctx.pt(x, y);
    let (dw, dh) = (dst.ctx.attrs.scale_x(w), dst.ctx.attrs.scale_x(h));
    // Source surface: clone when it is the destination surface.
    let source_surface = source_info.as_ref().map(|s| {
        let surface = match s.key {
            SurfaceKey::Window(top) => shared.gdi.surfaces.get(&top).cloned(),
            SurfaceKey::Bitmap(handle) => shared.gdi.bitmap_surface(handle).cloned(),
            SurfaceKey::Screen => Some(shared.gdi.screen.clone()),
        };
        (surface.unwrap_or_else(|| Surface::new(0, 0, 24)), s)
    });
    let Some(mut surface) = take_surface(shared, dst.key) else {
        return;
    };
    let (src_surface, src_x, src_y, src_w, src_h, src_bk) = match &source_surface {
        Some((surface, info)) => {
            let (sx, sy) = info.ctx.pt(src.0, src.1);
            let sw = info.ctx.attrs.scale_x(src.2);
            let sh = info.ctx.attrs.scale_x(src.3);
            (Some(surface), sx, sy, sw, sh, info.ctx.mono.background)
        }
        None => (None, 0, 0, src.2, src.3, 0xFFFFFF),
    };
    let fill = dst.ctx.fill.clone();
    let params = BlitParams {
        dest: Rect::new(dx, dy, dx + dw, dy + dh),
        src_x,
        src_y,
        src_width: src_w,
        src_height: src_h,
        rop,
        fill: &fill,
        origin: dst.ctx.brush_origin(),
        dest_colors: dst.ctx.mono,
        src_background: src_bk,
    };
    gfx::blit(&mut surface, &dst.clip, src_surface, &params);
    put_surface(shared, dst.key, surface);
    if let Some(handle) = dst.dib {
        sync_out(memory, shared, handle);
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    #[test]
    fn dib_round_trip_24bpp() {
        let info = DibInfo {
            width: 3,
            height: 2,
            bpp: 24,
            compression: 0,
            size_image: 0,
            header_size: 40,
            colors: Vec::new(),
            color_count: 0,
            masks: None,
            table_va: 0,
        };
        let mut surface = Surface::new(3, 2, 24);
        surface.put(0, 0, 0x112233);
        surface.put(2, 1, 0xABCDEF);
        let bits = encode_dib(&info, &surface, 0, 2);
        assert_eq!(bits.len(), 24);
        let mut decoded = Surface::new(3, 2, 24);
        decode_dib(&info, &bits, &mut decoded, 0, 2);
        assert_eq!(decoded.get(0, 0), Some(0x112233));
        assert_eq!(decoded.get(2, 1), Some(0xABCDEF));
    }

    #[test]
    fn anisotropic_mapping_scales() {
        let attrs = DcAttrs {
            map_mode: 8,
            window_ext: (100, 100),
            viewport_ext: (200, 50),
            ..DcAttrs::default()
        };
        assert_eq!(attrs.lp_to_dp(10, 10), (20, 5));
        assert_eq!(attrs.dp_to_lp(20, 5), (10, 10));
    }
}
