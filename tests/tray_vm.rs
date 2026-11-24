//! Taskbar notification-area icons: the hand-built TRAYTEST.EXE (see
//! scripts/build-tray-pe.py) adds an icon with `Shell_NotifyIconA`, changes
//! its tip on a double-click and removes it on a right-click.

use std::{cell::RefCell, rc::Rc};

use otw_vm::{kernel::pe::PeImage, session::Session, vm::Vm, win32::SharedState};

const TRAYTEST_EXE: &[u8] = include_bytes!("../binaries/guests/TRAYTEST.EXE");

fn settle(session: &Session) {
    for _ in 0..8 {
        session.run_programs();
    }
}

#[test]
fn a_program_adds_changes_and_removes_a_tray_icon() {
    let session = Session::new();
    let load = |shared: Rc<RefCell<SharedState>>| {
        let image = PeImage::parse(TRAYTEST_EXE.to_vec()).map_err(|e| e.to_string())?;
        Vm::with_modules_shared(
            vec![("TRAYTEST.EXE".into(), image)],
            shared,
            "TRAYTEST.EXE".into(),
        )
        .map_err(|e| e.to_string())
    };
    let pid = session.spawn("TRAYTEST.EXE", (1024, 700), load).unwrap();
    settle(&session);
    let program = session.program(pid).expect("the program is running");
    {
        let shared = program.shared.borrow();
        assert_eq!(shared.gui.tray.len(), 1);
        let icon = &shared.gui.tray[0];
        assert_eq!(
            (icon.id, icon.callback, icon.tip.as_str()),
            (1, 0x401, "OTW tray test")
        );
        // LoadIconA(NULL, IDI_APPLICATION) has a real image.
        let image = shared.gui.small_icon(icon.icon).expect("icon image");
        assert_eq!((image.width, image.height), (16, 16));
    }

    // A double-click reaches the program as its callback message.
    program.shared.borrow_mut().gui.tray_event(0, 0x203);
    settle(&session);
    assert_eq!(program.shared.borrow().gui.tray[0].tip, "Clicked");

    // A right-click removes the icon and ends the program.
    program.shared.borrow_mut().gui.tray_event(0, 0x205);
    settle(&session);
    assert!(program.shared.borrow().gui.tray.is_empty());
    let exits = session.take_exits();
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].result.as_ref().map(|r| r.exit_code), Ok(0));
}
