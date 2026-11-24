//! Win32 SEH and MSVC C++ exception handling, exercised by a hand-built
//! guest whose exception tables use the MSVC 2.x layout (see
//! scripts/build-eh-pe.py for the six scenarios it checks).

use otw_vm::{kernel::pe::PeImage, vm::Vm};

const EHTEST_EXE: &[u8] = include_bytes!("../binaries/guests/EHTEST.EXE");

#[test]
fn cxx_and_seh_exceptions_are_dispatched_unwound_and_resumed() {
    let image = PeImage::parse(EHTEST_EXE.to_vec()).expect("EH guest must parse");
    let vm = Vm::new(image).expect("EH guest must map");
    let result = vm.run().expect("EH guest must exit");
    // Non-zero is a bitmask of failed checks: 1 catch(int), 2 destructors,
    // 4 catch(...), 8 rethrow, 16 __except, 32 __finally, 64 continue,
    // 128 read fault, 256 repaired write fault, 512 divide by zero,
    // 1024 catch(...) of a fault, 2048 its destructor.
    assert_eq!(
        result.exit_code, 0,
        "failed checks bitmask {:#x}",
        result.exit_code
    );
}
