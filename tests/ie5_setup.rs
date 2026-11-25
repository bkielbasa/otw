//! Internet Explorer 5 installs with its own setup and then shows a page.
//!
//! The offline package (`ie5setup.zip`: `ie5setup.exe` and its CAB files) is
//! unpacked onto an empty drive C:, and `ie5setup.exe` runs. Its IExpress
//! stub starts the setup wizard, which is clicked through (license, a
//! customised minimal install without the two components the package lacks,
//! Finish); the wizard's install engine then runs the component installers
//! (DCOM95, the ACME engine, INF files) as programs of their own. Finishing
//! restarts Windows: pending file replacements and the `RunOnce` commands
//! (registering IE's DLLs) run. Then the installed IEXPLORE.EXE opens a page
//! from drive C: and one from a web server the test runs on the loopback
//! interface.
//!
//! The package is not distributed with OTW. Put `ie5setup.zip` in the
//! repository root or in `binaries/apps/` to run this test.

use std::{cell::RefCell, fs, path::{Path, PathBuf}, rc::Rc};

use otw_vm::{vm::{NestedLauncher, Vm}, win32::SharedState};

fn package() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let found = [root.join("ie5setup.zip"), root.join("binaries/apps/ie5setup.zip")]
        .into_iter()
        .find(|path| path.is_file());
    if found.is_none() {
        eprintln!("skipping: ie5setup.zip is not present");
    }
    found
}

fn u16_at(data: &[u8], at: usize) -> usize {
    u16::from_le_bytes([data[at], data[at + 1]]) as usize
}

fn u32_at(data: &[u8], at: usize) -> usize {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize
}

/// Unpack a ZIP file (stored or deflated entries) into `dir`.
fn unzip(zip: &[u8], dir: &Path) {
    let end = (0..=zip.len() - 22).rev().find(|&i| zip[i..i + 4] == [0x50, 0x4B, 0x05, 0x06]).expect("a ZIP file");
    let entries = u16_at(zip, end + 10);
    let mut at = u32_at(zip, end + 16);
    for _ in 0..entries {
        let method = u16_at(zip, at + 10);
        let compressed = u32_at(zip, at + 20);
        let name_length = u16_at(zip, at + 28);
        let skip = u16_at(zip, at + 30) + u16_at(zip, at + 32);
        let local = u32_at(zip, at + 42);
        let name = String::from_utf8_lossy(&zip[at + 46..at + 46 + name_length]).into_owned();
        at += 46 + name_length + skip;
        if name.ends_with('/') {
            continue;
        }
        let start = local + 30 + u16_at(zip, local + 26) + u16_at(zip, local + 28);
        let data = &zip[start..start + compressed];
        let bytes = if method == 8 { otw_vm::win32::cab::inflate(data).expect("deflate data") } else { data.to_vec() };
        let target = dir.join(name.rsplit('/').next().unwrap_or(&name));
        fs::write(target, bytes).unwrap();
    }
}

fn run(path: &[u8], command_line: &str) -> Rc<RefCell<SharedState>> {
    let shared = Rc::new(RefCell::new(SharedState::default()));
    NestedLauncher::new(None, None).install(&shared);
    let vm = Vm::from_drive(path, command_line.as_bytes(), shared.clone()).unwrap();
    vm.run().expect("the program exits cleanly");
    shared
}

#[test]
fn ie5_installs_and_shows_a_page() {
    let Some(zip) = package() else {
        return;
    };
    let drive = std::env::temp_dir().join(format!("otw-ie5-{}", std::process::id()));
    let _ = fs::remove_dir_all(&drive);
    fs::create_dir_all(drive.join("IE5")).unwrap();
    unzip(&fs::read(zip).unwrap(), &drive.join("IE5"));
    std::env::set_var("OTW_DRIVE_C", &drive);
    std::env::set_var("OTW_IDLE_EXIT", "40");
    std::env::set_var("OTW_HOST_CLIPBOARD", "0");

    // Accept the license and Next; choose "Install Minimal, or customize"
    // and Next; untick the Internet Connection Wizard and Windows Media
    // Player (not in this package) and Next; then Next past the list of
    // components not installed, and Finish (which restarts Windows).
    const NEXT: &str = "m 657 511; d; u";
    let down = "k 0x28; w 1; ".repeat(13);
    std::env::set_var(
        "OTW_INPUT",
        format!(
            "w 3; m 441 432; d; u; w 1; {NEXT}; w 8; m 277 369; d; u; w 1; {NEXT}; w 6; \
             m 320 407; d; u; w 2; {down}m 320 343; d; u; w 2; {NEXT}; w 300; {NEXT}; w 5; {NEXT}; w 60"
        ),
    );
    run(b"C:\\IE5\\ie5setup.exe", "\"C:\\IE5\\ie5setup.exe\"");

    let installed = drive.join("Program Files/Internet Explorer/IEXPLORE.EXE");
    assert!(installed.is_file(), "IEXPLORE.EXE was installed");
    for dll in ["MSHTML.DLL", "SHDOCVW.DLL", "BROWSEUI.DLL", "URLMON.DLL", "WININET.DLL"] {
        assert!(drive.join("WINDOWS/SYSTEM").join(dll).is_file(), "{dll} was installed");
    }
    // The restart registered the HTML document class (CLSID_HTMLDocument).
    let (_, server) = otw_vm::win32::registry::query(
        "HKEY_CLASSES_ROOT\\CLSID\\{25336920-03F9-11CF-8FD0-00AA00686F13}\\InprocServer32",
        "",
    )
    .expect("MSHTML is registered");
    assert!(String::from_utf8_lossy(&server).to_ascii_lowercase().ends_with("mshtml.dll"));

    // The installed browser opens a page.
    fs::write(
        drive.join("PAGE.HTM"),
        "<html><head><title>Hello OTW</title></head><body><h1>Hello from OTW</h1></body></html>",
    )
    .unwrap();
    std::env::set_var("OTW_IDLE_EXIT", "6");
    std::env::set_var("OTW_INPUT", "w 6");
    let shared = run(
        b"C:\\Program Files\\Internet Explorer\\IEXPLORE.EXE",
        "\"C:\\Program Files\\Internet Explorer\\IEXPLORE.EXE\" C:\\PAGE.HTM",
    );
    let state = shared.borrow();
    let frame = state
        .gui
        .windows
        .iter()
        .find(|w| w.class == "IEFrame")
        .expect("the browser window");
    assert_eq!(frame.title, "Hello OTW - Microsoft Internet Explorer");
    assert!(
        state.gui.windows.iter().any(|w| w.class == "Internet Explorer_Server"),
        "MSHTML shows the document"
    );
    drop(state);

    // And one from a web server, over the host's network stack.
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in server.incoming().flatten() {
            let mut stream = stream;
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let body = "<html><head><title>Served page</title></head><body><h1>Over the network</h1></body></html>";
            let _ = write!(
                stream,
                "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let url = format!("http://127.0.0.1:{port}/");
    let shared = run(
        b"C:\\Program Files\\Internet Explorer\\IEXPLORE.EXE",
        &format!("\"C:\\Program Files\\Internet Explorer\\IEXPLORE.EXE\" {url}"),
    );
    let state = shared.borrow();
    let frame = state.gui.windows.iter().find(|w| w.class == "IEFrame").expect("the browser window");
    assert_eq!(frame.title, "Served page - Microsoft Internet Explorer");
    drop(state);
    let _ = fs::remove_dir_all(&drive);
}
