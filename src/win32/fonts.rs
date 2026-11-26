//! TrueType fonts from the host (`/usr/share/fonts` and the like): a font
//! database of families and faces built from the files' tables, glyph
//! lookup and metrics, outlines, and a rasteriser — what DirectWrite
//! (`win32::dwrite`) serves to programs. Fonts with CFF outlines are left
//! out; TrueType collections are included face by face.
//!
//! Inside the guest the files appear as `C:\WINDOWS\FONTS\<name>`.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug)]
pub struct FontFile {
    pub path: PathBuf,
    /// The file's name in the guest's fonts folder.
    pub guest_name: String,
    pub size: u64,
    /// A font OTW carries itself (no host file).
    pub embedded: Option<&'static [u8]>,
}

/// OTW's own stand-in for the icon fonts Windows draws caption buttons
/// with (built by `scripts/build-caption-font.py`).
static CAPTION_FONT: &[u8] = include_bytes!("../../binaries/fonts/OTWCAPTION.TTF");

#[derive(Clone, Debug, Default)]
pub struct Face {
    pub file: usize,
    pub index: u32,
    pub family: String,
    pub face_name: String,
    pub full_name: String,
    pub postscript: String,
    pub win_family: String,
    pub win_subfamily: String,
    pub weight: u32,
    pub stretch: u32,
    /// 0 normal, 1 oblique, 2 italic.
    pub style: u32,
    pub symbol: bool,
    pub units_per_em: u16,
    pub ascent: u16,
    pub descent: u16,
    pub line_gap: i16,
    pub cap_height: u16,
    pub x_height: u16,
    pub underline_position: i16,
    pub underline_thickness: u16,
    pub strikeout_position: i16,
    pub strikeout_thickness: u16,
    pub glyph_count: u16,
}

#[derive(Clone, Debug)]
pub struct Family {
    pub name: String,
    pub faces: Vec<usize>,
}

pub struct FontDb {
    pub files: Vec<FontFile>,
    pub faces: Vec<Face>,
    pub families: Vec<Family>,
    data: Mutex<HashMap<usize, Arc<Vec<u8>>>>,
    parsed: Mutex<HashMap<usize, Arc<FaceData>>>,
}

/// The fonts, found once.
pub fn db() -> &'static FontDb {
    static DB: OnceLock<FontDb> = OnceLock::new();
    DB.get_or_init(build)
}

fn font_dirs() -> Vec<PathBuf> {
    if let Some(dirs) = std::env::var_os("OTW_FONT_DIRS") {
        return std::env::split_paths(&dirs).collect();
    }
    let mut dirs = vec![PathBuf::from("/usr/share/fonts"), PathBuf::from("/usr/local/share/fonts")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&home).join(".local/share/fonts"));
        dirs.push(PathBuf::from(home).join(".fonts"));
    }
    dirs
}

fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk(&path, out, depth + 1);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("ttf") || e.eq_ignore_ascii_case("ttc"))
        {
            out.push(path);
        }
    }
}

fn be16(data: &[u8], at: usize) -> u16 {
    data.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]])).unwrap_or(0)
}

fn be32(data: &[u8], at: usize) -> u32 {
    data.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0)
}

/// The tables of the face at `offset` (its table directory): tag → (offset, length).
fn table_directory(data: &[u8], offset: usize) -> HashMap<[u8; 4], (usize, usize)> {
    let count = be16(data, offset + 4) as usize;
    let mut tables = HashMap::new();
    for index in 0..count.min(64) {
        let at = offset + 12 + index * 16;
        let Some(tag) = data.get(at..at + 4) else { break };
        tables.insert([tag[0], tag[1], tag[2], tag[3]], (be32(data, at + 8) as usize, be32(data, at + 12) as usize));
    }
    tables
}

/// Read only what describing a file needs: its header and the small tables.
fn read_headers(path: &std::path::Path) -> Option<(Vec<u8>, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    // Small tables sit early in most files, but to keep it simple and
    // correct the whole file is read when it is small; big ones are read
    // table by table.
    if size <= 4 << 20 {
        let mut data = Vec::new();
        file.read_to_end(&mut data).ok()?;
        return Some((data, size));
    }
    let mut data = vec![0u8; size as usize];
    let mut header = vec![0u8; 12 + 64 * 16];
    file.read_exact(&mut header[..12]).ok()?;
    let mut offsets = vec![0usize];
    if &header[..4] == b"ttcf" {
        let mut more = vec![0u8; 4 * 64];
        let count = be32(&header, 8) as usize;
        file.read_exact(&mut more[..(4 * count.min(64))]).ok()?;
        data[..12].copy_from_slice(&header[..12]);
        data[12..12 + 4 * count.min(64)].copy_from_slice(&more[..4 * count.min(64)]);
        offsets = (0..count.min(64)).map(|i| be32(&more, i * 4) as usize).collect();
    }
    for offset in offsets {
        file.seek(SeekFrom::Start(offset as u64)).ok()?;
        let mut directory = vec![0u8; 12 + 64 * 16];
        let n = file.read(&mut directory).ok()?;
        data[offset..offset + n].copy_from_slice(&directory[..n]);
        for (tag, (at, length)) in table_directory(&data, offset) {
            if matches!(&tag, b"name" | b"OS/2" | b"head" | b"hhea" | b"maxp" | b"post" | b"cmap") && at + length <= data.len() {
                file.seek(SeekFrom::Start(at as u64)).ok()?;
                file.read_exact(&mut data[at..at + length]).ok()?;
            }
        }
    }
    Some((data, size))
}

/// The best English name with this ID.
fn name_string(data: &[u8], table: (usize, usize), id: u16) -> Option<String> {
    let (at, _) = table;
    let count = be16(data, at + 2) as usize;
    let strings = at + be16(data, at + 4) as usize;
    let mut best: Option<(u32, String)> = None;
    for index in 0..count {
        let record = at + 6 + index * 12;
        let (platform, encoding, language, name_id) = (be16(data, record), be16(data, record + 2), be16(data, record + 4), be16(data, record + 6));
        if name_id != id {
            continue;
        }
        let (length, offset) = (be16(data, record + 8) as usize, be16(data, record + 10) as usize);
        let bytes = data.get(strings + offset..strings + offset + length)?;
        let (rank, text) = match (platform, encoding) {
            (3, 1) | (3, 10) | (0, _) => {
                let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                let rank = if platform == 3 && language == 0x409 { 0 } else if platform == 3 { 2 } else { 3 };
                (rank, String::from_utf16_lossy(&units))
            }
            (1, 0) => (if language == 0 { 1 } else { 4 }, bytes.iter().map(|&b| b as char).collect()),
            _ => continue,
        };
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            best = Some((rank, text));
        }
    }
    best.map(|(_, t)| t)
}

fn describe(data: &[u8], file: usize, offset: usize, index: u32) -> Option<Face> {
    let tables = table_directory(data, offset);
    if !tables.contains_key(b"glyf") || !tables.contains_key(b"loca") {
        return None; // CFF outlines: not drawn here
    }
    let name = *tables.get(b"name")?;
    let head = tables.get(b"head")?.0;
    let hhea = tables.get(b"hhea")?.0;
    let maxp = tables.get(b"maxp")?.0;
    let win_family = name_string(data, name, 1)?;
    let win_subfamily = name_string(data, name, 2).unwrap_or_else(|| "Regular".into());
    let family = name_string(data, name, 16).unwrap_or_else(|| win_family.clone());
    let face_name = name_string(data, name, 17).unwrap_or_else(|| win_subfamily.clone());
    let full_name = name_string(data, name, 4).unwrap_or_else(|| format!("{family} {face_name}"));
    let postscript = name_string(data, name, 6).unwrap_or_else(|| full_name.replace(' ', ""));
    let units_per_em = be16(data, head + 18).max(16);
    let mut face = Face {
        file,
        index,
        family,
        face_name,
        full_name,
        postscript,
        win_family,
        win_subfamily,
        weight: 400,
        stretch: 5,
        style: 0,
        units_per_em,
        ascent: be16(data, hhea + 4),
        descent: (be16(data, hhea + 6) as i16).unsigned_abs(),
        line_gap: be16(data, hhea + 8) as i16,
        glyph_count: be16(data, maxp + 4),
        underline_position: -(units_per_em as i16 / 10),
        underline_thickness: units_per_em / 20,
        strikeout_position: units_per_em as i16 / 4,
        strikeout_thickness: units_per_em / 20,
        cap_height: units_per_em * 7 / 10,
        x_height: units_per_em / 2,
        symbol: false,
    };
    if let Some(&(os2, length)) = tables.get(b"OS/2") {
        face.weight = (be16(data, os2 + 4) as u32).clamp(1, 999);
        face.stretch = (be16(data, os2 + 6) as u32).clamp(1, 9);
        let selection = be16(data, os2 + 62);
        face.style = if selection & 1 != 0 { 2 } else if selection & 0x200 != 0 { 1 } else { 0 };
        face.strikeout_thickness = be16(data, os2 + 26);
        face.strikeout_position = be16(data, os2 + 28) as i16;
        // Windows uses the OS/2 Windows metrics for ascent and descent.
        let (win_ascent, win_descent) = (be16(data, os2 + 74), be16(data, os2 + 76));
        if win_ascent != 0 {
            face.ascent = win_ascent;
            face.descent = win_descent;
        }
        if length >= 96 && be16(data, os2) >= 2 {
            face.x_height = be16(data, os2 + 86);
            face.cap_height = be16(data, os2 + 88);
        }
    } else {
        let mac_style = be16(data, head + 44);
        if mac_style & 1 != 0 {
            face.weight = 700;
        }
        if mac_style & 2 != 0 {
            face.style = 2;
        }
    }
    if let Some(&(post, _)) = tables.get(b"post") {
        face.underline_position = be16(data, post + 8) as i16;
        face.underline_thickness = be16(data, post + 10);
    }
    if let Some(&(cmap, _)) = tables.get(b"cmap") {
        let count = be16(data, cmap + 2) as usize;
        let mut unicode = false;
        let mut symbol = false;
        for record in 0..count {
            let at = cmap + 4 + record * 8;
            match (be16(data, at), be16(data, at + 2)) {
                (3, 0) => symbol = true,
                (3, 1) | (3, 10) | (0, _) => unicode = true,
                _ => {}
            }
        }
        face.symbol = symbol && !unicode;
        if !unicode && !symbol {
            return None;
        }
    }
    Some(face)
}

/// Windows names programs ask for, and what stands in for them.
const ALIASES: &[(&str, &[&str])] = &[
    ("Arial", &["Liberation Sans", "Arimo", "DejaVu Sans", "Noto Sans"]),
    ("Helvetica", &["Liberation Sans", "Arimo", "DejaVu Sans", "Noto Sans"]),
    ("Times New Roman", &["Liberation Serif", "Tinos", "DejaVu Serif", "Noto Serif"]),
    ("Courier New", &["Liberation Mono", "Cousine", "DejaVu Sans Mono", "Noto Sans Mono"]),
    ("Consolas", &["DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono"]),
    ("Segoe UI", &["Noto Sans", "DejaVu Sans", "Liberation Sans"]),
    ("Tahoma", &["DejaVu Sans", "Noto Sans", "Liberation Sans"]),
    ("Verdana", &["DejaVu Sans", "Noto Sans", "Liberation Sans"]),
    ("Microsoft Sans Serif", &["DejaVu Sans", "Liberation Sans", "Noto Sans"]),
    ("MS Shell Dlg", &["DejaVu Sans", "Liberation Sans", "Noto Sans"]),
    ("MS Shell Dlg 2", &["DejaVu Sans", "Liberation Sans", "Noto Sans"]),
    ("Calibri", &["Carlito", "Noto Sans", "DejaVu Sans"]),
    ("Cambria", &["Caladea", "Noto Serif", "DejaVu Serif"]),
    ("Georgia", &["DejaVu Serif", "Noto Serif", "Liberation Serif"]),
    ("Trebuchet MS", &["DejaVu Sans", "Noto Sans"]),
    ("Lucida Console", &["DejaVu Sans Mono", "Liberation Mono"]),
    ("Segoe Fluent Icons", &["OTW Caption Icons"]),
    ("Segoe MDL2 Assets", &["OTW Caption Icons"]),
];

fn build() -> FontDb {
    let mut paths = Vec::new();
    for dir in font_dirs() {
        walk(&dir, &mut paths, 0);
    }
    let mut files = Vec::new();
    let mut faces = Vec::new();
    let mut names: HashMap<String, usize> = HashMap::new();
    for path in paths {
        let Some((data, size)) = read_headers(&path) else { continue };
        let offsets: Vec<usize> =
            if data.starts_with(b"ttcf") { (0..be32(&data, 8) as usize).map(|i| be32(&data, 12 + i * 4) as usize).collect() } else { vec![0] };
        let file = files.len();
        let mut found = Vec::new();
        for (index, offset) in offsets.into_iter().enumerate() {
            if let Some(face) = describe(&data, file, offset, index as u32) {
                found.push(face);
            }
        }
        if found.is_empty() {
            continue;
        }
        // A unique name in the guest's fonts folder.
        let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().to_uppercase();
        let count = names.entry(base.clone()).or_insert(0);
        *count += 1;
        let guest_name = if *count == 1 { base } else { format!("{count}_{base}") };
        files.push(FontFile { path, guest_name, size, embedded: None });
        faces.extend(found);
    }
    {
        let file = files.len();
        if let Some(face) = describe(CAPTION_FONT, file, 0, 0) {
            files.push(FontFile { path: PathBuf::new(), guest_name: "OTWCAPTION.TTF".into(), size: CAPTION_FONT.len() as u64, embedded: Some(CAPTION_FONT) });
            faces.push(face);
        }
    }
    let mut by_family: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, face) in faces.iter().enumerate() {
        by_family.entry(face.family.clone()).or_default().push(index);
    }
    let mut families: Vec<Family> = by_family.into_iter().map(|(name, faces)| Family { name, faces }).collect();
    families.sort_by_key(|f| f.name.to_lowercase());
    for (alias, targets) in ALIASES {
        if families.iter().any(|f| f.name.eq_ignore_ascii_case(alias)) {
            continue;
        }
        if let Some(target) = targets.iter().find_map(|t| families.iter().find(|f| f.name.eq_ignore_ascii_case(t))) {
            families.push(Family { name: alias.to_string(), faces: target.faces.clone() });
        }
    }
    if std::env::var_os("OTW_TRACE_FONTS").is_some() {
        eprintln!("[fonts] {} files, {} faces, {} families", files.len(), faces.len(), families.len());
    }
    FontDb { files, faces, families, data: Mutex::new(HashMap::new()), parsed: Mutex::new(HashMap::new()) }
}

impl FontDb {
    /// A whole font file.
    pub fn file_data(&self, file: usize) -> Arc<Vec<u8>> {
        let mut cache = self.data.lock().expect("font cache");
        cache
            .entry(file)
            .or_insert_with(|| {
                let bytes = self.files.get(file).and_then(|f| match f.embedded {
                    Some(bytes) => Some(bytes.to_vec()),
                    None => std::fs::read(&f.path).ok(),
                });
                Arc::new(bytes.unwrap_or_default())
            })
            .clone()
    }

    /// A face's tables, cmap and metrics, parsed once.
    pub fn face_data(&self, face: usize) -> Arc<FaceData> {
        if let Some(data) = self.parsed.lock().expect("font cache").get(&face) {
            return data.clone();
        }
        let info = &self.faces[face];
        let file = self.file_data(info.file);
        let parsed = Arc::new(FaceData::parse(file, info.index));
        self.parsed.lock().expect("font cache").insert(face, parsed.clone());
        parsed
    }

    pub fn family_index(&self, name: &str) -> Option<usize> {
        self.families.iter().position(|f| f.name.eq_ignore_ascii_case(name))
    }

    /// The face of a family closest to a weight, stretch and style.
    pub fn best_match(&self, family: usize, weight: u32, stretch: u32, style: u32) -> Option<usize> {
        let faces = &self.families.get(family)?.faces;
        faces.iter().copied().min_by_key(|&f| {
            let face = &self.faces[f];
            let style_cost = if face.style == style { 0 } else if face.style != 0 && style != 0 { 1 } else { 4 };
            (style_cost * 10_000 + (face.stretch as i32 - stretch as i32).unsigned_abs() * 1000 + (face.weight as i32 - weight as i32).unsigned_abs()) as u32
        })
    }

    /// Where a guest fonts-folder name is on the host.
    pub fn host_path(&self, guest_name: &str) -> Option<PathBuf> {
        self.files.iter().find(|f| f.embedded.is_none() && f.guest_name.eq_ignore_ascii_case(guest_name)).map(|f| f.path.clone())
    }
}

// ── One face's data ──────────────────────────────────────────────────────

pub struct FaceData {
    pub data: Arc<Vec<u8>>,
    pub tables: HashMap<[u8; 4], (usize, usize)>,
    cmap: HashMap<u32, u16>,
    loca: Vec<u32>,
    glyf: usize,
    advances: Vec<u16>,
    bearings: Vec<i16>,
    pub units_per_em: u16,
    pub vertical_origin: i32,
    pub advance_height: u32,
}

impl FaceData {
    fn parse(data: Arc<Vec<u8>>, index: u32) -> Self {
        let offset = if data.starts_with(b"ttcf") { be32(&data, 12 + index as usize * 4) as usize } else { 0 };
        let tables = table_directory(&data, offset);
        let table = |tag: &[u8; 4]| tables.get(tag).copied().unwrap_or((0, 0));
        let (head, _) = table(b"head");
        let (maxp, _) = table(b"maxp");
        let (hhea, _) = table(b"hhea");
        let (hmtx, _) = table(b"hmtx");
        let (loca, _) = table(b"loca");
        let glyphs = be16(&data, maxp + 4) as usize;
        let long = be16(&data, head + 50) != 0;
        let loca: Vec<u32> = (0..=glyphs)
            .map(|i| if long { be32(&data, loca + i * 4) } else { be16(&data, loca + i * 2) as u32 * 2 })
            .collect();
        let metrics = (be16(&data, hhea + 34) as usize).max(1);
        let mut advances = Vec::with_capacity(glyphs);
        let mut bearings = Vec::with_capacity(glyphs);
        for glyph in 0..glyphs {
            if glyph < metrics {
                advances.push(be16(&data, hmtx + glyph * 4));
                bearings.push(be16(&data, hmtx + glyph * 4 + 2) as i16);
            } else {
                advances.push(*advances.last().unwrap_or(&0));
                bearings.push(be16(&data, hmtx + metrics * 4 + (glyph - metrics) * 2) as i16);
            }
        }
        let ascent = be16(&data, hhea + 4) as i16 as i32;
        let descent = be16(&data, hhea + 6) as i16 as i32;
        let cmap = parse_cmap(&data, table(b"cmap").0);
        FaceData {
            units_per_em: be16(&data, head + 18).max(16),
            glyf: table(b"glyf").0,
            data,
            tables,
            cmap,
            loca,
            advances,
            bearings,
            vertical_origin: ascent,
            advance_height: (ascent - descent).max(1) as u32,
        }
    }

    pub fn glyph_index(&self, code_point: u32) -> u16 {
        self.cmap.get(&code_point).copied().unwrap_or(0)
    }

    pub fn glyph_count(&self) -> usize {
        self.advances.len()
    }

    /// The glyph's bounding box in font units (x min, y min, x max, y max).
    pub fn bounds(&self, glyph: u16) -> (i16, i16, i16, i16) {
        let g = glyph as usize;
        let (Some(&start), Some(&end)) = (self.loca.get(g), self.loca.get(g + 1)) else { return (0, 0, 0, 0) };
        if end <= start {
            return (0, 0, 0, 0);
        }
        let at = self.glyf + start as usize;
        let d = &self.data;
        (be16(d, at + 2) as i16, be16(d, at + 4) as i16, be16(d, at + 6) as i16, be16(d, at + 8) as i16)
    }

    /// DWRITE_GLYPH_METRICS: left side bearing, advance, right side
    /// bearing, top side bearing, advance height, bottom side bearing,
    /// vertical origin.
    pub fn glyph_metrics(&self, glyph: u16) -> [i32; 7] {
        let g = glyph as usize;
        let advance = self.advances.get(g).copied().unwrap_or(0) as i32;
        let (x_min, y_min, x_max, y_max) = self.bounds(glyph);
        let lsb = self.bearings.get(g).copied().unwrap_or(x_min) as i32;
        let rsb = advance - lsb - (x_max as i32 - x_min as i32);
        let tsb = self.vertical_origin - y_max as i32;
        let bsb = y_min as i32 - (self.vertical_origin - self.advance_height as i32);
        [lsb, advance, rsb, tsb, self.advance_height as i32, bsb, self.vertical_origin]
    }

    pub fn table(&self, tag: &[u8; 4]) -> Option<&[u8]> {
        let &(at, length) = self.tables.get(tag)?;
        self.data.get(at..at + length)
    }

    /// The glyph's contours in font units (x right, y up): points with an
    /// on-curve flag.
    pub fn outline(&self, glyph: u16) -> Vec<Vec<(f32, f32, bool)>> {
        let mut contours = Vec::new();
        self.outline_into(glyph, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], &mut contours, 0);
        contours
    }

    fn outline_into(&self, glyph: u16, m: [f32; 6], out: &mut Vec<Vec<(f32, f32, bool)>>, depth: u32) {
        let g = glyph as usize;
        let (Some(&start), Some(&end)) = (self.loca.get(g), self.loca.get(g + 1)) else { return };
        if end <= start || depth > 8 {
            return;
        }
        let d = &self.data;
        let at = self.glyf + start as usize;
        let contours = be16(d, at) as i16;
        let transform = |x: f32, y: f32| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
        if contours >= 0 {
            let contours = contours as usize;
            let ends: Vec<usize> = (0..contours).map(|i| be16(d, at + 10 + i * 2) as usize).collect();
            let points = ends.last().map(|e| e + 1).unwrap_or(0);
            let instructions = be16(d, at + 10 + contours * 2) as usize;
            let mut p = at + 12 + contours * 2 + instructions;
            let mut flags = Vec::with_capacity(points);
            while flags.len() < points {
                let flag = d.get(p).copied().unwrap_or(0);
                p += 1;
                flags.push(flag);
                if flag & 8 != 0 {
                    let repeat = d.get(p).copied().unwrap_or(0);
                    p += 1;
                    for _ in 0..repeat {
                        flags.push(flag);
                    }
                }
            }
            flags.truncate(points);
            let mut xs = Vec::with_capacity(points);
            let mut value = 0i32;
            for &flag in &flags {
                if flag & 2 != 0 {
                    let delta = d.get(p).copied().unwrap_or(0) as i32;
                    p += 1;
                    value += if flag & 16 != 0 { delta } else { -delta };
                } else if flag & 16 == 0 {
                    value += be16(d, p) as i16 as i32;
                    p += 2;
                }
                xs.push(value);
            }
            let mut ys = Vec::with_capacity(points);
            value = 0;
            for &flag in &flags {
                if flag & 4 != 0 {
                    let delta = d.get(p).copied().unwrap_or(0) as i32;
                    p += 1;
                    value += if flag & 32 != 0 { delta } else { -delta };
                } else if flag & 32 == 0 {
                    value += be16(d, p) as i16 as i32;
                    p += 2;
                }
                ys.push(value);
            }
            let mut first = 0;
            for end in ends {
                let mut contour = Vec::new();
                for i in first..=end.min(points.saturating_sub(1)) {
                    let (x, y) = transform(xs[i] as f32, ys[i] as f32);
                    contour.push((x, y, flags[i] & 1 != 0));
                }
                first = end + 1;
                if !contour.is_empty() {
                    out.push(contour);
                }
            }
        } else {
            // A composite: components with offsets and transforms.
            let mut p = at + 10;
            loop {
                let flags = be16(d, p);
                let component = be16(d, p + 2);
                p += 4;
                let (dx, dy) = if flags & 1 != 0 {
                    let v = (be16(d, p) as i16 as f32, be16(d, p + 2) as i16 as f32);
                    p += 4;
                    v
                } else {
                    let v = (d.get(p).copied().unwrap_or(0) as i8 as f32, d.get(p + 1).copied().unwrap_or(0) as i8 as f32);
                    p += 2;
                    v
                };
                let f2dot14 = |v: u16| v as i16 as f32 / 16384.0;
                let mut c = [1.0, 0.0, 0.0, 1.0];
                if flags & 8 != 0 {
                    let s = f2dot14(be16(d, p));
                    c = [s, 0.0, 0.0, s];
                    p += 2;
                } else if flags & 0x40 != 0 {
                    c = [f2dot14(be16(d, p)), 0.0, 0.0, f2dot14(be16(d, p + 2))];
                    p += 4;
                } else if flags & 0x80 != 0 {
                    c = [f2dot14(be16(d, p)), f2dot14(be16(d, p + 2)), f2dot14(be16(d, p + 4)), f2dot14(be16(d, p + 6))];
                    p += 8;
                }
                // Only offsets (ARGS_ARE_XY_VALUES); point matching is rare.
                let (dx, dy) = if flags & 2 != 0 { (dx, dy) } else { (0.0, 0.0) };
                let inner = [
                    m[0] * c[0] + m[2] * c[1],
                    m[1] * c[0] + m[3] * c[1],
                    m[0] * c[2] + m[2] * c[3],
                    m[1] * c[2] + m[3] * c[3],
                    m[0] * dx + m[2] * dy + m[4],
                    m[1] * dx + m[3] * dy + m[5],
                ];
                self.outline_into(component, inner, out, depth + 1);
                if flags & 0x20 == 0 {
                    break;
                }
            }
        }
    }
}

fn parse_cmap(data: &[u8], cmap: usize) -> HashMap<u32, u16> {
    let mut map = HashMap::new();
    if cmap == 0 {
        return map;
    }
    let count = be16(data, cmap + 2) as usize;
    let mut subtables: Vec<(u32, usize)> = Vec::new();
    for record in 0..count {
        let at = cmap + 4 + record * 8;
        let (platform, encoding) = (be16(data, at), be16(data, at + 2));
        let offset = cmap + be32(data, at + 4) as usize;
        let rank = match (platform, encoding) {
            (3, 10) => 0,
            (0, 4) | (0, 6) => 1,
            (3, 1) => 2,
            (0, _) => 3,
            (3, 0) => 4,
            _ => continue,
        };
        subtables.push((rank, offset));
    }
    subtables.sort();
    let Some(&(rank, at)) = subtables.first() else { return map };
    match be16(data, at) {
        4 => {
            let segments = be16(data, at + 6) as usize / 2;
            let ends = at + 14;
            let starts = ends + segments * 2 + 2;
            let deltas = starts + segments * 2;
            let ranges = deltas + segments * 2;
            for s in 0..segments {
                let (end, start) = (be16(data, ends + s * 2) as u32, be16(data, starts + s * 2) as u32);
                let delta = be16(data, deltas + s * 2);
                let range = be16(data, ranges + s * 2) as usize;
                if start > end {
                    continue;
                }
                for c in start..=end.min(0xFFFF) {
                    let glyph = if range == 0 {
                        (c as u16).wrapping_add(delta)
                    } else {
                        let at = ranges + s * 2 + range + (c - start) as usize * 2;
                        let g = be16(data, at);
                        if g == 0 { 0 } else { g.wrapping_add(delta) }
                    };
                    if glyph != 0 {
                        map.insert(c, glyph);
                        // A symbol font's characters are at U+F0xx.
                        if rank == 4 && (0xF000..=0xF0FF).contains(&c) {
                            map.insert(c - 0xF000, glyph);
                        }
                    }
                }
            }
        }
        12 => {
            let groups = be32(data, at + 12) as usize;
            for g in 0..groups.min(1 << 16) {
                let record = at + 16 + g * 12;
                let (start, end, glyph) = (be32(data, record), be32(data, record + 4), be32(data, record + 8));
                for c in start..=end.min(start + 0x10000) {
                    map.insert(c, (glyph + (c - start)) as u16);
                }
            }
        }
        6 => {
            let (first, count) = (be16(data, at + 6) as u32, be16(data, at + 8) as u32);
            for i in 0..count {
                map.insert(first + i, be16(data, at + 10 + i as usize * 2));
            }
        }
        0 => {
            for c in 0..256u32 {
                let g = data.get(at + 6 + c as usize).copied().unwrap_or(0) as u16;
                if g != 0 {
                    map.insert(c, g);
                }
            }
        }
        _ => {}
    }
    map
}

// ── Rasterising ──────────────────────────────────────────────────────────

/// Coverage accumulation (signed area per cell, summed along rows).
struct Raster {
    w: usize,
    h: usize,
    a: Vec<f32>,
}

impl Raster {
    fn new(w: usize, h: usize) -> Self {
        Raster { w, h, a: vec![0.0; w * h + 4] }
    }

    fn line(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        if (p0.1 - p1.1).abs() <= f32::EPSILON {
            return;
        }
        let (dir, p0, p1) = if p0.1 < p1.1 { (1.0, p0, p1) } else { (-1.0, p1, p0) };
        let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
        let mut x = p0.0;
        let y0 = p0.1.max(0.0) as usize;
        if p0.1 < 0.0 {
            x -= p0.1 * dxdy;
        }
        let w = self.w as f32;
        for y in y0..self.h.min(p1.1.ceil().max(0.0) as usize) {
            let linestart = y * self.w;
            let dy = ((y + 1) as f32).min(p1.1) - (y as f32).max(p0.1);
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            let (x0, x1) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let (x0, x1) = (x0.clamp(0.0, w - 1.0), x1.clamp(0.0, w - 1.0));
            let x0floor = x0.floor();
            let x0i = x0floor as usize;
            let x1ceil = x1.ceil();
            let x1i = x1ceil as usize;
            if x1i <= x0i + 1 {
                let xmf = 0.5 * (x0 + x1) - x0floor;
                self.a[linestart + x0i] += d - d * xmf;
                self.a[linestart + x0i + 1] += d * xmf;
            } else {
                let s = (x1 - x0).recip();
                let x0f = x0 - x0floor;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = x1 - x1ceil + 1.0;
                let am = 0.5 * s * x1f * x1f;
                self.a[linestart + x0i] += d * a0;
                if x1i == x0i + 2 {
                    self.a[linestart + x0i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - x0f);
                    self.a[linestart + x0i + 1] += d * (a1 - a0);
                    for xi in x0i + 2..x1i - 1 {
                        self.a[linestart + xi] += d * s;
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                    self.a[linestart + x1i - 1] += d * (1.0 - a2 - am);
                }
                self.a[linestart + x1i] += d * am;
            }
            x = xnext;
        }
    }

    fn quad(&mut self, p0: (f32, f32), p1: (f32, f32), p2: (f32, f32)) {
        let dev = ((p0.0 - 2.0 * p1.0 + p2.0).powi(2) + (p0.1 - 2.0 * p1.1 + p2.1).powi(2)).sqrt();
        if dev < 0.333 {
            self.line(p0, p2);
            return;
        }
        let n = 1 + (3.0 * dev).sqrt().sqrt().floor() as usize;
        let mut p = p0;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let mt = 1.0 - t;
            let q = (mt * mt * p0.0 + 2.0 * mt * t * p1.0 + t * t * p2.0, mt * mt * p0.1 + 2.0 * mt * t * p1.1 + t * t * p2.1);
            self.line(p, q);
            p = q;
        }
    }

    fn coverage(&self) -> Vec<u8> {
        let mut acc = 0.0f32;
        let mut out = Vec::with_capacity(self.w * self.h);
        for (i, a) in self.a.iter().take(self.w * self.h).enumerate() {
            if i % self.w == 0 {
                acc = 0.0;
            }
            acc += a;
            out.push((acc.abs().min(1.0) * 255.0 + 0.5) as u8);
        }
        out
    }
}

/// Draw contours (already in device pixels, y down) into a raster.
fn fill(raster: &mut Raster, contours: &[Vec<(f32, f32, bool)>]) {
    for contour in contours {
        if contour.len() < 2 {
            continue;
        }
        // Start at an on-curve point (or between two off-curve ones).
        let n = contour.len();
        let start = contour.iter().position(|p| p.2);
        let (first, offset) = match start {
            Some(i) => ((contour[i].0, contour[i].1), i),
            None => (((contour[0].0 + contour[1].0) / 2.0, (contour[0].1 + contour[1].1) / 2.0), 0),
        };
        let mut current = first;
        let mut control: Option<(f32, f32)> = None;
        for k in 1..=n {
            let p = contour[(offset + k) % n];
            let point = (p.0, p.1);
            if p.2 {
                match control.take() {
                    Some(c) => raster.quad(current, c, point),
                    None => raster.line(current, point),
                }
                current = point;
            } else if let Some(c) = control {
                let mid = ((c.0 + point.0) / 2.0, (c.1 + point.1) / 2.0);
                raster.quad(current, c, mid);
                current = mid;
                control = Some(point);
            } else {
                control = Some(point);
            }
        }
        match control {
            Some(c) => raster.quad(current, c, first),
            None => raster.line(current, first),
        }
    }
}

/// A run of glyphs drawn: its pixel bounds and coverage (one byte per
/// pixel, row by row).
#[derive(Clone, Debug, Default)]
pub struct Rendered {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub coverage: Vec<u8>,
}

/// Draw glyphs at their positions (device pixels, the baseline's y) with
/// a 2×2 transform of font units to pixels (font y up).
pub fn render(face: &FaceData, glyphs: &[(u16, f32, f32)], scale: [f32; 4]) -> Rendered {
    let mut contours: Vec<Vec<(f32, f32, bool)>> = Vec::new();
    for &(glyph, x, y) in glyphs {
        for contour in face.outline(glyph) {
            contours.push(
                contour
                    .into_iter()
                    .map(|(fx, fy, on)| (x + scale[0] * fx + scale[2] * fy, y - (scale[1] * fx + scale[3] * fy), on))
                    .collect(),
            );
        }
    }
    let points = contours.iter().flatten();
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in points {
        x0 = x0.min(p.0);
        y0 = y0.min(p.1);
        x1 = x1.max(p.0);
        y1 = y1.max(p.1);
    }
    if x0 > x1 || (x1 - x0) > 4096.0 || (y1 - y0) > 4096.0 {
        return Rendered { left: 0, top: 0, right: 0, bottom: 0, coverage: Vec::new() };
    }
    let (left, top) = (x0.floor() as i32, y0.floor() as i32);
    let (right, bottom) = (x1.ceil() as i32 + 1, y1.ceil() as i32 + 1);
    let (w, h) = ((right - left) as usize + 1, (bottom - top) as usize);
    let shifted: Vec<Vec<(f32, f32, bool)>> =
        contours.into_iter().map(|c| c.into_iter().map(|(x, y, on)| (x - left as f32, y - top as f32, on)).collect()).collect();
    let mut raster = Raster::new(w, h);
    fill(&mut raster, &shifted);
    let full = raster.coverage();
    let width = (right - left) as usize;
    let mut coverage = Vec::with_capacity(width * h);
    for row in 0..h {
        coverage.extend_from_slice(&full[row * w..row * w + width]);
    }
    Rendered { left, top, right, bottom, coverage }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_square_fills_its_pixels() {
        let mut raster = Raster::new(8, 8);
        let square = vec![vec![(2.0, 2.0, true), (6.0, 2.0, true), (6.0, 6.0, true), (2.0, 6.0, true)]];
        fill(&mut raster, &square);
        let coverage = raster.coverage();
        assert_eq!(coverage[3 * 8 + 3], 255);
        assert_eq!(coverage[0], 0);
        assert_eq!(coverage[3 * 8 + 6], 0);
    }
}
