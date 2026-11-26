//! The desktop: OTW's own shell (`otw_vm::shell`), the same code as on
//! Linux, drawn on the framebuffer and driven by the PS/2 keyboard and
//! mouse.

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicPtr, Ordering};

use otw_vm::shell::volume::Volume;
use otw_vm::shell::{Button, Key, Layer, Mods, PointerOutcome, Shell, Task};
use otw_vm::win32::gfx::Surface;
use otw_vm::win32::user::{top_level_at, WS_CHILD};
use otw_vm::win32::{RawInput, SharedState};

use crate::io::{self, Input};
use crate::{apic, log, System};

struct Input8042 {
    pointer: (i32, i32),
    buttons: u8,
    packet: [u8; 3],
    packet_len: usize,
    mods: Mods,
    extended: bool,
}

/// A program on the desktop: its windows, and where its input goes.
type Program = Rc<RefCell<SharedState>>;

/// The desktop's state, shared by its own loop and by programs' waits
/// (which keep the desktop going while a program waits for input).
pub struct Desktop {
    shell: Shell,
    input: Input8042,
    frame: Surface,
    screen: ScreenCopy,
    stats: FrameStats,
    dirty: bool,
    width: i32,
    height: i32,
    /// The running program (one at a time, for now).
    program: Option<(Program, String)>,
    last_frame: u64,
}

static DESKTOP: AtomicPtr<Desktop> = AtomicPtr::new(core::ptr::null_mut());

pub fn run(system: &System) -> ! {
    let info = &system.info;
    let (width, height) = (info.width, info.height);
    let started = apic::now_us();
    let mut shell = Shell::new(Some(Volume::drive_c()), Vec::new());
    shell.set_screen(width as i32, height as i32);
    log!("OTW kernel: shell ready in {} ms ({} ms since start)", (apic::now_us() - started) / 1000, crate::ms_since_start());
    for note in &system.notes {
        log!("OTW kernel: {note}");
    }
    let desktop = Box::leak(Box::new(Desktop {
        shell,
        input: Input8042 {
            pointer: (width as i32 / 2, height as i32 / 2),
            buttons: 0,
            packet: [0; 3],
            packet_len: 0,
            mods: Mods::default(),
            extended: false,
        },
        frame: Surface::new(width as i32, height as i32, 24),
        screen: ScreenCopy::new(info),
        stats: FrameStats::default(),
        dirty: true,
        width: width as i32,
        height: height as i32,
        program: None,
        last_frame: 0,
    }));
    DESKTOP.store(desktop, Ordering::SeqCst);
    loop {
        let desktop = unsafe { &mut *DESKTOP.load(Ordering::SeqCst) };
        desktop.step(true);
        if let Some(request) = desktop.shell.take_launch() {
            launch(&request.path, &request.name);
        }
    }
}

/// Run a program the shell asked for, to its end; the desktop keeps going
/// while it waits.
fn launch(path: &str, name: &str) {
    // The shell names paths on drive C: from its root.
    let path = &if path.starts_with('\\') { format!("C:{path}") } else { String::from(path) };
    log!("OTW kernel: asked to run {path}");
    let ended = crate::process::run(path, name);
    let desktop = unsafe { &mut *DESKTOP.load(Ordering::SeqCst) };
    desktop.program = None;
    desktop.shell.program_exited(None);
    match ended {
        crate::process::Ended::Exited(code) => {
            log!("OTW kernel: {name} exited with {code}");
            if code != 0 {
                desktop.shell.show_message(name, &format!("{name} exited with code {code}."));
            }
        }
        crate::process::Ended::Failed(why) => {
            log!("OTW kernel: {name} failed: {why}");
            desktop.shell.show_message("Program Error", &format!("{name}: {why}"));
        }
    }
    desktop.dirty = true;
}

/// A program starts: it is shown on the desktop, gets the input.
pub fn program_started(program: &Program, name: &str) {
    let desktop = unsafe { &mut *DESKTOP.load(Ordering::SeqCst) };
    program.borrow_mut().gui.screen = (desktop.width, desktop.height);
    desktop.program = Some((program.clone(), String::from(name)));
    desktop.shell.focus_program();
    desktop.dirty = true;
}

/// A program waits (for input, or to let time pass): keep the desktop
/// going — input, launching, drawing.
pub fn pump(block: bool) {
    let pointer = DESKTOP.load(Ordering::SeqCst);
    if pointer.is_null() {
        return;
    }
    let desktop = unsafe { &mut *pointer };
    desktop.step(block);
    if let Some(request) = desktop.shell.take_launch() {
        desktop.shell.show_message(&request.name, "One program runs at a time so far: close the running one first.");
        desktop.dirty = true;
    }
}

impl Desktop {
    fn step(&mut self, block: bool) {
        let pointer_before = self.input.pointer;
        while let Some(byte) = io::ps2_poll() {
            self.dirty |= self.handle(byte);
        }
        self.dirty |= self.shell.clock_changed();
        // Programs draw into their windows at any time: with one running,
        // the screen follows (at most 30 times a second; only changed rows
        // reach it).
        let now = apic::now_us();
        if self.program.is_some() && now - self.last_frame >= 33_000 {
            self.dirty = true;
        }
        if self.dirty {
            self.draw();
            self.last_frame = now;
        } else if self.input.pointer != pointer_before {
            // Only the pointer moved: put back what was under it, draw it
            // again.
            self.screen.move_pointer(self.input.pointer);
        }
        if block {
            // Sleep a millisecond (input interrupts; other tasks run), not
            // holding a program's subsystem lock meanwhile.
            otw_vm::kernel::lock::unlocked(|| crate::sched::sleep_us(1000));
        }
    }

    fn tasks(&self) -> Vec<Task> {
        let Some((program, name)) = &self.program else { return Vec::new() };
        let shared = program.borrow();
        shared
            .gui
            .windows
            .iter()
            .filter(|w| w.visible && w.parent == 0 && w.style & WS_CHILD == 0 && !w.title.is_empty())
            .map(|w| Task { label: w.title.clone(), active: w.hwnd == shared.gui.active_hwnd, program: name.clone() })
            .collect()
    }

    fn draw(&mut self) {
        let started = apic::now_us();
        let tasks = self.tasks();
        self.frame.pixels.fill(0);
        self.shell.paint(&mut self.frame, Layer::Background, &tasks);
        if let Some((program, _)) = &self.program {
            let shared = program.borrow();
            for window in &shared.gui.windows {
                if !window.visible || window.minimized || window.style & WS_CHILD != 0 {
                    continue;
                }
                if let Some(surface) = shared.gdi.surfaces.get(&window.hwnd) {
                    blit(&mut self.frame, surface, window.x, window.y);
                }
            }
        }
        self.shell.paint(&mut self.frame, Layer::Foreground, &tasks);
        self.shell.paint(&mut self.frame, Layer::Top, &tasks);
        let painted = apic::now_us();
        let rows = self.screen.show(&self.frame, self.input.pointer);
        if self.stats.frames == 0 {
            log!("OTW kernel: desktop on the screen ({} ms since start)", crate::ms_since_start());
        }
        self.stats.add(painted - started, apic::now_us() - painted, rows);
        self.dirty = false;
    }

    /// Whether the pointer is over one of the program's windows.
    fn over_program(&self, x: i32, y: i32) -> bool {
        self.program.as_ref().is_some_and(|(p, _)| top_level_at(&p.borrow().gui, x, y).is_some())
    }

    fn to_program(&self, event: RawInput) {
        if let Some((program, _)) = &self.program {
            let mut shared = program.borrow_mut();
            if let (RawInput::Move { .. }, Some(RawInput::Move { .. })) = (event, shared.gui.input.back()) {
                shared.gui.input.pop_back();
            }
            shared.gui.input.push_back(event);
        }
    }

    fn handle(&mut self, byte: Input) -> bool {
        let (width, height) = (self.width, self.height);
        match byte {
            Input::Mouse(byte) => {
                let input = &mut self.input;
                // Packets are 3 bytes; the first always has bit 3 set.
                if input.packet_len == 0 && byte & 0x08 == 0 {
                    return false;
                }
                input.packet[input.packet_len] = byte;
                input.packet_len += 1;
                if input.packet_len < 3 {
                    return false;
                }
                input.packet_len = 0;
                let [flags, dx, dy] = input.packet;
                let dx = dx as i32 - if flags & 0x10 != 0 { 256 } else { 0 };
                let dy = dy as i32 - if flags & 0x20 != 0 { 256 } else { 0 };
                let (x, y) = ((input.pointer.0 + dx).clamp(0, width - 1), (input.pointer.1 - dy).clamp(0, height - 1));
                let moved = (x, y) != input.pointer;
                input.pointer = (x, y);
                let held = input.buttons;
                // A move needs a frame only if the shell changed (dragging,
                // hovering); otherwise only the pointer is drawn again.
                let mut changed = moved && self.shell.pointer_move(x, y);
                if moved && (held != 0 || self.over_program(x, y)) {
                    self.to_program(RawInput::Move { x, y });
                }
                let tasks = self.tasks();
                for (bit, button, number) in [(1u8, Button::Left, 0u8), (2u8, Button::Right, 1u8)] {
                    let now = flags & bit != 0;
                    let before = held & bit != 0;
                    if now == before {
                        continue;
                    }
                    let over = self.over_program(x, y);
                    let outcome = if now {
                        self.shell.pointer_down(x, y, button, over, &tasks)
                    } else {
                        self.shell.pointer_up(x, y, button, over)
                    };
                    if matches!(outcome, PointerOutcome::Unhandled) && self.program.is_some() {
                        if now {
                            self.shell.focus_program();
                        }
                        self.to_program(RawInput::Button { x, y, button: number, down: now });
                    }
                    changed = true;
                }
                self.input.buttons = flags & 7;
                changed
            }
            Input::Key(0xE0) => {
                self.input.extended = true;
                false
            }
            Input::Key(code) => {
                let extended = core::mem::take(&mut self.input.extended);
                let released = code & 0x80 != 0;
                let scan = code & 0x7F;
                match scan {
                    0x2A | 0x36 => self.input.mods.shift = !released,
                    0x1D => self.input.mods.ctrl = !released,
                    0x38 => self.input.mods.alt = !released,
                    _ => {}
                }
                let to_program = self.program.is_some() && !self.shell.wants_keyboard();
                if to_program {
                    if let Some(vk) = virtual_key(scan, extended) {
                        let ch = if released {
                            None
                        } else {
                            match key_for(scan, extended, self.input.mods.shift) {
                                Some(Key::Char(c)) => Some(c as u32),
                                Some(Key::Enter) => Some(0x0D),
                                Some(Key::Backspace) => Some(0x08),
                                Some(Key::Tab) => Some(0x09),
                                Some(Key::Escape) => Some(0x1B),
                                _ => None,
                            }
                        };
                        let ch = if self.input.mods.ctrl && (0x41..=0x5A).contains(&vk) && !released { Some(vk - 0x40) } else { ch };
                        self.to_program(RawInput::Key { vk, down: !released, ch });
                    }
                    return false;
                }
                if released || matches!(scan, 0x2A | 0x36 | 0x1D | 0x38) {
                    return false;
                }
                match key_for(scan, extended, self.input.mods.shift) {
                    Some(key) => self.shell.key(key, self.input.mods),
                    None => false,
                }
            }
        }
    }
}

/// Copy a window's surface onto the frame.
fn blit(frame: &mut Surface, surface: &Surface, x: i32, y: i32) {
    let x0 = x.max(0);
    let x1 = (x + surface.width).min(frame.width);
    if x1 <= x0 {
        return;
    }
    for dy in y.max(0)..(y + surface.height).min(frame.height) {
        let source = ((dy - y) * surface.width + (x0 - x)) as usize;
        let target = (dy * frame.width + x0) as usize;
        let n = (x1 - x0) as usize;
        frame.pixels[target..target + n].copy_from_slice(&surface.pixels[source..source + n]);
    }
}

/// Set 1 scan codes to Windows virtual keys.
fn virtual_key(scan: u8, extended: bool) -> Option<u32> {
    if extended {
        return Some(match scan {
            0x48 => 0x26,
            0x50 => 0x28,
            0x4B => 0x25,
            0x4D => 0x27,
            0x47 => 0x24,
            0x4F => 0x23,
            0x49 => 0x21,
            0x51 => 0x22,
            0x52 => 0x2D,
            0x53 => 0x2E,
            0x1C => 0x0D,
            0x1D => 0x11,
            0x38 => 0x12,
            0x5B | 0x5C => 0x5B,
            0x5D => 0x5D,
            _ => return None,
        });
    }
    const LETTERS: &[(u8, &str)] = &[(0x10, "QWERTYUIOP"), (0x1E, "ASDFGHJKL"), (0x2C, "ZXCVBNM")];
    for (first, row) in LETTERS {
        let index = scan.wrapping_sub(*first) as usize;
        if index < row.len() {
            return Some(row.as_bytes()[index] as u32);
        }
    }
    Some(match scan {
        0x02..=0x0A => 0x31 + (scan - 0x02) as u32,
        0x0B => 0x30,
        0x01 => 0x1B,
        0x0E => 0x08,
        0x0F => 0x09,
        0x1C => 0x0D,
        0x39 => 0x20,
        0x2A | 0x36 => 0x10,
        0x1D => 0x11,
        0x38 => 0x12,
        0x3B..=0x44 => 0x70 + (scan - 0x3B) as u32,
        0x57 => 0x7A,
        0x58 => 0x7B,
        0x0C => 0xBD,
        0x0D => 0xBB,
        0x1A => 0xDB,
        0x1B => 0xDD,
        0x27 => 0xBA,
        0x28 => 0xDE,
        0x29 => 0xC0,
        0x2B => 0xDC,
        0x33 => 0xBC,
        0x34 => 0xBE,
        0x35 => 0xBF,
        _ => return None,
    })
}

/// Set 1 scan codes to the shell's keys.
fn key_for(scan: u8, extended: bool, shift: bool) -> Option<Key> {
    if extended {
        return Some(match scan {
            0x48 => Key::Up,
            0x50 => Key::Down,
            0x4B => Key::Left,
            0x4D => Key::Right,
            0x47 => Key::Home,
            0x4F => Key::End,
            0x49 => Key::PageUp,
            0x51 => Key::PageDown,
            0x53 => Key::Delete,
            0x1C => Key::Enter,
            0x5B | 0x5C => Key::Logo,
            0x5D => Key::Apps,
            _ => return None,
        });
    }
    Some(match scan {
        0x01 => Key::Escape,
        0x0E => Key::Backspace,
        0x0F => Key::Tab,
        0x1C => Key::Enter,
        0x3C => Key::F2,
        0x3E => Key::F4,
        0x3F => Key::F5,
        0x44 => Key::F10,
        0x39 => Key::Char(' '),
        _ => {
            const ROWS: [(u8, &str, &str); 4] = [
                (0x02, "1234567890-=", "!@#$%^&*()_+"),
                (0x10, "qwertyuiop[]", "QWERTYUIOP{}"),
                (0x1E, "asdfghjkl;'`", "ASDFGHJKL:\"~"),
                (0x2C, "zxcvbnm,./", "ZXCVBNM<>?"),
            ];
            let mut found = None;
            for (first, plain, shifted) in ROWS {
                let chars = if shift { shifted } else { plain };
                let index = scan.wrapping_sub(first) as usize;
                if index < chars.len() {
                    found = chars.chars().nth(index);
                    break;
                }
            }
            if found.is_none() && scan == 0x2B {
                found = Some(if shift { '|' } else { '\\' });
            }
            Key::Char(found?)
        }
    })
}

const ARROW: [&str; 16] = [
    "X", "XX", "X.X", "X..X", "X...X", "X....X", "X.....X", "X......X", "X.......X", "X........X",
    "X.....XXXXX", "X..X..X", "X.X X..X", "XX  X..X", "X    X..X", "     XXX",
];

/// What is on the screen (without the pointer), so that a frame writes only
/// the rows that changed, and the pointer moves without a frame.
struct ScreenCopy {
    framebuffer: *mut u32,
    width: usize,
    height: usize,
    stride: usize,
    bgr: bool,
    pixels: Vec<u32>,
    row: Vec<u32>,
    pointer: (i32, i32),
}

impl ScreenCopy {
    fn new(info: &crate::BootInfo) -> Self {
        Self {
            framebuffer: info.framebuffer,
            width: info.width,
            height: info.height,
            stride: info.stride,
            bgr: info.bgr,
            // Nothing matches: the first frame is written whole.
            pixels: vec![0xFF00_0000; info.width * info.height],
            row: vec![0; info.width],
            pointer: (-100, -100),
        }
    }

    /// Show `frame`; returns how many rows changed.
    fn show(&mut self, frame: &Surface, pointer: (i32, i32)) -> usize {
        let width = self.width.min(frame.width as usize);
        let mut changed = 0;
        for y in 0..self.height.min(frame.height as usize) {
            let source = &frame.pixels[y * frame.width as usize..][..width];
            // The frame is 0x00RRGGBB: as it is for a BGR screen, swapped
            // for RGB.
            if self.bgr {
                self.row[..width].copy_from_slice(source);
            } else {
                for (target, &p) in self.row.iter_mut().zip(source) {
                    *target = (p & 0xFF) << 16 | (p & 0xFF00) | (p >> 16 & 0xFF);
                }
            }
            let copy = &mut self.pixels[y * self.width..][..width];
            if copy[..] != self.row[..width] {
                copy.copy_from_slice(&self.row[..width]);
                self.write_row(y, 0, width);
                changed += 1;
            }
        }
        // The rows under the old pointer may not have changed: restore them.
        self.restore_pointer_area();
        self.pointer = pointer;
        self.draw_pointer();
        changed
    }

    fn move_pointer(&mut self, pointer: (i32, i32)) {
        self.restore_pointer_area();
        self.pointer = pointer;
        self.draw_pointer();
    }

    fn write_row(&self, y: usize, x0: usize, x1: usize) {
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.pixels.as_ptr().add(y * self.width + x0),
                self.framebuffer.add(y * self.stride + x0),
                x1 - x0,
            );
        }
    }

    fn restore_pointer_area(&self) {
        let (px, py) = self.pointer;
        let x0 = px.clamp(0, self.width as i32) as usize;
        let x1 = (px + 12).clamp(0, self.width as i32) as usize;
        for y in py.max(0)..(py + 16).min(self.height as i32) {
            if x1 > x0 {
                self.write_row(y as usize, x0, x1);
            }
        }
    }

    fn draw_pointer(&self) {
        let (px, py) = self.pointer;
        for (dy, line) in ARROW.iter().enumerate() {
            let y = py + dy as i32;
            if y < 0 || y >= self.height as i32 {
                continue;
            }
            for (dx, c) in line.chars().enumerate() {
                let x = px + dx as i32;
                let color = match c {
                    'X' => 0,
                    '.' => 0x00FF_FFFF,
                    _ => continue,
                };
                if x >= 0 && (x as usize) < self.width {
                    unsafe { *self.framebuffer.add(y as usize * self.stride + x as usize) = color };
                }
            }
        }
    }
}

/// How long frames take, reported now and then.
#[derive(Default)]
struct FrameStats {
    frames: u64,
    paint_us: u64,
    show_us: u64,
    rows: u64,
}

impl FrameStats {
    fn add(&mut self, paint_us: u64, show_us: u64, rows: usize) {
        self.frames += 1;
        self.paint_us += paint_us;
        self.show_us += show_us;
        self.rows += rows as u64;
        if self.frames % 20 == 1 {
            log!(
                "OTW kernel: frames: {} so far; on average {} us painting, {} us showing, {} rows changed",
                self.frames,
                self.paint_us / self.frames,
                self.show_us / self.frames,
                self.rows / self.frames
            );
        }
    }
}
