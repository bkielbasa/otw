//! DirectWrite (`DWrite.dll`): the font and glyph rendering API current
//! Windows programs draw text with, over the host's TrueType fonts
//! (`win32::fonts`). Served: the factory, the system font collection with
//! its families, fonts and localized names, font faces (metrics, glyph
//! lookup, font tables), font files with the local loader and streams,
//! rendering parameters, glyph-run analyses that draw glyphs into alpha
//! textures, and GDI interop for LOGFONTs. Interfaces added after Windows
//! 7 (IDWriteFactory1 and on) are not: programs fall back without them.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::collections::HashMap;

use crate::win32::{
    fonts::{self, FontDb},
    unicode, ApiContext, ApiOutcome, GuestMemory,
};

const S_OK: u64 = 0;
const E_NOTIMPL: u64 = 0x8000_4001;
const E_NOINTERFACE: u64 = 0x8000_4002;
const E_INVALIDARG: u64 = 0x8007_0057;
const E_FAIL: u64 = 0x8000_4005;
const E_NOT_SUFFICIENT_BUFFER: u64 = 0x8007_007A;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Factory,
    Collection,
    Family,
    FontList,
    Font,
    Strings,
    Face,
    File,
    Loader,
    Stream,
    Params,
    Analysis,
    Interop,
}

const UNKNOWN: [&str; 3] = ["QueryInterface", "AddRef", "Release"];

fn methods(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Factory => &[
            "GetSystemFontCollection",
            "CreateCustomFontCollection",
            "RegisterFontCollectionLoader",
            "UnregisterFontCollectionLoader",
            "CreateFontFileReference",
            "CreateCustomFontFileReference",
            "CreateFontFace",
            "CreateRenderingParams",
            "CreateMonitorRenderingParams",
            "CreateCustomRenderingParams",
            "RegisterFontFileLoader",
            "UnregisterFontFileLoader",
            "CreateTextFormat",
            "CreateTypography",
            "GetGdiInterop",
            "CreateTextLayout",
            "CreateGdiCompatibleTextLayout",
            "CreateEllipsisTrimmingSign",
            "CreateTextAnalyzer",
            "CreateNumberSubstitution",
            "CreateGlyphRunAnalysis",
        ],
        Kind::Collection => &["GetFontFamilyCount", "GetFontFamily", "FindFamilyName", "GetFontFromFontFace"],
        Kind::FontList => &["GetFontCollection", "GetFontCount", "GetFont"],
        Kind::Family => {
            &["GetFontCollection", "GetFontCount", "GetFont", "GetFamilyNames", "GetFirstMatchingFont", "GetMatchingFonts"]
        }
        Kind::Font => &[
            "GetFontFamily",
            "GetWeight",
            "GetStretch",
            "GetStyle",
            "IsSymbolFont",
            "GetFaceNames",
            "GetInformationalStrings",
            "GetSimulations",
            "GetMetrics",
            "HasCharacter",
            "CreateFontFace",
        ],
        Kind::Strings => {
            &["GetCount", "FindLocaleName", "GetLocaleNameLength", "GetLocaleName", "GetStringLength", "GetString"]
        }
        Kind::Face => &[
            "GetType",
            "GetFiles",
            "GetIndex",
            "GetSimulations",
            "IsSymbolFont",
            "GetMetrics",
            "GetGlyphCount",
            "GetDesignGlyphMetrics",
            "GetGlyphIndices",
            "TryGetFontTable",
            "ReleaseFontTable",
            "GetGlyphRunOutline",
            "GetRecommendedRenderingMode",
            "GetGdiCompatibleMetrics",
            "GetGdiCompatibleGlyphMetrics",
        ],
        Kind::File => &["GetReferenceKey", "GetLoader", "Analyze"],
        Kind::Loader => &["CreateStreamFromKey", "GetFilePathLengthFromKey", "GetFilePathFromKey", "GetLastWriteTimeFromKey"],
        Kind::Stream => &["ReadFileFragment", "ReleaseFileFragment", "GetFileSize", "GetLastWriteTime"],
        Kind::Params => &["GetGamma", "GetEnhancedContrast", "GetClearTypeLevel", "GetPixelGeometry", "GetRenderingMode"],
        Kind::Analysis => &["GetAlphaTextureBounds", "CreateAlphaTexture", "GetAlphaBlendParams"],
        Kind::Interop => &[
            "CreateFontFromLOGFONT",
            "ConvertFontToLOGFONT",
            "ConvertFontFaceToLOGFONT",
            "CreateFontFaceFromHdc",
            "CreateBitmapRenderTarget",
        ],
    }
}

const KINDS: [Kind; 13] = [
    Kind::Factory,
    Kind::Collection,
    Kind::Family,
    Kind::FontList,
    Kind::Font,
    Kind::Strings,
    Kind::Face,
    Kind::File,
    Kind::Loader,
    Kind::Stream,
    Kind::Params,
    Kind::Analysis,
    Kind::Interop,
];

fn prefix(kind: Kind) -> String {
    format!("DW_{kind:?}_")
}

/// Every method's entry-point name, for the thunk table.
pub fn entry_names() -> Vec<String> {
    let mut names = vec!["DWriteCreateFactory".to_string()];
    for kind in KINDS {
        for method in UNKNOWN.iter().chain(methods(kind)) {
            names.push(format!("{}{method}", prefix(kind)));
        }
    }
    names
}

/// The interfaces each object answers QueryInterface for (beyond
/// IUnknown): GUIDs as written.
fn interfaces(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Factory => &["b859ee5a-d838-4b5b-a2e8-1adc7d93db48"],
        Kind::Collection => &["a84cee02-3eea-4eee-a827-87c1a02a0fcc"],
        Kind::Family => &["da20d8ef-812a-4c43-9802-62ec4abd7add", "1a0d8438-1d97-4ec1-aef9-a2fb86ed6acb"],
        Kind::FontList => &["1a0d8438-1d97-4ec1-aef9-a2fb86ed6acb"],
        Kind::Font => &["acd16696-8c14-4f5d-877e-fe3fc1d32737"],
        Kind::Strings => &["08eb5d8f-0ae8-4d6e-a1b2-2f21b72c7c5b"],
        Kind::Face => &["5f49804d-7024-4d43-bfa9-d25984f53849"],
        Kind::File => &["739d886a-cef5-47dc-8769-1a8b41bebbb0"],
        Kind::Loader => &["b2d9f3ec-c9fe-4a11-a2ec-d86208f7c0a2", "727cad4e-d6af-4c9e-8a08-d695b11caa49"],
        Kind::Stream => &["6d4865fe-0ab8-4d91-8f62-5dd6be34a3e0"],
        Kind::Params => &["2f0da53a-2add-47cd-82ee-d9ec34688e75"],
        Kind::Analysis => &["7d97dbf7-e085-42d4-81e3-6a883bded118"],
        Kind::Interop => &["1edd9491-9853-4299-898f-6432983b6f3a"],
    }
}

fn guid_bytes(text: &str) -> [u8; 16] {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    let byte = |i: usize| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    let mut out = [0u8; 16];
    // Data1, Data2, Data3 little-endian; Data4 as written.
    out[..4].copy_from_slice(&[byte(3), byte(2), byte(1), byte(0)]);
    out[4..6].copy_from_slice(&[byte(5), byte(4)]);
    out[6..8].copy_from_slice(&[byte(7), byte(6)]);
    for i in 8..16 {
        out[i] = byte(i);
    }
    out
}

#[derive(Clone, Debug)]
enum Object {
    Factory,
    Collection,
    Family(usize),
    FontList(Vec<usize>),
    Font(usize),
    Strings(Vec<(String, String)>),
    Face(usize, u32),
    File(usize),
    Loader,
    Stream(usize),
    Params { gamma: f32, contrast: f32, cleartype: f32, geometry: u32, mode: u32 },
    Analysis { rendered: fonts::Rendered, aliased: bool },
    Interop,
}

impl Object {
    fn kind(&self) -> Kind {
        match self {
            Object::Factory => Kind::Factory,
            Object::Collection => Kind::Collection,
            Object::Family(_) => Kind::Family,
            Object::FontList(_) => Kind::FontList,
            Object::Font(_) => Kind::Font,
            Object::Strings(_) => Kind::Strings,
            Object::Face(..) => Kind::Face,
            Object::File(_) => Kind::File,
            Object::Loader => Kind::Loader,
            Object::Stream(_) => Kind::Stream,
            Object::Params { .. } => Kind::Params,
            Object::Analysis { .. } => Kind::Analysis,
            Object::Interop => Kind::Interop,
        }
    }
}

#[derive(Default)]
pub struct State {
    objects: HashMap<u32, (Object, u32)>,
    vtables: HashMap<Kind, u32>,
    singletons: HashMap<Kind, u32>,
    faces: HashMap<(usize, u32), u32>,
    /// Font files copied into guest memory for streams.
    file_views: HashMap<usize, u32>,
    /// Font tables handed out (TryGetFontTable), freed on release.
    tables: HashMap<u32, u32>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dwrite::State({} objects)", self.objects.len())
    }
}

fn trace() -> bool {
    std::env::var_os("OTW_TRACE_DWRITE").is_some()
}

pub fn arg_count(name: &str) -> Option<usize> {
    if name == "DWriteCreateFactory" {
        return Some(3);
    }
    name.starts_with("DW_").then_some(10)
}

fn vtable(ctx: &ApiContext, memory: &mut dyn GuestMemory, kind: Kind) -> u32 {
    if let Some(&table) = ctx.shared.borrow().dwrite.vtables.get(&kind) {
        return table;
    }
    let names: Vec<String> = UNKNOWN.iter().chain(methods(kind)).map(|m| format!("{}{m}", prefix(kind))).collect();
    let table = ctx.shared.borrow_mut().heap.alloc(8 * names.len() as u32).unwrap_or(0);
    for (index, name) in names.iter().enumerate() {
        let _ = memory.write_u64(table + 8 * index as u32, ctx.builtin_proc(name) as u64);
    }
    ctx.shared.borrow_mut().dwrite.vtables.insert(kind, table);
    table
}

fn new_object(ctx: &ApiContext, memory: &mut dyn GuestMemory, object: Object) -> u32 {
    let kind = object.kind();
    let table = vtable(ctx, memory, kind);
    let va = ctx.shared.borrow_mut().heap.alloc(16).unwrap_or(0);
    let _ = memory.write_u64(va, table as u64);
    let _ = memory.write_u64(va + 8, 0);
    ctx.shared.borrow_mut().dwrite.objects.insert(va, (object, 1));
    va
}

fn singleton(ctx: &ApiContext, memory: &mut dyn GuestMemory, object: Object) -> u32 {
    let kind = object.kind();
    if let Some(&va) = ctx.shared.borrow().dwrite.singletons.get(&kind) {
        return va;
    }
    let va = new_object(ctx, memory, object);
    ctx.shared.borrow_mut().dwrite.singletons.insert(kind, va);
    va
}

fn face_object(ctx: &ApiContext, memory: &mut dyn GuestMemory, face: usize, simulations: u32) -> u32 {
    if let Some(&va) = ctx.shared.borrow().dwrite.faces.get(&(face, simulations)) {
        return va;
    }
    let va = new_object(ctx, memory, Object::Face(face, simulations));
    ctx.shared.borrow_mut().dwrite.faces.insert((face, simulations), va);
    va
}

fn object(ctx: &ApiContext, va: u32) -> Option<Object> {
    ctx.shared.borrow().dwrite.objects.get(&va).map(|(o, _)| o.clone())
}

fn put_ptr(memory: &mut dyn GuestMemory, at: u32, value: u32) {
    if at != 0 {
        let _ = memory.write_u64(at, value as u64);
    }
}

fn put32(memory: &mut dyn GuestMemory, at: u32, value: u32) {
    if at != 0 {
        let _ = memory.write_u32(at, value);
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn font_metrics(face: &fonts::Face) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(20);
    for value in [
        face.units_per_em,
        face.ascent,
        face.descent,
        face.line_gap as u16,
        face.cap_height,
        face.x_height,
        face.underline_position as u16,
        face.underline_thickness,
        face.strikeout_position as u16,
        face.strikeout_thickness,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn f32_at(value: u64) -> f32 {
    f32::from_bits(value as u32)
}

/// Copy a font file into guest memory once (for streams).
fn file_view(ctx: &ApiContext, memory: &mut dyn GuestMemory, db: &FontDb, file: usize) -> u32 {
    if let Some(&va) = ctx.shared.borrow().dwrite.file_views.get(&file) {
        return va;
    }
    let data = db.file_data(file);
    let va = memory.map_region(crate::kernel::memory::POOL_BASE, (data.len() as u32).max(1)).unwrap_or(0);
    if va != 0 {
        let _ = memory.write_bytes(va, &data);
    }
    ctx.shared.borrow_mut().dwrite.file_views.insert(file, va);
    va
}

/// LOGFONTW for a face: height, width, escapement, orientation, weight,
/// italic, underline, strike out, charset, precisions, quality, pitch,
/// face name.
fn logfont(memory: &mut dyn GuestMemory, at: u32, face: &fonts::Face, family: &str) {
    let mut bytes = vec![0u8; 92];
    bytes[0..4].copy_from_slice(&(-16i32).to_le_bytes());
    bytes[16..20].copy_from_slice(&face.weight.to_le_bytes());
    bytes[20] = u8::from(face.style != 0);
    bytes[23] = 1; // DEFAULT_CHARSET
    for (index, unit) in wide(family).into_iter().take(31).enumerate() {
        bytes[28 + index * 2..30 + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    let _ = memory.write_bytes(at, &bytes);
}

/// The family index a face belongs to (its first listing).
fn family_of(db: &FontDb, face: usize) -> usize {
    db.families.iter().position(|f| f.faces.contains(&face)).unwrap_or(0)
}

/// A glyph run analysis: the run's glyphs drawn into coverage.
#[allow(clippy::too_many_arguments)]
fn analyse(
    ctx: &ApiContext,
    memory: &mut dyn GuestMemory,
    db: &FontDb,
    run: u32,
    pixels_per_dip: f32,
    transform: u32,
    mode: u32,
    origin: (f32, f32),
) -> Option<Object> {
    // DWRITE_GLYPH_RUN: face, em size, count, indices, advances, offsets.
    let face_va = memory.read_u64(run)? as u32;
    let em = f32::from_bits(memory.read_u32(run + 8)?);
    let count = memory.read_u32(run + 12)? as usize;
    let (indices, advances, offsets) = (memory.read_u64(run + 16)? as u32, memory.read_u64(run + 24)? as u32, memory.read_u64(run + 32)? as u32);
    let Some(Object::Face(face, _)) = object(ctx, face_va) else { return None };
    let data = db.face_data(face);
    let m: [f32; 6] = if transform != 0 {
        let bytes = memory.read_bytes(transform, 24)?;
        std::array::from_fn(|i| f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap_or([0; 4])))
    } else {
        [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
    };
    let ppd = if pixels_per_dip > 0.0 { pixels_per_dip } else { 1.0 };
    let scale = em * ppd / data.units_per_em as f32;
    let mut glyphs = Vec::with_capacity(count);
    let mut pen = 0.0f32;
    for i in 0..count.min(4096) {
        let glyph = memory.read_bytes(indices + i as u32 * 2, 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
        let advance = if advances != 0 {
            f32::from_bits(memory.read_u32(advances + i as u32 * 4).unwrap_or(0))
        } else {
            data.glyph_metrics(glyph)[1] as f32 * em / data.units_per_em as f32
        };
        let (dx, dy) = if offsets != 0 {
            (f32::from_bits(memory.read_u32(offsets + i as u32 * 8).unwrap_or(0)), f32::from_bits(memory.read_u32(offsets + i as u32 * 8 + 4).unwrap_or(0)))
        } else {
            (0.0, 0.0)
        };
        // Position in DIPs, then pixels, then the transform.
        let (x, y) = ((origin.0 + pen + dx) * ppd, (origin.1 - dy) * ppd);
        let (tx, ty) = (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
        glyphs.push((glyph, tx, ty));
        pen += advance;
    }
    // Font units to pixels with the transform's 2×2 part (font y is up:
    // the y components are negated in `render`).
    let s = [scale * m[0], -scale * m[1], -scale * m[2], scale * m[3]];
    let rendered = fonts::render(&data, &glyphs, s);
    Some(Object::Analysis { rendered, aliased: mode == 1 })
}

pub fn call(ctx: &ApiContext, memory: &mut dyn GuestMemory, name: &str, a: &[u64]) -> Option<ApiOutcome> {
    let count = 10;
    let arg = |i: usize| a.get(i).copied().unwrap_or(0);
    let p = |i: usize| arg(i) as u32;
    let ok = |value: u64| Some(ApiOutcome::wide(value, count));
    let db = fonts::db();
    if name == "DWriteCreateFactory" {
        // (type, iid, &factory)
        let factory = singleton(ctx, memory, Object::Factory);
        put_ptr(memory, p(2), factory);
        return ok(S_OK);
    }
    let rest = name.strip_prefix("DW_")?;
    let (kind_name, method) = rest.split_once('_')?;
    let this = p(0);
    let Some(object_here) = object(ctx, this) else {
        return ok(E_INVALIDARG);
    };
    if trace() {
        eprintln!("[dwrite] {kind_name}.{method}");
    }
    let kind = object_here.kind();
    match method {
        "QueryInterface" => {
            let iid = memory.read_bytes(p(1), 16).unwrap_or_default();
            let known = iid == guid_bytes("00000000-0000-0000-c000-000000000046")
                || interfaces(kind).iter().any(|g| iid == guid_bytes(g));
            if known {
                put_ptr(memory, p(2), this);
                return ok(S_OK);
            }
            if trace() {
                eprintln!("[dwrite] {kind_name}.QueryInterface: no {iid:02x?}");
            }
            put_ptr(memory, p(2), 0);
            return ok(E_NOINTERFACE);
        }
        "AddRef" | "Release" => {
            let mut shared = ctx.shared.borrow_mut();
            let refs = shared.dwrite.objects.get_mut(&this).map(|(_, r)| {
                if method == "AddRef" {
                    *r += 1;
                } else {
                    *r = r.saturating_sub(1);
                }
                *r
            });
            return ok(refs.unwrap_or(0).max(1) as u64);
        }
        _ => {}
    }
    match (object_here, method) {
        // ── Factory ──
        (Object::Factory, "GetSystemFontCollection") => {
            let collection = singleton(ctx, memory, Object::Collection);
            put_ptr(memory, p(1), collection);
            ok(S_OK)
        }
        (Object::Factory, "CreateFontFileReference") => {
            // (path, &time, &file): a file of the fonts folder.
            let path = String::from_utf16_lossy(&unicode::read_wstr(memory, p(1)));
            let name = path.rsplit(['\\', '/']).next().unwrap_or("").to_uppercase();
            match db.files.iter().position(|f| f.guest_name == name) {
                Some(file) => {
                    let object = new_object(ctx, memory, Object::File(file));
                    put_ptr(memory, p(3), object);
                    ok(S_OK)
                }
                None => {
                    put_ptr(memory, p(3), 0);
                    ok(0x8898_500A) // DWRITE_E_FILENOTFOUND
                }
            }
        }
        (Object::Factory, "CreateFontFace") => {
            // (type, file count, files, index, simulations, &face)
            let first = memory.read_u64(p(3)).unwrap_or(0) as u32;
            let Some(Object::File(file)) = object(ctx, first) else { return ok(E_INVALIDARG) };
            match db.faces.iter().position(|f| f.file == file && f.index == p(4)) {
                Some(face) => {
                    let object = face_object(ctx, memory, face, p(5));
                    put_ptr(memory, p(6), object);
                    ok(S_OK)
                }
                None => ok(0x8898_5004), // DWRITE_E_FILEFORMAT
            }
        }
        (Object::Factory, "CreateRenderingParams" | "CreateMonitorRenderingParams") => {
            let out = if method == "CreateRenderingParams" { p(1) } else { p(2) };
            let params = new_object(ctx, memory, Object::Params { gamma: 1.8, contrast: 0.5, cleartype: 1.0, geometry: 1, mode: 5 });
            put_ptr(memory, out, params);
            ok(S_OK)
        }
        (Object::Factory, "CreateCustomRenderingParams") => {
            // (gamma, contrast, cleartype level: XMM1-3, geometry, mode, &params)
            let params = Object::Params {
                gamma: f32::from_bits(memory.xmm(1) as u32),
                contrast: f32::from_bits(memory.xmm(2) as u32),
                cleartype: f32::from_bits(memory.xmm(3) as u32),
                geometry: p(4),
                mode: p(5),
            };
            let object = new_object(ctx, memory, params);
            put_ptr(memory, p(6), object);
            ok(S_OK)
        }
        (Object::Factory, "RegisterFontFileLoader" | "UnregisterFontFileLoader" | "RegisterFontCollectionLoader" | "UnregisterFontCollectionLoader") => {
            ok(S_OK)
        }
        (Object::Factory, "GetGdiInterop") => {
            let interop = singleton(ctx, memory, Object::Interop);
            put_ptr(memory, p(1), interop);
            ok(S_OK)
        }
        (Object::Factory, "CreateGlyphRunAnalysis") => {
            // (run, pixels per dip: XMM2, transform, rendering mode,
            // measuring mode, origin x, origin y, &analysis)
            let ppd = f32::from_bits(memory.xmm(2) as u32);
            let origin = (f32_at(arg(6)), f32_at(arg(7)));
            match analyse(ctx, memory, db, p(1), ppd, p(3), p(4), origin) {
                Some(analysis) => {
                    let object = new_object(ctx, memory, analysis);
                    put_ptr(memory, p(8), object);
                    ok(S_OK)
                }
                None => ok(E_INVALIDARG),
            }
        }
        // ── Font collection ──
        (Object::Collection, "GetFontFamilyCount") => ok(db.families.len() as u64),
        (Object::Collection, "GetFontFamily") => {
            if p(1) as usize >= db.families.len() {
                return ok(E_INVALIDARG);
            }
            let family = new_object(ctx, memory, Object::Family(p(1) as usize));
            put_ptr(memory, p(2), family);
            ok(S_OK)
        }
        (Object::Collection, "FindFamilyName") => {
            let name = String::from_utf16_lossy(&unicode::read_wstr(memory, p(1)));
            match db.family_index(&name) {
                Some(index) => {
                    put32(memory, p(2), index as u32);
                    put32(memory, p(3), 1);
                }
                None => {
                    put32(memory, p(2), u32::MAX);
                    put32(memory, p(3), 0);
                }
            }
            ok(S_OK)
        }
        (Object::Collection, "GetFontFromFontFace") => match object(ctx, p(1)) {
            Some(Object::Face(face, _)) => {
                let font = new_object(ctx, memory, Object::Font(face));
                put_ptr(memory, p(2), font);
                ok(S_OK)
            }
            _ => ok(0x8898_5002), // DWRITE_E_NOFONT
        },
        // ── Families and lists ──
        (Object::Family(_) | Object::FontList(_), "GetFontCollection") => {
            let collection = singleton(ctx, memory, Object::Collection);
            put_ptr(memory, p(1), collection);
            ok(S_OK)
        }
        (Object::Family(family), "GetFontCount") => ok(db.families[family].faces.len() as u64),
        (Object::FontList(faces), "GetFontCount") => ok(faces.len() as u64),
        (Object::Family(family), "GetFont") => match db.families[family].faces.get(p(1) as usize) {
            Some(&face) => {
                let font = new_object(ctx, memory, Object::Font(face));
                put_ptr(memory, p(2), font);
                ok(S_OK)
            }
            None => ok(E_INVALIDARG),
        },
        (Object::FontList(faces), "GetFont") => match faces.get(p(1) as usize) {
            Some(&face) => {
                let font = new_object(ctx, memory, Object::Font(face));
                put_ptr(memory, p(2), font);
                ok(S_OK)
            }
            None => ok(E_INVALIDARG),
        },
        (Object::Family(family), "GetFamilyNames") => {
            let strings = new_object(ctx, memory, Object::Strings(vec![("en-us".into(), db.families[family].name.clone())]));
            put_ptr(memory, p(1), strings);
            ok(S_OK)
        }
        (Object::Family(family), "GetFirstMatchingFont") => match db.best_match(family, p(1), p(2), p(3)) {
            Some(face) => {
                let font = new_object(ctx, memory, Object::Font(face));
                put_ptr(memory, p(4), font);
                ok(S_OK)
            }
            None => ok(0x8898_5002),
        },
        (Object::Family(family), "GetMatchingFonts") => {
            let mut faces = db.families[family].faces.clone();
            let best = db.best_match(family, p(1), p(2), p(3));
            faces.sort_by_key(|&f| Some(f) != best);
            let list = new_object(ctx, memory, Object::FontList(faces));
            put_ptr(memory, p(4), list);
            ok(S_OK)
        }
        // ── Fonts ──
        (Object::Font(face), "GetFontFamily") => {
            let family = new_object(ctx, memory, Object::Family(family_of(db, face)));
            put_ptr(memory, p(1), family);
            ok(S_OK)
        }
        (Object::Font(face), "GetWeight") => ok(db.faces[face].weight as u64),
        (Object::Font(face), "GetStretch") => ok(db.faces[face].stretch as u64),
        (Object::Font(face), "GetStyle") => ok(db.faces[face].style as u64),
        (Object::Font(face) | Object::Face(face, _), "IsSymbolFont") => ok(u64::from(db.faces[face].symbol)),
        (Object::Font(face), "GetFaceNames") => {
            let strings = new_object(ctx, memory, Object::Strings(vec![("en-us".into(), db.faces[face].face_name.clone())]));
            put_ptr(memory, p(1), strings);
            ok(S_OK)
        }
        (Object::Font(face), "GetInformationalStrings") => {
            let info = &db.faces[face];
            let text = match p(1) {
                11 => Some(info.win_family.clone()),
                12 => Some(info.win_subfamily.clone()),
                13 => Some(info.family.clone()),
                14 => Some(info.face_name.clone()),
                16 => Some(info.full_name.clone()),
                17 => Some(info.postscript.clone()),
                _ => None,
            };
            match text {
                Some(text) => {
                    let strings = new_object(ctx, memory, Object::Strings(vec![("en-us".into(), text)]));
                    put_ptr(memory, p(2), strings);
                    put32(memory, p(3), 1);
                }
                None => {
                    put_ptr(memory, p(2), 0);
                    put32(memory, p(3), 0);
                }
            }
            ok(S_OK)
        }
        (Object::Font(_), "GetSimulations") => ok(0),
        (Object::Font(face), "GetMetrics") | (Object::Face(face, _), "GetMetrics") => {
            let _ = memory.write_bytes(p(1), &font_metrics(&db.faces[face]));
            ok(S_OK)
        }
        (Object::Font(face), "HasCharacter") => {
            let data = db.face_data(face);
            put32(memory, p(2), u32::from(data.glyph_index(p(1)) != 0));
            ok(S_OK)
        }
        (Object::Font(face), "CreateFontFace") => {
            let object = face_object(ctx, memory, face, 0);
            put_ptr(memory, p(1), object);
            ok(S_OK)
        }
        // ── Localized strings ──
        (Object::Strings(list), "GetCount") => ok(list.len() as u64),
        (Object::Strings(list), "FindLocaleName") => {
            let wanted = String::from_utf16_lossy(&unicode::read_wstr(memory, p(1)));
            match list.iter().position(|(l, _)| l.eq_ignore_ascii_case(&wanted)) {
                Some(index) => {
                    put32(memory, p(2), index as u32);
                    put32(memory, p(3), 1);
                }
                None => {
                    put32(memory, p(2), u32::MAX);
                    put32(memory, p(3), 0);
                }
            }
            ok(S_OK)
        }
        (Object::Strings(list), "GetLocaleNameLength" | "GetStringLength") => match list.get(p(1) as usize) {
            Some((locale, text)) => {
                let length = if method == "GetLocaleNameLength" { wide(locale).len() } else { wide(text).len() };
                put32(memory, p(2), length as u32);
                ok(S_OK)
            }
            None => ok(E_INVALIDARG),
        },
        (Object::Strings(list), "GetLocaleName" | "GetString") => match list.get(p(1) as usize) {
            Some((locale, text)) => {
                let units = if method == "GetLocaleName" { wide(locale) } else { wide(text) };
                if (p(3) as usize) <= units.len() {
                    if p(3) > 0 {
                        let _ = memory.write_bytes(p(2), &[0, 0]);
                    }
                    return ok(E_NOT_SUFFICIENT_BUFFER);
                }
                unicode::write_wide(memory, p(2), &units, true);
                ok(S_OK)
            }
            None => ok(E_INVALIDARG),
        },
        // ── Font faces ──
        (Object::Face(face, _), "GetType") => ok(if db.files[db.faces[face].file].guest_name.ends_with(".TTC") { 2 } else { 1 }),
        (Object::Face(face, _), "GetFiles") => {
            put32(memory, p(1), 1);
            if p(2) != 0 {
                let file = new_object(ctx, memory, Object::File(db.faces[face].file));
                put_ptr(memory, p(2), file);
            }
            ok(S_OK)
        }
        (Object::Face(face, _), "GetIndex") => ok(db.faces[face].index as u64),
        (Object::Face(_, simulations), "GetSimulations") => ok(simulations as u64),
        (Object::Face(face, _), "GetGlyphCount") => ok(db.face_data(face).glyph_count() as u64),
        (Object::Face(face, _), "GetDesignGlyphMetrics") | (Object::Face(face, _), "GetGdiCompatibleGlyphMetrics") => {
            // (indices, count, metrics) — after the size arguments for the
            // GDI-compatible form.
            let (indices, count, out) =
                if method == "GetDesignGlyphMetrics" { (p(1), p(2), p(3)) } else { (p(5), p(6), p(7)) };
            let data = db.face_data(face);
            let glyphs = memory.read_bytes(indices, count as usize * 2).unwrap_or_default();
            let mut bytes = Vec::with_capacity(count as usize * 28);
            for chunk in glyphs.chunks_exact(2) {
                let metrics = data.glyph_metrics(u16::from_le_bytes([chunk[0], chunk[1]]));
                for value in metrics {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
            let _ = memory.write_bytes(out, &bytes);
            ok(S_OK)
        }
        (Object::Face(face, _), "GetGlyphIndices") => {
            let data = db.face_data(face);
            let codes = memory.read_bytes(p(1), p(2) as usize * 4).unwrap_or_default();
            let glyphs: Vec<u8> = codes
                .chunks_exact(4)
                .flat_map(|c| data.glyph_index(u32::from_le_bytes([c[0], c[1], c[2], c[3]])).to_le_bytes())
                .collect();
            let _ = memory.write_bytes(p(3), &glyphs);
            ok(S_OK)
        }
        (Object::Face(face, _), "TryGetFontTable") => {
            // (tag, &data, &size, &context, &exists): the tag's bytes in order.
            let tag = p(1).to_le_bytes();
            let data = db.face_data(face);
            match data.table(&tag) {
                Some(table) => {
                    let block = ctx.shared.borrow_mut().heap.alloc((table.len() as u32).max(1)).unwrap_or(0);
                    let _ = memory.write_bytes(block, table);
                    ctx.shared.borrow_mut().dwrite.tables.insert(block, table.len() as u32);
                    put_ptr(memory, p(2), block);
                    put32(memory, p(3), table.len() as u32);
                    put_ptr(memory, p(4), block);
                    put32(memory, p(5), 1);
                }
                None => {
                    put_ptr(memory, p(2), 0);
                    put32(memory, p(3), 0);
                    put_ptr(memory, p(4), 0);
                    put32(memory, p(5), 0);
                }
            }
            ok(S_OK)
        }
        (Object::Face(..), "ReleaseFontTable") => {
            if ctx.shared.borrow_mut().dwrite.tables.remove(&p(1)).is_some() {
                ctx.shared.borrow_mut().heap.free(p(1));
            }
            ok(0)
        }
        (Object::Face(..), "GetRecommendedRenderingMode") => {
            // (em size, pixels per dip, measuring mode, params, &mode)
            put32(memory, p(5), 5); // NATURAL_SYMMETRIC
            ok(S_OK)
        }
        (Object::Face(face, _), "GetGdiCompatibleMetrics") => {
            let _ = memory.write_bytes(p(4), &font_metrics(&db.faces[face]));
            ok(S_OK)
        }
        // ── Files, the loader, streams ──
        (Object::File(file), "GetReferenceKey") => {
            // The key: the file's index.
            let key = ctx.shared.borrow_mut().heap.alloc(8).unwrap_or(0);
            let _ = memory.write_u32(key, file as u32);
            put_ptr(memory, p(1), key);
            put32(memory, p(2), 4);
            ok(S_OK)
        }
        (Object::File(_), "GetLoader") => {
            let loader = singleton(ctx, memory, Object::Loader);
            put_ptr(memory, p(1), loader);
            ok(S_OK)
        }
        (Object::File(file), "Analyze") => {
            let faces = db.faces.iter().filter(|f| f.file == file).count() as u32;
            let collection = db.files[file].guest_name.ends_with(".TTC");
            put32(memory, p(1), 1);
            put32(memory, p(2), if collection { 3 } else { 2 }); // TRUETYPE(_COLLECTION) file type
            put32(memory, p(3), if collection { 2 } else { 1 });
            put32(memory, p(4), faces);
            ok(S_OK)
        }
        (Object::Loader, _) if p(1) == 0 => ok(E_INVALIDARG),
        (Object::Loader, "CreateStreamFromKey") => {
            let file = memory.read_u32(p(1)).unwrap_or(0) as usize;
            let stream = new_object(ctx, memory, Object::Stream(file));
            put_ptr(memory, p(3), stream);
            ok(S_OK)
        }
        (Object::Loader, "GetFilePathLengthFromKey" | "GetFilePathFromKey") => {
            let file = memory.read_u32(p(1)).unwrap_or(0) as usize;
            let Some(info) = db.files.get(file) else { return ok(E_INVALIDARG) };
            let path = wide(&format!("C:\\WINDOWS\\FONTS\\{}", info.guest_name));
            if method == "GetFilePathLengthFromKey" {
                put32(memory, p(3), path.len() as u32);
                return ok(S_OK);
            }
            if (p(4) as usize) <= path.len() {
                return ok(E_NOT_SUFFICIENT_BUFFER);
            }
            unicode::write_wide(memory, p(3), &path, true);
            ok(S_OK)
        }
        (Object::Loader, "GetLastWriteTimeFromKey") => {
            let _ = memory.write_u64(p(3), 0x01D0_0000_0000_0000);
            ok(S_OK)
        }
        (Object::Stream(file), "ReadFileFragment") => {
            // (&start, offset, size, &context)
            let base = file_view(ctx, memory, db, file);
            let size = db.files.get(file).map(|f| f.size).unwrap_or(0);
            if base == 0 || arg(2).saturating_add(arg(3)) > size {
                return ok(E_FAIL);
            }
            put_ptr(memory, p(1), base + arg(2) as u32);
            put_ptr(memory, p(4), 0);
            ok(S_OK)
        }
        (Object::Stream(_), "ReleaseFileFragment") => ok(0),
        (Object::Stream(file), "GetFileSize") => {
            let _ = memory.write_u64(p(1), db.files.get(file).map(|f| f.size).unwrap_or(0));
            ok(S_OK)
        }
        (Object::Stream(_), "GetLastWriteTime") => {
            let _ = memory.write_u64(p(1), 0x01D0_0000_0000_0000);
            ok(S_OK)
        }
        // ── Rendering parameters ──
        (Object::Params { gamma, contrast, cleartype, geometry, mode }, _) => {
            let float = |memory: &mut dyn GuestMemory, value: f32| {
                memory.set_xmm0(value.to_bits() as u64);
                Some(ApiOutcome::wide(0, count))
            };
            match method {
                "GetGamma" => float(memory, gamma),
                "GetEnhancedContrast" => float(memory, contrast),
                "GetClearTypeLevel" => float(memory, cleartype),
                "GetPixelGeometry" => ok(geometry as u64),
                _ => ok(mode as u64),
            }
        }
        // ── Glyph run analyses ──
        (Object::Analysis { rendered, aliased }, "GetAlphaTextureBounds") => {
            // Aliased runs have a 1×1 texture, the others a 3×1 one.
            let wanted = (p(1) == 0) == aliased;
            let rect = if wanted && !rendered.coverage.is_empty() {
                [rendered.left, rendered.top, rendered.right, rendered.bottom]
            } else {
                [0; 4]
            };
            let bytes: Vec<u8> = rect.iter().flat_map(|v| v.to_le_bytes()).collect();
            let _ = memory.write_bytes(p(2), &bytes);
            ok(S_OK)
        }
        (Object::Analysis { rendered, aliased }, "CreateAlphaTexture") => {
            // (type, bounds, buffer, size)
            let bounds = memory.read_bytes(p(2), 16).unwrap_or_else(|| vec![0; 16]);
            let field = |i: usize| i32::from_le_bytes(bounds[i * 4..i * 4 + 4].try_into().unwrap_or([0; 4]));
            let (left, top, right, bottom) = (field(0), field(1), field(2), field(3));
            let channels = if p(1) == 0 { 1 } else { 3 };
            let (w, h) = ((right - left).max(0) as usize, (bottom - top).max(0) as usize);
            if w * h * channels > p(4) as usize {
                return ok(E_NOT_SUFFICIENT_BUFFER);
            }
            let wanted = (p(1) == 0) == aliased;
            let width = (rendered.right - rendered.left).max(0) as usize;
            let mut out = vec![0u8; w * h * channels];
            if wanted {
                for y in 0..h {
                    let sy = top + y as i32 - rendered.top;
                    if sy < 0 || sy >= rendered.bottom - rendered.top {
                        continue;
                    }
                    for x in 0..w {
                        let sx = left + x as i32 - rendered.left;
                        if sx < 0 || sx as usize >= width {
                            continue;
                        }
                        let mut value = rendered.coverage[sy as usize * width + sx as usize];
                        if aliased {
                            value = if value >= 128 { 255 } else { 0 };
                        }
                        for c in 0..channels {
                            out[(y * w + x) * channels + c] = value;
                        }
                    }
                }
            }
            let _ = memory.write_bytes(p(3), &out);
            ok(S_OK)
        }
        (Object::Analysis { .. }, "GetAlphaBlendParams") => {
            for (index, value) in [(2usize, 1.8f32), (3, 0.5), (4, 1.0)] {
                put32(memory, p(index), value.to_bits());
            }
            ok(S_OK)
        }
        // ── GDI interop ──
        (Object::Interop, "CreateFontFromLOGFONT") => {
            let name: Vec<u16> = memory
                .read_bytes(p(1) + 28, 64)
                .unwrap_or_default()
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&u| u != 0)
                .collect();
            let name = String::from_utf16_lossy(&name);
            let weight = memory.read_u32(p(1) + 16).unwrap_or(400);
            let italic = memory.read_bytes(p(1) + 20, 1).and_then(|b| b.first().copied()).unwrap_or(0) != 0;
            let family = db.family_index(&name).or_else(|| db.family_index("Arial")).or(if db.families.is_empty() { None } else { Some(0) });
            match family.and_then(|f| db.best_match(f, if weight == 0 { 400 } else { weight }, 5, if italic { 2 } else { 0 })) {
                Some(face) => {
                    let font = new_object(ctx, memory, Object::Font(face));
                    put_ptr(memory, p(2), font);
                    ok(S_OK)
                }
                None => ok(0x8898_5002),
            }
        }
        (Object::Interop, "ConvertFontToLOGFONT") => match object(ctx, p(1)) {
            Some(Object::Font(face)) => {
                let family = db.families[family_of(db, face)].name.clone();
                logfont(memory, p(2), &db.faces[face], &family);
                put32(memory, p(3), 1);
                ok(S_OK)
            }
            _ => ok(E_INVALIDARG),
        },
        (Object::Interop, "ConvertFontFaceToLOGFONT") => match object(ctx, p(1)) {
            Some(Object::Face(face, _)) => {
                let family = db.families[family_of(db, face)].name.clone();
                logfont(memory, p(2), &db.faces[face], &family);
                ok(S_OK)
            }
            _ => ok(E_INVALIDARG),
        },
        (_, method) => {
            if trace() || std::env::var_os("OTW_STUB_MISSING").is_some() {
                eprintln!("[dwrite] {kind_name}.{method} is not implemented");
            }
            if matches!(method, "CreateCustomFontCollection" | "CreateTextAnalyzer" | "CreateTextFormat" | "CreateTextLayout") {
                put_ptr(memory, p(match method { "CreateCustomFontCollection" => 3, "CreateTextAnalyzer" => 1, _ => 2 }), 0);
            }
            ok(E_NOTIMPL)
        }
    }
}
