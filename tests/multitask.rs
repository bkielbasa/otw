//! Several genuine programs running at the same time in one session: each
//! keeps its own windows and text, the clipboard is shared between them, and
//! one exiting leaves the others running.

mod common;

use std::fs;

use otw_vm::{
    session::{Program, Session},
    shell::launch::{self, LaunchRequest},
    shell::{setup, volume::Volume},
    win32::{GuestMessage, RawInput},
};

fn notepad() -> LaunchRequest {
    LaunchRequest {
        name: "NOTEPAD.EXE".into(),
        path: "\\WINDOWS\\NOTEPAD.EXE".into(),
        arguments: String::new(),
    }
}

fn settle(session: &Session) {
    for _ in 0..8 {
        session.run_programs();
    }
}

fn main_window(program: &Program) -> u32 {
    program
        .shared
        .borrow()
        .gui
        .first_visible_top_level()
        .expect("program has a window")
}

/// Bring a program to the front and activate its window, as a click does.
fn activate(session: &Session, program: &Program) {
    session.bring_to_front(program.pid);
    let hwnd = main_window(program);
    program.shared.borrow_mut().gui.pending_activate = Some(hwnd);
    settle(session);
}

fn key(program: &Program, vk: u32, ch: Option<u32>) {
    let mut shared = program.shared.borrow_mut();
    shared
        .gui
        .input
        .push_back(RawInput::Key { vk, down: true, ch });
    shared.gui.input.push_back(RawInput::Key {
        vk,
        down: false,
        ch: None,
    });
}

fn hold(program: &Program, vk: u32, down: bool) {
    program
        .shared
        .borrow_mut()
        .gui
        .input
        .push_back(RawInput::Key { vk, down, ch: None });
}

fn type_text(session: &Session, program: &Program, text: &str) {
    for ch in text.chars() {
        key(program, ch.to_ascii_uppercase() as u32, Some(ch as u32));
    }
    settle(session);
}

fn text(program: &Program) -> Vec<u8> {
    let hwnd = main_window(program);
    program
        .shared
        .borrow()
        .gui
        .client_text_for(hwnd)
        .unwrap_or(&[])
        .to_vec()
}

#[test]
fn programs_run_side_by_side_and_share_the_clipboard() {
    if !common::have(common::NOTEPAD) {
        return;
    }
    std::env::set_var("OTW_HOST_CLIPBOARD", "0");
    let root = std::env::temp_dir().join(format!("otw-multitask-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    std::env::set_var("OTW_DRIVE_C", root.join("C"));
    let volume = Volume::Host(root.join("C"));
    setup::install(&volume, setup::default_source().as_deref()).unwrap();

    let session = Session::new();
    let first = launch::spawn(&session, &volume, &notepad(), (1024, 700)).unwrap();
    settle(&session);
    let second = launch::spawn(&session, &volume, &notepad(), (1024, 700)).unwrap();
    settle(&session);
    assert_eq!(session.programs().len(), 2, "both programs are running");
    let (a, b) = (
        session.program(first).unwrap(),
        session.program(second).unwrap(),
    );
    // Each program has its own address space and window list.
    assert!(!a.shared.borrow().gui.windows.is_empty() && !b.shared.borrow().gui.windows.is_empty());

    activate(&session, &a);
    type_text(&session, &a, "One");
    activate(&session, &b);
    // The program that lost the foreground was deactivated.
    assert_eq!(a.shared.borrow().gui.active_hwnd, 0);
    type_text(&session, &b, "Two");
    assert_eq!(text(&a), b"One");
    assert_eq!(text(&b), b"Two");

    // Copy in the second program (Shift+Home, Ctrl+C), paste in the first.
    hold(&b, 0x10, true);
    key(&b, 0x24, None);
    hold(&b, 0x10, false);
    hold(&b, 0x11, true);
    key(&b, 0x43, Some(3));
    hold(&b, 0x11, false);
    settle(&session);
    activate(&session, &a);
    hold(&a, 0x11, true);
    key(&a, 0x56, Some(22));
    hold(&a, 0x11, false);
    settle(&session);
    assert_eq!(text(&a), b"OneTwo");

    // A third, unmodified Notepad closes; the others keep running.
    let third = launch::spawn(&session, &volume, &notepad(), (1024, 700)).unwrap();
    settle(&session);
    let c = session.program(third).unwrap();
    let hwnd = main_window(&c);
    c.shared.borrow_mut().gui.enqueue(GuestMessage {
        hwnd,
        message: 0x0010,
        wparam: 0,
        lparam: 0,
    });
    settle(&session);
    let exits = session.take_exits();
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].pid, third);
    assert_eq!(exits[0].result.as_ref().map(|r| r.exit_code), Ok(0));
    assert_eq!(session.programs().len(), 2);
    type_text(&session, &a, "!");
    assert_eq!(text(&a), b"OneTwo!");

    let _ = fs::remove_dir_all(root);
}
