//! First-run setup of drive C:, like Win95 Setup: system files in
//! `C:\WINDOWS`, Start Menu shortcuts, a desktop folder, `My Documents`, and
//! the `[Extensions]` file associations in `WIN.INI`. Existing files are
//! never overwritten, so the user's changes survive.

use std::path::{Path, PathBuf};

use super::{
    lnk::{self, ShellLink},
    volume::{join, parse_path, VolPath, Volume},
};

pub const WINDOWS: &str = "\\WINDOWS";
pub const DESKTOP: &str = "\\WINDOWS\\Desktop";
pub const PROGRAMS: &str = "\\WINDOWS\\Start Menu\\Programs";
pub const DOCUMENTS: &str = "\\My Documents";

/// Start Menu shortcuts created for programs present in `C:\WINDOWS`
/// (folder under Programs, shortcut name, program file).
const SHORTCUTS: &[(&str, &str, &str)] = &[
    ("Accessories", "Notepad", "NOTEPAD.EXE"),
    ("Accessories", "Paint", "MSPAINT.EXE"),
];

const WIN_INI: &str = "[windows]\r\nload=\r\nrun=\r\n\r\n[Desktop]\r\nWallpaper=(None)\r\n\r\n[Extensions]\r\ntxt=notepad.exe ^.txt\r\nini=notepad.exe ^.ini\r\nlog=notepad.exe ^.log\r\nbat=notepad.exe ^.bat\r\nbmp=mspaint.exe ^.bmp\r\n";

/// Where the genuine program files to install come from.
pub fn default_source() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OTW_SYSTEM_FILES") {
        return Some(PathBuf::from(dir));
    }
    [
        PathBuf::from("binaries/win95"),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries/win95"),
    ]
    .into_iter()
    .find(|dir| dir.is_dir())
}

/// Set up drive C: from `source` (a folder of program files).
pub fn install(volume: &Volume, source: Option<&Path>) -> Result<(), String> {
    for dir in [WINDOWS, DESKTOP, PROGRAMS, DOCUMENTS] {
        volume.create_dir(&parse_path(dir))?;
    }
    let windows = parse_path(WINDOWS);
    if let Some(source) = source {
        if let Ok(entries) = std::fs::read_dir(source) {
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                let target = join(&windows, &name);
                if entry.path().is_file() && !volume.exists(&target) {
                    let bytes = std::fs::read(entry.path()).map_err(|error| error.to_string())?;
                    volume.write(&target, &bytes)?;
                }
            }
        }
    }
    for (folder, name, file) in SHORTCUTS {
        let program = join(&windows, file);
        if !volume.exists(&program) {
            continue;
        }
        let dir = join(&parse_path(PROGRAMS), folder);
        volume.create_dir(&dir)?;
        let link = join(&dir, &format!("{name}.lnk"));
        if !volume.exists(&link) {
            let bytes = lnk::write(&ShellLink {
                target: volume.guest_path(&program),
                working_dir: volume.guest_path(&windows),
                ..Default::default()
            });
            volume.write(&link, &bytes)?;
        }
    }
    let ini = join(&windows, "WIN.INI");
    if !volume.exists(&ini) {
        volume.write(&ini, WIN_INI.as_bytes())?;
    }
    Ok(())
}

/// The program registered for a file extension in `WIN.INI [Extensions]`.
pub fn association(volume: &Volume, extension: &str) -> Option<VolPath> {
    let ini = volume.read(&parse_path("\\WINDOWS\\WIN.INI")).ok()?;
    let text = String::from_utf8_lossy(&ini);
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[Extensions]");
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !in_section || !key.trim().eq_ignore_ascii_case(extension) {
            continue;
        }
        let program = value.split_whitespace().next()?;
        if program.contains(['\\', ':']) {
            return Some(volume.canonical(&parse_path(program)));
        }
        return ["\\WINDOWS", "\\WINDOWS\\SYSTEM", "\\"]
            .iter()
            .map(|dir| join(&parse_path(dir), program))
            .find(|path| volume.exists(path))
            .map(|path| volume.canonical(&path));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_creates_folders_shortcuts_and_associations_once() {
        let root = std::env::temp_dir().join(format!("otw-setup-{}", std::process::id()));
        let source = root.join("source");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("NOTEPAD.EXE"), b"MZ").unwrap();
        let volume = Volume::Host(root.join("C"));
        install(&volume, Some(&source)).unwrap();
        let link = volume
            .read(&parse_path(
                "\\WINDOWS\\Start Menu\\Programs\\Accessories\\Notepad.lnk",
            ))
            .unwrap();
        assert_eq!(lnk::read(&link).unwrap().target, "C:\\WINDOWS\\NOTEPAD.EXE");
        // Paint is not installed, so it gets no shortcut.
        assert!(!volume.exists(&parse_path(
            "\\WINDOWS\\Start Menu\\Programs\\Accessories\\Paint.lnk"
        )));
        assert!(volume.is_dir(&parse_path(DOCUMENTS)) && volume.is_dir(&parse_path(DESKTOP)));
        assert_eq!(
            association(&volume, "TXT"),
            Some(parse_path("\\WINDOWS\\NOTEPAD.EXE"))
        );
        assert_eq!(association(&volume, "bmp"), None);
        // A second run keeps user changes.
        volume
            .write(&parse_path("\\WINDOWS\\NOTEPAD.EXE"), b"changed")
            .unwrap();
        install(&volume, Some(&source)).unwrap();
        assert_eq!(
            volume.read(&parse_path("\\WINDOWS\\NOTEPAD.EXE")).unwrap(),
            b"changed"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
