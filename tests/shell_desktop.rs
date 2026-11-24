//! The desktop shell end to end: create a document in the folder window,
//! open it, and have genuine Notepad (started by the shell with the
//! document's path) edit and save it on drive C:.

mod common;

use std::{cell::RefCell, fs, rc::Rc};

use otw_vm::{
    shell::launch::build_vm,
    shell::{setup, volume::Volume, Key, Mods, Shell},
    win32::SharedState,
};

fn press(shell: &mut Shell, key: Key) {
    shell.key(key, Mods::default());
}

#[test]
fn a_document_created_in_explorer_opens_and_saves_in_notepad() {
    if !common::have(common::NOTEPAD) {
        return;
    }
    let root = std::env::temp_dir().join(format!("otw-shell-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let drive = root.join("C");
    std::env::set_var("OTW_DRIVE_C", &drive);
    std::env::set_var("OTW_IDLE_EXIT", "3");
    std::env::set_var("OTW_HOST_CLIPBOARD", "0");
    let volume = Volume::Host(drive.clone());
    setup::install(&volume, setup::default_source().as_deref()).unwrap();

    let mut shell = Shell::new(Some(volume.clone()), Vec::new());
    // Start > Run... > "C:\My Documents" opens the folder window.
    shell.key(
        Key::Escape,
        Mods {
            ctrl: true,
            ..Mods::default()
        },
    );
    press(&mut shell, Key::Char('r'));
    for ch in "C:\\My Documents".chars() {
        press(&mut shell, Key::Char(ch));
    }
    press(&mut shell, Key::Enter);
    assert_eq!(shell.explorer_path().as_deref(), Some("\\My Documents"));
    // File > New > Text Document, named "My Letter.txt".
    shell.key(
        Key::Char('f'),
        Mods {
            alt: true,
            ..Mods::default()
        },
    );
    press(&mut shell, Key::Char('w'));
    press(&mut shell, Key::Down); // past the separator, to Text Document
    press(&mut shell, Key::Enter);
    for ch in "My Letter.txt".chars() {
        press(&mut shell, Key::Char(ch));
    }
    press(&mut shell, Key::Enter);
    let document = drive.join("My Documents/My Letter.txt");
    assert!(document.is_file());
    // Enter opens it with the program WIN.INI associates with .txt.
    press(&mut shell, Key::Enter);
    let request = shell
        .take_launch()
        .expect("opening the document starts Notepad");
    assert_eq!(request.path, "\\WINDOWS\\NOTEPAD.EXE");
    assert_eq!(request.arguments, "C:\\My Documents\\My Letter.txt");

    // Run the program the way the desktop does, typing "Hi" and saving with
    // File > Save (Win95 Notepad has no Ctrl+S).
    std::env::set_var(
        "OTW_INPUT",
        "w 2; k 0x48 H; k 0x49 i; kd 0x12; k 0x46 f; ku 0x12; w 1; k 0x53 s; w 2",
    );
    let shared = Rc::new(RefCell::new(SharedState::default()));
    let command_line = format!("{} {}", request.name, request.arguments);
    let vm =
        build_vm(&volume, &request, shared, command_line).expect("Notepad loads from drive C:");
    shell.running = vec![request.name.clone()];
    let result = vm.run().expect("Notepad exits cleanly");
    assert_eq!(result.exit_code, 0);
    assert_eq!(fs::read(&document).unwrap(), b"Hi");
    shell.program_exited(None);
    assert!(shell.dialog_title().is_none());

    let _ = fs::remove_dir_all(root);
}
