//! Generic Win95 desktop programs and process launching.
//!
//! The desktop is driven by the contents of the mounted FAT32 volume: every
//! executable file is a shortcut. Nothing here names a particular Microsoft
//! program. Double-clicking a shortcut reads the genuine bytes from the image,
//! discovers the DLLs that binary imports, and maps them into a fresh guest VM.

use std::{
    cell::RefCell,
    collections::{HashSet, VecDeque},
    rc::Rc,
};

use crate::{
    fat32::Fat32FileSystem,
    kernel::pe::PeImage,
    session::Session,
    shell::volume::{parse_path, Volume},
    vm::Vm,
    win32::{is_rust_backed_dll, SharedState},
};

/// A program icon discovered on the mounted FAT32 volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopShortcut {
    /// Display name from the FAT32 directory entry.
    pub name: String,
    /// Full path inside the FAT32 volume.
    pub path: String,
    /// How this program must be started.
    pub kind: ProgramKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramKind {
    /// 32-bit Windows PE, executable by the Unicorn backend.
    Pe32,
    /// 16-bit DOS command file. Recognised so it can be reported honestly.
    DosCom,
}

/// A request produced by a double-click and consumed outside the event callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub name: String,
    pub path: String,
    /// Command-line arguments after the program name.
    pub arguments: String,
}

impl From<&DesktopShortcut> for LaunchRequest {
    fn from(shortcut: &DesktopShortcut) -> Self {
        Self {
            name: shortcut.name.clone(),
            path: shortcut.path.clone(),
            arguments: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchError {
    NotFound(String),
    Fat32(String),
    Unsupported(String),
    Pe(String),
    Vm(String),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "file not found on the disk image: {path}"),
            Self::Fat32(message) => write!(f, "disk image error: {message}"),
            Self::Unsupported(name) => {
                write!(
                    f,
                    "{name} is a DOS .COM program; this VM runs 32-bit PE files"
                )
            }
            Self::Pe(message) => write!(f, "invalid PE image: {message}"),
            Self::Vm(message) => write!(f, "failed to build guest VM: {message}"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Return true for the binary formats the desktop should expose.
pub fn is_executable_name(name: &str) -> bool {
    matches!(extension(name).as_deref(), Some("EXE") | Some("COM"))
}

fn extension(name: &str) -> Option<String> {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_uppercase())
        .filter(|ext| ext == "EXE" || ext == "COM")
}

fn program_kind(name: &str) -> ProgramKind {
    match extension(name).as_deref() {
        Some("COM") => ProgramKind::DosCom,
        _ => ProgramKind::Pe32,
    }
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() || parent == "/" || parent == "\\" {
        name.to_string()
    } else {
        format!("{parent}\\{name}")
    }
}

fn parent_path(path: &str) -> String {
    path.rsplit_once(['\\', '/'])
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

/// Recursively collect every executable on the volume. Directory names and
/// file names come from the image, not from a hardcoded shortcut list.
pub fn scan_executables(fs: &Fat32FileSystem) -> Vec<DesktopShortcut> {
    let mut programs = Vec::new();
    scan_directory(fs, "/", &mut programs, 0);
    programs.sort_by(|a, b| {
        a.path
            .to_ascii_lowercase()
            .cmp(&b.path.to_ascii_lowercase())
    });
    programs
}

fn scan_directory(fs: &Fat32FileSystem, path: &str, out: &mut Vec<DesktopShortcut>, depth: usize) {
    if depth > 16 {
        return;
    }
    let Ok(entries) = fs.list(path) else {
        return;
    };
    for entry in entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        let child = join_path(path, &entry.name);
        if entry.is_directory() {
            scan_directory(fs, &child, out, depth + 1);
        } else if is_executable_name(&entry.short_name) || is_executable_name(&entry.name) {
            out.push(DesktopShortcut {
                name: entry.name.clone(),
                path: child,
                kind: program_kind(&entry.short_name),
            });
        }
    }
}

/// Build a guest VM from a program on the shell's volume (drive C: or a
/// disk image).
///
/// DLLs are discovered from the program's own import table and loaded from the
/// same image. This keeps the desktop generic: dropping another PE and its DLLs
/// onto the disk is all that is required to launch it.
pub fn build_vm(
    fs: &Volume,
    request: &LaunchRequest,
    shared: Rc<RefCell<SharedState>>,
    command_line: String,
) -> Result<Vm, LaunchError> {
    if request.path.to_ascii_lowercase().ends_with(".com") {
        return Err(LaunchError::Unsupported(request.name.clone()));
    }
    // A program on drive C: starts as Explorer starts it: its full path on
    // the command line, its folder as the current directory, and its DLLs
    // found and named the way Windows loads them. Installers find their
    // files beside themselves this way.
    if let Volume::Host(_) = fs {
        let guest = format!("C:{}", super::volume::path_text(&parse_path(&request.path)));
        let folder = match guest.rsplit_once('\\') {
            Some((folder, _)) if folder.len() > 2 => folder.to_string(),
            _ => "C:\\".to_string(),
        };
        shared.borrow_mut().current_dir = folder.into_bytes();
        let command_line = if request.arguments.is_empty() {
            format!("\"{guest}\"")
        } else {
            format!("\"{guest}\" {}", request.arguments)
        };
        return Vm::from_drive(guest.as_bytes(), command_line.as_bytes(), shared).map_err(LaunchError::Vm);
    }
    let bytes = fs
        .read(&parse_path(&request.path))
        .map_err(|error| LaunchError::Fat32(error.to_string()))?;
    let image = PeImage::parse(bytes).map_err(|error| LaunchError::Pe(error.to_string()))?;

    let directory = parent_path(&request.path);
    let mut inputs = vec![(request.name.clone(), image)];
    let mut seen_modules = HashSet::new();
    seen_modules.insert(request.name.to_ascii_lowercase());
    let mut seen_imports = HashSet::new();
    let mut pending: VecDeque<String> = inputs[0]
        .1
        .imports
        .iter()
        .map(|import| import.dll.clone())
        .collect();

    while let Some(dll) = pending.pop_front() {
        let key = dll.to_ascii_lowercase();
        if !seen_imports.insert(key.clone()) || seen_modules.contains(&key) {
            continue;
        }
        // Winsock and the CRT must keep their host-backed Rust handlers even
        // when genuine copies are present on the disk image.
        if is_rust_backed_dll(&dll) {
            continue;
        }
        let Some(bytes) = find_module(fs, &directory, &dll) else {
            continue;
        };
        let Ok(dll_image) = PeImage::parse(bytes) else {
            continue;
        };
        for import in &dll_image.imports {
            pending.push_back(import.dll.clone());
        }
        if !seen_modules.insert(key) {
            continue;
        }
        inputs.push((dll, dll_image));
    }

    let vm = Vm::with_modules_shared(inputs, shared, command_line)
        .map_err(|error| LaunchError::Vm(error.to_string()))?;
    let guest_path = format!("C:{}", super::volume::path_text(&parse_path(&request.path)));
    vm.set_program_path(guest_path.as_bytes());
    Ok(vm)
}

/// Start a program from the volume in a desktop session, running beside
/// the others. Returns its process id.
pub fn spawn(
    session: &Rc<Session>,
    fs: &Volume,
    request: &LaunchRequest,
    screen: (i32, i32),
) -> Result<u32, String> {
    let command_line = if request.arguments.is_empty() {
        request.name.clone()
    } else {
        format!("{} {}", request.name, request.arguments)
    };
    session.spawn(&request.name, screen, |shared| {
        build_vm(fs, request, shared, command_line).map_err(|error| error.to_string())
    })
}

fn find_module(fs: &Volume, directory: &str, dll: &str) -> Option<Vec<u8>> {
    let candidates = [
        join_path(directory, dll),
        dll.to_string(),
        join_path("/WINDOWS", dll),
        join_path("/WINDOWS/SYSTEM", dll),
    ];
    for path in candidates {
        if let Ok(bytes) = fs.read(&parse_path(&path)) {
            return Some(bytes);
        }
    }
    find_module_in_tree(fs, dll, "/", 0)
}

fn find_module_in_tree(fs: &Volume, dll: &str, path: &str, depth: usize) -> Option<Vec<u8>> {
    if depth > 16 {
        return None;
    }
    let entries = fs.list(&parse_path(path)).ok()?;
    for entry in entries {
        let child = join_path(path, &entry.name);
        if entry.is_dir {
            if let Some(bytes) = find_module_in_tree(fs, dll, &child, depth + 1) {
                return Some(bytes);
            }
        } else if entry.name.eq_ignore_ascii_case(dll) {
            if let Ok(bytes) = fs.read(&parse_path(&child)) {
                return Some(bytes);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_names_are_detected_case_insensitively() {
        assert!(is_executable_name("NOTEPAD.EXE"));
        assert!(is_executable_name("setup.exe"));
        assert!(is_executable_name("PBRUSH.COM"));
        assert!(!is_executable_name("README.TXT"));
        assert!(!is_executable_name("NOEXTENSION"));
    }

    #[test]
    fn program_kind_distinguishes_pe_and_dos_binaries() {
        assert_eq!(program_kind("GAME.EXE"), ProgramKind::Pe32);
        assert_eq!(program_kind("GAME.COM"), ProgramKind::DosCom);
    }

    #[test]
    fn paths_are_joined_with_fat32_separators() {
        assert_eq!(join_path("/", "HELLO.EXE"), "HELLO.EXE");
        assert_eq!(
            join_path("\\WINDOWS", "SYSTEM.INI"),
            "\\WINDOWS\\SYSTEM.INI"
        );
        assert_eq!(parent_path("\\WINDOWS\\NOTEPAD.EXE"), "\\WINDOWS");
        assert_eq!(parent_path("HELLO.EXE"), "");
    }

    fn demo_fs() -> Option<Fat32FileSystem> {
        Some(Fat32FileSystem::new(crate::test_binary("disks/OTWDISK.IMG")?).unwrap())
    }

    fn demo_volume() -> Option<Volume> {
        Some(Volume::Image(Rc::new(demo_fs()?)))
    }

    #[test]
    fn scanning_the_demo_disk_finds_real_binaries() {
        let Some(fs) = demo_fs() else {
            return;
        };
        let programs = scan_executables(&fs);
        let paths: Vec<&str> = programs
            .iter()
            .map(|program| program.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "HELLO.EXE",
                "MSPAINT.EXE",
                "NOTEPAD.EXE",
                "PBRUSH.EXE",
                "WINSOCK.EXE"
            ]
        );
        assert!(programs
            .iter()
            .all(|program| program.kind == ProgramKind::Pe32));
    }

    #[test]
    fn builds_a_guest_vm_from_a_binary_on_the_disk() {
        let Some(fs) = demo_volume() else {
            return;
        };
        let request = LaunchRequest {
            name: "HELLO.EXE".to_string(),
            path: "HELLO.EXE".to_string(),
            arguments: String::new(),
        };
        let shared = Rc::new(RefCell::new(SharedState::default()));
        let vm = build_vm(&fs, &request, shared, "HELLO.EXE".to_string()).unwrap();
        assert_eq!(vm.modules().len(), 1);
        assert_eq!(vm.modules()[0].name, "HELLO.EXE");
    }

    #[test]
    fn dos_com_files_are_reported_as_unsupported() {
        let Some(fs) = demo_volume() else {
            return;
        };
        let request = LaunchRequest {
            name: "GAME.COM".to_string(),
            path: "GAME.COM".to_string(),
            arguments: String::new(),
        };
        let shared = Rc::new(RefCell::new(SharedState::default()));
        let result = build_vm(&fs, &request, shared, "GAME.COM".to_string());
        assert!(matches!(
            result,
            Err(LaunchError::Unsupported(name)) if name == "GAME.COM"
        ));
    }
}
