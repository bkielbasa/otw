use otw_vm::machine::{self, KERNEL_LOAD_VA};

const KERNEL: &[u8] = include_bytes!("../binaries/kernels/HELLO.BIN");

#[test]
fn minimal_kernel_boots_and_writes_vga_and_serial() {
    let result = machine::run(KERNEL).expect("minimal kernel must run successfully");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.serial, "Hello from the OTW minimal kernel!");
    assert_eq!(result.vga, "Hello from the OTW minimal kernel!");
}

#[test]
fn kernel_image_fits_the_machine_window() {
    assert!(KERNEL.len() > 64);
    assert!(KERNEL.len() < 1024 * 1024);
    assert_eq!(KERNEL_LOAD_VA, 0x0010_0000);
}
