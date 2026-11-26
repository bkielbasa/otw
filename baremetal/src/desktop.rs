//! The desktop: OTW's own shell (`otw_vm::shell`), the same code as on
//! Linux, drawn on the framebuffer and driven by the PS/2 keyboard and
//! mouse.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use otw_vm::shell::volume::Volume;
use otw_vm::shell::{Button, Key, Layer, Mods, Shell};
use otw_vm::win32::gfx::Surface;

use crate::io::{self, Input};
use crate::{apic, arch, log, System};

struct Input8042 {
    pointer: (i32, i32),
    buttons: u8,
    packet: [u8; 3],
    packet_len: usize,
    mods: Mods,
    extended: bool,
}

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

    let mut input = Input8042 {
        pointer: (width as i32 / 2, height as i32 / 2),
        buttons: 0,
        packet: [0; 3],
        packet_len: 0,
        mods: Mods::default(),
        extended: false,
    };
    let mut frame = Surface::new(width as i32, height as i32, 24);
    let mut screen = ScreenCopy::new(info);
    let mut dirty = true;
    let mut stats = FrameStats::default();
    loop {
        let pointer_before = input.pointer;
        while let Some(byte) = io::ps2_poll() {
            dirty |= handle(&mut shell, &mut input, byte, width as i32, height as i32);
        }
        if let Some(request) = shell.take_launch() {
            log!("OTW kernel: asked to run {}", request.path);
            let ended = crate::process::run(&request.path, &request.name);
            shell.program_exited(None);
            match ended {
                crate::process::Ended::Exited(code) => {
                    log!("OTW kernel: {} exited with {code}", request.name);
                    shell.show_message(&request.name, &format!("{} ran and exited with code {code}.", request.name));
                }
                crate::process::Ended::Failed(why) => {
                    log!("OTW kernel: {} failed: {why}", request.name);
                    shell.show_message("Program Error", &format!("{}: {why}", request.name));
                }
            }
            dirty = true;
        }
        dirty |= shell.clock_changed();
        if dirty {
            let started = apic::now_us();
            frame.pixels.fill(0);
            shell.paint(&mut frame, Layer::Background, &[]);
            shell.paint(&mut frame, Layer::Foreground, &[]);
            shell.paint(&mut frame, Layer::Top, &[]);
            let painted = apic::now_us();
            let rows = screen.show(&frame, input.pointer);
            if stats.frames == 0 {
                log!("OTW kernel: desktop on the screen ({} ms since start)", crate::ms_since_start());
            }
            stats.add(painted - started, apic::now_us() - painted, rows);
            dirty = false;
        } else if input.pointer != pointer_before {
            // Only the pointer moved: put back what was under it, draw it
            // again.
            screen.move_pointer(input.pointer);
        }
        // Sleep until the next interrupt (the timer, 100 times a second).
        arch::wait_for_interrupt();
    }
}

fn handle(shell: &mut Shell, input: &mut Input8042, byte: Input, width: i32, height: i32) -> bool {
    match byte {
        Input::Mouse(byte) => {
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
            let (x, y) = (
                (input.pointer.0 + dx).clamp(0, width - 1),
                (input.pointer.1 - dy).clamp(0, height - 1),
            );
            let moved = (x, y) != input.pointer;
            input.pointer = (x, y);
            // A move needs a frame only if the shell changed (dragging,
            // hovering); otherwise only the pointer is drawn again.
            let mut changed = moved && shell.pointer_move(x, y);
            for (bit, button) in [(1u8, Button::Left), (2u8, Button::Right)] {
                let now = flags & bit != 0;
                let before = input.buttons & bit != 0;
                if now && !before {
                    shell.pointer_down(x, y, button, false, &[]);
                    changed = true;
                } else if !now && before {
                    shell.pointer_up(x, y, button, false);
                    changed = true;
                }
            }
            input.buttons = flags & 7;
            changed
        }
        Input::Key(0xE0) => {
            input.extended = true;
            false
        }
        Input::Key(code) => {
            let extended = core::mem::take(&mut input.extended);
            let released = code & 0x80 != 0;
            match code & 0x7F {
                0x2A | 0x36 => input.mods.shift = !released,
                0x1D => input.mods.ctrl = !released,
                0x38 => input.mods.alt = !released,
                _ if released => {}
                scan => {
                    if let Some(key) = key_for(scan, extended, input.mods.shift) {
                        return shell.key(key, input.mods);
                    }
                }
            }
            false
        }
    }
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
