//! OTW on bare metal: the kernel a UEFI machine boots. No other operating
//! system underneath: the kernel takes the framebuffer and the memory map
//! from the firmware, ends the firmware's boot services and owns the
//! machine from then on.
//!
//! What it does so far: the heap, the serial log, the screen, PS/2
//! keyboard and mouse, the CMOS clock, and the desktop drawn on them.
//! Running programs (natively: their x86 code on the CPU itself, in user
//! mode, with the Win32 layers in the kernel) comes next.

#![no_std]
#![no_main]

extern crate alloc;

mod desktop;
mod heap;
mod io;
mod screen;
mod sync;
mod uefi;

use core::ffi::c_void;
use core::ptr::null_mut;

#[global_allocator]
static HEAP: heap::Heap = heap::Heap::new();

/// What the kernel learnt from the firmware.
pub struct BootInfo {
    pub framebuffer: *mut u32,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub bgr: bool,
    pub memory: usize,
}

#[no_mangle]
pub extern "efiapi" fn efi_main(image: uefi::Handle, system: *mut uefi::SystemTable) -> uefi::Status {
    io::serial_init();
    log!("OTW kernel: starting");
    let info = unsafe { take_machine(image, system) };
    log!(
        "OTW kernel: {}x{} screen, {} MiB of memory for the kernel",
        info.width,
        info.height,
        info.memory >> 20
    );
    desktop::run(&info, &HEAP)
}

/// Pick the screen mode, read the memory map, end boot services, and hand
/// the free memory to the heap.
unsafe fn take_machine(image: uefi::Handle, system: *mut uefi::SystemTable) -> BootInfo {
    let boot = (*system).boot_services;
    // No watchdog: the firmware would reset the machine after 5 minutes.
    ((*boot).set_watchdog_timer)(0, 0, 0, core::ptr::null());
    uefi::print(system, "OTW\n");

    let mut gop: *mut c_void = null_mut();
    let status = ((*boot).locate_protocol)(&uefi::GRAPHICS_OUTPUT_GUID, null_mut(), &mut gop);
    if status != uefi::SUCCESS || gop.is_null() {
        uefi::print(system, "OTW: no graphics output\n");
        halt();
    }
    let gop = gop as *mut uefi::GraphicsOutput;
    choose_mode(gop);
    log!("OTW kernel: screen mode chosen");
    let mode = &*(*gop).mode;
    let mode_info = &*mode.info;

    // The memory map, in a buffer on the stack; ExitBootServices needs the
    // key of the latest map, so read it again right before.
    let mut buffer = [0u8; 32 * 1024];
    let mut map_size;
    let mut key = 0usize;
    let mut descriptor_size = 0usize;
    let mut version = 0u32;
    let mut tries = 0;
    loop {
        map_size = buffer.len();
        let status = ((*boot).get_memory_map)(&mut map_size, buffer.as_mut_ptr(), &mut key, &mut descriptor_size, &mut version);
        if status != uefi::SUCCESS {
            log!("OTW kernel: cannot read the memory map ({status:#x}, {map_size} bytes)");
            uefi::print(system, "OTW: cannot read the memory map\n");
            halt();
        }
        if ((*boot).exit_boot_services)(image, key) == uefi::SUCCESS {
            break;
        }
        log!("OTW kernel: ExitBootServices refused, retrying");
        tries += 1;
        if tries > 4 {
            halt();
        }
    }
    // The firmware is gone: no more boot services, and no interrupts until
    // the kernel has its own handlers.
    core::arch::asm!("cli");
    log!("OTW kernel: boot services ended ({} memory map entries)", map_size / descriptor_size.max(1));

    let mut memory = 0;
    let mut offset = 0;
    while offset + descriptor_size <= map_size {
        let descriptor = &*(buffer.as_ptr().add(offset) as *const uefi::MemoryDescriptor);
        // Only memory nothing uses any more: this kernel is loader code and
        // data, its stack and this map are boot services data.
        if uefi::is_usable(descriptor.kind) && descriptor.physical_start >= 0x10_0000 {
            let size = descriptor.number_of_pages as usize * 4096;
            HEAP.add_region(descriptor.physical_start as usize, size);
            memory += size;
        }
        offset += descriptor_size;
    }

    BootInfo {
        framebuffer: mode.frame_buffer_base as *mut u32,
        width: mode_info.horizontal_resolution as usize,
        height: mode_info.vertical_resolution as usize,
        stride: mode_info.pixels_per_scan_line as usize,
        bgr: mode_info.pixel_format == 1,
        memory,
    }
}

/// The largest mode up to 1920x1080 with a linear framebuffer (the
/// firmware's own if none).
unsafe fn choose_mode(gop: *mut uefi::GraphicsOutput) {
    let mode = &*(*gop).mode;
    let mut best: Option<(u32, u32)> = None;
    for number in 0..mode.max_mode {
        let mut size = 0usize;
        let mut info: *const uefi::ModeInformation = core::ptr::null();
        if ((*gop).query_mode)(gop, number, &mut size, &mut info) != uefi::SUCCESS || info.is_null() {
            continue;
        }
        let info = &*info;
        if info.pixel_format > 1 || info.horizontal_resolution > 1920 || info.vertical_resolution > 1080 {
            continue;
        }
        let pixels = info.horizontal_resolution * info.vertical_resolution;
        if best.is_none_or(|(_, area)| pixels > area) {
            best = Some((number, pixels));
        }
    }
    if let Some((number, _)) = best {
        if number != mode.mode {
            ((*gop).set_mode)(gop, number);
        }
    }
}

pub fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("cli; hlt") };
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log!("OTW kernel panic: {info}");
    halt()
}
