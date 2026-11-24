use otw_vm::machine::{self, VideoMode};

const VGA_BOOT: &[u8] = include_bytes!("../binaries/boot/OTWVGA.BIN");

#[test]
fn mode_13h_boot_sector_renders_a_rgb_frame() {
    let result =
        machine::run_boot_sector(VGA_BOOT).expect("VGA mode 13h boot sector must run successfully");

    assert_eq!(result.exit_code, 0);
    let frame = result
        .video
        .expect("graphics boot must produce a video frame");
    assert_eq!(frame.mode, VideoMode::Graphics320x200x256);
    assert_eq!(frame.width, 320);
    assert_eq!(frame.height, 200);
    assert_eq!(frame.pixels.len(), 320 * 200 * 3);

    // DAC entry 0x2A was rewritten to bright red by the guest.
    assert_eq!(&frame.pixels[0..3], &[0xFF, 0x00, 0x00]);

    // The default identity palette makes unprogrammed pixel values grayscale.
    let center = (199 * 320 + 160) * 3;
    assert_eq!(&frame.pixels[center..center + 3], &[0x10, 0x10, 0x10]);

    let last = (200 * 320 - 1) * 3;
    assert_eq!(&frame.pixels[last..last + 3], &[0xFF, 0xFF, 0xFF]);
}

#[test]
fn vga_boot_sector_has_expected_layout() {
    assert_eq!(VGA_BOOT.len(), 512);
    assert_eq!(&VGA_BOOT[510..512], &[0x55, 0xAA]);
}
