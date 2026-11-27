//! Rebuilding an OTW disk image keeps drive C:: make-disk.py --update makes
//! the kernel's partition again without touching C:, and otw-disk adds
//! files to C: (replacing OTW's own with --replace) without losing what
//! was written there.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use otw_vm::fat32::{Fat32FileSystem, MemoryVolume};

/// Partition `label` of `image` (GPT) as a FAT32 volume.
fn partition(image: &[u8], label: &str) -> Fat32FileSystem {
    let u32_at = |o: usize| u32::from_le_bytes(image[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(image[o..o + 8].try_into().unwrap());
    let (entries, count, size) = (u64_at(512 + 72) as usize * 512, u32_at(512 + 80) as usize, u32_at(512 + 84) as usize);
    for i in 0..count {
        let e = entries + i * size;
        let units: Vec<u16> = image[e + 56..e + 128].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        if String::from_utf16_lossy(&units) == label {
            let (first, last) = (u64_at(e + 32) as usize, u64_at(e + 40) as usize);
            let bytes = image[first * 512..(last + 1) * 512].to_vec();
            return Fat32FileSystem::from_volume(Arc::new(MemoryVolume::new(bytes))).unwrap();
        }
    }
    panic!("no partition {label}");
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn run(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_rebuilt_image_keeps_drive_c() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("otw-disk-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (esp, seed, own) = (dir.join("esp"), dir.join("seed"), dir.join("own"));
    write(&esp.join("EFI/BOOT/BOOTX64.EFI"), "kernel 1");
    write(&seed.join("Program Files/App/app.exe"), "app 1");
    write(&own.join("OTW/HELLO.EXE"), "hello 1");
    let image = dir.join("disk.img");
    let make_disk = root.join("scripts/make-disk.py");
    let python = |args: &[&str]| {
        let mut command = Command::new("python3");
        command.arg("-I").arg(&make_disk).args(args);
        command
    };
    let esp_spec = format!("esp:OTW:40:{}", esp.display());
    let c_spec = format!("data:OTW-C:auto:{}+{}", seed.display(), own.display());
    run(&mut python(&[image.to_str().unwrap(), &esp_spec, &c_spec]));

    // What a program saves on C: (written here with the writer the kernel
    // uses).
    let saved = dir.join("saved");
    write(&saved.join("My Documents/download.zip"), "saved by a program");
    let otw_disk = env!("CARGO_BIN_EXE_otw-disk");
    run(Command::new(otw_disk).args(["sync", image.to_str().unwrap(), "OTW-C", saved.to_str().unwrap()]));

    // Rebuilt: a new kernel, OTW's files changed, a new seed file.
    write(&esp.join("EFI/BOOT/BOOTX64.EFI"), "kernel 2");
    write(&own.join("OTW/HELLO.EXE"), "hello 2");
    write(&seed.join("Program Files/App/app.exe"), "app 2");
    write(&seed.join("new.txt"), "new");
    run(&mut python(&["--update", image.to_str().unwrap(), &esp_spec]));
    let report = run(Command::new(otw_disk).args(["sync", image.to_str().unwrap(), "OTW-C", "--replace", own.to_str().unwrap()]));
    assert!(report.contains("1 replaced"), "{report}");
    let report = run(Command::new(otw_disk).args(["sync", image.to_str().unwrap(), "OTW-C", seed.to_str().unwrap()]));
    assert!(report.contains("1 files added"), "{report}");

    let bytes = std::fs::read(&image).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let esp = partition(&bytes, "OTW");
    assert_eq!(esp.read_file("/EFI/BOOT/BOOTX64.EFI").unwrap(), b"kernel 2");
    let c = partition(&bytes, "OTW-C");
    assert_eq!(c.read_file("/My Documents/download.zip").unwrap(), b"saved by a program");
    assert_eq!(c.read_file("/OTW/HELLO.EXE").unwrap(), b"hello 2");
    // Files already on C: stay as they are there (a program may have
    // changed them); new ones come.
    assert_eq!(c.read_file("/Program Files/App/app.exe").unwrap(), b"app 1");
    assert_eq!(c.read_file("/new.txt").unwrap(), b"new");
}

