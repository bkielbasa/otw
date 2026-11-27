//! What OTW writes to a FAT32 volume is what other systems read: a volume
//! made by mkfs.fat, changed by OTW's writer, is clean to fsck.fat and
//! reads back the same after mounting again. Skipped without dosfstools.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use otw_vm::fat32::{Fat32FileSystem, MemoryVolume};

fn tool(name: &str) -> Option<PathBuf> {
    ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().map(|dir| Path::new(dir).join(name)).find(|p| p.exists())
}

fn fresh_volume(dir: &Path, mkfs: &Path) -> Vec<u8> {
    let path = dir.join("fresh.img");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(64 * 1024 * 1024).unwrap();
    drop(file);
    let status = Command::new(mkfs).args(["-F", "32", "-s", "1", "-n", "OTWTEST"]).arg(&path).output().unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    std::fs::read(&path).unwrap()
}

#[test]
fn written_volumes_pass_fsck() {
    let (Some(mkfs), Some(fsck)) = (tool("mkfs.fat"), tool("fsck.fat")) else {
        eprintln!("no mkfs.fat/fsck.fat: skipped");
        return;
    };
    let dir = std::env::temp_dir().join(format!("otw-fat32-fsck-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let volume = Arc::new(MemoryVolume::new(fresh_volume(&dir, &mkfs)));
    let fs = Fat32FileSystem::from_volume(volume.clone()).unwrap();

    // What a browser does with a download: a part file grown, renamed.
    fs.create_dir("/My Documents").unwrap();
    fs.create_dir("/My Documents/Downloads").unwrap();
    let setup: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    fs.write_file("/My Documents/Downloads/npp.8.7.Installer.x64.exe.part", &setup[..1_000_000]).unwrap();
    fs.write_file("/My Documents/Downloads/npp.8.7.Installer.x64.exe.part", &setup).unwrap();
    fs.rename("/My Documents/Downloads/npp.8.7.Installer.x64.exe.part", "/My Documents/Downloads/npp.8.7.Installer.x64.exe").unwrap();
    // Many small files (a profile), some removed, a directory moved.
    fs.create_dir("/Profile").unwrap();
    for i in 0..300 {
        fs.write_file(&format!("/Profile/cache entry number {i}.bin"), &vec![i as u8; 100 + i * 37]).unwrap();
    }
    for i in (0..300).step_by(3) {
        fs.remove(&format!("/Profile/cache entry number {i}.bin")).unwrap();
    }
    fs.write_file("/PREFS.JS", b"user_pref(\"a\", 1);\n").unwrap();
    fs.create_dir("/Old").unwrap();
    fs.rename("/Profile", "/Old/Profile").unwrap();

    let image = volume.image();
    let path = dir.join("written.img");
    std::fs::write(&path, &image).unwrap();
    let check = Command::new(&fsck).arg("-n").arg("-v").arg(&path).output().unwrap();
    let report = String::from_utf8_lossy(&check.stdout).into_owned() + &String::from_utf8_lossy(&check.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(check.status.success(), "fsck.fat found problems:\n{report}");

    let again = Fat32FileSystem::new(image).unwrap();
    assert_eq!(again.read_file("/My Documents/Downloads/npp.8.7.Installer.x64.exe").unwrap(), setup);
    assert!(again.read_file("/My Documents/Downloads/npp.8.7.Installer.x64.exe.part").is_err());
    let left: Vec<_> = again.list("/Old/Profile").unwrap().into_iter().filter(|e| !e.name.starts_with('.')).collect();
    assert_eq!(left.len(), 200);
    assert_eq!(again.read_file("/Old/Profile/cache entry number 299.bin").unwrap(), vec![299u32 as u8; 100 + 299 * 37]);
    assert_eq!(again.read_file("/prefs.js").unwrap(), b"user_pref(\"a\", 1);\n");
}
