//! Clipboard and Edit Colors with genuine Notepad and Paint, driven by
//! scripted input. Runs sequentially in one test because the scripting
//! switches are process-wide environment variables.

mod common;

use std::{cell::RefCell, fs, rc::Rc};

use otw_vm::{
    kernel::pe::PeImage,
    vm::Vm,
    win32::clipboard::{ClipFormat, CF_DIB, CF_TEXT},
    win32::SharedState,
};

fn module(name: &str) -> (String, PeImage) {
    let bytes = fs::read(format!(
        "{}/binaries/win95/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("artifact must be present");
    (
        name.to_string(),
        PeImage::parse(bytes).expect("PE must parse"),
    )
}

fn paint_modules() -> Vec<(String, PeImage)> {
    vec![
        module("MSPAINT.EXE"),
        module("MFC30.DLL"),
        module("MFCO30.DLL"),
        module("MFCANS32.dll"),
    ]
}

fn run(modules: Vec<(String, PeImage)>, shared: &Rc<RefCell<SharedState>>, script: &str) {
    std::env::set_var("OTW_INPUT", script);
    let vm =
        Vm::with_modules_shared(modules, shared.clone(), "APP.EXE".into()).expect("VM must map");
    let result = vm.run().expect("guest must exit cleanly");
    assert_eq!(result.exit_code, 0);
    shared.borrow_mut().reset_for_process();
}

fn pixel(ppm: &[u8], x: usize, y: usize) -> (u8, u8, u8) {
    let header_end = ppm
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .nth(2)
        .map(|(i, _)| i + 1)
        .unwrap();
    let width: usize = String::from_utf8_lossy(&ppm[..header_end])
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let i = header_end + (y * width + x) * 3;
    (ppm[i], ppm[i + 1], ppm[i + 2])
}

fn entry(shared: &Rc<RefCell<SharedState>>, format: ClipFormat) -> Option<Vec<u8>> {
    shared
        .borrow()
        .clipboard
        .entries
        .iter()
        .find(|(f, _)| *f == format)
        .map(|(_, b)| b.clone())
}

#[test]
fn clipboard_and_edit_colors_work_in_notepad_and_paint() {
    if !common::have(common::NOTEPAD) || !common::have(common::PAINT) {
        return;
    }
    std::env::set_var("OTW_IDLE_EXIT", "3");
    std::env::set_var("OTW_HOST_CLIPBOARD", "0");
    let drive = std::env::temp_dir().join(format!("otw-clip-test-{}", std::process::id()));
    std::env::set_var("OTW_DRIVE_C", &drive);
    let shared = Rc::new(RefCell::new(SharedState::default()));

    // Notepad: type, select with Shift+Home, copy with Ctrl+C.
    run(
        vec![module("NOTEPAD.EXE")],
        &shared,
        "w 2; k 0x48 H; k 0x49 i; kd 0x10; k 0x24; ku 0x10; kd 0x11; k 0x43 #3; ku 0x11; w 2",
    );
    assert_eq!(
        entry(&shared, ClipFormat::Standard(CF_TEXT)).as_deref(),
        Some(b"Hi".as_slice())
    );

    // A second Notepad launched afterwards pastes it twice.
    let shot = std::env::temp_dir().join(format!("otw-clip-test-{}.ppm", std::process::id()));
    run(
        vec![module("NOTEPAD.EXE")],
        &shared,
        &format!(
            "w 2; kd 0x11; k 0x56 #22; k 0x56 #22; ku 0x11; w 2; s {}",
            shot.display()
        ),
    );
    // "HiHi" was drawn at the top-left of the edit control.
    let ppm = fs::read(&shot).expect("screenshot must be written");
    let dark = (28..90)
        .flat_map(|x| (60..76).map(move |y| (x, y)))
        .filter(|&(x, y)| pixel(&ppm, x, y) == (0, 0, 0))
        .count();
    assert!(
        dark > 40,
        "pasted text should be visible, found {dark} dark pixels"
    );
    assert_eq!(
        entry(&shared, ClipFormat::Standard(CF_TEXT)).as_deref(),
        Some(b"Hi".as_slice())
    );

    // Paint: Select All, Copy puts the whole 1024x700 picture on the
    // clipboard as a DIB (through OleSetClipboard).
    run(paint_modules(), &shared, "w 2; m 120 100; d; m 200 180; u; w 2; kd 0x11; k 0x4C #12; ku 0x11; w 2; kd 0x11; k 0x43 #3; ku 0x11; w 2");
    let dib = entry(&shared, ClipFormat::Standard(CF_DIB)).expect("Paint copy must store a DIB");
    let width = u32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
    let height = u32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]);
    assert_eq!((width, height), (1024, 700));

    // Edit Colors: double-click the red palette swatch, pick basic blue,
    // press OK; the foreground color swatch turns blue.
    run(
        paint_modules(),
        &shared,
        &format!(
            "w 2; m 99 370; d; u; d; u; w 3; m 173 171; d; u; w 1; m 95 335; d; u; w 3; s {}",
            shot.display()
        ),
    );
    let ppm = fs::read(&shot).expect("screenshot must be written");
    assert_eq!(
        pixel(&ppm, 38, 357),
        (0, 0, 255),
        "foreground swatch should be blue"
    );

    let _ = fs::remove_file(shot);
    let _ = fs::remove_dir_all(drive);
}
