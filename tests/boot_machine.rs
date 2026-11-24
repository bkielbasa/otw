use otw_vm::machine::{self, BOOT_LOAD_ADDR};

const BOOT_SECTOR: &[u8] = include_bytes!("../binaries/boot/OTWBOOT.BIN");

#[test]
fn boot_sector_uses_bios_teletype_and_exits() {
    let result =
        machine::run_boot_sector(BOOT_SECTOR).expect("real-mode boot sector must run successfully");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.vga, "OTW BIOS boot OK");
    assert_eq!(result.serial, "OTW BIOS boot OK\n");
    assert!(result.video.is_none());
}

#[test]
fn boot_sector_has_expected_layout() {
    assert_eq!(BOOT_SECTOR.len(), 512);
    assert_eq!(BOOT_LOAD_ADDR, 0x0000_7C00);
    assert_eq!(&BOOT_SECTOR[510..512], &[0x55, 0xAA]);
}

#[test]
fn boot_sector_rejects_missing_signature() {
    let mut invalid = BOOT_SECTOR.to_vec();
    invalid[510] = 0;
    invalid[511] = 0;

    let error = machine::run_boot_sector(&invalid).expect_err("invalid signature must fail");
    assert!(error.to_string().contains("0x55AA"));
}
