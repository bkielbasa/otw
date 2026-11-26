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
mod net;
mod nvme;
mod paging;
mod pci;
mod process;
mod sched;
mod screen;
mod smp;
mod sync;
mod system;
mod uefi;
mod user;
mod virtio_net;

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
/// The time-stamp counter when the firmware started the kernel.
static BOOT_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Milliseconds since the firmware started the kernel.
pub fn ms_since_start() -> u64 {
    (arch::rdtsc() - BOOT_TSC.load(Ordering::SeqCst)) / apic::tsc_mhz().max(1) / 1000
}

/// Page tables are made in this much memory before there is a heap.
const BOOT_TABLES: u64 = 16 << 20;

#[no_mangle]
pub extern "efiapi" fn efi_main(image: uefi::Handle, system: *mut uefi::SystemTable) -> uefi::Status {
    BOOT_TSC.store(arch::rdtsc(), Ordering::SeqCst);
    io::serial_init();
    log!("OTW kernel: starting");
    let (info, mut regions, count) = unsafe { take_machine(image, system) };
    // Memory for the first page tables: the start of a big free region.
    let tables = regions[..count]
        .iter_mut()
        .find(|r| r.kind == 7 && r.start >= 0x10_0000 && r.size >= 2 * BOOT_TABLES && r.start + BOOT_TABLES < 1 << 32)
        .map(|r| {
            let start = r.start;
            r.start += BOOT_TABLES;
            r.size -= BOOT_TABLES;
            start
        })
        .unwrap_or_else(|| fatal("no memory for page tables"));
    paging::take_over(info.memory_end.max(4 << 30), tables, BOOT_TABLES);
    // Into the direct map: fix the kernel's absolute addresses, then move
    // the stack and continue there.
    unsafe { relocate(paging::DIRECT_BASE) };
    let rsp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp) };
    let stack = ((rsp + paging::DIRECT_BASE - 4096) & !15) - 8;
    let main = kernel_main as *const () as u64;
    let main = if main < paging::DIRECT_BASE { main + paging::DIRECT_BASE } else { main };
    let regions_high = regions.as_ptr() as u64 + paging::DIRECT_BASE;
    let info_high = &info as *const BootInfo as u64 + paging::DIRECT_BASE;
    unsafe {
        core::arch::asm!(
            "mov rsp, {stack}",
            "jmp {main}",
            stack = in(reg) stack,
            main = in(reg) main,
            in("rdi") regions_high,
            in("rsi") count,
            in("rdx") info_high,
            options(noreturn)
        )
    }
}

extern "C" {
    /// The start of this image in memory (from the linker).
    static __ImageBase: u8;
}

/// Add `delta` to every absolute address in the image (its base relocation
/// table), so that it runs `delta` higher.
unsafe fn relocate(delta: u64) {
    let base = core::ptr::addr_of!(__ImageBase) as u64;
    let u32_at = |a: u64| core::ptr::read_unaligned(a as *const u32);
    let pe = base + u32_at(base + 0x3C) as u64;
    let optional = pe + 24;
    // PE32+: the data directories start at 112; base relocations are 5th.
    let (rva, size) = (u32_at(optional + 112 + 40) as u64, u32_at(optional + 112 + 44) as u64);
    let mut block = base + rva;
    let end = block + size;
    let mut count = 0;
    while block + 8 <= end {
        let page = u32_at(block) as u64;
        let length = u32_at(block + 4) as u64;
        if length < 8 {
            break;
        }
        for i in 0..(length - 8) / 2 {
            let entry = core::ptr::read_unaligned((block + 8 + i * 2) as *const u16);
            if entry >> 12 == 10 {
                let at = (base + page + (entry & 0xFFF) as u64) as *mut u64;
                core::ptr::write_unaligned(at, core::ptr::read_unaligned(at).wrapping_add(delta));
                count += 1;
            }
        }
        block += length;
    }
    log!("OTW kernel: moved into the direct map ({count} addresses fixed)");
}

/// Now in the direct map: make the heap, and leave the firmware's stack.
extern "sysv64" fn kernel_main(regions: *const Region, count: usize, info: *const BootInfo) -> ! {
    paging::set_direct();
    let regions = unsafe { core::slice::from_raw_parts(regions, count) };
    for r in regions.iter().filter(|r| r.kind == 7 && r.start >= 0x10_0000) {
        unsafe { HEAP.add_region(paging::to_virtual(r.start) as usize, r.size as usize) };
    }
    paging::end_boot_tables();
    let mut info = unsafe { *info };
    info.framebuffer = paging::map_framebuffer(info.framebuffer as u64, (info.stride * info.height * 4) as u64) as *mut u32;
    let info = alloc::boxed::Box::leak(alloc::boxed::Box::new(info));
    BOOT_INFO.store(info, Ordering::SeqCst);
    let regions: Vec<Region> = regions.to_vec();
    let regions = alloc::boxed::Box::leak(alloc::boxed::Box::new(regions));
    let stack = alloc::boxed::Box::leak(alloc::vec![0u8; 1 << 20].into_boxed_slice());
    let top = (stack.as_ptr() as u64 + stack.len() as u64) & !15;
    unsafe {
        core::arch::asm!(
            "mov rsp, {top}",
            "call {main}",
            top = in(reg) top,
            main = sym kernel_main2,
            in("rdi") regions as *const Vec<Region>,
            options(noreturn)
        )
    }
}

extern "sysv64" fn kernel_main2(regions: &Vec<Region>) -> ! {
    let info = unsafe { *BOOT_INFO.load(Ordering::SeqCst) };
    let tss = arch::load_cpu_tables();
    arch::load_idt();
    arch::load_page_attributes();
    // The firmware's boot services memory is free now (the kernel has its
    // own stack, tables and page tables).
    let mut reclaimed = 0;
    for region in regions.iter().filter(|r| matches!(r.kind, 3 | 4) && r.start >= 0x10_0000) {
        unsafe { HEAP.add_region(paging::to_virtual(region.start) as usize, region.size as usize) };
        reclaimed += region.size;
    }
    log!(
        "OTW kernel: own tables loaded; {} MiB of memory ({} MiB from the firmware)",
        HEAP.total() >> 20,
        reclaimed >> 20
    );

    apic::init_boot_cpu();
    arch::register_tss(tss);
    sched::init_this_cpu(false);
    apic::start_timer(1000);
    arch::enable_interrupts();
    log!(
        "OTW kernel: APIC ({}), clock {} MHz",
        if apic::x2apic() { "x2APIC" } else { "xAPIC" },
        apic::tsc_mhz()
    );

    {
        let cr4: u64;
        unsafe { core::arch::asm!("mov {}, cr4", out(reg) cr4) };
        let xcr0 = if cr4 & (1 << 18) != 0 { unsafe { core::arch::x86_64::_xgetbv(0) } } else { 0 };
        log!("OTW kernel: CR4 {cr4:#x}, XCR0 {xcr0:#x}");
    }
    let machine = acpi::parse(info.rsdp);
    if info.trampoline != 0 && machine.cpus.len() > 1 {
        smp::start_others(&machine.cpus, info.trampoline);
    }
    log!("OTW kernel: {} of {} CPUs running ({} ms)", apic::cpu_count(), machine.cpus.len().max(1), ms_since_start());
    // The keyboard and mouse interrupt (rather than being asked 100 times
    // a second).
    let ps2 = io::ps2_init();
    if ps2 {
        io::ps2_route_interrupts(&machine);
    }

    let (disks, drive_c) = find_disks();
    log!("OTW kernel: disks found ({} ms)", ms_since_start());
    if !net::init(&pci::scan()) {
        log!("OTW kernel: no network card this kernel drives");
    }
    // With the disk-test feature: read a big file whole at boot, to see the
    // disk work (and how fast).
    let mut notes = Vec::new();
    if cfg!(feature = "disk-test") {
        if let Some(fs) = &drive_c {
            let started = apic::now_us();
            if let Ok(data) = fs.read_file("Program Files/Mozilla Firefox/xul.dll") {
                let us = (apic::now_us() - started).max(1);
                notes.push(alloc::format!("read xul.dll, {} MiB in {} ms", data.len() >> 20, us / 1000));
            }
        }
    }
    // OTW's library gets its clock, files and log from here on.
    let kernel: &'static system::Kernel = alloc::boxed::Box::leak(alloc::boxed::Box::new(system::Kernel::new(drive_c.clone())));
    otw_vm::platform::set_system(kernel);
    log!(
        "OTW kernel: {} MiB of memory free; {} CPUs; disks: {}",
        HEAP.free_bytes() >> 20,
        apic::cpu_count(),
        disks.join(", ")
    );
    let system = System { info, cpus: apic::cpu_count(), disks, drive_c, notes };
    desktop::run(&system)
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
