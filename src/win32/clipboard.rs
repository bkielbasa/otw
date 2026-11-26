//! The system clipboard behind USER32's clipboard API and OLE's
//! `OleSetClipboard`/`OleGetClipboard`.
//!
//! Data is kept as host bytes, so it outlives the process that put it there
//! (programs launched one after another on the desktop share it). Images are
//! stored as packed DIBs (`CF_DIB`); `CF_BITMAP` is synthesised on demand, as
//! Windows does. While a host display is attached, `CF_TEXT` is mirrored to
//! the host's X11 clipboard through `xclip`.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::collections::HashMap;

use crate::{
    win32::gdi::{self, Bitmap, DibInfo, GdiObject},
    win32::gfx::Surface,
    win32::{read_cstr, write_cstr, ApiContext, ApiOutcome, GuestMemory},
};

pub const CF_TEXT: u32 = 1;
pub const CF_BITMAP: u32 = 2;
pub const CF_OEMTEXT: u32 = 7;
pub const CF_DIB: u32 = 8;
pub const CF_UNICODETEXT: u32 = 13;

const TYMED_HGLOBAL: u32 = 1;
const TYMED_GDI: u32 = 16;

/// A clipboard format that keeps its meaning across processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipFormat {
    Standard(u32),
    /// Registered formats are identified by name, since atom numbers are
    /// per process.
    Named(String),
}

#[derive(Debug, Default)]
pub struct Clipboard {
    pub entries: Vec<(ClipFormat, Vec<u8>)>,
    /// Per-process state: the window that opened the clipboard, handles
    /// handed out by GetClipboardData, and the OLE data object that owns
    /// the current contents.
    pub open: u32,
    pub owner: u32,
    pub handles: HashMap<u32, u32>,
    pub ole_source: u32,
    /// Host clipboard text as last seen or written, and when it was polled.
    pub host_text: Option<Vec<u8>>,
    pub host_polled: Option<std::time::Instant>,
    /// Bumped whenever `entries` changes, so the desktop can share the
    /// contents between programs.
    pub generation: u64,
}

impl Clipboard {
    /// Forget everything that refers to the current process.
    pub fn reset_process(&mut self) {
        self.open = 0;
        self.owner = 0;
        self.handles.clear();
        self.ole_source = 0;
    }
}

// ── Format names ─────────────────────────────────────────────────────────

fn format_of(ctx: &ApiContext, cf: u32) -> ClipFormat {
    if cf >= 0xC000 {
        if let Some(name) = ctx.shared.borrow().gui.atoms.get((cf - 0xC000) as usize) {
            return ClipFormat::Named(name.clone());
        }
    }
    ClipFormat::Standard(cf)
}

fn id_of(ctx: &ApiContext, format: &ClipFormat) -> u32 {
    match format {
        ClipFormat::Standard(cf) => *cf,
        ClipFormat::Named(name) => {
            let mut shared = ctx.shared.borrow_mut();
            let index = match shared.gui.atoms.iter().position(|a| a == name) {
                Some(index) => index,
                None => {
                    shared.gui.atoms.push(name.clone());
                    shared.gui.atoms.len() - 1
                }
            };
            0xC000 + index as u32
        }
    }
}

fn stored(ctx: &ApiContext, format: &ClipFormat) -> Option<Vec<u8>> {
    ctx.shared
        .borrow()
        .clipboard
        .entries
        .iter()
        .find(|(f, _)| f == format)
        .map(|(_, b)| b.clone())
}

/// Formats currently available, including synthesised ones, in the order
/// a program should prefer them.
pub fn available_formats<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &M) -> Vec<u32> {
    poll_host(ctx, memory);
    let entries: Vec<ClipFormat> = ctx
        .shared
        .borrow()
        .clipboard
        .entries
        .iter()
        .map(|(f, _)| f.clone())
        .collect();
    let mut formats = Vec::new();
    for format in &entries {
        let id = id_of(ctx, format);
        if !formats.contains(&id) {
            formats.push(id);
        }
        match format {
            ClipFormat::Standard(CF_DIB) => {
                if !formats.contains(&CF_BITMAP) {
                    formats.push(CF_BITMAP);
                }
            }
            ClipFormat::Standard(CF_TEXT) => {
                for synthesized in [CF_OEMTEXT, CF_UNICODETEXT] {
                    if !formats.contains(&synthesized) {
                        formats.push(synthesized);
                    }
                }
            }
            _ => {}
        }
    }
    formats
}

pub fn is_available<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &M, cf: u32) -> bool {
    available_formats(ctx, memory).contains(&cf)
}

// ── Host clipboard (X11 via xclip) ───────────────────────────────────────

fn host_enabled<M: GuestMemory + ?Sized>(memory: &M) -> bool {
    memory.has_host()
        && std::env::var_os("DISPLAY").is_some()
        && std::env::var("OTW_HOST_CLIPBOARD")
            .map(|v| v != "0")
            .unwrap_or(true)
}

/// Windows text (Latin-1, CRLF) to host UTF-8 with LF.
#[cfg_attr(not(feature = "std"), allow(dead_code))]
fn to_host_text(text: &[u8]) -> String {
    text.iter()
        .filter(|b| **b != b'\r')
        .map(|b| *b as char)
        .collect()
}

fn from_host_text(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\n' {
            out.extend_from_slice(b"\r\n");
        } else if ch == '\r' {
            continue;
        } else {
            out.push(if (ch as u32) < 0x100 { ch as u8 } else { b'?' });
        }
    }
    out
}

#[cfg(not(feature = "std"))]
fn push_host(_ctx: &ApiContext, _text: &[u8]) {}

#[cfg(feature = "std")]
fn push_host(ctx: &ApiContext, text: &[u8]) {
    use std::io::Write;
    let host = to_host_text(text);
    let child = std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-i"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(host.as_bytes());
        }
        // xclip forks to serve the selection; reap the foreground process.
        let _ = child.wait();
        ctx.shared.borrow_mut().clipboard.host_text = Some(host.into_bytes());
    }
}

/// Adopt host clipboard text that changed since we last looked.
fn poll_host<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &M) {
    if !host_enabled(memory) {
        return;
    }
    {
        let mut shared = ctx.shared.borrow_mut();
        let clipboard = &mut shared.clipboard;
        if clipboard
            .host_polled
            .map(|t| t.elapsed() < std::time::Duration::from_millis(500))
            .unwrap_or(false)
        {
            return;
        }
        clipboard.host_polled = Some(std::time::Instant::now());
    }
    let Ok(output) = std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-o"])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return;
    };
    if !output.status.success() || output.stdout.is_empty() {
        return;
    }
    let mut shared = ctx.shared.borrow_mut();
    if shared.clipboard.host_text.as_deref() == Some(output.stdout.as_slice()) {
        return;
    }
    let text = from_host_text(&String::from_utf8_lossy(&output.stdout));
    shared.clipboard.host_text = Some(output.stdout);
    shared.clipboard.entries = vec![(ClipFormat::Standard(CF_TEXT), text)];
    shared.clipboard.generation += 1;
    shared.clipboard.handles.clear();
    shared.clipboard.owner = 0;
    shared.clipboard.ole_source = 0;
}

// ── Storing data ─────────────────────────────────────────────────────────

/// Pack a surface as a 24-bit bottom-up DIB (BITMAPINFOHEADER + bits).
pub fn surface_to_dib(surface: &Surface) -> Vec<u8> {
    let info = DibInfo {
        width: surface.width,
        height: surface.height,
        bpp: 24,
        compression: 0,
        size_image: 0,
        header_size: 40,
        colors: Vec::new(),
        color_count: 0,
        masks: None,
        table_va: 0,
    };
    let bits = gdi::encode_dib(&info, surface, 0, surface.height.max(0) as usize);
    let mut out = Vec::with_capacity(40 + bits.len());
    for value in [40u32, surface.width as u32, surface.height as u32] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    for value in [0u32, bits.len() as u32, 3780, 3780, 0, 0] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&bits);
    out
}

/// Decode a packed DIB held in host memory.
pub fn dib_to_surface(bytes: &[u8]) -> Option<Surface> {
    let u32_at = |i: usize| {
        bytes
            .get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let u16_at = |i: usize| {
        bytes
            .get(i..i + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let header_size = u32_at(0)?;
    let (width, height, bpp, compression, clr_used, quad) = if header_size == 12 {
        (
            u16_at(4)? as i16 as i32,
            u16_at(6)? as i16 as i32,
            u16_at(10)?,
            0,
            0,
            false,
        )
    } else {
        (
            u32_at(4)? as i32,
            u32_at(8)? as i32,
            u16_at(14)?,
            u32_at(16)?,
            u32_at(32)?,
            true,
        )
    };
    if width <= 0 || height == 0 || width > 16384 || height.abs() > 16384 {
        return None;
    }
    let mut offset = header_size as usize;
    let mut masks = None;
    if compression == 3 && (bpp == 16 || bpp == 32) {
        masks = Some((u32_at(offset)?, u32_at(offset + 4)?, u32_at(offset + 8)?));
        offset += 12;
    }
    let count = if bpp <= 8 {
        if clr_used == 0 {
            1usize << bpp
        } else {
            clr_used as usize
        }
    } else {
        clr_used as usize
    };
    let entry = if quad { 4 } else { 3 };
    let mut colors = Vec::new();
    if bpp <= 8 {
        for index in 0..count {
            let c = bytes.get(offset + index * entry..offset + index * entry + 3)?;
            colors.push((c[2] as u32) << 16 | (c[1] as u32) << 8 | c[0] as u32);
        }
    }
    offset += if bpp <= 8 { count * entry } else { count * 4 };
    let info = DibInfo {
        width,
        height,
        bpp,
        compression,
        size_image: bytes.len().saturating_sub(offset) as u32,
        header_size,
        colors: colors.clone(),
        color_count: colors.len(),
        masks,
        table_va: 0,
    };
    let mut surface = Surface::new(width, height.abs(), 24);
    gdi::decode_dib(
        &info,
        bytes.get(offset..)?,
        &mut surface,
        0,
        height.unsigned_abs() as usize,
    );
    Some(surface)
}

fn bitmap_surface(ctx: &ApiContext, handle: u32) -> Option<Surface> {
    ctx.shared.borrow().gdi.bitmap_surface(handle).cloned()
}

/// Bytes of a global memory block (our handles are pointers).
fn global_bytes<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    handle: u32,
) -> Option<Vec<u8>> {
    let size = ctx.shared.borrow().heap.size_of(handle)?;
    memory.read_bytes(handle, size as usize)
}

fn text_of(bytes: &[u8]) -> Vec<u8> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    bytes[..end].to_vec()
}

/// Remember the host clipboard's current text without adopting it, so only
/// later host changes replace what the guest is about to store.
fn snapshot_host<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &M) {
    if !host_enabled(memory) {
        return;
    }
    let output = std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-o"])
        .stderr(std::process::Stdio::null())
        .output();
    let mut shared = ctx.shared.borrow_mut();
    shared.clipboard.host_text = output.ok().filter(|o| o.status.success()).map(|o| o.stdout);
    shared.clipboard.host_polled = Some(std::time::Instant::now());
}

/// Remove all data (EmptyClipboard).
pub fn empty<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) {
    snapshot_host(ctx, memory);
    let (handles, source) = {
        let mut shared = ctx.shared.borrow_mut();
        let clipboard = &mut shared.clipboard;
        clipboard.entries.clear();
        clipboard.generation += 1;
        let handles: Vec<(u32, u32)> = clipboard.handles.drain().collect();
        let source = std::mem::take(&mut clipboard.ole_source);
        (handles, source)
    };
    for (cf, handle) in handles {
        free_handle(ctx, cf, handle);
    }
    if source != 0 {
        com_call(memory, source, 2, &[]);
    }
}

fn free_handle(ctx: &ApiContext, cf: u32, handle: u32) {
    let mut shared = ctx.shared.borrow_mut();
    if cf == CF_BITMAP {
        shared.gdi.objects.remove(&handle);
    } else {
        shared.heap.free(handle);
    }
}

/// Store data for `cf` given a guest handle (SetClipboardData).
pub fn set_data<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    cf: u32,
    handle: u32,
) -> bool {
    if handle == 0 {
        return false;
    }
    let (format, bytes) = match cf {
        CF_BITMAP => match bitmap_surface(ctx, handle) {
            Some(surface) => (ClipFormat::Standard(CF_DIB), surface_to_dib(&surface)),
            None => return false,
        },
        CF_TEXT | CF_OEMTEXT => match global_bytes(ctx, memory, handle) {
            Some(bytes) => (ClipFormat::Standard(CF_TEXT), text_of(&bytes)),
            None => return false,
        },
        CF_UNICODETEXT => match global_bytes(ctx, memory, handle) {
            Some(bytes) => {
                let text: Vec<u8> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|u| *u != 0)
                    .map(|u| if u < 0x100 { u as u8 } else { b'?' })
                    .collect();
                (ClipFormat::Standard(CF_TEXT), text)
            }
            None => return false,
        },
        _ => match global_bytes(ctx, memory, handle) {
            Some(bytes) => (format_of(ctx, cf), bytes),
            None => return false,
        },
    };
    let is_text = format == ClipFormat::Standard(CF_TEXT);
    {
        let mut shared = ctx.shared.borrow_mut();
        let clipboard = &mut shared.clipboard;
        clipboard.entries.retain(|(f, _)| *f != format);
        clipboard.entries.push((format, bytes.clone()));
        clipboard.generation += 1;
        // The caller's handle now belongs to the clipboard.
        clipboard.handles.insert(cf, handle);
    }
    if is_text && host_enabled(memory) {
        push_host(ctx, &bytes);
    }
    true
}

/// Put text on the clipboard directly (used by the EDIT control).
pub fn set_text<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    owner: u32,
    text: &[u8],
) {
    empty(ctx, memory);
    {
        let mut shared = ctx.shared.borrow_mut();
        shared
            .clipboard
            .entries
            .push((ClipFormat::Standard(CF_TEXT), text.to_vec()));
        shared.clipboard.generation += 1;
        shared.clipboard.owner = owner;
    }
    if host_enabled(memory) {
        push_host(ctx, text);
    }
}

/// Clipboard text, if any (used by the EDIT control).
pub fn get_text<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &M) -> Option<Vec<u8>> {
    poll_host(ctx, memory);
    stored(ctx, &ClipFormat::Standard(CF_TEXT))
}

/// Allocate a fresh guest handle holding the data for `cf`.
pub fn render<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, cf: u32) -> Option<u32> {
    poll_host(ctx, memory);
    match cf {
        CF_BITMAP => {
            let bytes = stored(ctx, &ClipFormat::Standard(CF_DIB))?;
            let surface = dib_to_surface(&bytes)?;
            Some(
                ctx.shared
                    .borrow_mut()
                    .gdi
                    .insert(GdiObject::Bitmap(Bitmap { surface, dib: None })),
            )
        }
        CF_TEXT | CF_OEMTEXT | CF_UNICODETEXT => {
            let text = stored(ctx, &ClipFormat::Standard(CF_TEXT))?;
            let bytes: Vec<u8> = if cf == CF_UNICODETEXT {
                text.iter().flat_map(|b| [*b, 0]).chain([0, 0]).collect()
            } else {
                text.iter().copied().chain([0]).collect()
            };
            let handle = ctx.shared.borrow_mut().heap.alloc(bytes.len() as u32)?;
            memory.write_bytes(handle, &bytes)?;
            Some(handle)
        }
        _ => {
            let bytes = stored(ctx, &format_of(ctx, cf))?;
            let handle = ctx
                .shared
                .borrow_mut()
                .heap
                .alloc(bytes.len().max(1) as u32)?;
            memory.write_bytes(handle, &bytes)?;
            Some(handle)
        }
    }
}

/// GetClipboardData: the handle stays owned by the clipboard.
pub fn get_data<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, cf: u32) -> u32 {
    if !is_available(ctx, memory, cf) {
        return 0;
    }
    if let Some(handle) = ctx.shared.borrow().clipboard.handles.get(&cf).copied() {
        return handle;
    }
    match render(ctx, memory, cf) {
        Some(handle) => {
            ctx.shared.borrow_mut().clipboard.handles.insert(cf, handle);
            handle
        }
        None => 0,
    }
}

// ── OLE data objects ─────────────────────────────────────────────────────

/// Call method `index` of a COM interface (stdcall, `this` first).
pub fn com_call<M: GuestMemory + ?Sized>(
    memory: &mut M,
    object: u32,
    index: u32,
    args: &[u32],
) -> Option<u32> {
    let vtable = memory.read_u32(object)?;
    let method = memory.read_u32(vtable + index * 4)?;
    let mut call_args = Vec::with_capacity(args.len() + 1);
    call_args.push(object);
    call_args.extend_from_slice(args);
    memory.call_guest(method, &call_args)
}

/// Release a STGMEDIUM (ReleaseStgMedium).
pub fn release_medium<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, medium: u32) {
    let tymed = memory.read_u32(medium).unwrap_or(0);
    let handle = memory.read_u32(medium + 4).unwrap_or(0);
    let release = memory.read_u32(medium + 8).unwrap_or(0);
    if release != 0 {
        com_call(memory, release, 2, &[]);
    } else if handle != 0 {
        match tymed {
            TYMED_HGLOBAL => {
                ctx.shared.borrow_mut().heap.free(handle);
            }
            TYMED_GDI => {
                ctx.shared.borrow_mut().gdi.objects.remove(&handle);
            }
            _ => {}
        }
    }
    let _ = memory.write_bytes(medium, &[0u8; 12]);
}

/// Copy everything a guest IDataObject offers into the clipboard
/// (OleSetClipboard). Only memory and GDI media can be stored.
pub fn capture_ole<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, object: u32) {
    let scratch = ctx.shared.borrow_mut().heap.alloc(64).unwrap_or(0);
    if scratch == 0 {
        return;
    }
    let (fmt, medium, enum_out, fetched) = (scratch, scratch + 20, scratch + 32, scratch + 36);
    // Ask the object which formats it offers; fall back to the usual ones.
    let mut offered: Vec<(u32, u32)> = Vec::new();
    let _ = memory.write_u32(enum_out, 0);
    if com_call(memory, object, 8, &[1, enum_out]) == Some(0) {
        let enumerator = memory.read_u32(enum_out).unwrap_or(0);
        if enumerator != 0 {
            for _ in 0..32 {
                let _ = memory.write_u32(fetched, 0);
                let result = com_call(memory, enumerator, 3, &[1, fmt, fetched]);
                if result != Some(0) || memory.read_u32(fetched) != Some(1) {
                    break;
                }
                let cf = memory
                    .read_bytes(fmt, 2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]) as u32)
                    .unwrap_or(0);
                let tymed = memory.read_u32(fmt + 16).unwrap_or(0);
                offered.push((cf, tymed));
            }
            com_call(memory, enumerator, 2, &[]);
        }
    }
    if offered.is_empty() {
        offered = vec![
            (CF_DIB, TYMED_HGLOBAL),
            (CF_BITMAP, TYMED_GDI),
            (CF_TEXT, TYMED_HGLOBAL),
        ];
    }
    let mut captured: Vec<(ClipFormat, Vec<u8>)> = Vec::new();
    for (cf, tymed) in offered {
        let want = if cf == CF_BITMAP {
            TYMED_GDI
        } else {
            TYMED_HGLOBAL
        };
        if tymed & want == 0 {
            continue;
        }
        let mut request = Vec::with_capacity(20);
        request.extend_from_slice(&(cf as u16).to_le_bytes());
        request.extend_from_slice(&[0, 0]);
        for value in [0u32, 1, u32::MAX, want] {
            request.extend_from_slice(&value.to_le_bytes());
        }
        let _ = memory.write_bytes(fmt, &request);
        let _ = memory.write_bytes(medium, &[0u8; 12]);
        if com_call(memory, object, 3, &[fmt, medium]) != Some(0) {
            continue;
        }
        let handle = memory.read_u32(medium + 4).unwrap_or(0);
        if std::env::var_os("OTW_TRACE_CLIPBOARD").is_some() {
            eprintln!(
                "[clipboard] GetData cf={cf} tymed={} handle={handle:#x}",
                memory.read_u32(medium).unwrap_or(0)
            );
        }
        let entry = match (cf, memory.read_u32(medium).unwrap_or(0)) {
            (CF_BITMAP, TYMED_GDI) => bitmap_surface(ctx, handle)
                .map(|s| (ClipFormat::Standard(CF_DIB), surface_to_dib(&s))),
            (CF_TEXT | CF_OEMTEXT, TYMED_HGLOBAL) => global_bytes(ctx, memory, handle)
                .map(|b| (ClipFormat::Standard(CF_TEXT), text_of(&b))),
            (_, TYMED_HGLOBAL) => {
                global_bytes(ctx, memory, handle).map(|b| (format_of(ctx, cf), b))
            }
            _ => None,
        };
        release_medium(ctx, memory, medium);
        if let Some((format, bytes)) = entry {
            if !captured.iter().any(|(f, _)| *f == format) {
                captured.push((format, bytes));
            }
        }
    }
    ctx.shared.borrow_mut().heap.free(scratch);
    if std::env::var_os("OTW_TRACE_CLIPBOARD").is_some() {
        for (format, bytes) in &captured {
            eprintln!(
                "[clipboard] captured {format:?}: {} bytes, head {:02x?}",
                bytes.len(),
                &bytes[..bytes.len().min(48)]
            );
        }
    }
    let text = captured
        .iter()
        .find(|(f, _)| *f == ClipFormat::Standard(CF_TEXT))
        .map(|(_, b)| b.clone());
    {
        let mut shared = ctx.shared.borrow_mut();
        shared.clipboard.entries = captured;
        shared.clipboard.generation += 1;
        shared.clipboard.ole_source = object;
    }
    if let Some(text) = text {
        if host_enabled(memory) {
            push_host(ctx, &text);
        }
    }
}

// ── USER32 API ───────────────────────────────────────────────────────────

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "CloseClipboard"
        | "EmptyClipboard"
        | "CountClipboardFormats"
        | "GetClipboardOwner"
        | "GetOpenClipboardWindow" => 0,
        "OpenClipboard"
        | "GetClipboardData"
        | "IsClipboardFormatAvailable"
        | "EnumClipboardFormats" => 1,
        "SetClipboardData" | "GetPriorityClipboardFormat" => 2,
        "GetClipboardFormatNameA" => 3,
        _ => return None,
    })
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
    count: usize,
) -> Option<ApiOutcome> {
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    match name {
        "OpenClipboard" => {
            let mut shared = ctx.shared.borrow_mut();
            if shared.clipboard.open != 0 && shared.clipboard.open != args[0].max(1) {
                return ok(0);
            }
            shared.clipboard.open = args[0].max(1);
            ok(1)
        }
        "CloseClipboard" => {
            ctx.shared.borrow_mut().clipboard.open = 0;
            ok(1)
        }
        "GetOpenClipboardWindow" => ok(ctx.shared.borrow().clipboard.open),
        "EmptyClipboard" => {
            let opener = ctx.shared.borrow().clipboard.open;
            if opener == 0 {
                return ok(0);
            }
            empty(ctx, memory);
            ctx.shared.borrow_mut().clipboard.owner = opener;
            ok(1)
        }
        "SetClipboardData" => ok(if set_data(ctx, memory, args[0], args[1]) {
            args[1]
        } else {
            0
        }),
        "GetClipboardData" => ok(get_data(ctx, memory, args[0])),
        "IsClipboardFormatAvailable" => ok(is_available(ctx, memory, args[0]) as u32),
        "CountClipboardFormats" => ok(available_formats(ctx, memory).len() as u32),
        "EnumClipboardFormats" => {
            let formats = available_formats(ctx, memory);
            let next = if args[0] == 0 {
                formats.first().copied()
            } else {
                formats
                    .iter()
                    .position(|f| *f == args[0])
                    .and_then(|p| formats.get(p + 1))
                    .copied()
            };
            ok(next.unwrap_or(0))
        }
        "GetClipboardOwner" => ok(ctx.shared.borrow().clipboard.owner),
        "GetPriorityClipboardFormat" => {
            let formats = available_formats(ctx, memory);
            if formats.is_empty() {
                return ok(0);
            }
            for index in 0..args[1].min(64) {
                let cf = memory.read_u32(args[0] + index * 4).unwrap_or(0);
                if formats.contains(&cf) {
                    return ok(cf);
                }
            }
            ok(u32::MAX)
        }
        "GetClipboardFormatNameA" => match format_of(ctx, args[0]) {
            ClipFormat::Named(name) => {
                let mut bytes = name.into_bytes();
                bytes.truncate((args[2] as usize).saturating_sub(1));
                let _ = write_cstr(memory, args[1], &bytes);
                ok(bytes.len() as u32)
            }
            ClipFormat::Standard(_) => ok(0),
        },
        _ => {
            let _ = read_cstr::<M>;
            None
        }
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
    fn dib_round_trip() {
        let mut surface = Surface::new(5, 3, 24);
        surface.put(0, 0, 0xFF0000);
        surface.put(4, 2, 0x00FF00);
        let dib = surface_to_dib(&surface);
        let back = dib_to_surface(&dib).unwrap();
        assert_eq!(back.get(0, 0), Some(0xFF0000));
        assert_eq!(back.get(4, 2), Some(0x00FF00));
    }

    #[test]
    fn host_text_conversion_uses_crlf() {
        assert_eq!(from_host_text("a\nb"), b"a\r\nb");
        assert_eq!(to_host_text(b"a\r\nb"), "a\nb");
    }
}
