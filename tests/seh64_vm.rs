//! x64 structured exception handling: a software exception and an access
//! violation dispatched through the function tables to a language handler,
//! which unwinds to continuations with RtlUnwindEx. Built by
//! `scripts/build-seh64-pe.py`.

use otw_vm::{kernel::pe::PeImage, vm::Vm};

const SEH64_EXE: &[u8] = include_bytes!("../binaries/guests/SEH64.EXE");

#[test]
fn exceptions_unwind_to_their_handlers() {
    std::env::set_var("OTW_HOST_CLIPBOARD", "0");
    let image = PeImage::parse(SEH64_EXE.to_vec()).unwrap();
    assert!(image.is64);
    let vm = Vm::new(image).expect("maps");
    let result = vm.run().expect("runs to ExitProcess");
    // 0x1000 from the software exception plus 0x234 from the fault.
    assert_eq!(result.exit_code, 0x1234);
}
