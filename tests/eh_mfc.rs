//! MFC's TRY/CATCH in genuine Paint: an injected OLE failure makes MFC throw
//! a COleException through a rethrowing catch block to its command handler,
//! which reports the error, and Paint keeps running.

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
fn paint_recovers_from_an_mfc_exception() {
    if !common::have(common::PAINT) {
        return;
    }
    let shot = std::env::temp_dir().join(format!("otw-eh-mfc-{}.ppm", std::process::id()));
    let drive = std::env::temp_dir().join(format!("otw-eh-mfc-{}", std::process::id()));
    std::env::set_var("OTW_DRIVE_C", &drive);
    std::env::set_var("OTW_IDLE_EXIT", "3");
    std::env::set_var("OTW_HOST_CLIPBOARD", "0");
    std::env::set_var("OTW_FAIL_API", "CreateILockBytesOnHGlobal=0x80004005");
    // Select All, Copy (throws), dismiss "Command failed.", then draw.
    std::env::set_var(
        "OTW_INPUT",
        format!(
            "w 2; kd 0x11; k 0x4C #12; ku 0x11; w 2; kd 0x11; k 0x43 #3; ku 0x11; w 3; k 0x0D #13; w 2; \
             m 44 142; d; u; m 120 250; d; m 250 250; u; w 2; s {}",
            shot.display()
        ),
    );
    let vm = Vm::with_modules(vec![
        module("MSPAINT.EXE"),
        module("MFC30.DLL"),
        module("MFCO30.DLL"),
        module("MFCANS32.dll"),
    ])
    .expect("Paint VM must map");
    let result = vm
        .run()
        .expect("Paint must keep running after the exception");
    assert_eq!(result.exit_code, 0);

    // The stroke drawn after the exception reached the canvas.
    let ppm = fs::read(&shot).expect("screenshot must be written");
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
    let i = header_end + (250 * width + 180) * 3;
    assert_eq!(&ppm[i..i + 3], &[0, 0, 0], "stroke drawn after recovery");
    let _ = fs::remove_file(shot);
    let _ = fs::remove_dir_all(drive);
}
