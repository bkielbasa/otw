//! The IrfanView 4.76 64-bit installer, the same Unicode MFC wizard built
//! for x64, installs on OTW's 64-bit support: the
//! wizard pages are clicked through, the payload zip is unpacked into
//! `C:\Program Files\IrfanView` (the registry's program files folder), the INI file is written and the Start Menu shortcuts are
//! saved through IShellLinkW / IPersistFile.
//!
//! Then the installed IrfanView starts with a picture on its command line
//! and shows it.
//!
//! The installer is not distributed with OTW. Put `iview476_x64_setup.exe` in
//! the repository root or in `binaries/apps/` to run this test.

use std::{cell::RefCell, fs, path::PathBuf, rc::Rc};

use otw_vm::{kernel::pe::PeImage, vm::Vm, win32::SharedState};

fn installer() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let found = [root.join("iview476_x64_setup.exe"), root.join("binaries/apps/iview476_x64_setup.exe")]
        .into_iter()
        .find(|path| path.is_file());
    if found.is_none() {
        eprintln!("skipping: iview476_x64_setup.exe is not present");
    }
    found
}

/// A 64x48 24-bit BMP filled with one colour.
fn solid_bmp(colour: u32) -> Vec<u8> {
    let (w, h) = (64u32, 48u32);
    let stride = w * 3;
    let size = stride * h;
    let mut bmp = Vec::new();
    bmp.extend_from_slice(b"BM");
    for value in [54 + size, 0, 54, 40, w, h] {
        bmp.extend_from_slice(&value.to_le_bytes());
    }
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    for value in [0, size, 2835, 2835, 0, 0] {
        bmp.extend_from_slice(&value.to_le_bytes());
    }
    for _ in 0..w * h {
        bmp.extend_from_slice(&[colour as u8, (colour >> 8) as u8, (colour >> 16) as u8]);
    }
    bmp
}

#[test]
fn irfanview_64_installs_and_shows_a_picture() {
    let Some(setup) = installer() else {
        return;
    };
    let drive = std::env::temp_dir().join(format!("otw-irfanview64-{}", std::process::id()));
    let _ = fs::remove_dir_all(&drive);
    std::env::set_var("OTW_DRIVE_C", &drive);
    std::env::set_var("OTW_IDLE_EXIT", "3");
    std::env::set_var("OTW_HOST_CLIPBOARD", "0");
    // Next on the welcome, associations, what's new and ready pages; then
    // Done on the last page (with "Start IrfanView" unchecked first, so the
    // test does not depend on launching).
    const NEXT: &str = "m 630 535; d; u; w 3";
    std::env::set_var(
        "OTW_INPUT",
        format!("w 3; {NEXT}; {NEXT}; {NEXT}; {NEXT}; w 30; m 268 433; d; u; w 1; m 550 535; d; u; w 5"),
    );

    let image = PeImage::parse(fs::read(&setup).unwrap()).unwrap();
    assert!(image.is64);
    let shared = Rc::new(RefCell::new(SharedState::default()));
    let vm = Vm::with_modules_shared(
        vec![("IVIEW476_SETUP.EXE".into(), image)],
        shared.clone(),
        "IVIEW476_SETUP.EXE".into(),
    )
    .unwrap();
    assert_eq!(vm.run().expect("setup exits cleanly").exit_code, 0);

    let install = drive.join("Program Files/IrfanView");
    for file in ["i_view64.exe", "iv_uninstall.exe", "i_view64.ini", "Plugins/Effects.dll"] {
        assert!(install.join(file).is_file(), "{file} was not installed");
    }
    assert_eq!(fs::metadata(install.join("i_view64.exe")).unwrap().len(), 2_558_104);

    let desktop = drive.join("WINDOWS/Desktop");
    let link = otw_vm::win32::lnk::read(&fs::read(desktop.join("IrfanView 64 Thumbnails.lnk")).unwrap()).unwrap();
    assert_eq!(link.target, "C:\\Program Files\\IrfanView\\i_view64.exe");
    assert_eq!(link.arguments, "/thumbs");
    let menu = drive.join("WINDOWS/Start Menu/Programs/IrfanView");
    assert_eq!(fs::read_dir(&menu).unwrap().count(), 9);

    // Start IrfanView on a picture.
    const COLOUR: u32 = 0x3366CC;
    fs::write(drive.join("PICTURE.BMP"), solid_bmp(COLOUR)).unwrap();
    std::env::set_var("OTW_INPUT", "w 3");
    let image = PeImage::parse(fs::read(install.join("i_view64.exe")).unwrap()).unwrap();
    let shared = Rc::new(RefCell::new(SharedState::default()));
    let vm = Vm::with_modules_shared(
        vec![("i_view64.exe".into(), image)],
        shared.clone(),
        "\"C:\\Program Files\\IrfanView\\i_view64.exe\" C:\\PICTURE.BMP".into(),
    )
    .unwrap();
    vm.set_program_path(b"C:\\Program Files\\IrfanView\\i_view64.exe");
    assert_eq!(vm.run().expect("IrfanView exits cleanly").exit_code, 0);
    let state = shared.borrow();
    let main = state
        .gui
        .windows
        .iter()
        .find(|w| w.title == "PICTURE.BMP - IrfanView" && w.parent == 0)
        .expect("IrfanView's main window, titled with the picture");
    let surface = &state.gdi.surfaces[&main.hwnd];
    let shown = surface.pixels.iter().filter(|&&p| p == COLOUR).count();
    assert!(shown >= 60 * 44, "the picture is drawn ({shown} pixels)");
    drop(state);

    let _ = fs::remove_dir_all(&drive);
}
