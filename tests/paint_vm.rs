//! End-to-end check of the genuine Windows 95 Paint (MSPAINT.EXE + MFC 3.0)
//! running on the Rust USER32/GDI32 layers, driven by scripted input.

mod common;

use std::fs;

use otw_vm::{kernel::pe::PeImage, vm::Vm};

fn module(name: &str) -> (String, PeImage) {
    let bytes = fs::read(format!(
        "{}/binaries/win95/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("Paint artifact must be present");
    (
        name.to_string(),
        PeImage::parse(bytes).expect("PE must parse"),
    )
}

#[test]
fn genuine_paint_starts_draws_and_exits() {
    if !common::have(common::PAINT) {
        return;
    }
    // Headless: draw one pencil stroke, screenshot, then quit when idle.
    let shot = std::env::temp_dir().join(format!("otw-paint-test-{}.ppm", std::process::id()));
    std::env::set_var(
        "OTW_INPUT",
        format!(
            "w 2; m 120 100; d; m 160 140; m 200 180; u; w 2; s {}",
            shot.display()
        ),
    );
    std::env::set_var("OTW_IDLE_EXIT", "3");
    let drive = std::env::temp_dir().join(format!("otw-paint-test-{}", std::process::id()));
    std::env::set_var("OTW_DRIVE_C", &drive);

    let vm = Vm::with_modules(vec![
        module("MSPAINT.EXE"),
        module("MFC30.DLL"),
        module("MFCO30.DLL"),
        module("MFCANS32.dll"),
    ])
    .expect("Paint VM must map");
    let result = vm.run().expect("Paint must run to a clean exit");
    assert_eq!(result.exit_code, 0);
    assert!(
        result.api_calls > 5000,
        "Paint should run its full MFC startup"
    );

    let shared = vm.api_context().shared.clone();
    let shared = shared.borrow();
    // The frame window was destroyed on exit, but the tool windows' classes
    // and the drawn pixels prove the UI ran. Check the recorded classes.
    for class in ["MSPaintApp", "AfxControlBar", "AfxFrameOrView"] {
        assert!(
            shared.gui.classes.iter().any(|c| c.name == class),
            "Paint should register {class}"
        );
    }
    drop(shared);

    // The pencil stroke is black on the white canvas; the canvas next to it
    // stays white, and the desktop outside the window is teal.
    let ppm = fs::read(&shot).expect("scripted screenshot must be written");
    let header_end = ppm
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .nth(2)
        .map(|(i, _)| i + 1)
        .unwrap();
    let header = String::from_utf8_lossy(&ppm[..header_end]).to_string();
    let width: usize = header.split_whitespace().nth(1).unwrap().parse().unwrap();
    let pixel = |x: usize, y: usize| {
        let i = header_end + (y * width + x) * 3;
        (ppm[i], ppm[i + 1], ppm[i + 2])
    };
    assert_eq!(pixel(160, 140), (0, 0, 0), "pencil stroke pixel");
    assert_eq!(pixel(180, 120), (255, 255, 255), "untouched canvas pixel");
    assert_eq!(pixel(600, 600), (0, 128, 128), "desktop pixel");
    let _ = fs::remove_file(shot);
    let _ = fs::remove_dir_all(drive);
}

#[test]
fn paint_launches_from_the_demo_disk_with_its_mfc_dlls() {
    if !common::have(common::DEMO_DISK) {
        return;
    }
    use otw_vm::{
        fat32::Fat32FileSystem,
        shell::launch::{build_vm, LaunchRequest},
        shell::volume::Volume,
        win32::SharedState,
    };
    use std::{cell::RefCell, rc::Rc};

    let image = fs::read(format!(
        "{}/binaries/disks/OTWDISK.IMG",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let fs = Volume::Image(Rc::new(
        Fat32FileSystem::new(image).expect("demo disk must mount"),
    ));
    let request = LaunchRequest {
        name: "MSPAINT.EXE".into(),
        path: "MSPAINT.EXE".into(),
        arguments: String::new(),
    };
    let shared = Rc::new(RefCell::new(SharedState::default()));
    let vm = build_vm(&fs, &request, shared, "MSPAINT.EXE".into())
        .expect("Paint must load from the disk");
    let names: Vec<String> = vm
        .modules()
        .iter()
        .map(|m| m.name.to_ascii_uppercase())
        .collect();
    for dll in ["MFC30.DLL", "MFCO30.DLL", "MFCANS32.DLL"] {
        assert!(
            names.iter().any(|n| n == dll),
            "{dll} must be loaded from the disk, got {names:?}"
        );
    }
}
