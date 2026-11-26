//! Threads of a 64-bit program on CPUs of their own: four threads take a
//! one-byte lock built like Rust's standard mutex (lock cmpxchg, xchg,
//! WaitOnAddress / WakeByAddressSingle) 20000 times each. Built by
//! `scripts/build-threads64-pe.py`.

use otw_vm::{kernel::pe::PeImage, vm::Vm};

const THREADS64_EXE: &[u8] = include_bytes!("../binaries/guests/THREADS64.EXE");

#[test]
fn locked_increments_on_parallel_cpus_are_not_lost() {
    std::env::set_var("OTW_HOST_CLIPBOARD", "0");
    std::env::set_var("OTW_PARALLEL_THREADS", "1");
    for _ in 0..5 {
        let image = PeImage::parse(THREADS64_EXE.to_vec()).unwrap();
        let vm = Vm::new(image).expect("maps");
        let result = vm.run().expect("runs to ExitProcess");
        assert_eq!(result.exit_code, 4 * 20000);
    }
}
