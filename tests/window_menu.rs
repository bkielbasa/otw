//! The window menu (system menu) and minimize/maximize on genuine Notepad:
//! the caption icon and Alt+Space open it, its commands maximize, restore,
//! move and minimize the window.

mod common;

use std::{cell::RefCell, fs, rc::Rc};

use otw_vm::{kernel::pe::PeImage, vm::Vm, win32::SharedState};

fn notepad(shared: &Rc<RefCell<SharedState>>, script: &str) -> (i32, i32, bool, bool) {
    std::env::set_var("OTW_INPUT", script);
    let bytes = fs::read(format!(
        "{}/binaries/win95/NOTEPAD.EXE",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let image = PeImage::parse(bytes).unwrap();
    let vm = Vm::with_modules_shared(
        vec![("NOTEPAD.EXE".into(), image)],
        shared.clone(),
        "NOTEPAD.EXE".into(),
    )
    .unwrap();
    assert_eq!(vm.run().expect("Notepad exits cleanly").exit_code, 0);
    let state = shared.borrow();
    let window = state
        .gui
        .windows
        .iter()
        .find(|w| w.class == "Notepad")
        .expect("main window");
    (window.x, window.y, window.maximized, window.minimized)
}

#[test]
fn window_menu_maximizes_restores_moves_and_minimizes() {
    if !common::have(common::NOTEPAD) {
        return;
    }
    std::env::set_var("OTW_IDLE_EXIT", "3");
    std::env::set_var("OTW_HOST_CLIPBOARD", "0");
    std::env::set_var(
        "OTW_DRIVE_C",
        std::env::temp_dir().join(format!("otw-winmenu-{}", std::process::id())),
    );
    const ALT_SPACE: &str = "kd 0x12; k 0x20 #32; ku 0x12; w 1";

    // The caption icon opens the window menu; X maximizes.
    let shared = Rc::new(RefCell::new(SharedState::default()));
    let (_, _, maximized, _) = notepad(&shared, "w 2; m 33 28; d; u; w 1; k 0x58 x; w 2");
    assert!(maximized);

    // Move: two steps right with the arrow keys, Enter to accept.
    let shared = Rc::new(RefCell::new(SharedState::default()));
    let script = format!("w 2; {ALT_SPACE}; k 0x4D m; w 1; k 0x27; k 0x27; k 0x0D #13; w 2");
    let (x, y, maximized, minimized) = notepad(&shared, &script);
    assert_eq!((x, y, maximized, minimized), (24 + 16, 16, false, false));

    // Maximize then Restore from the menu, then Minimize.
    let shared = Rc::new(RefCell::new(SharedState::default()));
    let script = format!(
        "w 2; {ALT_SPACE}; k 0x58 x; w 1; {ALT_SPACE}; k 0x52 r; w 1; {ALT_SPACE}; k 0x4E n; w 2"
    );
    let (x, y, maximized, minimized) = notepad(&shared, &script);
    assert_eq!((x, y, maximized, minimized), (24, 16, false, true));
    // The minimized window gave up the activation.
    assert_eq!(shared.borrow().gui.active_hwnd, 0);
}
