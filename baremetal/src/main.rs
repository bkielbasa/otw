//! OTW on bare metal: the kernel a UEFI machine boots. No other operating
//! system underneath: the kernel takes the framebuffer, the memory map and
//! the ACPI tables from the firmware, ends the firmware's boot services and
//! owns the machine from then on — its own segments, interrupts, page
//! tables and stack, every CPU, and the disks.
//!
//! Running programs (natively: their x86 code on the CPU itself, in user
//! mode, with the Win32 layers in the kernel) comes next.

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod ahci;
mod apic;
mod arch;
mod desktop;
mod disk;
mod heap;
mod io;
mod nvme;
mod pci;
mod screen;
mod smp;
mod sync;
mod uefi;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, Ordering};

#[global_allocator]
static HEAP: heap::Heap = heap::Heap::new();

/// What the kernel learnt from the firmware.
#[derive(Clone, Copy)]
pub struct BootInfo {
    pub framebuffer: *mut u32,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub bgr: bool,
    /// The ACPI root pointer (RSDP), or 0.
    pub rsdp: u64,
    /// A page below 1 MiB for starting other CPUs, or 0.
    pub trampoline: u64,
    /// The end of physical memory (RAM and framebuffer).
    pub memory_end: u64,
}

/// A region of the firmware's memory map.
#[derive(Clone, Copy)]
struct Region {
    kind: u32,
    start: u64,
    size: u64,
}

/// What the machine turned out to have, for the desktop.
pub struct System {
    pub info: BootInfo,
    pub cpus: usize,
    pub disks: Vec<String>,
    /// Drive C: (the GPT partition named OTW-C), if found.
    pub drive_c: Option<disk::fat32::Fat32FileSystem>,
    /// More to show (what reading a file from drive C: took).
    pub notes: Vec<String>,
}

static BOOT_INFO: AtomicPtr<BootInfo> = AtomicPtr::new(null_mut());

#[no_mangle]
pub extern "efiapi" fn efi_main(image: uefi::Handle, system: *mut uefi::SystemTable) -> uefi::Status {
    io::serial_init();
    log!("OTW kernel: starting");
    let (info, regions, count) = unsafe { take_machine(image, system) };
    let info = alloc::boxed::Box::leak(alloc::boxed::Box::new(info));
    BOOT_INFO.store(info, Ordering::SeqCst);
    // The firmware's stack is boot services memory, about to be reused:
    // move to one of the kernel's own.
    let regions: Vec<Region> = regions[..count].to_vec();
    let regions = alloc::boxed::Box::leak(alloc::boxed::Box::new(regions));
    let stack = alloc::boxed::Box::leak(alloc::vec![0u8; 1 << 20].into_boxed_slice());
    let top = (stack.as_ptr() as u64 + stack.len() as u64) & !15;
    unsafe {
        core::arch::asm!(
            "mov rsp, {top}",
            "call {main}",
            top = in(reg) top,
            main = sym kernel_main,
            in("rdi") regions as *const Vec<Region>,
            options(noreturn)
        )
    }
}

extern "sysv64" fn kernel_main(regions: &Vec<Region>) -> ! {
    let info = unsafe { *BOOT_INFO.load(Ordering::SeqCst) };
    arch::load_cpu_tables();
    arch::load_idt();
    arch::build_page_tables(info.memory_end.max(4 << 30));
    // The firmware's boot services memory is free now (the kernel has its
    // own stack, tables and page tables).
    let mut reclaimed = 0;
    for region in regions.iter().filter(|r| matches!(r.kind, 3 | 4) && r.start >= 0x10_0000) {
        unsafe { HEAP.add_region(region.start as usize, region.size as usize) };
        reclaimed += region.size;
    }
    log!(
        "OTW kernel: own tables loaded; {} MiB of memory ({} MiB from the firmware)",
        HEAP.total() >> 20,
        reclaimed >> 20
    );

    apic::init_boot_cpu();
    apic::start_timer(100);
    arch::enable_interrupts();
    log!(
        "OTW kernel: APIC ({}), clock {} MHz",
        if apic::x2apic() { "x2APIC" } else { "xAPIC" },
        apic::tsc_mhz()
    );

    let machine = acpi::parse(info.rsdp);
    if info.trampoline != 0 && machine.cpus.len() > 1 {
        smp::start_others(&machine.cpus, info.trampoline);
    }
    log!("OTW kernel: {} of {} CPUs running", apic::cpu_count(), machine.cpus.len().max(1));

    let (disks, drive_c) = find_disks();
    let mut notes = Vec::new();
    if let Some(fs) = &drive_c {
        // Read a big file whole, to see the disk work (and how fast).
        let path = "Program Files/Mozilla Firefox/xul.dll";
        let started = apic::now_us();
        match fs.read_file(path) {
            Ok(data) => {
                let us = (apic::now_us() - started).max(1);
                let sum = data.iter().fold(0u32, |a, &b| a.wrapping_mul(31).wrapping_add(b as u32));
                let note = alloc::format!(
                    "Read        xul.dll, {} MiB in {} ms ({} MiB/s)",
                    data.len() >> 20,
                    us / 1000,
                    data.len() as u64 / us
                );
                log!("OTW kernel: {note}, checksum {sum:#010x}");
                notes.push(note);
            }
            Err(error) => log!("OTW kernel: reading {path}: {error}"),
        }
    }
    let system = System { info, cpus: apic::cpu_count(), disks, drive_c, notes };
    desktop::run(&system, &HEAP)
}

/// The disks on NVMe and AHCI controllers, and drive C: among their
/// partitions.
fn find_disks() -> (Vec<String>, Option<disk::fat32::Fat32FileSystem>) {
    let mut disks: Vec<Arc<dyn disk::Disk>> = Vec::new();
    for device in pci::scan() {
        match (device.class, device.subclass, device.interface) {
            (0x01, 0x08, 0x02) => {
                if let Some(nvme) = nvme::probe(&device) {
                    disks.push(Arc::new(nvme));
                }
            }
            (0x01, 0x06, 0x01) => disks.extend(ahci::probe(&device)),
            _ => {}
        }
    }
    let mut names = Vec::new();
    let mut drive_c = None;
    for disk in &disks {
        let size_mib = disk.sectors() * disk.sector_size() as u64 >> 20;
        log!("OTW kernel: disk {} ({} MiB)", disk.name(), size_mib);
        names.push(alloc::format!("{} ({} MiB)", disk.name(), size_mib));
        for partition in disk::partitions(disk) {
            log!("OTW kernel:   partition {:?} at sector {}", partition.name, partition.first_lba);
            if drive_c.is_none() && partition.name == "OTW-C" {
                let volume = Arc::new(disk::PartitionVolume::new(&partition));
                match disk::fat32::Fat32FileSystem::from_volume(volume) {
                    Ok(fs) => drive_c = Some(fs),
                    Err(error) => log!("OTW kernel: drive C: cannot be read: {error}"),
                }
            }
        }
    }
    (names, drive_c)
}

/// Pick the screen mode, find the ACPI tables, read the memory map, end
/// boot services, and hand conventional memory to the heap.
unsafe fn take_machine(image: uefi::Handle, system: *mut uefi::SystemTable) -> (BootInfo, [Region; 512], usize) {
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
    let mode = &*(*gop).mode;
    let mode_info = &*mode.info;
    let rsdp = uefi::find_acpi(system);

    // The memory map, in a buffer on the stack; ExitBootServices needs the
    // key of the latest map, so read it again right before.
    let mut buffer = [0u8; 48 * 1024];
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
        tries += 1;
        if tries > 4 {
            halt();
        }
    }
    // The firmware is gone: no more boot services, and no interrupts until
    // the kernel has its own handlers.
    core::arch::asm!("cli");

    let mut regions = [Region { kind: 0, start: 0, size: 0 }; 512];
    let mut count = 0;
    let mut memory_end = mode.frame_buffer_base + mode.frame_buffer_size as u64;
    let mut trampoline = 0;
    let mut offset = 0;
    while offset + descriptor_size <= map_size && count < regions.len() {
        let d = &*(buffer.as_ptr().add(offset) as *const uefi::MemoryDescriptor);
        let size = d.number_of_pages * 4096;
        regions[count] = Region { kind: d.kind, start: d.physical_start, size };
        count += 1;
        // RAM (not device ranges, which are mapped when a driver asks).
        if (1..=10).contains(&d.kind) && d.kind != 8 {
            memory_end = memory_end.max(d.physical_start + size);
        }
        if d.kind == 7 && d.physical_start >= 0x10_0000 {
            HEAP.add_region(d.physical_start as usize, size as usize);
        }
        // A free page below 1 MiB (not page 0) to start other CPUs from.
        if trampoline == 0 && d.kind == 7 && d.physical_start < 0x9F000 {
            let page = d.physical_start.max(0x1000);
            if page + 4096 <= (d.physical_start + size).min(0x9F000) {
                trampoline = page;
            }
        }
        offset += descriptor_size;
    }

    let info = BootInfo {
        framebuffer: mode.frame_buffer_base as *mut u32,
        width: mode_info.horizontal_resolution as usize,
        height: mode_info.vertical_resolution as usize,
        stride: mode_info.pixels_per_scan_line as usize,
        bgr: mode_info.pixel_format == 1,
        rsdp,
        trampoline,
        memory_end,
    };
    (info, regions, count)
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

/// Stop the machine with `message` on the screen (and in the log).
pub fn fatal(message: &str) -> ! {
    arch::disable_interrupts();
    log!("OTW kernel: stopped: {message}");
    let info = BOOT_INFO.load(Ordering::SeqCst);
    if !info.is_null() {
        unsafe { screen::blue_screen(&*info, message) };
    }
    halt()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let mut message = String::new();
    let _ = core::fmt::write(&mut message, format_args!("{info}"));
    fatal(&message)
}
