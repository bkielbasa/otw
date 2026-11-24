use otw_vm::{kernel::pe::PeImage, vm::Vm};

const HELLO_EXE: &[u8] = include_bytes!("../binaries/guests/HELLO.EXE");

#[test]
fn parses_hand_built_hello_guest() {
    let image = PeImage::parse(HELLO_EXE.to_vec()).expect("hello PE must parse");

    assert_eq!(image.machine, 0x014c);
    assert_eq!(image.image_base, 0x0040_0000);
    assert_eq!(image.entry_va(), 0x0040_1000);
    assert_eq!(image.sections.len(), 1);
    assert_eq!(image.import_count(), 3);

    let names: Vec<String> = image.imports[0]
        .names
        .iter()
        .map(|name| name.display())
        .collect();
    assert_eq!(names, ["GetStdHandle", "WriteFile", "ExitProcess"]);
}

#[test]
fn executes_hello_guest_to_exit_process() {
    let image = PeImage::parse(HELLO_EXE.to_vec()).expect("hello PE must parse");
    let machine = Vm::new(image).expect("hello VM must map");

    assert_eq!(machine.implemented_imports(), 3);

    let result = machine.run().expect("hello guest must run successfully");
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.api_calls, 3);
}
