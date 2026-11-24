//! Minimal PC machine for freestanding x86 code.
//!
//! This is separate from the Win32 process-level VM in [`crate::kernel::cpu`]. It
//! provides two modes: a real-mode boot sector with BIOS video services, and
//! a protected-mode flat kernel with a flat GDT. Both share flat physical
//! memory, the VGA text buffer, COM1 output, and a debug-exit port.

use std::{cell::RefCell, fmt, rc::Rc};

use unicorn_engine::{
    uc_error,
    unicorn_const::{Arch, HookType, Mode, Prot, SECOND_SCALE},
    RegisterX86 as Reg, Unicorn,
};

pub const KERNEL_LOAD_VA: u32 = 0x0010_0000;
pub const KERNEL_STACK_TOP: u32 = 0x0020_0000;
pub const BOOT_LOAD_ADDR: u32 = 0x0000_7C00;

const PAGE_SIZE: u32 = 0x1000;
const LOW_MEM_BASE: u32 = 0x0000_0000;
const LOW_MEM_SIZE: u32 = 0x000A_0000;
const VGA_BASE: u32 = 0x000B_8000;
const VGA_SIZE: u32 = 0x1000;
const STACK_SIZE: u32 = 0x0001_0000;

const SERIAL_PORT: u32 = 0x3F8;
const EXIT_PORT: u32 = 0xF4;
const DAC_INDEX_PORT: u32 = 0x3C8;
const DAC_DATA_PORT: u32 = 0x3C9;

const BIOS_VIDEO_INT: u32 = 0x10;
const BIOS_SET_MODE: u8 = 0x00;
const BIOS_TELETYPE: u8 = 0x0E;
const VIDEO_MODE_TEXT: u8 = 0x03;
const VIDEO_MODE_320X200X256: u8 = 0x13;

const BOOT_SECTOR_SIZE: usize = 512;
const BOOT_SIGNATURE: [u8; 2] = [0x55, 0xAA];

const GDT_BASE: u32 = 0x500;
const GDT_LIMIT: u32 = 0x17;
const CODE_SELECTOR: u32 = 0x08;
const DATA_SELECTOR: u32 = 0x10;

const VGA_COLUMNS: usize = 80;
const VGA_ROWS: usize = 25;
const VGA_GRAPHICS_BASE: u32 = 0x000A_0000;
const VGA_GRAPHICS_SIZE: u32 = 0x0001_0000;
const GRAPHICS_WIDTH: usize = 320;
const GRAPHICS_HEIGHT: usize = 200;
const MAX_INSTRUCTIONS: usize = 10_000_000;
const TIMEOUT_SECONDS: u64 = 15;

/// Video modes the machine layer can render to a host image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoMode {
    /// 80x25 text mode; exposed as [`MachineRunResult::vga`], not as pixels.
    Text80x25,
    /// BIOS mode 13h: 320x200, 256 colors from the DAC palette.
    Graphics320x200x256,
}

/// RGB24 snapshot of the guest framebuffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    pub mode: VideoMode,
    pub width: usize,
    pub height: usize,
    /// `width * height * 3` bytes in RGB order.
    pub pixels: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineRunResult {
    pub exit_code: u32,
    pub serial: String,
    pub vga: String,
    /// Present when the guest selected a graphics mode.
    pub video: Option<VideoFrame>,
}

#[derive(Debug)]
pub enum MachineError {
    ImageTooLarge(usize),
    InvalidBootSector(String),
    Unicorn(&'static str, uc_error),
    Guest(String),
    NoExit,
}

impl fmt::Display for MachineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MachineError::ImageTooLarge(size) => {
                write!(f, "kernel image is {size} bytes; maximum is 1 MiB")
            }
            MachineError::InvalidBootSector(message) => {
                write!(f, "invalid boot sector: {message}")
            }
            MachineError::Unicorn(operation, error) => {
                write!(f, "x86 machine failed while {operation}: {error:?}")
            }
            MachineError::Guest(message) => write!(f, "machine execution error: {message}"),
            MachineError::NoExit => write!(f, "guest stopped without writing the exit port"),
        }
    }
}

impl std::error::Error for MachineError {}

struct RunState {
    exit_code: Option<u32>,
    serial: Vec<u8>,
    error: Option<String>,
    vga_column: usize,
    vga_row: usize,
    mode: VideoMode,
    palette: Box<[[u8; 3]; 256]>,
    dac_index: u8,
    dac_component: u8,
}

impl Default for RunState {
    fn default() -> Self {
        let mut palette = Box::new([[0u8; 3]; 256]);
        // Identity palette keeps pixel values readable in dumps even when the
        // guest does not program the DAC.
        for (index, color) in palette.iter_mut().enumerate() {
            *color = [index as u8; 3];
        }
        Self {
            exit_code: None,
            serial: Vec::new(),
            error: None,
            vga_column: 0,
            vga_row: 0,
            mode: VideoMode::Text80x25,
            palette,
            dac_index: 0,
            dac_component: 0,
        }
    }
}

fn align_up(value: u32, alignment: u32) -> u32 {
    (value.wrapping_add(alignment - 1)) & !(alignment - 1)
}

fn descriptor(access: u8) -> [u8; 8] {
    // Flat 4 GiB descriptor; base 0, granularity 4 KiB, 32-bit.
    [0xFF, 0xFF, 0x00, 0x00, 0x00, access, 0xCF, 0x00]
}

fn write_gdt(uc: &mut Unicorn<'_, ()>) -> Result<(), uc_error> {
    let mut gdt = Vec::new();
    gdt.extend_from_slice(&[0u8; 8]);
    gdt.extend_from_slice(&descriptor(0x9A)); // ring-0 code, readable
    gdt.extend_from_slice(&descriptor(0x92)); // ring-0 data, writable
    uc.mem_write(GDT_BASE as u64, &gdt)?;

    // uc_x86_mmr: u16 selector, padding, u64 base, u32 limit, u32 flags.
    let mut mmr = [0u8; 24];
    mmr[8..16].copy_from_slice(&(GDT_BASE as u64).to_le_bytes());
    mmr[16..20].copy_from_slice(&GDT_LIMIT.to_le_bytes());
    uc.reg_write_long(Reg::GDTR, &mmr)
}

fn render_vga(uc: &Unicorn<'_, ()>) -> Result<String, uc_error> {
    let mut cells = [0u8; VGA_COLUMNS * VGA_ROWS * 2];
    uc.mem_read(VGA_BASE as u64, &mut cells)?;

    let mut lines = Vec::new();
    for row in 0..VGA_ROWS {
        let mut line = String::with_capacity(VGA_COLUMNS);
        for column in 0..VGA_COLUMNS {
            let byte = cells[(row * VGA_COLUMNS + column) * 2];
            line.push(if byte == 0 { ' ' } else { byte as char });
        }
        if !line.trim().is_empty() || !lines.is_empty() {
            lines.push(line.trim_end().to_string());
        }
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    Ok(lines.join("\n"))
}

fn abort(state: &Rc<RefCell<RunState>>, uc: &mut Unicorn<'_, ()>, message: String) {
    state.borrow_mut().error = Some(message);
    let _ = uc.emu_stop();
}

fn scroll_vga(uc: &mut Unicorn<'_, ()>) -> Result<(), uc_error> {
    let mut cells = [0u8; VGA_COLUMNS * VGA_ROWS * 2];
    uc.mem_read(VGA_BASE as u64, &mut cells)?;

    let row_bytes = VGA_COLUMNS * 2;
    cells.copy_within(row_bytes.., 0);
    for column in 0..VGA_COLUMNS {
        let index = ((VGA_ROWS - 1) * VGA_COLUMNS + column) * 2;
        cells[index] = b' ';
        cells[index + 1] = 0x07;
    }
    uc.mem_write(VGA_BASE as u64, &cells)
}

/// Emulate BIOS INT 10h/AH=0x0E (teletype output).
fn teletype(
    uc: &mut Unicorn<'_, ()>,
    state: &Rc<RefCell<RunState>>,
    byte: u8,
) -> Result<(), uc_error> {
    let mut state = state.borrow_mut();

    match byte {
        b'\r' => {
            state.vga_column = 0;
            return Ok(());
        }
        b'\n' => {
            state.serial.push(b'\n');
            state.vga_row += 1;
            if state.vga_row >= VGA_ROWS {
                scroll_vga(uc)?;
                state.vga_row = VGA_ROWS - 1;
            }
            return Ok(());
        }
        _ => state.serial.push(byte),
    }

    if state.vga_column >= VGA_COLUMNS {
        state.vga_column = 0;
        state.vga_row += 1;
    }
    if state.vga_row >= VGA_ROWS {
        scroll_vga(uc)?;
        state.vga_row = VGA_ROWS - 1;
    }

    let offset = (state.vga_row * VGA_COLUMNS + state.vga_column) * 2;
    uc.mem_write(VGA_BASE as u64 + offset as u64, &[byte, 0x07])?;
    state.vga_column += 1;
    Ok(())
}

fn set_video_mode(
    uc: &mut Unicorn<'_, ()>,
    state: &Rc<RefCell<RunState>>,
    mode: u8,
) -> Result<(), uc_error> {
    let blank_text = vec![0u8; VGA_COLUMNS * VGA_ROWS * 2];
    uc.mem_write(VGA_BASE as u64, &blank_text)?;

    match mode {
        VIDEO_MODE_TEXT => {
            let mut state = state.borrow_mut();
            state.mode = VideoMode::Text80x25;
            state.vga_column = 0;
            state.vga_row = 0;
        }
        VIDEO_MODE_320X200X256 => {
            let blank_graphics = vec![0u8; GRAPHICS_WIDTH * GRAPHICS_HEIGHT];
            uc.mem_write(VGA_GRAPHICS_BASE as u64, &blank_graphics)?;
            state.borrow_mut().mode = VideoMode::Graphics320x200x256;
        }
        _ => return Ok(()),
    }
    Ok(())
}

fn render_video(uc: &Unicorn<'_, ()>, state: &RunState) -> Result<Option<VideoFrame>, uc_error> {
    match state.mode {
        VideoMode::Text80x25 => Ok(None),
        VideoMode::Graphics320x200x256 => {
            let mut indexed = vec![0u8; GRAPHICS_WIDTH * GRAPHICS_HEIGHT];
            uc.mem_read(VGA_GRAPHICS_BASE as u64, &mut indexed)?;

            let mut pixels = Vec::with_capacity(indexed.len() * 3);
            for index in indexed {
                pixels.extend_from_slice(&state.palette[index as usize]);
            }
            Ok(Some(VideoFrame {
                mode: state.mode,
                width: GRAPHICS_WIDTH,
                height: GRAPHICS_HEIGHT,
                pixels,
            }))
        }
    }
}

fn install_common_hooks(
    uc: &mut Unicorn<'_, ()>,
    state: &Rc<RefCell<RunState>>,
) -> Result<(), MachineError> {
    let io_state = state.clone();
    uc.add_insn_out_hook(move |uc, port, _size, value| {
        let byte = value as u8;
        match port {
            SERIAL_PORT => io_state.borrow_mut().serial.push(byte),
            EXIT_PORT => {
                {
                    let mut state = io_state.borrow_mut();
                    state.exit_code = Some(value);
                }
                let _ = uc.emu_stop();
            }
            DAC_INDEX_PORT => {
                let mut state = io_state.borrow_mut();
                state.dac_index = byte;
                state.dac_component = 0;
            }
            DAC_DATA_PORT => {
                let mut state = io_state.borrow_mut();
                let index = state.dac_index as usize;
                let component = state.dac_component as usize;
                if component < 3 {
                    state.palette[index][component] = byte;
                }
                let next = (component + 1) % 3;
                state.dac_component = next as u8;
                if next == 0 {
                    state.dac_index = state.dac_index.wrapping_add(1);
                }
            }
            _ => {}
        }
    })
    .map_err(|error| MachineError::Unicorn("installing I/O hook", error))?;

    let fault_state = state.clone();
    uc.add_mem_hook(
        HookType::MEM_READ_UNMAPPED
            | HookType::MEM_WRITE_UNMAPPED
            | HookType::MEM_FETCH_UNMAPPED
            | HookType::MEM_READ_PROT
            | HookType::MEM_WRITE_PROT
            | HookType::MEM_FETCH_PROT,
        0,
        u32::MAX as u64,
        move |uc, access, address, size, _value| {
            abort(
                &fault_state,
                uc,
                format!("invalid memory access {access:?} at {address:#010x}, size {size}"),
            );
            false
        },
    )
    .map_err(|error| MachineError::Unicorn("installing memory-fault hook", error))?;

    let invalid_state = state.clone();
    uc.add_insn_invalid_hook(move |uc| {
        abort(
            &invalid_state,
            uc,
            "invalid or privileged instruction".to_string(),
        );
        false
    })
    .map_err(|error| MachineError::Unicorn("installing invalid-instruction hook", error))?;

    Ok(())
}

fn finish_run(
    uc: &Unicorn<'_, ()>,
    state: &RunState,
    emulation: Result<(), uc_error>,
) -> Result<MachineRunResult, MachineError> {
    let vga = render_vga(uc).map_err(|error| MachineError::Unicorn("reading VGA memory", error))?;
    let video = render_video(uc, state)
        .map_err(|error| MachineError::Unicorn("reading video memory", error))?;
    if let Some(error) = &state.error {
        return Err(MachineError::Guest(error.clone()));
    }
    emulation.map_err(|error| MachineError::Unicorn("emulating guest code", error))?;

    let exit_code = state.exit_code.ok_or(MachineError::NoExit)?;
    Ok(MachineRunResult {
        exit_code,
        serial: String::from_utf8_lossy(&state.serial).into_owned(),
        vga,
        video,
    })
}

pub fn run(kernel: &[u8]) -> Result<MachineRunResult, MachineError> {
    if kernel.len() > 1024 * 1024 {
        return Err(MachineError::ImageTooLarge(kernel.len()));
    }

    let mut uc = Unicorn::new(Arch::X86, Mode::MODE_32)
        .map_err(|error| MachineError::Unicorn("creating the CPU", error))?;

    uc.mem_map(LOW_MEM_BASE as u64, LOW_MEM_SIZE as u64, Prot::ALL)
        .map_err(|error| MachineError::Unicorn("mapping low memory", error))?;
    uc.mem_map(VGA_BASE as u64, VGA_SIZE as u64, Prot::ALL)
        .map_err(|error| MachineError::Unicorn("mapping VGA text buffer", error))?;

    let kernel_size = align_up(kernel.len() as u32, PAGE_SIZE).max(PAGE_SIZE);
    uc.mem_map(KERNEL_LOAD_VA as u64, kernel_size as u64, Prot::ALL)
        .map_err(|error| MachineError::Unicorn("mapping kernel image", error))?;
    uc.mem_write(KERNEL_LOAD_VA as u64, kernel)
        .map_err(|error| MachineError::Unicorn("writing kernel image", error))?;

    uc.mem_map(
        (KERNEL_STACK_TOP - STACK_SIZE) as u64,
        STACK_SIZE as u64,
        Prot::ALL,
    )
    .map_err(|error| MachineError::Unicorn("mapping kernel stack", error))?;

    write_gdt(&mut uc).map_err(|error| MachineError::Unicorn("installing the GDT", error))?;
    uc.reg_write(Reg::CR0, 0x11)
        .map_err(|error| MachineError::Unicorn("enabling protected mode", error))?;
    uc.reg_write(Reg::CS, CODE_SELECTOR as u64)
        .map_err(|error| MachineError::Unicorn("setting CS", error))?;
    for register in [Reg::DS, Reg::ES, Reg::FS, Reg::GS, Reg::SS] {
        uc.reg_write(register, DATA_SELECTOR as u64)
            .map_err(|error| MachineError::Unicorn("setting data segments", error))?;
    }
    uc.reg_write(Reg::ESP, KERNEL_STACK_TOP as u64)
        .map_err(|error| MachineError::Unicorn("setting ESP", error))?;
    uc.reg_write(Reg::EBP, 0)
        .map_err(|error| MachineError::Unicorn("setting EBP", error))?;
    uc.reg_write(Reg::EFLAGS, 0x2)
        .map_err(|error| MachineError::Unicorn("setting EFLAGS", error))?;

    let state = Rc::new(RefCell::new(RunState::default()));
    install_common_hooks(&mut uc, &state)?;

    let emulation = uc.emu_start(
        KERNEL_LOAD_VA as u64,
        0,
        TIMEOUT_SECONDS * SECOND_SCALE,
        MAX_INSTRUCTIONS,
    );

    let state = state.borrow();
    finish_run(&uc, &*state, emulation)
}

/// Boot a 512-byte real-mode image at `0x7C00` with minimal BIOS services.
///
/// Implemented video services:
/// - `INT 10h/AH=0x00` mode set: text mode `03h` and 320x200x256 mode `13h`
/// - `INT 10h/AH=0x0E` teletype output in text mode
///
/// BIOS teletype bytes are mirrored into the serial capture so both
/// host-facing views show the same console text. Graphics-mode results are
/// rendered into [`MachineRunResult::video`].
pub fn run_boot_sector(boot_sector: &[u8]) -> Result<MachineRunResult, MachineError> {
    if boot_sector.len() != BOOT_SECTOR_SIZE {
        return Err(MachineError::InvalidBootSector(format!(
            "expected {BOOT_SECTOR_SIZE} bytes, got {}",
            boot_sector.len()
        )));
    }
    if boot_sector[BOOT_SECTOR_SIZE - 2..] != BOOT_SIGNATURE {
        return Err(MachineError::InvalidBootSector(
            "missing 0x55AA signature at offset 510".to_string(),
        ));
    }

    let mut uc = Unicorn::new(Arch::X86, Mode::MODE_16)
        .map_err(|error| MachineError::Unicorn("creating the real-mode CPU", error))?;

    uc.mem_map(LOW_MEM_BASE as u64, LOW_MEM_SIZE as u64, Prot::ALL)
        .map_err(|error| MachineError::Unicorn("mapping low memory", error))?;
    uc.mem_map(VGA_BASE as u64, VGA_SIZE as u64, Prot::ALL)
        .map_err(|error| MachineError::Unicorn("mapping VGA text buffer", error))?;
    uc.mem_map(
        VGA_GRAPHICS_BASE as u64,
        VGA_GRAPHICS_SIZE as u64,
        Prot::ALL,
    )
    .map_err(|error| MachineError::Unicorn("mapping VGA graphics framebuffer", error))?;
    uc.mem_write(BOOT_LOAD_ADDR as u64, boot_sector)
        .map_err(|error| MachineError::Unicorn("writing boot sector", error))?;

    for register in [Reg::CS, Reg::DS, Reg::ES, Reg::FS, Reg::GS, Reg::SS] {
        uc.reg_write(register, 0)
            .map_err(|error| MachineError::Unicorn("setting real-mode segments", error))?;
    }
    uc.reg_write(Reg::IP, BOOT_LOAD_ADDR as u64)
        .map_err(|error| MachineError::Unicorn("setting IP", error))?;
    uc.reg_write(Reg::SP, BOOT_LOAD_ADDR as u64)
        .map_err(|error| MachineError::Unicorn("setting SP", error))?;
    uc.reg_write(Reg::BP, 0)
        .map_err(|error| MachineError::Unicorn("setting BP", error))?;
    uc.reg_write(Reg::EFLAGS, 0x2)
        .map_err(|error| MachineError::Unicorn("setting EFLAGS", error))?;

    let state = Rc::new(RefCell::new(RunState::default()));
    install_common_hooks(&mut uc, &state)?;

    let bios_state = state.clone();
    uc.add_intr_hook(move |uc, intno| {
        if intno != BIOS_VIDEO_INT {
            abort(
                &bios_state,
                uc,
                format!("unhandled BIOS interrupt {intno:#04x}"),
            );
            return;
        }

        let ah = match uc.reg_read(Reg::AH) {
            Ok(value) => (value & 0xFF) as u8,
            Err(error) => {
                abort(
                    &bios_state,
                    uc,
                    format!("reading INT 10h service number: {error:?}"),
                );
                return;
            }
        };

        let al = match uc.reg_read(Reg::AL) {
            Ok(value) => (value & 0xFF) as u8,
            Err(error) => {
                abort(
                    &bios_state,
                    uc,
                    format!("reading INT 10h argument: {error:?}"),
                );
                return;
            }
        };

        match ah {
            BIOS_SET_MODE => {
                if !matches!(al, VIDEO_MODE_TEXT | VIDEO_MODE_320X200X256) {
                    abort(&bios_state, uc, format!("unsupported video mode {al:#04x}"));
                    return;
                }
                if let Err(error) = set_video_mode(uc, &bios_state, al) {
                    abort(
                        &bios_state,
                        uc,
                        format!("setting video mode {al:#04x}: {error:?}"),
                    );
                }
            }
            BIOS_TELETYPE => {
                if bios_state.borrow().mode != VideoMode::Text80x25 {
                    abort(
                        &bios_state,
                        uc,
                        "BIOS teletype requires text mode".to_string(),
                    );
                    return;
                }
                if let Err(error) = teletype(uc, &bios_state, al) {
                    abort(
                        &bios_state,
                        uc,
                        format!("writing BIOS teletype output: {error:?}"),
                    );
                }
            }
            _ => abort(
                &bios_state,
                uc,
                format!("unsupported INT 10h service {ah:#04x}"),
            ),
        }
    })
    .map_err(|error| MachineError::Unicorn("installing BIOS interrupt hook", error))?;

    let emulation = uc.emu_start(
        BOOT_LOAD_ADDR as u64,
        0,
        TIMEOUT_SECONDS * SECOND_SCALE,
        MAX_INSTRUCTIONS,
    );

    let state = state.borrow();
    finish_run(&uc, &*state, emulation)
}
