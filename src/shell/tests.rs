//! Shell behaviour driven through pointer and key input, on a temporary
//! drive C: set up like a fresh install.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use super::*;

struct Fixture {
    shell: Shell,
    root: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture(tag: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!("otw-shell-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let source = root.join("source");
    std::fs::create_dir_all(&source).unwrap();
    // The genuine programs when present (for their icons); the shell only
    // needs the files to exist otherwise.
    for name in ["NOTEPAD.EXE", "MSPAINT.EXE"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("binaries/win95")
            .join(name);
        let bytes = std::fs::read(path).unwrap_or_else(|_| b"MZ".to_vec());
        std::fs::write(source.join(name), bytes).unwrap();
    }
    let volume = Volume::Host(root.join("C"));
    setup::install(&volume, Some(&source)).unwrap();
    let mut shell = Shell::new(Some(volume), Vec::new());
    shell.set_screen(1024, 700);
    Fixture { shell, root }
}

fn center(rect: Rect) -> (i32, i32) {
    (rect.left + rect.width() / 2, rect.top + rect.height() / 2)
}

fn click(shell: &mut Shell, (x, y): (i32, i32)) {
    shell.pointer_move(x, y);
    shell.pointer_down(x, y, Button::Left, false, &[]);
    shell.pointer_up(x, y, Button::Left, false);
}

fn right_click(shell: &mut Shell, (x, y): (i32, i32)) {
    shell.pointer_down(x, y, Button::Right, false, &[]);
    shell.pointer_up(x, y, Button::Right, false);
}

fn hover_item(shell: &mut Shell, label: &str) {
    let rect = shell
        .menu_item_rect(label)
        .unwrap_or_else(|| panic!("menu item {label} is open"));
    let (x, y) = center(rect);
    shell.pointer_move(x, y);
}

fn click_item(shell: &mut Shell, label: &str) {
    let rect = shell
        .menu_item_rect(label)
        .unwrap_or_else(|| panic!("menu item {label} is open"));
    click(shell, center(rect));
}

fn press(shell: &mut Shell, key: Key) {
    shell.key(key, Mods::default());
}

fn ctrl(shell: &mut Shell, ch: char) {
    shell.key(
        Key::Char(ch),
        Mods {
            ctrl: true,
            ..Mods::default()
        },
    );
}

fn type_text(shell: &mut Shell, text: &str) {
    for ch in text.chars() {
        press(shell, Key::Char(ch));
    }
}

fn disk(fixture: &Fixture, path: &str) -> std::path::PathBuf {
    fixture.root.join("C").join(path)
}

fn double_click(shell: &mut Shell, at: (i32, i32)) {
    click(shell, at);
    click(shell, at);
}

#[test]
fn start_menu_programs_launch_shortcuts_by_mouse() {
    let mut f = fixture("start-mouse");
    let start = f.shell.start_rect();
    click(&mut f.shell, center(start));
    assert!(f.shell.menu_open());
    hover_item(&mut f.shell, "Programs");
    hover_item(&mut f.shell, "Accessories");
    // The Accessories folder lists both shortcuts, without extensions.
    assert!(f.shell.menu_item_rect("Paint").is_some());
    click_item(&mut f.shell, "Notepad");
    assert!(!f.shell.menu_open());
    let launch = f.shell.take_launch().expect("Notepad launches");
    assert_eq!(launch.path, "\\WINDOWS\\NOTEPAD.EXE");
    assert_eq!(launch.arguments, "");
    // Clicking Start again toggles the menu.
    click(&mut f.shell, center(start));
    click(&mut f.shell, center(start));
    assert!(!f.shell.menu_open());
}

#[test]
fn start_menu_works_from_the_keyboard() {
    let mut f = fixture("start-keys");
    f.shell.key(
        Key::Escape,
        Mods {
            ctrl: true,
            ..Mods::default()
        },
    );
    assert!(f.shell.menu_open());
    press(&mut f.shell, Key::Char('p'));
    press(&mut f.shell, Key::Right);
    press(&mut f.shell, Key::Char('p'));
    press(&mut f.shell, Key::Enter);
    assert_eq!(
        f.shell.take_launch().unwrap().path,
        "\\WINDOWS\\MSPAINT.EXE"
    );
    press(&mut f.shell, Key::Logo);
    press(&mut f.shell, Key::Escape);
    assert!(!f.shell.menu_open());
}

#[test]
fn my_computer_opens_explorer_and_folders_navigate() {
    let mut f = fixture("explore");
    let at = center(f.shell.icon_hit(0));
    double_click(&mut f.shell, at);
    assert_eq!(f.shell.explorer_path().as_deref(), Some("\\"));
    // Type-ahead selects "My Documents"; Enter opens it, Backspace goes up.
    press(&mut f.shell, Key::Char('m'));
    press(&mut f.shell, Key::Enter);
    assert_eq!(f.shell.explorer_path().as_deref(), Some("\\My Documents"));
    press(&mut f.shell, Key::Backspace);
    assert_eq!(f.shell.explorer_path().as_deref(), Some("\\"));
    // Double-clicking a row opens it.
    let e = f.shell.explorer.as_ref().unwrap();
    let windows_row = e
        .entries
        .iter()
        .position(|entry| entry.name == "WINDOWS")
        .unwrap();
    let rows = f.shell.rows_rect(e);
    let at = (
        rows.left + 40,
        rows.top + windows_row as i32 * ROW_HEIGHT + 8,
    );
    double_click(&mut f.shell, at);
    assert_eq!(f.shell.explorer_path().as_deref(), Some("\\WINDOWS"));
    // Alt+F4 closes the folder window.
    f.shell.key(
        Key::F4,
        Mods {
            alt: true,
            ..Mods::default()
        },
    );
    assert!(f.shell.explorer_path().is_none());
}

#[test]
fn file_menu_creates_and_renames_a_folder() {
    let mut f = fixture("new-folder");
    f.shell.explore(parse_path(setup::DOCUMENTS));
    let file = f.shell.bar_rects()[0];
    click(&mut f.shell, center(file));
    hover_item(&mut f.shell, "New");
    click_item(&mut f.shell, "Folder");
    assert!(disk(&f, "My Documents/New Folder").is_dir());
    // The new folder is in rename mode with its name selected.
    type_text(&mut f.shell, "Projects");
    press(&mut f.shell, Key::Enter);
    assert!(disk(&f, "My Documents/Projects").is_dir());
    assert!(!disk(&f, "My Documents/New Folder").exists());
    // Alt+F, W, F through the keyboard creates the next one.
    f.shell.key(
        Key::Char('f'),
        Mods {
            alt: true,
            ..Mods::default()
        },
    );
    press(&mut f.shell, Key::Char('w'));
    press(&mut f.shell, Key::Char('f'));
    press(&mut f.shell, Key::Escape);
    assert!(disk(&f, "My Documents/New Folder").is_dir());
}

#[test]
fn desktop_context_menu_creates_a_text_document_that_opens_in_notepad() {
    let mut f = fixture("new-text");
    right_click(&mut f.shell, (600, 400));
    hover_item(&mut f.shell, "New");
    click_item(&mut f.shell, "Text Document");
    press(&mut f.shell, Key::Enter);
    assert!(disk(&f, "WINDOWS/Desktop/New Text Document.txt").is_file());
    let index = f
        .shell
        .icons
        .iter()
        .position(|icon| icon.label == "New Text Document.txt")
        .expect("desktop icon");
    let at = center(f.shell.icon_hit(index));
    double_click(&mut f.shell, at);
    let launch = f.shell.take_launch().unwrap();
    assert_eq!(launch.path, "\\WINDOWS\\NOTEPAD.EXE");
    assert_eq!(
        launch.arguments,
        "C:\\WINDOWS\\Desktop\\New Text Document.txt"
    );
}

#[test]
fn delete_asks_for_confirmation() {
    let mut f = fixture("delete");
    std::fs::write(disk(&f, "My Documents/old.txt"), b"x").unwrap();
    f.shell.explore(parse_path(setup::DOCUMENTS));
    press(&mut f.shell, Key::Home);
    press(&mut f.shell, Key::Delete);
    assert_eq!(f.shell.dialog_title(), Some("Confirm File Delete"));
    press(&mut f.shell, Key::Char('n'));
    assert!(disk(&f, "My Documents/old.txt").exists());
    press(&mut f.shell, Key::Delete);
    press(&mut f.shell, Key::Enter);
    assert!(!disk(&f, "My Documents/old.txt").exists());
}

#[test]
fn copy_paste_and_cut_paste_files() {
    let mut f = fixture("clipboard");
    std::fs::write(disk(&f, "My Documents/note.txt"), b"hi").unwrap();
    f.shell.explore(parse_path(setup::DOCUMENTS));
    press(&mut f.shell, Key::Home);
    ctrl(&mut f.shell, 'c');
    ctrl(&mut f.shell, 'v');
    assert_eq!(
        std::fs::read(disk(&f, "My Documents/Copy of note.txt")).unwrap(),
        b"hi"
    );
    // Cut the original and paste it on the desktop.
    f.shell.select_name(Place::Explorer, "note.txt");
    ctrl(&mut f.shell, 'x');
    click(&mut f.shell, (700, 500));
    ctrl(&mut f.shell, 'v');
    assert!(disk(&f, "WINDOWS/Desktop/note.txt").exists());
    assert!(!disk(&f, "My Documents/note.txt").exists());
}

#[test]
fn rename_rejects_invalid_and_duplicate_names() {
    let mut f = fixture("rename");
    std::fs::write(disk(&f, "My Documents/a.txt"), b"").unwrap();
    std::fs::write(disk(&f, "My Documents/b.txt"), b"").unwrap();
    f.shell.explore(parse_path(setup::DOCUMENTS));
    press(&mut f.shell, Key::Home);
    press(&mut f.shell, Key::F2);
    type_text(&mut f.shell, "b.txt");
    press(&mut f.shell, Key::Enter);
    assert_eq!(f.shell.dialog_title(), Some("Error Renaming File"));
    press(&mut f.shell, Key::Enter);
    press(&mut f.shell, Key::F2);
    press(&mut f.shell, Key::End);
    for _ in 0..4 {
        press(&mut f.shell, Key::Backspace);
    }
    type_text(&mut f.shell, "?.txt");
    press(&mut f.shell, Key::Enter);
    assert!(f.shell.dialog_title().is_some());
    press(&mut f.shell, Key::Escape);
    press(&mut f.shell, Key::F2);
    type_text(&mut f.shell, "Letter.txt");
    press(&mut f.shell, Key::Escape);
    assert!(disk(&f, "My Documents/a.txt").exists());
}

#[test]
fn run_dialog_starts_programs_with_arguments() {
    let mut f = fixture("run");
    f.shell.key(Key::Logo, Mods::default());
    press(&mut f.shell, Key::Char('r'));
    assert_eq!(f.shell.dialog_title(), Some("Run"));
    type_text(&mut f.shell, "notepad C:\\README.TXT");
    press(&mut f.shell, Key::Enter);
    let launch = f.shell.take_launch().unwrap();
    assert_eq!(
        (launch.path.as_str(), launch.arguments.as_str()),
        ("\\WINDOWS\\NOTEPAD.EXE", "C:\\README.TXT")
    );
}

#[test]
fn programs_start_while_others_run_but_block_shut_down() {
    let mut f = fixture("busy");
    f.shell.running = vec!["NOTEPAD.EXE".into()];
    f.shell.open_path(parse_path("\\WINDOWS\\MSPAINT.EXE"), 0);
    assert_eq!(f.shell.take_launch().unwrap().name, "MSPAINT.EXE");
    f.shell.execute(Command::ShutDown, Place::Desktop);
    assert_eq!(f.shell.dialog_title(), Some("Shut Down"));
    press(&mut f.shell, Key::Char('y'));
    assert!(!f.shell.quit, "a message, not the confirmation");
}

#[test]
fn shut_down_asks_first() {
    let mut f = fixture("shutdown");
    f.shell.key(
        Key::F4,
        Mods {
            alt: true,
            ..Mods::default()
        },
    );
    assert_eq!(f.shell.dialog_title(), Some("Shut Down"));
    press(&mut f.shell, Key::Char('y'));
    assert!(f.shell.quit);
}

#[test]
fn programs_get_the_keyboard_only_when_focused() {
    let mut f = fixture("focus");
    assert!(f.shell.wants_keyboard());
    assert_eq!(
        f.shell.pointer_down(300, 300, Button::Left, true, &[]),
        PointerOutcome::Unhandled
    );
    assert!(!f.shell.wants_keyboard());
    assert!(!f.shell.key(Key::Char('a'), Mods::default()));
    let tasks = [Task {
        label: "Untitled - Notepad".into(),
        active: true,
        program: "NOTEPAD.EXE".into(),
    }];
    let button = f.shell.task_rects(1)[0];
    let (x, y) = center(button);
    assert_eq!(
        f.shell.pointer_down(x, y, Button::Left, false, &tasks),
        PointerOutcome::Task(0)
    );
}

#[test]
fn every_layer_paints() {
    let mut f = fixture("paint");
    f.shell.explore(parse_path(setup::WINDOWS));
    f.shell.open_menu(MenuOwner::Start, None, false);
    let mut surface = Surface::new(1024, 700, 24);
    for layer in [Layer::Background, Layer::Foreground, Layer::Top] {
        f.shell.paint(
            &mut surface,
            layer,
            &[Task {
                label: "Paint".into(),
                active: false,
                program: "MSPAINT.EXE".into(),
            }],
        );
    }
    // Desktop teal, taskbar face, and the Start menu's banner.
    assert_eq!(surface.get(1000, 600), Some(0x008080));
    assert_eq!(surface.get(600, 690), Some(0xC0C0C0));
    let menu = f
        .shell
        .menu
        .as_ref()
        .unwrap()
        .menu
        .item_rect("Programs")
        .unwrap();
    assert_eq!(surface.get(menu.left - 10, menu.top + 2), Some(0x808080));
    // The folder window shows Notepad's own icon (not the generic one).
    let e = f.shell.explorer.as_ref().unwrap();
    let row = e
        .entries
        .iter()
        .position(|entry| entry.name == "NOTEPAD.EXE")
        .unwrap();
    let (icon, _) = f
        .shell
        .icon_for(&parse_path("\\WINDOWS\\NOTEPAD.EXE"), false, 16, 0);
    if crate::test_binary("win95/NOTEPAD.EXE").is_some() {
        assert_ne!(icon, icons::builtin(Builtin::Program));
    }
    assert!(row < f.shell.visible_rows(e));
}

#[test]
fn folder_window_minimizes_maximizes_and_restores() {
    let mut f = fixture("winstate");
    f.shell.explore(parse_path(setup::WINDOWS));
    let normal = f.shell.explorer.as_ref().unwrap().rect;

    // The maximize button fills the desktop above the taskbar.
    let max = Shell::max_rect(f.shell.explorer.as_ref().unwrap());
    click(&mut f.shell, center(max));
    assert_eq!(
        f.shell.explorer.as_ref().unwrap().rect,
        Rect::new(0, 0, 1024, 700 - TASKBAR_HEIGHT)
    );
    // Double-clicking the title bar restores it.
    let caption = Shell::caption_rect(f.shell.explorer.as_ref().unwrap());
    double_click(&mut f.shell, (caption.left + 100, caption.top + 8));
    assert_eq!(f.shell.explorer.as_ref().unwrap().rect, normal);

    // Minimize hides it; its taskbar button brings it back, and clicking
    // the button of the window in use minimizes it again.
    let min = Shell::min_rect(f.shell.explorer.as_ref().unwrap());
    click(&mut f.shell, center(min));
    assert!(!f.shell.foreground_explorer());
    assert!(!f.shell.in_explorer(normal.left + 50, normal.top + 50));
    let button = center(f.shell.task_rects(1)[0]);
    click(&mut f.shell, button);
    assert!(f.shell.foreground_explorer());
    click(&mut f.shell, button);
    assert!(!f.shell.foreground_explorer());
    click(&mut f.shell, button);

    // The window menu from the keyboard: Alt+Space, X maximizes; from the
    // caption icon, R restores.
    f.shell.key(
        Key::Char(' '),
        Mods {
            alt: true,
            ..Mods::default()
        },
    );
    assert!(f.shell.menu_open());
    press(&mut f.shell, Key::Char('x'));
    assert!(f.shell.explorer.as_ref().unwrap().restore.is_some());
    let icon = Shell::icon_rect(f.shell.explorer.as_ref().unwrap());
    click(&mut f.shell, center(icon));
    press(&mut f.shell, Key::Char('r'));
    assert_eq!(f.shell.explorer.as_ref().unwrap().rect, normal);
}

#[test]
fn tray_icons_report_clicks_and_show_tips() {
    let mut f = fixture("tray");
    f.shell.set_tray(vec![TrayItem {
        icon: None,
        tip: "Volume".into(),
    }]);
    let icon = center(f.shell.tray_icon_rect(0));
    // The clock and icon take space from the task buttons.
    assert!(f.shell.task_rects(1)[0].right <= f.shell.tray_rect().left);

    assert_eq!(
        f.shell
            .pointer_down(icon.0, icon.1, Button::Left, false, &[]),
        PointerOutcome::Tray(0, TrayEvent::Down(Button::Left))
    );
    assert_eq!(
        f.shell.pointer_up(icon.0, icon.1, Button::Left, false),
        PointerOutcome::Tray(0, TrayEvent::Up(Button::Left))
    );
    assert_eq!(
        f.shell
            .pointer_down(icon.0, icon.1, Button::Left, false, &[]),
        PointerOutcome::Tray(0, TrayEvent::DoubleClick(Button::Left))
    );
    f.shell.pointer_up(icon.0, icon.1, Button::Left, false);
    assert_eq!(
        f.shell
            .pointer_down(icon.0, icon.1, Button::Right, false, &[]),
        PointerOutcome::Tray(0, TrayEvent::Down(Button::Right))
    );
    assert_eq!(
        f.shell.pointer_up(icon.0, icon.1, Button::Right, false),
        PointerOutcome::Tray(0, TrayEvent::Up(Button::Right))
    );
    assert_eq!(TrayEvent::DoubleClick(Button::Left).message(), 0x203);
    assert_eq!(TrayEvent::Up(Button::Right).message(), 0x205);

    // Hovering shows the tip above the taskbar.
    assert!(f.shell.pointer_move(icon.0, icon.1));
    let mut surface = Surface::new(1024, 700, 24);
    f.shell.paint(&mut surface, Layer::Top, &[]);
    let tray = f.shell.tray_rect();
    assert_eq!(
        surface.get(icon.0, tray.top - 20),
        Some(0xFFFFE1),
        "tooltip background"
    );
}
