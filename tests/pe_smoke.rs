mod common;
use std::{fs, path::PathBuf};

use otw_vm::{kernel::iat, kernel::pe::PeImage, vm::Vm};

fn target() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries/win95/NOTEPAD.EXE")
}

#[test]
fn parses_genuine_win95_notepad() {
    if !common::have(common::NOTEPAD) {
        return;
    }
    let bytes = fs::read(target()).expect("target binary must be present");
    let image = PeImage::parse(bytes).expect("PE parse must succeed");

    assert_eq!(image.machine, 0x014c);
    assert_eq!(image.image_base, 0x0040_0000);
    assert_eq!(image.entry_rva, 0x1000);
    assert_eq!(image.entry_va(), 0x0040_1000);
    assert_eq!(image.sections.len(), 6);
    assert_eq!(image.imports.len(), 5);
    assert_eq!(image.import_count(), 127);
}

#[test]
fn maps_image_and_patches_iat() {
    if !common::have(common::NOTEPAD) {
        return;
    }
    let bytes = fs::read(target()).expect("target binary must be present");
    let image = PeImage::parse(bytes).expect("PE parse must succeed");
    let expected_entry = image.entry_va();
    let machine = Vm::new(image).expect("VM mapping must succeed");

    assert!(machine.memory_size() >= 0x1000);
    assert_eq!(machine.read_u32(expected_entry).is_some(), true);
    assert_eq!(machine.iat().len(), 127);

    // Synthetic built-in procedure thunks have no IAT slot.
    for entry in machine
        .iat()
        .entries
        .iter()
        .filter(|entry| entry.slot_va != 0)
    {
        assert_eq!(machine.read_u32(entry.slot_va), Some(entry.thunk_va));
        assert!(entry.thunk_va >= iat::THUNK_BASE);
    }
}

#[test]
fn iat_has_unique_thunks() {
    if !common::have(common::NOTEPAD) {
        return;
    }
    let bytes = fs::read(target()).expect("target binary must be present");
    let image = PeImage::parse(bytes).expect("PE parse must succeed");
    let table = iat::Iat::build(&image).expect("IAT build must succeed");

    let mut thunks: Vec<u32> = table.entries.iter().map(|entry| entry.thunk_va).collect();
    thunks.sort_unstable();
    thunks.dedup();
    assert_eq!(thunks.len(), 127);
}
