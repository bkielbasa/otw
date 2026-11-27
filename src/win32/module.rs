//! DLLs loaded while a program runs: `LoadLibrary`, `GetProcAddress` and
//! `GetModuleFileName` for them.
//!
//! A genuine DLL is found on drive C: the way Windows searches (the
//! program's directory, the current directory, `C:\WINDOWS\SYSTEM`,
//! `C:\WINDOWS`), mapped at its preferred base or relocated, its imports
//! bound to other genuine DLLs (loaded the same way) or to the subsystem's
//! own functions, and its DllMain called.
//!
//! The system DLLs the subsystem reimplements (KERNEL32, USER32, OLE32 and
//! so on) are never loaded from disk, even when a genuine copy is there:
//! their handles are synthetic and their exports are dynamic thunks.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::{
    kernel::{
        loader,
        pe::{ImportName, PeImage},
    },
    win32::{crt, kernel32, prefer_rust_handler, ApiContext, GuestMemory, LoadedModule},
};

/// DLLs that only the subsystem provides. (COMCTL32 is the subsystem's
/// unless a genuine copy is installed, as Internet Explorer does.)
pub fn subsystem_only(dll: &str) -> bool {
    let name = dll.to_ascii_lowercase();
    let name = name.rsplit(['\\', '/']).next().unwrap_or(&name);
    let stem = name
        .strip_suffix(".dll")
        .or_else(|| name.strip_suffix(".drv"))
        .unwrap_or(name);
    matches!(
        stem,
        "kernel32"
            | "user32"
            | "gdi32"
            | "advapi32"
            | "shell32"
            | "comdlg32"
            | "ole32"
            | "winspool"
            | "wsock32"
            | "winsock"
            | "msvcrt20"
            | "rpcrt4"
            | "advpack"
            | "version"
            | "mpr"
            | "riched32"
            | "lz32"
            | "imm32"
            | "uxtheme"
            | "dwmapi"
            | "bcrypt"
            | "crypt32"
            | "wintrust"
            | "sensapi"
            | "msimg32"
            | "dbghelp"
            | "ntdll"
    )
}

/// Give a just-mapped module's `__declspec(thread)` data a TLS index and
/// a copy in every thread. Returns its TLS callbacks.
fn setup_tls<M: GuestMemory + ?Sized>(memory: &mut M, base: u32) -> Vec<u32> {
    let Some(pe) = memory.read_u32(base + 0x3C) else { return Vec::new() };
    let x64 = memory.read_bytes(base + pe + 24, 2).is_some_and(|m| m == [0x0B, 0x02]);
    let directory = base + pe + 24 + if x64 { 112 } else { 96 } + 9 * 8;
    let rva = memory.read_u32(directory).unwrap_or(0);
    if rva == 0 {
        return Vec::new();
    }
    let width = if x64 { 8 } else { 4 };
    let field = |memory: &mut M, index: u32| memory.read_u32(base + rva + index * width).unwrap_or(0);
    let (start, end, index_va, callbacks) = (field(memory, 0), field(memory, 1), field(memory, 2), field(memory, 3));
    let zero_fill = memory.read_u32(base + rva + 4 * width).unwrap_or(0);
    let mut template = if end > start { memory.read_bytes(start, (end - start) as usize).unwrap_or_default() } else { Vec::new() };
    template.extend(std::iter::repeat_n(0, zero_fill as usize));
    if let Some(index) = memory.add_tls(&template) {
        let _ = memory.write_u32(index_va, index);
    }
    let mut list = Vec::new();
    if callbacks != 0 {
        for slot in 0..64 {
            let callback = memory.read_ptr(callbacks + slot * width).unwrap_or(0) as u32;
            if callback == 0 {
                break;
            }
            list.push(callback);
        }
    }
    list
}

/// `LOAD_LIBRARY_AS_DATAFILE`: map for resources only.
pub const AS_DATAFILE: u32 = 2;

/// Load (or find) a DLL. Returns its module handle, or the Win32 error.
pub fn load_library<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    requested: &str,
    flags: u32,
) -> Result<u32, u32> {
    let requested = requested.trim();
    // An API set name (api-ms-win-*) is its host DLL.
    if let Some(host) = crate::win32::api_set_host(&file_name(requested)) {
        if let Some(handle) = kernel32::system_module_handle(host) {
            return Ok(handle);
        }
    }
    let file = file_name(requested);
    if let Some(base) = find_loaded(ctx, &file) {
        return Ok(base);
    }
    if subsystem_only(&file) {
        return kernel32::system_module_handle(&file).ok_or(126);
    }
    let Some((guest_path, bytes)) = find_file(ctx, requested) else {
        // A DLL with Rust handlers but no genuine copy (SHLWAPI, OLEAUT32).
        return kernel32::system_module_handle(&file).ok_or(126);
    };
    let image = PeImage::parse(bytes).map_err(|_| 193u32)?; // ERROR_BAD_EXE_FORMAT
    // A 64-bit process loads only 64-bit DLLs, a 32-bit one 32-bit DLLs;
    // a system DLL of the other kind is the subsystem's own.
    if image.is64 != (memory.ptr_size() == 8) {
        return kernel32::system_module_handle(&file).ok_or(193);
    }
    let mut mapped = loader::map_image(&file, &image).map_err(|_| 193u32)?;
    let base = memory
        .map_region(image.image_base, mapped.len() as u32)
        .ok_or(8u32)?; // ERROR_NOT_ENOUGH_MEMORY
    if base != image.image_base {
        if image.is64 {
            loader::relocate64(&mut mapped, image.relocations, (base as u64).wrapping_sub(image.image_base as u64));
        } else {
            loader::relocate(&mut mapped, image.relocations, base.wrapping_sub(image.image_base));
        }
    }
    memory.write_bytes(base, &mapped).ok_or(8u32)?;
    let mut module = LoadedModule::at(file.clone(), &image, base);
    module.path = guest_path;
    ctx.modules.borrow_mut().push(module);
    if trace() {
        eprintln!("[module] {file} at {base:#010x}");
    }
    if flags & AS_DATAFILE != 0 {
        return Ok(base);
    }

    for dll in &image.imports {
        let target = if subsystem_only(&dll.dll) {
            None
        } else {
            match load_library(ctx, memory, &dll.dll, 0) {
                Ok(handle) if is_genuine(ctx, handle) => Some(handle),
                _ => None,
            }
        };
        for (index, name) in dll.names.iter().enumerate() {
            let genuine = target
                .filter(|_| !prefer_rust_handler(&dll.dll, name))
                .and_then(|handle| proc_address(ctx, memory, handle, name, 0));
            if genuine.is_none() && trace() && !crate::win32::has_implementation(&dll.dll, name) {
                eprintln!("[module] {file}: missing {}!{}", dll.dll, name.display());
            }
            let address = genuine
                .or_else(|| name.as_str().and_then(|n| crate::win32::native::address(ctx, memory, &dll.dll, n, || Some(import_thunk(ctx, &dll.dll, name)))))
                .unwrap_or_else(|| import_thunk(ctx, &dll.dll, name));
            let size = memory.ptr_size();
            let slot = base
                .wrapping_add(dll.first_thunk_rva)
                .wrapping_add(index as u32 * size);
            memory.write_ptr(slot, u64::from(address));
        }
    }

    if file.eq_ignore_ascii_case("comctl32.dll") {
        crate::win32::user::yield_common_controls(ctx);
    }
    // Static thread-local storage: an index of its own and a copy in every
    // thread; then the TLS callbacks (C++ thread_local initialisation).
    for callback in setup_tls(memory, base) {
        memory.call_guest_wide(callback, &[base as u64, 1, 0]);
    }
    if image.entry_rva != 0 {
        let entry = base.wrapping_add(image.entry_rva);
        let result = memory.call_guest(entry, &[base, 1, 0]);
        if trace() {
            eprintln!("[module] {file} DllMain -> {result:?}");
        }
        if result == Some(0) {
            return Err(1114); // ERROR_DLL_INIT_FAILED
        }
    }
    Ok(base)
}

fn trace() -> bool {
    std::env::var_os("OTW_TRACE_MODULES").is_some()
}

/// A guest address for an import the subsystem handles.
fn import_thunk(ctx: &ApiContext, dll: &str, name: &ImportName) -> u32 {
    let name = match name {
        ImportName::ByName(name) => name.clone(),
        ImportName::Ordinal(ordinal) => format!("#{ordinal}"),
    };
    kernel32::import_thunk(ctx, dll, &name).unwrap_or(0)
}

fn is_genuine(ctx: &ApiContext, handle: u32) -> bool {
    ctx.modules.borrow().iter().any(|m| m.image_base == handle)
}

/// The last path component, with `.dll` added when there is no extension.
fn file_name(requested: &str) -> String {
    let file = requested.rsplit(['\\', '/']).next().unwrap_or(requested);
    if file.contains('.') {
        file.trim_end_matches('.').to_owned()
    } else {
        format!("{file}.dll")
    }
}

fn find_loaded(ctx: &ApiContext, file: &str) -> Option<u32> {
    let stem = |name: &str| {
        let name = name.rsplit(['\\', '/']).next().unwrap_or(name).to_ascii_lowercase();
        name.strip_suffix(".dll").map(str::to_owned).unwrap_or(name)
    };
    let wanted = stem(file);
    ctx.modules
        .borrow()
        .iter()
        .find(|m| stem(&m.name) == wanted)
        .map(|m| m.image_base)
}

/// The DLL's guest path and contents, searched for as Windows does.
fn find_file(ctx: &ApiContext, requested: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let has_dir = requested.contains(['\\', '/', ':']);
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    let last = requested.rsplit(['\\', '/']).next().unwrap_or(requested);
    let mut with_ext = requested.as_bytes().to_vec();
    if !last.contains('.') {
        with_ext.extend_from_slice(b".dll");
    }
    if has_dir {
        candidates.push(crt::absolute(ctx, &with_ext));
    } else {
        let shared = ctx.shared.borrow();
        let program = &shared.program_path;
        if let Some(end) = program.iter().rposition(|&b| b == b'\\') {
            candidates.push(program[..end].to_vec());
        }
        candidates.push(shared.current_dir.clone());
        drop(shared);
        candidates.push(b"C:\\WINDOWS\\SYSTEM".to_vec());
        candidates.push(b"C:\\WINDOWS".to_vec());
        for dir in &mut candidates {
            if dir.last() != Some(&b'\\') {
                dir.push(b'\\');
            }
            dir.extend_from_slice(&with_ext);
        }
    }
    for guest in candidates {
        let Some(host) = crt::host_path_in(ctx, &guest) else {
            continue;
        };
        if let Some(bytes) = read_dll(&host) {
            return Some((guest, bytes));
        }
    }
    None
}

/// Big DLLs as last read, by host path, with the size and time of the file
/// they were read from.
struct CachedFile {
    len: u64,
    modified: Option<std::time::SystemTime>,
    bytes: std::sync::Arc<Vec<u8>>,
}

/// Files from this size up are kept once read; at most this much in all.
const CACHE_FROM: u64 = 1 << 20;
const CACHE_LIMIT: u64 = 1 << 30;

fn file_cache() -> std::sync::MutexGuard<'static, std::collections::HashMap<std::path::PathBuf, CachedFile>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, CachedFile>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner())
}

/// A DLL's bytes. Each process loads its DLLs again (each of Firefox's
/// content processes its 180 MB xul.dll, a quarter of a second to read):
/// big ones are read once and copied from memory while the file is
/// unchanged.
fn read_dll(host: &std::path::Path) -> Option<Vec<u8>> {
    let metadata = std::fs::metadata(host).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let (len, modified) = (metadata.len(), metadata.modified().ok());
    if len < CACHE_FROM {
        return std::fs::read(host).ok();
    }
    if let Some(cached) = file_cache().get(host).filter(|c| c.len == len && c.modified == modified) {
        let bytes = cached.bytes.clone();
        return Some(bytes.as_ref().clone());
    }
    let bytes = std::fs::read(host).ok()?;
    let mut cache = file_cache();
    let used: u64 = cache.iter().filter(|(path, _)| path.as_path() != host).map(|(_, c)| c.len).sum();
    if used + len <= CACHE_LIMIT {
        cache.insert(host.to_path_buf(), CachedFile { len, modified, bytes: std::sync::Arc::new(bytes.clone()) });
    }
    Some(bytes)
}

/// An export of a genuine module, following forwarders.
pub fn proc_address<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    handle: u32,
    name: &ImportName,
    depth: usize,
) -> Option<u32> {
    if depth > 8 {
        return None;
    }
    if !is_genuine(ctx, handle) {
        // A DLL the subsystem provides: its functions as dynamic thunks.
        let dll = kernel32::system_module_name(handle)?;
        let name = match name {
            ImportName::ByName(name) => name.clone(),
            ImportName::Ordinal(ordinal) => format!("#{ordinal}"),
        };
        if let Some(native) = crate::win32::native::address(ctx, memory, dll, &name, || kernel32::dynamic_thunk(ctx, dll, &name)) {
            return Some(native);
        }
        return kernel32::dynamic_thunk(ctx, dll, &name);
    }
    let forward = {
        let modules = ctx.modules.borrow();
        let module = modules.iter().find(|m| m.image_base == handle)?;
        let (found, forward) = match name {
            ImportName::ByName(name) => (
                module.exports_by_name.get(name).copied(),
                module.forwards_by_name.get(name).cloned(),
            ),
            ImportName::Ordinal(ordinal) => (
                module.exports_by_ordinal.get(ordinal).copied(),
                module.forwards_by_ordinal.get(ordinal).cloned(),
            ),
        };
        if found.is_some() {
            return found;
        }
        forward?
    };
    let (dll, symbol) = forward.rsplit_once('.')?;
    let symbol = match symbol.strip_prefix('#') {
        Some(ordinal) => ImportName::Ordinal(ordinal.parse().ok()?),
        None => ImportName::ByName(symbol.to_owned()),
    };
    let dll = format!("{dll}.dll");
    if subsystem_only(&dll) {
        return Some(import_thunk(ctx, &dll, &symbol));
    }
    let target = load_library(ctx, memory, &dll, 0).ok()?;
    if is_genuine(ctx, target) {
        proc_address(ctx, memory, target, &symbol, depth + 1)
    } else {
        Some(import_thunk(ctx, &dll, &symbol))
    }
}

/// The guest path of a module loaded at run time.
pub fn module_path(ctx: &ApiContext, handle: u32) -> Option<Vec<u8>> {
    ctx.modules
        .borrow()
        .iter()
        .find(|m| m.image_base == handle && !m.path.is_empty())
        .map(|m| m.path.clone())
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn big_dlls_are_read_once_while_unchanged() {
        let dir = std::env::temp_dir().join(format!("otw-dll-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.dll");
        let first = vec![1u8; CACHE_FROM as usize + 10];
        std::fs::write(&path, &first).unwrap();
        assert_eq!(read_dll(&path).unwrap(), first);
        assert!(file_cache().contains_key(&path));
        assert_eq!(read_dll(&path).unwrap(), first);
        // A changed file (another size) is read again.
        let second = vec![2u8; CACHE_FROM as usize + 20];
        std::fs::write(&path, &second).unwrap();
        assert_eq!(read_dll(&path).unwrap(), second);
        // Small files are not kept.
        let small = dir.join("small.dll");
        std::fs::write(&small, b"MZ").unwrap();
        assert_eq!(read_dll(&small).unwrap(), b"MZ");
        assert!(!file_cache().contains_key(&small));
        assert!(read_dll(&dir).is_none());
        file_cache().remove(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
