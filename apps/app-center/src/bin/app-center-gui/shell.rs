//! Folders and opening files through the shell.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Where downloads go when the shell does not say.
pub const FALLBACK_DOWNLOADS: &str = "C:\\My Documents\\Downloads";

/// A NUL-terminated UTF-16 string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The user's Downloads folder (FOLDERID_Downloads), or
/// `C:\My Documents\Downloads`.
pub fn downloads_dir() -> PathBuf {
    let mut out: *mut u16 = std::ptr::null_mut();
    // SAFETY: SHGetKnownFolderPath writes a CoTaskMemAlloc'd, NUL-terminated
    // string to `out` on success; it is read and freed here.
    let found = unsafe {
        let hr = SHGetKnownFolderPath(&FOLDERID_Downloads, 0, 0, &mut out);
        let path = if hr >= 0 && !out.is_null() {
            let mut n = 0;
            while *out.add(n) != 0 {
                n += 1;
            }
            Some(String::from_utf16_lossy(std::slice::from_raw_parts(out, n)))
        } else {
            None
        };
        if !out.is_null() {
            CoTaskMemFree(out as *const _);
        }
        path
    };
    found
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(FALLBACK_DOWNLOADS))
}

fn execute(hwnd: HWND, file: &[u16], params: Option<&[u16]>, dir: Option<&[u16]>) -> isize {
    let open = wide("open");
    // SAFETY: all strings are NUL-terminated and live across the call.
    unsafe {
        ShellExecuteW(
            hwnd,
            open.as_ptr(),
            file.as_ptr(),
            params.map_or(std::ptr::null(), |p| p.as_ptr()),
            dir.map_or(std::ptr::null(), |d| d.as_ptr()),
            SW_SHOWNORMAL,
        )
    }
}

fn check(code: isize, what: &str) -> Result<(), String> {
    if code > 32 {
        Ok(())
    } else {
        Err(format!("Couldn't open {what} (error {code})"))
    }
}

/// Opens a downloaded file the way Explorer would: an .exe is started.
pub fn open_file(hwnd: HWND, path: &Path) -> Result<(), String> {
    let dir = path.parent().map(wide_path);
    check(
        execute(hwnd, &wide_path(path), None, dir.as_deref()),
        &path.display().to_string(),
    )
}

/// Opens Explorer on the file's folder with the file selected, or just the
/// folder if that fails.
pub fn show_in_folder(hwnd: HWND, path: &Path) -> Result<(), String> {
    let params = wide(&format!("/select,\"{}\"", path.display()));
    if execute(hwnd, &wide("explorer.exe"), Some(&params), None) > 32 {
        return Ok(());
    }
    let folder = path.parent().unwrap_or(path);
    check(
        execute(hwnd, &wide_path(folder), None, None),
        &folder.display().to_string(),
    )
}

pub fn open_url(hwnd: HWND, url: &str) -> Result<(), String> {
    check(execute(hwnd, &wide(url), None, None), url)
}
