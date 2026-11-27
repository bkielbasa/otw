//! A small read-only reader for SQLite database files.
//!
//! The winget index is an SQLite database. Linking the SQLite C library
//! would need a C cross compiler for every Windows target, so this module
//! reads the file format directly (<https://www.sqlite.org/fileformat.html>).
//! It only supports what reading a whole table needs: the schema table,
//! table B-trees (ordinary tables), index B-trees (WITHOUT ROWID tables),
//! records and overflow pages. It does not support WAL files (the index is
//! shipped fully checkpointed) or text encodings other than UTF-8.

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            Value::Blob(b) => Some(b),
            _ => None,
        }
    }
}

/// One row of a table. For ordinary tables `rowid` is the row's key; a
/// column declared `INTEGER PRIMARY KEY` is stored as NULL and its value is
/// the rowid.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub rowid: Option<i64>,
    pub values: Vec<Value>,
}

impl Row {
    /// The value of column `i`. Columns past the end of the stored record
    /// (added later with ALTER TABLE) read as NULL.
    pub fn get(&self, i: usize) -> &Value {
        self.values.get(i).unwrap_or(&Value::Null)
    }
}

#[derive(Debug, Clone)]
pub struct SchemaEntry {
    pub kind: String,
    pub name: String,
    pub root_page: u32,
    pub sql: String,
}

pub struct Database<'a> {
    data: &'a [u8],
    page_size: usize,
    usable_size: usize,
    page_count: usize,
}

const MAX_DEPTH: usize = 64;

impl<'a> Database<'a> {
    pub fn open(data: &'a [u8]) -> Result<Database<'a>> {
        if data.len() < 100 || &data[..16] != b"SQLite format 3\0" {
            return Err(Error::format("not an SQLite database"));
        }
        let page_size = match u16::from_be_bytes([data[16], data[17]]) {
            1 => 65536,
            n if n >= 512 && n.is_power_of_two() => n as usize,
            n => return Err(Error::format(format!("bad SQLite page size {n}"))),
        };
        let reserved = data[20] as usize;
        let encoding = u32::from_be_bytes([data[56], data[57], data[58], data[59]]);
        if encoding > 1 {
            return Err(Error::format("only UTF-8 SQLite databases are supported"));
        }
        if data[18] == 2 || data[19] == 2 {
            return Err(Error::format("SQLite database is in WAL mode"));
        }
        Ok(Database {
            data,
            page_size,
            usable_size: page_size - reserved,
            page_count: data.len() / page_size,
        })
    }

    /// The entries of the `sqlite_schema` table.
    pub fn schema(&self) -> Result<Vec<SchemaEntry>> {
        let mut out = Vec::new();
        for row in self.scan_table_btree(1)? {
            let text = |i: usize| row.get(i).as_text().unwrap_or("").to_string();
            out.push(SchemaEntry {
                kind: text(0),
                name: text(1),
                root_page: row.get(3).as_int().unwrap_or(0) as u32,
                sql: text(4),
            });
        }
        Ok(out)
    }

    /// Every row of the table `name`, in key order.
    pub fn table(&self, name: &str) -> Result<Vec<Row>> {
        let entry = self
            .schema()?
            .into_iter()
            .find(|e| e.kind == "table" && e.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::format(format!("no table {name} in the database")))?;
        if entry.sql.to_ascii_uppercase().contains("WITHOUT ROWID") {
            self.scan_index_btree(entry.root_page)
        } else {
            self.scan_table_btree(entry.root_page)
        }
    }

    fn page(&self, number: u32) -> Result<&'a [u8]> {
        let n = number as usize;
        if n == 0 || n > self.page_count {
            return Err(Error::format(format!("SQLite page {number} out of range")));
        }
        Ok(&self.data[(n - 1) * self.page_size..n * self.page_size])
    }

    /// Where the B-tree header of a page starts (page 1 has the file header first).
    fn header_offset(number: u32) -> usize {
        if number == 1 {
            100
        } else {
            0
        }
    }

    fn cells(&self, page: &'a [u8], hdr: usize) -> Result<(u8, Vec<usize>, Option<u32>)> {
        let kind = *page.get(hdr).ok_or_else(|| Error::format("short page"))?;
        let interior = matches!(kind, 2 | 5);
        if !matches!(kind, 2 | 5 | 10 | 13) {
            return Err(Error::format(format!("bad B-tree page type {kind}")));
        }
        let count = be16(page, hdr + 3)? as usize;
        let ptrs = hdr + if interior { 12 } else { 8 };
        let right = if interior {
            Some(be32(page, hdr + 8)?)
        } else {
            None
        };
        let mut cells = Vec::with_capacity(count);
        for i in 0..count {
            let off = be16(page, ptrs + 2 * i)? as usize;
            if off >= page.len() {
                return Err(Error::format("cell pointer out of range"));
            }
            cells.push(off);
        }
        Ok((kind, cells, right))
    }

    fn scan_table_btree(&self, root: u32) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        self.walk_table(root, 0, &mut out)?;
        Ok(out)
    }

    fn walk_table(&self, number: u32, depth: usize, out: &mut Vec<Row>) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::format("SQLite B-tree too deep"));
        }
        let page = self.page(number)?;
        let hdr = Self::header_offset(number);
        let (kind, cells, right) = self.cells(page, hdr)?;
        match kind {
            5 => {
                for off in cells {
                    self.walk_table(be32(page, off)?, depth + 1, out)?;
                }
                self.walk_table(right.unwrap_or(0), depth + 1, out)?;
            }
            13 => {
                for off in cells {
                    let (size, n1) = varint(page, off)?;
                    let (rowid, n2) = varint(page, off + n1)?;
                    let payload = self.payload(page, off + n1 + n2, size as usize, true)?;
                    out.push(Row {
                        rowid: Some(rowid as i64),
                        values: record(&payload)?,
                    });
                }
            }
            _ => return Err(Error::format("index page inside a table B-tree")),
        }
        Ok(())
    }

    fn scan_index_btree(&self, root: u32) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        self.walk_index(root, 0, &mut out)?;
        Ok(out)
    }

    fn walk_index(&self, number: u32, depth: usize, out: &mut Vec<Row>) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::format("SQLite B-tree too deep"));
        }
        let page = self.page(number)?;
        let hdr = Self::header_offset(number);
        let (kind, cells, right) = self.cells(page, hdr)?;
        let interior = match kind {
            2 => true,
            10 => false,
            _ => return Err(Error::format("table page inside an index B-tree")),
        };
        for off in cells {
            let mut pos = off;
            if interior {
                self.walk_index(be32(page, off)?, depth + 1, out)?;
                pos += 4;
            }
            // In an index B-tree the interior cells hold keys too, ordered
            // between their left child and the next one.
            let (size, n) = varint(page, pos)?;
            let payload = self.payload(page, pos + n, size as usize, false)?;
            out.push(Row {
                rowid: None,
                values: record(&payload)?,
            });
        }
        if let Some(right) = right {
            self.walk_index(right, depth + 1, out)?;
        }
        Ok(())
    }

    /// The full payload of a cell, following overflow pages.
    fn payload(&self, page: &[u8], start: usize, size: usize, table_leaf: bool) -> Result<Vec<u8>> {
        let u = self.usable_size;
        let max_local = if table_leaf {
            u - 35
        } else {
            (u - 12) * 64 / 255 - 23
        };
        let min_local = (u - 12) * 32 / 255 - 23;
        let local = if size <= max_local {
            size
        } else {
            let k = min_local + (size - min_local) % (u - 4);
            if k <= max_local {
                k
            } else {
                min_local
            }
        };
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(slice(page, start, local)?);
        if local < size {
            let mut next = be32(page, start + local)?;
            let mut hops = 0;
            while out.len() < size {
                hops += 1;
                if next == 0 || hops > self.page_count {
                    return Err(Error::format("broken SQLite overflow chain"));
                }
                let p = self.page(next)?;
                next = be32(p, 0)?;
                let take = (size - out.len()).min(u - 4);
                out.extend_from_slice(slice(p, 4, take)?);
            }
        }
        Ok(out)
    }
}

fn slice(b: &[u8], off: usize, len: usize) -> Result<&[u8]> {
    b.get(off..off + len)
        .ok_or_else(|| Error::format("SQLite data out of range"))
}

fn be16(b: &[u8], off: usize) -> Result<u16> {
    let s = slice(b, off, 2)?;
    Ok(u16::from_be_bytes([s[0], s[1]]))
}

fn be32(b: &[u8], off: usize) -> Result<u32> {
    let s = slice(b, off, 4)?;
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Reads an SQLite varint; returns the value and its length in bytes.
pub fn varint(b: &[u8], off: usize) -> Result<(u64, usize)> {
    let mut v: u64 = 0;
    for i in 0..9 {
        let byte = *b
            .get(off + i)
            .ok_or_else(|| Error::format("truncated SQLite varint"))?;
        if i == 8 {
            return Ok(((v << 8) | byte as u64, 9));
        }
        v = (v << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Ok((v, i + 1));
        }
    }
    unreachable!()
}

/// Decodes a record (the payload of a cell) into its values.
pub fn record(p: &[u8]) -> Result<Vec<Value>> {
    let (header_len, mut pos) = varint(p, 0)?;
    let header_len = header_len as usize;
    if header_len > p.len() {
        return Err(Error::format("SQLite record header too long"));
    }
    let mut types = Vec::new();
    while pos < header_len {
        let (t, n) = varint(p, pos)?;
        types.push(t);
        pos += n;
    }
    let mut body = header_len;
    let mut values = Vec::with_capacity(types.len());
    for t in types {
        let int = |len: usize, body: usize| -> Result<i64> {
            let s = slice(p, body, len)?;
            let mut v: i64 = if s[0] & 0x80 != 0 { -1 } else { 0 };
            for &b in s {
                v = (v << 8) | b as i64;
            }
            Ok(v)
        };
        let (value, len) = match t {
            0 => (Value::Null, 0),
            1 => (Value::Int(int(1, body)?), 1),
            2 => (Value::Int(int(2, body)?), 2),
            3 => (Value::Int(int(3, body)?), 3),
            4 => (Value::Int(int(4, body)?), 4),
            5 => (Value::Int(int(6, body)?), 6),
            6 => (Value::Int(int(8, body)?), 8),
            7 => {
                let s = slice(p, body, 8)?;
                let bits = u64::from_be_bytes(s.try_into().unwrap());
                (Value::Real(f64::from_bits(bits)), 8)
            }
            8 => (Value::Int(0), 0),
            9 => (Value::Int(1), 0),
            10 | 11 => return Err(Error::format("reserved SQLite serial type")),
            t if t % 2 == 0 => {
                let len = ((t - 12) / 2) as usize;
                (Value::Blob(slice(p, body, len)?.to_vec()), len)
            }
            t => {
                let len = ((t - 13) / 2) as usize;
                let text = String::from_utf8_lossy(slice(p, body, len)?).into_owned();
                (Value::Text(text), len)
            }
        };
        values.push(value);
        body += len;
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/index.db"
        ))
        .unwrap()
    }

    #[test]
    fn varints() {
        assert_eq!(varint(&[0x05], 0).unwrap(), (5, 1));
        assert_eq!(varint(&[0x81, 0x00], 0).unwrap(), (128, 2));
        assert_eq!(varint(&[0xff; 9], 0).unwrap(), (u64::MAX, 9));
        assert!(varint(&[0x81], 0).is_err());
    }

    #[test]
    fn records_decode_every_serial_type() {
        // header: len 8, types NULL, i8, i16, 0, 1, text(2), blob(1)
        let rec = [8, 0, 1, 2, 8, 9, 17, 14, 0xfe, 0x01, 0x00, b'h', b'i', 0xaa];
        let values = record(&rec).unwrap();
        assert_eq!(
            values,
            vec![
                Value::Null,
                Value::Int(-2),
                Value::Int(256),
                Value::Int(0),
                Value::Int(1),
                Value::Text("hi".into()),
                Value::Blob(vec![0xaa]),
            ]
        );
    }

    #[test]
    fn rejects_other_files() {
        assert!(Database::open(b"hello").is_err());
        assert!(Database::open(&[0u8; 200]).is_err());
    }

    #[test]
    fn reads_the_schema() {
        let data = fixture();
        let db = Database::open(&data).unwrap();
        let names: Vec<String> = db.schema().unwrap().into_iter().map(|e| e.name).collect();
        for t in [
            "metadata",
            "packages",
            "tags2",
            "tags2_map",
            "commands2",
            "commands2_map",
        ] {
            assert!(names.iter().any(|n| n == t), "missing {t}");
        }
    }

    #[test]
    fn reads_a_multi_level_table() {
        let data = fixture();
        let db = Database::open(&data).unwrap();
        let rows = db.table("packages").unwrap();
        assert_eq!(rows.len(), 611);
        // Rows come in rowid order.
        assert!(rows.windows(2).all(|w| w[0].rowid < w[1].rowid));
        let seven = rows
            .iter()
            .find(|r| r.get(1).as_text() == Some("7zip.7zip"))
            .unwrap();
        assert_eq!(seven.get(0), &Value::Null, "rowid alias is stored as NULL");
        assert_eq!(seven.get(2).as_text(), Some("7-Zip"));
        assert_eq!(seven.get(4).as_text(), Some("26.06"));
        assert_eq!(seven.get(7).as_blob().map(<[u8]>::len), Some(32));
    }

    #[test]
    fn follows_overflow_pages() {
        let data = fixture();
        let db = Database::open(&data).unwrap();
        let rows = db.table("packages").unwrap();
        let long = rows.iter().find(|r| r.rowid == Some(200000)).unwrap();
        let name = long.get(2).as_text().unwrap();
        assert_eq!(name.len(), 5 + 5 * 3000);
        assert!(name.starts_with("Long name name"));
        assert_eq!(long.get(3).as_text(), Some("longname"));
    }

    #[test]
    fn reads_without_rowid_tables() {
        let data = fixture();
        let db = Database::open(&data).unwrap();
        let map = db.table("tags2_map").unwrap();
        assert_eq!(map.len(), 723);
        // Index B-trees are ordered by their key (tag, package).
        let keys: Vec<(i64, i64)> = map
            .iter()
            .map(|r| (r.get(0).as_int().unwrap(), r.get(1).as_int().unwrap()))
            .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        let meta = db.table("metadata").unwrap();
        assert!(
            meta.iter()
                .any(|r| r.get(0).as_text() == Some("majorVersion")
                    && r.get(1).as_text() == Some("2"))
        );
    }

    #[test]
    fn missing_table_is_an_error() {
        let data = fixture();
        let db = Database::open(&data).unwrap();
        assert!(db.table("nope").is_err());
    }

    #[test]
    fn truncated_file_is_an_error_not_a_panic() {
        let data = fixture();
        let cut = &data[..data.len() / 3];
        let db = Database::open(cut).unwrap();
        assert!(db.table("packages").is_err());
    }
}
