//! A 64-bit (PE32+) program: linked above 4 GiB and moved below it, calling
//! Win32 with the x64 convention. Built by `scripts/build-hello64-pe.py`.

use otw_vm::{kernel::pe::PeImage, vm::Vm};

const HELLO64_EXE: &[u8] = include_bytes!("../binaries/guests/HELLO64.EXE");

#[test]
fn parses_a_pe32_plus_image() {
    let image = PeImage::parse(HELLO64_EXE.to_vec()).expect("PE32+ parses");
    assert!(image.is64);
    assert_eq!(image.machine, 0x8664);
    assert_eq!(image.preferred_base, 0x1_4000_0000);
    assert_eq!(image.image_base, 0x0040_0000, "moved below 4 GiB");
    let names: Vec<String> = image.imports.iter().flat_map(|dll| dll.names.iter().map(|n| n.display())).collect();
    assert_eq!(names, ["GetStdHandle", "WriteFile", "ExitProcess", "MessageBoxW"]);
}

#[test]
fn runs_a_64_bit_program() {
    std::env::set_var("OTW_HOST_CLIPBOARD", "0");
    let image = PeImage::parse(HELLO64_EXE.to_vec()).unwrap();
    let vm = Vm::new(image).expect("maps");
    let result = vm.run().expect("runs to ExitProcess");
    // MessageBoxW returns IDOK (1) headless; the program exits with it + 40.
    assert_eq!(result.exit_code, 41);
    assert_eq!(result.api_calls, 4);
}
