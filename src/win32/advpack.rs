//! ADVPACK, the "Advanced INF" installer library that Internet Explorer's
//! DLLs and setup programs use: `RegInstall` registers a DLL from the INF
//! text in its `REGINST` resource; `LaunchINFSection` and
//! `RunSetupCommand` install INF sections from files; and small helpers
//! (file versions, `DelNode`, reboot checks).

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::{
    kernel::pe::ImportName,
    win32::{crt, inf::Inf, module, read_cstr, user, write_cstr, ApiContext, ApiOutcome, GuestMemory},
};

const S_OK: u32 = 0;
const E_FAIL: u32 = 0x8000_4005;
const E_INVALIDARG: u32 = 0x8007_0057;

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "NeedRebootInit" => 0,
        "NeedReboot" => 1,
        "IsNTAdmin" | "DelNode" => 2,
        "DelNodeRunDLL32" => 4,
        "RegInstall" | "ExecuteCab" => 3,
        "GetVersionFromFile" | "GetVersionFromFileEx" | "LaunchINFSection" | "LaunchINFSectionEx" => 4,
        // (window, file list, folder, base name, flags)
        "FileSaveRestore" => 5,
        // (window, title, backup key, root key, subkey, value name, flags)
        "RegSaveRestore" => 7,
        "AdvInstallFile" => 7,
        "RunSetupCommand" | "TranslateInfString" => 8,
        "SetPerUserSecValues" => 1,
        "FileSaveMarkNotExist" => 3,
        "AddDelBackupEntry" | "UserInstStubWrapper" | "UserUnInstStubWrapper" => 4,
        "RegSaveRestoreOnINF" => 7,
        "ExtractFiles" => 6,
        _ => return None,
    })
}

fn call<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, dll: &str, name: &str, args: &[u32]) -> u32 {
    crate::win32::dispatch_with_memory(ctx, memory, dll, &ImportName::ByName(name.into()), args, 0)
        .map(|o| o.value)
        .unwrap_or(0)
}

fn text<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> String {
    if va == 0 {
        String::new()
    } else {
        read_cstr(memory, va).iter().map(|&b| b as char).collect()
    }
}

fn trace() -> bool {
    std::env::var_os("OTW_TRACE_SETUP").is_some()
}

/// Read an INF file from drive C:.
fn read_inf(ctx: &ApiContext, path: &str) -> Option<Inf> {
    let host = crt::host_path_in(ctx, path.as_bytes())?;
    let bytes = std::fs::read(host).ok()?;
    let text: String = bytes.iter().map(|&b| b as char).collect();
    let mut inf = Inf::parse(&text);
    let full = String::from_utf8_lossy(&crt::absolute(ctx, path.as_bytes())).into_owned();
    inf.dir = full.rsplit_once('\\').map(|(dir, _)| dir.to_owned()).unwrap_or_default();
    Some(inf)
}

/// Install a section: registry changes, then the DLLs it registers.
pub fn install_section<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, inf: &Inf, section: &str) {
    if trace() {
        eprintln!("[setup] install section [{section}]");
    }
    let mut inf = inf.clone();
    inf.apply_custom_destinations(section);
    let inf = &inf;
    for command in inf.directive_lines(section, "RunPreSetupCommands") {
        run_command(ctx, memory, &command);
    }
    copy_files(ctx, inf, section);
    inf.install_registry(section);
    // Lines are `path[,flags[,DllInstall argument]]`: flag I calls
    // DllInstall, N skips DllRegisterServer.
    for dll in inf.directive(section, "RegisterOCXs") {
        for line in inf.section(&dll).unwrap_or_default().to_vec() {
            let fields = inf.fields(&line);
            let path = fields.first().cloned().unwrap_or_default();
            let flags = fields.get(1).cloned().unwrap_or_default().to_ascii_uppercase();
            if !flags.contains('N') {
                register_dll(ctx, memory, &path, "DllRegisterServer");
            }
            if flags.contains('I') {
                let argument = fields.get(2).cloned().unwrap_or_default();
                dll_install(ctx, memory, &path, &argument);
            }
        }
    }
    for command in inf.directive_lines(section, "RunPostSetupCommands") {
        run_command(ctx, memory, &command);
    }
}

/// Copy the files of a section's `CopyFiles` lists from the INF's folder.
fn copy_files(ctx: &ApiContext, inf: &Inf, section: &str) {
    for list in inf.directive(section, "CopyFiles") {
        let Some(dir) = inf.destination(&list) else {
            if trace() {
                eprintln!("[setup] {list}: no destination");
            }
            continue;
        };
        for (target, source) in inf.copy_list(&list) {
            let from = format!("{}\\{source}", inf.dir.trim_end_matches('\\'));
            let to = format!("{}\\{target}", dir.trim_end_matches('\\'));
            let (Some(from_host), Some(to_host)) = (crt::host_path_in(ctx, from.as_bytes()), crt::host_path_in(ctx, to.as_bytes())) else {
                continue;
            };
            let source_file = if from_host.is_file() {
                Some(from_host)
            } else {
                // The file may differ in case.
                from_host.parent().and_then(|parent| {
                    std::fs::read_dir(parent).ok()?.flatten().map(|e| e.path()).find(|p| {
                        p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&source))
                    })
                })
            };
            let Some(source_file) = source_file else {
                if trace() {
                    eprintln!("[setup] copy {from}: missing");
                }
                continue;
            };
            if let Some(parent) = to_host.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let result = std::fs::copy(&source_file, &to_host);
            if trace() {
                eprintln!("[setup] copy {from} -> {to}: {}", if result.is_ok() { "ok" } else { "failed" });
            }
        }
    }
}

/// Run a setup command line (a program started as CreateProcess would).
fn run_command<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, command: &str) {
    if trace() {
        eprintln!("[setup] run {command:?}");
    }
    let text = ctx.shared.borrow_mut().heap.alloc(command.len() as u32 + 1).unwrap_or(0);
    let _ = memory.write_bytes(text, &[command.as_bytes(), &[0]].concat());
    let info = ctx.shared.borrow_mut().heap.alloc(16 + 68).unwrap_or(0);
    let _ = memory.write_bytes(info, &[0; 84]);
    let _ = crate::win32::dispatch_with_memory(
        ctx,
        memory,
        "KERNEL32.dll",
        &ImportName::ByName("CreateProcessA".into()),
        &[0, text, 0, 0, 0, 0, 0, 0, info + 16, info],
        0,
    );
    ctx.shared.borrow_mut().heap.free(text);
    ctx.shared.borrow_mut().heap.free(info);
}

/// Load a DLL and call one of its registration entry points.
pub fn register_dll<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, path: &str, entry: &str) -> u32 {
    let Ok(handle) = module::load_library(ctx, memory, path, 0) else {
        if trace() {
            eprintln!("[setup] {path}: not loaded");
        }
        return E_FAIL;
    };
    let Some(function) = module::proc_address(ctx, memory, handle, &ImportName::ByName(entry.into()), 0) else {
        return E_FAIL;
    };
    let result = memory.call_guest(function, &[]).unwrap_or(E_FAIL);
    if trace() {
        eprintln!("[setup] {path} {entry} -> {result:#010x}");
    }
    result
}

/// Call a DLL's `DllInstall(TRUE, argument)`.
pub fn dll_install<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, path: &str, argument: &str) -> u32 {
    let Ok(handle) = module::load_library(ctx, memory, path, 0) else {
        return E_FAIL;
    };
    let Some(function) = module::proc_address(ctx, memory, handle, &ImportName::ByName("DllInstall".into()), 0) else {
        return E_FAIL;
    };
    let wide: Vec<u8> = argument.bytes().flat_map(|b| [b, 0]).chain([0, 0]).collect();
    let text = ctx.shared.borrow_mut().heap.alloc(wide.len() as u32).unwrap_or(0);
    let _ = memory.write_bytes(text, &wide);
    let result = memory.call_guest(function, &[1, text]).unwrap_or(E_FAIL);
    ctx.shared.borrow_mut().heap.free(text);
    if trace() {
        eprintln!("[setup] {path} DllInstall({argument:?}) -> {result:#010x}");
    }
    result
}

/// The INF text in a module's `REGINST` resource.
fn reginst<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, module: u32) -> Option<String> {
    let name = user::ResName::Name("REGINST".into());
    let (va, size) = user::find_resource_any(ctx, memory, module, &name, &name)?;
    let bytes = memory.read_bytes(va, size as usize)?;
    Some(bytes.iter().map(|&b| b as char).collect())
}

/// A file's version (most and least significant DWORDs) from its
/// VS_FIXEDFILEINFO.
fn file_version(bytes: &[u8]) -> Option<(u32, u32)> {
    let signature = 0xFEEF_04BDu32.to_le_bytes();
    let at = bytes.windows(4).position(|w| w == signature)?;
    let read = |offset: usize| {
        bytes
            .get(at + offset..at + offset + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    Some((read(8)?, read(12)?))
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &str,
    args: &[u32],
) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let value = match name {
        "RegInstall" => {
            // (module, section, STRTABLE*): STRTABLE { count, STRENTRY* },
            // STRENTRY { name, value }.
            let section = text(memory, args[1]);
            let Some(source) = reginst(ctx, memory, args[0]) else {
                return Some(ApiOutcome::new(E_FAIL, count));
            };
            let mut inf = Inf::parse(&source);
            if args[2] != 0 {
                let entries = memory.read_u32(args[2]).unwrap_or(0);
                let table = memory.read_u32(args[2] + 4).unwrap_or(0);
                for index in 0..entries.min(4096) {
                    let name_va = memory.read_u32(table + index * 8).unwrap_or(0);
                    let value_va = memory.read_u32(table + index * 8 + 4).unwrap_or(0);
                    let (key, value) = (text(memory, name_va), text(memory, value_va));
                    inf.define(&key, &value);
                }
            }
            let path_va = ctx.shared.borrow_mut().heap.alloc(260).unwrap_or(0);
            call(ctx, memory, "KERNEL32.dll", "GetModuleFileNameA", &[args[0], path_va, 260]);
            let path = text(memory, path_va);
            ctx.shared.borrow_mut().heap.free(path_va);
            inf.define("_SYS_MOD_PATH", &path);
            inf.define("_MOD_PATH", &path);
            if !inf.has_section(&section) {
                return Some(ApiOutcome::new(E_INVALIDARG, count));
            }
            install_section(ctx, memory, &inf, &section);
            S_OK
        }
        "LaunchINFSection" | "LaunchINFSectionEx" => {
            // (window, instance, "file.inf,section,flags", show)
            let command = text(memory, args[2]);
            let parts = crate::win32::inf::fields(&command);
            let (file, section) = (parts.first().cloned().unwrap_or_default(), parts.get(1).cloned());
            let section = section.filter(|s| !s.is_empty()).unwrap_or_else(|| "DefaultInstall".into());
            match read_inf(ctx, &file) {
                Some(inf) => {
                    install_section(ctx, memory, &inf, &section);
                    S_OK
                }
                None => E_FAIL,
            }
        }
        "RunSetupCommand" => {
            // (window, exe or INF, section, directory, title, &process, flags, reserved)
            const RSC_FLAG_INF: u32 = 1;
            let file = text(memory, args[1]);
            let directory = text(memory, args[3]);
            if trace() {
                eprintln!("[setup] RunSetupCommand {file:?} [{}] in {directory:?} flags {:#x}", text(memory, args[2]), args[6]);
            }
            if args[5] != 0 {
                let _ = memory.write_u32(args[5], 0);
            }
            if args[6] & RSC_FLAG_INF != 0 {
                let path = if file.contains('\\') || directory.is_empty() {
                    file.clone()
                } else {
                    format!("{}\\{file}", directory.trim_end_matches('\\'))
                };
                let section = text(memory, args[2]);
                let section = if section.is_empty() { "DefaultInstall".to_owned() } else { section };
                match read_inf(ctx, &path) {
                    Some(inf) => {
                        install_section(ctx, memory, &inf, &section);
                        S_OK
                    }
                    None => E_FAIL,
                }
            } else {
                eprintln!("[setup] RunSetupCommand {file:?} (programs cannot be started yet)");
                E_FAIL
            }
        }
        "TranslateInfString" => {
            // (inf, install section, translate section, key, buffer, size, &needed, reserved)
            let file = text(memory, args[0]);
            let (section, key) = (text(memory, args[2]), text(memory, args[3]));
            let install = text(memory, args[1]);
            let value = read_inf(ctx, &file).and_then(|mut inf| {
                if !install.is_empty() {
                    inf.apply_custom_destinations(&install);
                }
                inf.value(&section, &key)
            });
            let Some(value) = value else {
                return Some(ApiOutcome::new(0x8007_0002, count)); // file or key not found
            };
            let needed = value.len() as u32 + 1;
            if args[6] != 0 {
                let _ = memory.write_u32(args[6], needed);
            }
            if args[4] == 0 {
                S_OK
            } else if args[5] < needed {
                0x8007_007A // ERROR_INSUFFICIENT_BUFFER
            } else {
                let _ = write_cstr(memory, args[4], value.as_bytes());
                S_OK
            }
        }
        "GetVersionFromFile" | "GetVersionFromFileEx" => {
            // (path, &major, &minor, version or language)
            let path = text(memory, args[0]);
            let bytes = crt::host_path_in(ctx, path.as_bytes()).and_then(|host| std::fs::read(host).ok());
            let (major, minor) = if args[3] != 0 {
                bytes.as_deref().and_then(file_version).unwrap_or((0, 0))
            } else {
                (0x0409, 1252) // language and code page
            };
            let _ = memory.write_u32(args[1], major);
            let _ = memory.write_u32(args[2], minor);
            S_OK
        }
        "DelNode" => {
            let path = text(memory, args[0]);
            match crt::host_path_in(ctx, path.as_bytes()) {
                Some(host) if host.is_dir() => {
                    let _ = std::fs::remove_dir_all(host);
                    S_OK
                }
                Some(host) => {
                    let _ = std::fs::remove_file(host);
                    S_OK
                }
                None => E_FAIL,
            }
        }
        "ExtractFiles" => {
            // (cabinet, folder, flags, "file:file" or NULL, reserved, reserved)
            let cabinet = text(memory, args[0]);
            let folder = text(memory, args[1]);
            let wanted: Option<Vec<String>> = (args[3] != 0).then(|| {
                text(memory, args[3]).split(':').filter(|n| !n.is_empty()).map(str::to_owned).collect()
            });
            let (Some(cab_host), Some(dir_host)) = (crt::host_path_in(ctx, cabinet.as_bytes()), crt::host_path_in(ctx, folder.as_bytes())) else {
                return Some(ApiOutcome::new(E_FAIL, count));
            };
            let _ = std::fs::create_dir_all(&dir_host);
            match crate::win32::cab::CabSet::open(&cab_host).and_then(|set| set.extract(&dir_host, wanted.as_deref())) {
                Ok(names) => {
                    if trace() {
                        eprintln!("[setup] extracted {} files from {cabinet} to {folder}", names.len());
                    }
                    S_OK
                }
                Err(error) => {
                    eprintln!("[setup] {cabinet}: {error}");
                    E_FAIL
                }
            }
        }
        "DelNodeRunDLL32" => {
            // rundll32 form: (window, instance, "path,flags", show)
            let command = text(memory, args[2]);
            let path = command.split(',').next().unwrap_or("").trim().to_owned();
            match crt::host_path_in(ctx, path.as_bytes()) {
                Some(host) if host.is_dir() => {
                    let _ = std::fs::remove_dir_all(host);
                    S_OK
                }
                Some(host) => {
                    let _ = std::fs::remove_file(host);
                    S_OK
                }
                None => E_FAIL,
            }
        }
        "IsNTAdmin" => 1,
        "NeedRebootInit" | "NeedReboot" => 0,
        // Backups for uninstalling are not kept.
        "RegSaveRestore" | "FileSaveRestore" | "FileSaveMarkNotExist" | "AddDelBackupEntry"
        | "RegSaveRestoreOnINF" | "SetPerUserSecValues" | "UserInstStubWrapper" | "UserUnInstStubWrapper" => S_OK,
        "ExecuteCab" => {
            // CABINFO { cab, inf, section, source path[260], flags }
            let info = args[1];
            let pointers: Vec<u32> = (0..3).map(|i| memory.read_u32(info + i * 4).unwrap_or(0)).collect();
            let cab = text(memory, pointers[0]);
            let inf_name = text(memory, pointers[1]);
            let section = text(memory, pointers[2]);
            let source = text(memory, info + 12);
            let section = if section.is_empty() { "DefaultInstall".to_owned() } else { section };
            let inf_path = if inf_name.contains('\\') || source.is_empty() {
                inf_name.clone()
            } else {
                format!("{}\\{inf_name}", source.trim_end_matches('\\'))
            };
            if trace() {
                eprintln!("[setup] ExecuteCab {cab:?} {inf_path:?} [{section}]");
            }
            // The INF may still be in the cabinet.
            let missing = crt::host_path_in(ctx, inf_path.as_bytes()).is_none_or(|host| !host.is_file());
            if missing && !cab.is_empty() {
                if let (Some(cab_host), Some(dir)) = (crt::host_path_in(ctx, cab.as_bytes()), crt::host_path_in(ctx, source.as_bytes())) {
                    let wanted = [inf_name.rsplit('\\').next().unwrap_or(&inf_name).to_owned()];
                    let _ = crate::win32::cab::CabSet::open(&cab_host).and_then(|set| set.extract(&dir, Some(&wanted)));
                }
            }
            match read_inf(ctx, &inf_path) {
                Some(inf) => {
                    install_section(ctx, memory, &inf, &section);
                    S_OK
                }
                None => E_FAIL,
            }
        }
        "AdvInstallFile" => E_FAIL,
        _ => return None,
    };
    Some(ApiOutcome::new(value, count))
}
