//! The desktop on bare metal: the Windows 95 style desktop, taskbar, Start
//! menu and pointer, driven by the PS/2 keyboard and mouse.

use alloc::format;
use alloc::string::String;

use crate::heap::Heap;
use crate::io::{self, Input};
use crate::screen::{Screen, BLACK, FACE, NAVY, SHADOW, TEAL, WHITE};
use crate::System;

const TASKBAR: i32 = 28;

struct State {
    pointer: (i32, i32),
    buttons: u8,
    packet: [u8; 3],
    packet_len: usize,
    menu_open: bool,
    typed: String,
    shift: bool,
    ps2: bool,
}

pub fn run(system: &System, heap: &Heap) -> ! {
    let info = &system.info;
    let drive_c: alloc::vec::Vec<String> = system
        .drive_c
        .as_ref()
        .and_then(|fs| fs.list("").ok())
        .map(|entries| entries.into_iter().map(|e| if e.is_directory() { format!("{}\\", e.name) } else { e.name }).collect())
        .unwrap_or_default();
    crate::log!("OTW kernel: desktop");
    let mut screen = Screen::new(info.framebuffer, info.width, info.height, info.stride, info.bgr);
    let ps2 = io::ps2_init();
    crate::log!("OTW kernel: PS/2 controller {}", if ps2 { "found" } else { "missing" });
    let mut state = State {
        pointer: (info.width as i32 / 2, info.height as i32 / 2),
        buttons: 0,
        packet: [0; 3],
        packet_len: 0,
        menu_open: false,
        typed: String::new(),
        shift: false,
        ps2,
    };
    let mut last_clock = (99, 99, 99);
    let mut dirty = true;
    loop {
        while let Some(input) = io::ps2_poll() {
            dirty |= handle(&mut state, input, &screen);
        }
        let now = io::rtc_time();
        if now != last_clock {
            last_clock = now;
            dirty = true;
        }
        if dirty {
            draw(&mut screen, &state, system, &drive_c, heap, now);
            screen.present();
            dirty = false;
        }
        // Sleep until the next interrupt (the timer, 100 times a second).
        crate::arch::wait_for_interrupt();
    }
}

fn handle(state: &mut State, input: Input, screen: &Screen) -> bool {
    match input {
        Input::Mouse(byte) => {
            // Packets are 3 bytes; the first always has bit 3 set.
            if state.packet_len == 0 && byte & 0x08 == 0 {
                return false;
            }
            state.packet[state.packet_len] = byte;
            state.packet_len += 1;
            if state.packet_len < 3 {
                return false;
            }
            state.packet_len = 0;
            let [flags, dx, dy] = state.packet;
            let dx = dx as i32 - if flags & 0x10 != 0 { 256 } else { 0 };
            let dy = dy as i32 - if flags & 0x20 != 0 { 256 } else { 0 };
            state.pointer.0 = (state.pointer.0 + dx).clamp(0, screen.width as i32 - 1);
            state.pointer.1 = (state.pointer.1 - dy).clamp(0, screen.height as i32 - 1);
            let pressed = flags & 1 != 0 && state.buttons & 1 == 0;
            state.buttons = flags & 7;
            if pressed {
                click(state, screen);
            }
            true
        }
        Input::Key(code) => {
            match code {
                0x2A | 0x36 => state.shift = true,
                0xAA | 0xB6 => state.shift = false,
                0x0E => {
                    state.typed.pop();
                }
                0x01 => state.menu_open = false,
                _ if code & 0x80 == 0 => {
                    if let Some(c) = scan_to_char(code, state.shift) {
                        if state.typed.len() < 60 {
                            state.typed.push(c);
                        }
                    }
                }
                _ => return false,
            }
            true
        }
    }
}

fn click(state: &mut State, screen: &Screen) {
    let (x, y) = state.pointer;
    let taskbar_y = screen.height as i32 - TASKBAR;
    if (2..72).contains(&x) && y >= taskbar_y + 2 {
        state.menu_open = !state.menu_open;
        return;
    }
    if state.menu_open {
        let menu_top = taskbar_y - 60;
        if (2..160).contains(&x) && (menu_top..taskbar_y).contains(&y) {
            // "Restart": pulse the CPU reset line through the keyboard
            // controller.
            crate::log!("OTW kernel: restarting");
            unsafe { io::outb(0x64, 0xFE) };
        }
        state.menu_open = false;
    }
}

fn draw(screen: &mut Screen, state: &State, system: &System, drive_c: &[String], heap: &Heap, now: (u8, u8, u8)) {
    let info = &system.info;
    let (w, h) = (screen.width as i32, screen.height as i32);
    screen.fill(0, 0, w, h, TEAL);

    // A window: what this machine is.
    let disk_lines = system.disks.len().max(1);
    let extra = disk_lines + drive_c.len().min(8).div_ceil(2) + system.notes.len();
    let (ww, wh) = (720, 330 + 24 * extra as i32);
    let (wx, wy) = (w / 2 - ww / 2, (h / 2 - wh / 2).max(10));
    screen.fill(wx, wy, ww, wh, FACE);
    screen.bevel(wx, wy, ww, wh, false);
    screen.fill(wx + 3, wy + 3, ww - 6, 20, NAVY);
    screen.text(wx + 8, wy + 6, "Welcome to OTW", WHITE, 2);
    let mut lines = alloc::vec![
        String::from("OTW is running on this machine by itself:"),
        String::from("no other operating system underneath."),
        String::new(),
        format!("Screen      {} x {}", info.width, info.height),
        format!("Memory      {} MiB ({} MiB free)", heap.total() >> 20, heap.free_bytes() >> 20),
        format!("CPUs        {} running", system.cpus),
        format!("Up for      {} s", crate::apic::now_us() / 1_000_000),
        format!("Keyboard    {}", if state.ps2 { "PS/2" } else { "none found (USB to come)" }),
    ];
    if system.disks.is_empty() {
        lines.push(String::from("Disks       none found"));
    }
    for (i, disk) in system.disks.iter().enumerate() {
        lines.push(format!("{}{}", if i == 0 { "Disks       " } else { "            " }, disk));
    }
    match &system.drive_c {
        None => lines.push(String::from("Drive C:    not found")),
        Some(fs) => {
            lines.push(format!("Drive C:    {} ({} MiB), holding:", fs.volume_label(), fs.total_sectors() as u64 * fs.bytes_per_sector() as u64 >> 20));
            for row in drive_c.chunks(2).take(4) {
                let mut line = String::from("            ");
                for name in row {
                    let mut name: String = name.chars().take(20).collect();
                    while name.len() < 22 {
                        name.push(' ');
                    }
                    line.push_str(&name);
                }
                lines.push(line);
            }
        }
    }
    lines.extend(system.notes.iter().cloned());
    lines.push(String::new());
    lines.push(format!("Type here:  {}_", state.typed));
    for (i, line) in lines.iter().enumerate() {
        screen.text(wx + 16, wy + 36 + i as i32 * 24, line, BLACK, 2);
    }

    // The taskbar, the Start button and the clock.
    let ty = h - TASKBAR;
    screen.fill(0, ty, w, TASKBAR, FACE);
    screen.fill(0, ty, w, 1, FACE);
    screen.fill(0, ty + 1, w, 1, WHITE);
    screen.bevel(2, ty + 3, 70, TASKBAR - 5, state.menu_open);
    screen.text(10, ty + 8, "Start", BLACK, 2);
    let clock = format!("{:02}:{:02}", now.0, now.1);
    screen.bevel(w - 74, ty + 3, 72, TASKBAR - 5, true);
    screen.text(w - 68, ty + 8, &clock, BLACK, 2);

    if state.menu_open {
        let top = ty - 60;
        screen.fill(2, top, 158, 60, FACE);
        screen.bevel(2, top, 158, 60, false);
        screen.fill(5, top + 3, 20, 54, SHADOW);
        screen.text(32, top + 22, "Restart", BLACK, 2);
    }

    screen.pointer(state.pointer.0, state.pointer.1);
}

/// Set 1 scan codes for letters, digits and a few signs.
fn scan_to_char(code: u8, shift: bool) -> Option<char> {
    const ROW: [(u8, &str, &str); 4] = [
        (0x02, "1234567890-=", "!@#$%^&*()_+"),
        (0x10, "qwertyuiop[]", "QWERTYUIOP{}"),
        (0x1E, "asdfghjkl;'`", "ASDFGHJKL:\"~"),
        (0x2C, "zxcvbnm,./", "ZXCVBNM<>?"),
    ];
    if code == 0x39 {
        return Some(' ');
    }
    for (first, plain, shifted) in ROW {
        let index = code.wrapping_sub(first) as usize;
        let chars = if shift { shifted } else { plain };
        if let Some(c) = chars.chars().nth(index) {
            if index < chars.len() {
                return Some(c);
            }
        }
    }
    None
}
