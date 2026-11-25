//! Microsoft cabinet files: reading the folders and files of a cabinet set
//! and decompressing them (stored, MSZIP and LZX). Installers extract their
//! payload through ADVPACK's `ExtractFiles` and SETUPAPI.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct CabError(pub String);

impl std::fmt::Display for CabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn error<T>(message: impl Into<String>) -> Result<T, CabError> {
    Err(CabError(message.into()))
}

fn u16_at(data: &[u8], at: usize) -> Result<u16, CabError> {
    data.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| CabError("truncated cabinet".into()))
}

fn u32_at(data: &[u8], at: usize) -> Result<u32, CabError> {
    data.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| CabError("truncated cabinet".into()))
}

fn cstr_at(data: &[u8], at: usize) -> Result<(String, usize), CabError> {
    let end = data[at.min(data.len())..]
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| CabError("unterminated name".into()))?;
    Ok((data[at..at + end].iter().map(|&b| b as char).collect(), at + end + 1))
}

/// A file in a cabinet set.
#[derive(Clone, Debug)]
pub struct CabFile {
    pub name: String,
    pub size: u32,
    /// Offset in its folder's uncompressed data.
    pub offset: u32,
    /// Index into the set's folders.
    pub folder: usize,
    pub date: u16,
    pub time: u16,
    pub attributes: u16,
}

/// A folder: a compressed stream holding files back to back.
#[derive(Clone, Debug, Default)]
struct Folder {
    compression: u16,
    /// Compressed data blocks with their uncompressed sizes.
    blocks: Vec<(Vec<u8>, u16)>,
}

/// One cabinet file, parsed.
struct Cabinet {
    folders: Vec<Folder>,
    /// (file, folder index within this cabinet or a continuation marker).
    files: Vec<(CabFile, u16)>,
    next: Option<String>,
}

fn parse(data: &[u8]) -> Result<Cabinet, CabError> {
    if data.get(0..4) != Some(b"MSCF") {
        return error("not a cabinet");
    }
    let files_offset = u32_at(data, 16)? as usize;
    let folder_count = u16_at(data, 26)? as usize;
    let file_count = u16_at(data, 28)? as usize;
    let flags = u16_at(data, 30)?;
    let mut at = 36;
    let (mut folder_reserve, mut data_reserve) = (0usize, 0usize);
    if flags & 4 != 0 {
        let header_reserve = u16_at(data, at)? as usize;
        folder_reserve = data[at + 2] as usize;
        data_reserve = data[at + 3] as usize;
        at += 4 + header_reserve;
    }
    if flags & 1 != 0 {
        at = cstr_at(data, at)?.1;
        at = cstr_at(data, at)?.1;
    }
    let mut next = None;
    if flags & 2 != 0 {
        let (name, after) = cstr_at(data, at)?;
        next = Some(name);
        at = cstr_at(data, after)?.1;
    }
    let mut folders = Vec::with_capacity(folder_count);
    for _ in 0..folder_count {
        let start = u32_at(data, at)? as usize;
        let block_count = u16_at(data, at + 4)? as usize;
        let compression = u16_at(data, at + 6)?;
        at += 8 + folder_reserve;
        let mut blocks = Vec::with_capacity(block_count);
        let mut block = start;
        for _ in 0..block_count {
            let compressed = u16_at(data, block + 4)? as usize;
            let uncompressed = u16_at(data, block + 6)?;
            let body = block + 8 + data_reserve;
            let bytes = data
                .get(body..body + compressed)
                .ok_or_else(|| CabError("truncated data block".into()))?;
            blocks.push((bytes.to_vec(), uncompressed));
            block = body + compressed;
        }
        folders.push(Folder { compression, blocks });
    }
    let mut files = Vec::with_capacity(file_count);
    let mut at = files_offset;
    for _ in 0..file_count {
        let size = u32_at(data, at)?;
        let offset = u32_at(data, at + 4)?;
        let folder = u16_at(data, at + 8)?;
        let date = u16_at(data, at + 10)?;
        let time = u16_at(data, at + 12)?;
        let attributes = u16_at(data, at + 14)?;
        let (name, after) = cstr_at(data, at + 16)?;
        at = after;
        files.push((
            CabFile { name, size, offset, folder: 0, date, time, attributes },
            folder,
        ));
    }
    Ok(Cabinet { folders, files, next })
}

/// A cabinet and the ones it continues into, as one set of folders and
/// files.
pub struct CabSet {
    folders: Vec<Folder>,
    pub files: Vec<CabFile>,
}

/// Find a file in `dir` by name, ignoring case.
fn find_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|path| {
        path.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
    })
}

impl CabSet {
    /// Open a cabinet and follow its continuations in the same directory.
    pub fn open(path: &Path) -> Result<Self, CabError> {
        let data = std::fs::read(path).map_err(|e| CabError(e.to_string()))?;
        let mut set = CabSet { folders: Vec::new(), files: Vec::new() };
        let mut cabinet = parse(&data)?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut volumes = 0;
        // Whether the previous cabinet's last folder goes on in this one.
        let mut continued = false;
        loop {
            let base = set.folders.len();
            let merged = continued && base > 0 && !cabinet.folders.is_empty();
            // Global index of each local folder.
            let global = |local: usize| if merged { base - 1 + local } else { base + local };
            let mut folders = cabinet.folders.into_iter();
            if merged {
                let first = folders.next().expect("checked");
                let last = set.folders.last_mut().expect("a previous folder");
                let mut blocks = first.blocks.into_iter();
                // A block split across cabinets has no uncompressed size in
                // its first part.
                if matches!(last.blocks.last(), Some((_, 0))) {
                    if let Some((head, size)) = blocks.next() {
                        let (tail, _) = last.blocks.pop().expect("checked");
                        last.blocks.push(([tail, head].concat(), size));
                    }
                }
                last.blocks.extend(blocks);
            }
            set.folders.extend(folders);
            let last_folder = set.folders.len().saturating_sub(1);
            continued = false;
            for (mut file, folder) in cabinet.files {
                file.folder = match folder {
                    0xFFFD => global(0),
                    0xFFFE => {
                        continued = true;
                        last_folder
                    }
                    0xFFFF => {
                        continued = true;
                        global(0)
                    }
                    index => global(index as usize),
                };
                if !set.files.iter().any(|f| f.name == file.name && f.folder == file.folder) {
                    set.files.push(file);
                }
            }
            let Some(next) = cabinet.next else { break };
            volumes += 1;
            if volumes > 64 {
                return error("too many volumes");
            }
            let next_path = find_in(dir, &next).ok_or_else(|| CabError(format!("missing volume {next}")))?;
            let data = std::fs::read(&next_path).map_err(|e| CabError(e.to_string()))?;
            cabinet = parse(&data)?;
        }
        Ok(set)
    }

    /// The uncompressed contents of a folder.
    pub fn folder_data(&self, index: usize) -> Result<Vec<u8>, CabError> {
        let folder = self.folders.get(index).ok_or_else(|| CabError("no such folder".into()))?;
        let total: usize = folder.blocks.iter().map(|(_, size)| *size as usize).sum();
        let mut out = Vec::with_capacity(total);
        match folder.compression & 0x0F {
            0 => {
                for (block, _) in &folder.blocks {
                    out.extend_from_slice(block);
                }
            }
            1 => {
                let mut inflater = Inflater::default();
                for (block, size) in &folder.blocks {
                    if block.get(0..2) != Some(b"CK") {
                        return error("bad MSZIP block");
                    }
                    let start = out.len();
                    inflater.inflate(&block[2..], &mut out)?;
                    if out.len() - start != *size as usize {
                        return error("MSZIP block size mismatch");
                    }
                }
            }
            3 => {
                let window_bits = ((folder.compression >> 8) & 0x1F) as u32;
                let mut lzx = Lzx::new(window_bits)?;
                let input: Vec<u8> = folder.blocks.iter().flat_map(|(b, _)| b.iter().copied()).collect();
                let sizes: Vec<usize> = folder.blocks.iter().map(|(_, s)| *s as usize).collect();
                lzx.decompress(&input, &sizes, &mut out)?;
            }
            other => return error(format!("unsupported compression {other}")),
        }
        Ok(out)
    }

    /// Extract the files (all, or those `wanted` names, ignoring case) into
    /// `dir`. Returns the names written.
    pub fn extract(&self, dir: &Path, wanted: Option<&[String]>) -> Result<Vec<String>, CabError> {
        let mut written = Vec::new();
        let mut cache: Option<(usize, Vec<u8>)> = None;
        for file in &self.files {
            if let Some(wanted) = wanted {
                if !wanted.iter().any(|w| w.eq_ignore_ascii_case(&file.name)) {
                    continue;
                }
            }
            if cache.as_ref().map(|(i, _)| *i) != Some(file.folder) {
                cache = Some((file.folder, self.folder_data(file.folder)?));
            }
            let data = &cache.as_ref().expect("filled").1;
            let start = file.offset as usize;
            let bytes = data
                .get(start..start + file.size as usize)
                .ok_or_else(|| CabError(format!("{} is outside its folder", file.name)))?;
            let target = dir.join(file.name.replace('\\', "/"));
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&target, bytes).map_err(|e| CabError(e.to_string()))?;
            written.push(file.name.clone());
        }
        Ok(written)
    }
}

// ── Inflate (MSZIP) ────────────────────────────────────────────────────────

/// Decompress a raw deflate stream (as in ZIP files).
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, CabError> {
    let mut out = Vec::new();
    Inflater::default().inflate(data, &mut out)?;
    Ok(out)
}

/// A canonical Huffman code: (code, length) to symbol lookups by length.
struct Huffman {
    /// Number of codes of each length.
    counts: [u16; 17],
    /// Symbols ordered by code.
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Self {
        let mut counts = [0u16; 17];
        for &length in lengths {
            counts[length as usize] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 17];
        for length in 1..16 {
            offsets[length + 1] = offsets[length] + counts[length];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                symbols[offsets[length as usize] as usize] = symbol as u16;
                offsets[length as usize] += 1;
            }
        }
        Self { counts, symbols }
    }
}

/// Deflate's bit order: least significant bit first.
struct LsbBits<'a> {
    data: &'a [u8],
    pos: usize,
    buffer: u32,
    count: u32,
}

impl<'a> LsbBits<'a> {
    fn bits(&mut self, n: u32) -> Result<u32, CabError> {
        while self.count < n {
            let byte = *self.data.get(self.pos).ok_or_else(|| CabError("deflate data ends early".into()))?;
            self.pos += 1;
            self.buffer |= (byte as u32) << self.count;
            self.count += 8;
        }
        let value = self.buffer & ((1u64 << n) - 1) as u32;
        self.buffer >>= n;
        self.count -= n;
        Ok(value)
    }

    fn decode(&mut self, huffman: &Huffman) -> Result<u16, CabError> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for length in 1..16 {
            code |= self.bits(1)? as i32;
            let count = huffman.counts[length] as i32;
            if code - count < first {
                return Ok(huffman.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        error("bad Huffman code")
    }
}

/// Inflate keeping the last 32 KiB as the dictionary of the next block, as
/// MSZIP does.
#[derive(Default)]
struct Inflater {
    history: Vec<u8>,
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

impl Inflater {
    fn inflate(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), CabError> {
        let mut bits = LsbBits { data, pos: 0, buffer: 0, count: 0 };
        // The block's output, after the history it may refer back into.
        let mut window = std::mem::take(&mut self.history);
        let start = window.len();
        loop {
            let last = bits.bits(1)? == 1;
            match bits.bits(2)? {
                0 => {
                    bits.buffer = 0;
                    bits.count = 0;
                    let length = u16_at(data, bits.pos)? as usize;
                    bits.pos += 4;
                    let stored = data.get(bits.pos..bits.pos + length).ok_or_else(|| CabError("stored block ends early".into()))?;
                    window.extend_from_slice(stored);
                    bits.pos += length;
                }
                kind @ (1 | 2) => {
                    let (literal, distance) = if kind == 1 {
                        let mut lengths = [0u8; 288];
                        lengths[..144].fill(8);
                        lengths[144..256].fill(9);
                        lengths[256..280].fill(7);
                        lengths[280..].fill(8);
                        (Huffman::new(&lengths), Huffman::new(&[5u8; 30]))
                    } else {
                        let literals = bits.bits(5)? as usize + 257;
                        let distances = bits.bits(5)? as usize + 1;
                        let codes = bits.bits(4)? as usize + 4;
                        const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                        let mut code_lengths = [0u8; 19];
                        for &index in ORDER.iter().take(codes) {
                            code_lengths[index] = bits.bits(3)? as u8;
                        }
                        let code = Huffman::new(&code_lengths);
                        let mut lengths = vec![0u8; literals + distances];
                        let mut index = 0;
                        while index < lengths.len() {
                            let symbol = bits.decode(&code)?;
                            let (value, repeat) = match symbol {
                                0..=15 => (symbol as u8, 1),
                                16 => {
                                    let previous = *lengths.get(index.wrapping_sub(1)).ok_or_else(|| CabError("bad repeat".into()))?;
                                    (previous, 3 + bits.bits(2)? as usize)
                                }
                                17 => (0, 3 + bits.bits(3)? as usize),
                                _ => (0, 11 + bits.bits(7)? as usize),
                            };
                            for _ in 0..repeat {
                                if index < lengths.len() {
                                    lengths[index] = value;
                                    index += 1;
                                }
                            }
                        }
                        (Huffman::new(&lengths[..literals]), Huffman::new(&lengths[literals..]))
                    };
                    loop {
                        let symbol = bits.decode(&literal)? as usize;
                        if symbol < 256 {
                            window.push(symbol as u8);
                        } else if symbol == 256 {
                            break;
                        } else {
                            let index = symbol - 257;
                            if index >= 29 {
                                return error("bad length code");
                            }
                            let length = LENGTH_BASE[index] as usize + bits.bits(LENGTH_EXTRA[index] as u32)? as usize;
                            let code = bits.decode(&distance)? as usize;
                            if code >= 30 {
                                return error("bad distance code");
                            }
                            let back = DIST_BASE[code] as usize + bits.bits(DIST_EXTRA[code] as u32)? as usize;
                            if back > window.len() {
                                return error("distance before the start");
                            }
                            let from = window.len() - back;
                            for i in 0..length {
                                let byte = window[from + i];
                                window.push(byte);
                            }
                        }
                    }
                }
                _ => return error("bad deflate block type"),
            }
            if last {
                break;
            }
        }
        out.extend_from_slice(&window[start..]);
        let keep = window.len().saturating_sub(32768);
        self.history = window[keep..].to_vec();
        Ok(())
    }
}

// ── LZX ────────────────────────────────────────────────────────────────────

/// LZX's bit order: 16-bit little-endian words, most significant bit first.
struct MsbBits<'a> {
    data: &'a [u8],
    pos: usize,
    buffer: u32,
    left: u32,
}

impl<'a> MsbBits<'a> {
    fn ensure(&mut self, n: u32) {
        while self.left < n {
            let lo = *self.data.get(self.pos).unwrap_or(&0) as u32;
            let hi = *self.data.get(self.pos + 1).unwrap_or(&0) as u32;
            self.pos += 2;
            self.buffer |= ((hi << 8) | lo) << (16 - self.left);
            self.left += 16;
        }
    }

    fn peek(&mut self, n: u32) -> u32 {
        self.ensure(n);
        if n == 0 {
            0
        } else {
            self.buffer >> (32 - n)
        }
    }

    fn remove(&mut self, n: u32) {
        self.buffer = if n >= 32 { 0 } else { self.buffer << n };
        self.left -= n;
    }

    fn bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if n > 16 {
            let high = self.bits(n - 16);
            return (high << 16) | self.bits(16);
        }
        let value = self.peek(n);
        self.remove(n);
        value
    }

    fn decode(&mut self, huffman: &Huffman) -> Result<u16, CabError> {
        // Canonical codes, most significant bit first.
        self.ensure(16);
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for length in 1..17 {
            code |= ((self.buffer >> (32 - length)) & 1) as i32;
            let count = huffman.counts[length as usize] as i32;
            if code - count < first {
                self.remove(length);
                return Ok(huffman.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        error("bad LZX code")
    }

    fn align16(&mut self) {
        if self.left > 0 {
            self.ensure(16);
        }
        let drop = self.left & 15;
        self.remove(drop);
    }
}

struct Lzx {
    window_bits: u32,
    main_lengths: Vec<u8>,
    length_lengths: [u8; 249],
    position_slots: usize,
}

fn position_tables() -> ([u32; 51], [u8; 51]) {
    let mut base = [0u32; 51];
    let mut extra = [0u8; 51];
    let mut j = 0u8;
    for i in (0..51).step_by(2) {
        extra[i] = j;
        if i + 1 < 51 {
            extra[i + 1] = j;
        }
        if i != 0 && j < 17 {
            j += 1;
        }
    }
    for i in 1..51 {
        base[i] = base[i - 1] + (1u32 << extra[i - 1]);
    }
    (base, extra)
}

impl Lzx {
    fn new(window_bits: u32) -> Result<Self, CabError> {
        let position_slots = match window_bits {
            15 => 30,
            16 => 32,
            17 => 34,
            18 => 36,
            19 => 38,
            20 => 42,
            21 => 50,
            _ => return error("unsupported LZX window"),
        };
        Ok(Self {
            window_bits,
            main_lengths: vec![0; 256 + position_slots * 8],
            length_lengths: [0; 249],
            position_slots,
        })
    }

    /// Read code lengths for `lengths[first..last]` through a pretree, as
    /// deltas from their previous values.
    fn read_lengths(bits: &mut MsbBits, lengths: &mut [u8], first: usize, last: usize) -> Result<(), CabError> {
        let mut pre = [0u8; 20];
        for length in pre.iter_mut() {
            *length = bits.bits(4) as u8;
        }
        let pretree = Huffman::new(&pre);
        let mut x = first;
        while x < last {
            let z = bits.decode(&pretree)?;
            match z {
                17 => {
                    let run = bits.bits(4) as usize + 4;
                    for _ in 0..run {
                        if x < last {
                            lengths[x] = 0;
                            x += 1;
                        }
                    }
                }
                18 => {
                    let run = bits.bits(5) as usize + 20;
                    for _ in 0..run {
                        if x < last {
                            lengths[x] = 0;
                            x += 1;
                        }
                    }
                }
                19 => {
                    let run = bits.bits(1) as usize + 4;
                    let z = bits.decode(&pretree)? as i32;
                    let value = ((lengths[x] as i32 - z + 17) % 17) as u8;
                    for _ in 0..run {
                        if x < last {
                            lengths[x] = value;
                            x += 1;
                        }
                    }
                }
                _ => {
                    lengths[x] = ((lengths[x] as i32 - z as i32 + 17) % 17) as u8;
                    x += 1;
                }
            }
        }
        Ok(())
    }

    /// Decompress a folder's data: `input` is its blocks' compressed bytes
    /// back to back and `frames` their uncompressed sizes.
    fn decompress(&mut self, input: &[u8], frames: &[usize], out: &mut Vec<u8>) -> Result<(), CabError> {
        let (position_base, extra_bits) = position_tables();
        let window_size = 1usize << self.window_bits;
        let mut window = vec![0u8; window_size];
        let mut window_pos = 0usize;
        let mut bits = MsbBits { data: input, pos: 0, buffer: 0, left: 0 };
        let (mut r0, mut r1, mut r2) = (1u32, 1u32, 1u32);
        let mut block_type = 0u32;
        let mut block_remaining = 0usize;
        let mut block_length = 0usize;
        let mut main = Huffman::new(&[0]);
        let mut length = Huffman::new(&[0]);
        let mut aligned = Huffman::new(&[0]);
        let intel_filesize = if bits.bits(1) == 1 { bits.bits(32) as i64 } else { 0 };
        let mut intel_position = 0i64;
        let main_count = 256 + self.position_slots * 8;
        for &frame_size in frames {
            let mut frame_left = frame_size;
            let frame_start = window_pos;
            while frame_left > 0 {
                if block_remaining == 0 {
                    block_type = bits.bits(3);
                    let high = bits.bits(16);
                    let low = bits.bits(8);
                    block_remaining = ((high << 8) | low) as usize;
                    block_length = block_remaining;
                    match block_type {
                        2 | 1 => {
                            if block_type == 2 {
                                let mut lengths = [0u8; 8];
                                for l in lengths.iter_mut() {
                                    *l = bits.bits(3) as u8;
                                }
                                aligned = Huffman::new(&lengths);
                            }
                            Self::read_lengths(&mut bits, &mut self.main_lengths, 0, 256)?;
                            Self::read_lengths(&mut bits, &mut self.main_lengths, 256, main_count)?;
                            main = Huffman::new(&self.main_lengths);
                            Self::read_lengths(&mut bits, &mut self.length_lengths, 0, 249)?;
                            length = Huffman::new(&self.length_lengths);
                        }
                        3 => {
                            // Align to the byte stream, then R0-R2 stored.
                            if bits.left == 0 {
                                bits.ensure(16);
                            }
                            bits.left = 0;
                            bits.buffer = 0;
                            let at = bits.pos;
                            r0 = u32_at(input, at)?;
                            r1 = u32_at(input, at + 4)?;
                            r2 = u32_at(input, at + 8)?;
                            bits.pos += 12;
                        }
                        _ => return error("bad LZX block type"),
                    }
                }
                let mut run = block_remaining.min(frame_left);
                block_remaining -= run;
                frame_left -= run;
                if block_type == 3 {
                    while run > 0 {
                        let byte = *input.get(bits.pos).ok_or_else(|| CabError("LZX data ends early".into()))?;
                        bits.pos += 1;
                        window[window_pos] = byte;
                        window_pos = (window_pos + 1) & (window_size - 1);
                        run -= 1;
                    }
                    if block_remaining == 0 && block_length & 1 == 1 {
                        // An odd-length stored block is padded to a word.
                        bits.pos += 1;
                    }
                    continue;
                }
                // `run` may end inside a match: the overrun is taken from the
                // next frame's share.
                let mut produced = 0usize;
                while produced < run {
                    let element = bits.decode(&main)? as usize;
                    if element < 256 {
                        window[window_pos] = element as u8;
                        window_pos = (window_pos + 1) & (window_size - 1);
                        produced += 1;
                        continue;
                    }
                    let element = element - 256;
                    let mut match_length = element & 7;
                    if match_length == 7 {
                        match_length += bits.decode(&length)? as usize;
                    }
                    match_length += 2;
                    let slot = element >> 3;
                    let offset = match slot {
                        0 => r0,
                        1 => {
                            std::mem::swap(&mut r0, &mut r1);
                            r0
                        }
                        2 => {
                            std::mem::swap(&mut r0, &mut r2);
                            r0
                        }
                        _ => {
                            let extra = extra_bits[slot] as u32;
                            let mut offset = position_base[slot] - 2;
                            if block_type == 2 && extra >= 3 {
                                offset += bits.bits(extra - 3) << 3;
                                offset += bits.decode(&aligned)? as u32;
                            } else {
                                offset += bits.bits(extra);
                            }
                            r2 = r1;
                            r1 = r0;
                            r0 = offset;
                            offset
                        }
                    };
                    let mut from = (window_pos + window_size - offset as usize) & (window_size - 1);
                    for _ in 0..match_length {
                        window[window_pos] = window[from];
                        window_pos = (window_pos + 1) & (window_size - 1);
                        from = (from + 1) & (window_size - 1);
                    }
                    produced += match_length;
                }
                if produced > run {
                    // A match ran past the planned run: it ends within the
                    // block and frame.
                    let over = produced - run;
                    if over > block_remaining || over > frame_left {
                        return error("LZX match crosses a frame");
                    }
                    block_remaining -= over;
                    frame_left -= over;
                }
            }
            // The frame's output, with CALL targets translated back.
            let mut frame: Vec<u8> = (0..frame_size).map(|i| window[(frame_start + i) & (window_size - 1)]).collect();
            if intel_filesize != 0 && frame_size > 10 {
                let mut i = 0usize;
                while i < frame_size - 10 {
                    if frame[i] != 0xE8 {
                        i += 1;
                        continue;
                    }
                    let current = intel_position + i as i64;
                    let absolute = i32::from_le_bytes([frame[i + 1], frame[i + 2], frame[i + 3], frame[i + 4]]) as i64;
                    if absolute >= -current && absolute < intel_filesize {
                        let relative = if absolute >= 0 { absolute - current } else { absolute + intel_filesize };
                        frame[i + 1..i + 5].copy_from_slice(&(relative as i32).to_le_bytes());
                    }
                    i += 5;
                }
            }
            intel_position += frame_size as i64;
            out.extend_from_slice(&frame);
            // Each frame starts on a 16-bit boundary of the input.
            bits.align16();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compare with a reference extraction: OTW_TEST_CAB=file.cab
    /// OTW_TEST_CAB_REF=dir (skipped without them).
    #[test]
    fn extracts_like_the_reference() {
        let (Ok(cab), Ok(reference)) = (std::env::var("OTW_TEST_CAB"), std::env::var("OTW_TEST_CAB_REF")) else {
            return;
        };
        let set = CabSet::open(Path::new(&cab)).unwrap();
        let out = std::env::temp_dir().join(format!("otw-cab-{}", std::process::id()));
        let names = set.extract(&out, None).unwrap();
        for name in names {
            let ours = std::fs::read(out.join(&name)).unwrap();
            let theirs = std::fs::read(Path::new(&reference).join(&name)).unwrap();
            assert!(ours == theirs, "{name} differs");
        }
        let _ = std::fs::remove_dir_all(out);
    }

    #[test]
    fn inflate_stored_and_fixed_blocks() {
        // "hello hello hello" with fixed Huffman codes (zlib.compressobj(wbits=-15)).
        let data = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
        let mut out = Vec::new();
        Inflater::default().inflate(&data, &mut out).unwrap();
        assert_eq!(out, b"hello hello hello");
    }
}
