//! Open-source Windows 95 compatibility VM — loader, IAT dispatcher, and x86 backend.
//!
//! Usage:
//!   cargo run -- [--hello | path/to/PE.EXE] [--run] [--list-imports] [--window]
//!   cargo run --features display -- --desktop [--fat32 DISK.IMG]
//!   cargo run -- --kernel [path/to/KERNEL.BIN]
//!   cargo run -- --boot [--vga] [path/to/BOOT.BIN] [--video-dump FILE] [--window]
//!   cargo run -- --fat32 DISK.IMG --list-files [PATH]
//!   cargo run --features display -- --fat32 DISK.IMG --explorer

#[cfg(feature = "display")]
use std::{cell::RefCell, rc::Rc};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[cfg(feature = "display")]
use otw_vm::shell::launch::scan_executables;
use otw_vm::{fat32::Fat32FileSystem, kernel::iat, kernel::pe::PeImage, machine, vm::Vm, win32};

#[cfg(not(feature = "embedded-target"))]
const DEFAULT_TARGET: &str = "binaries/win95/NOTEPAD.EXE";
#[cfg(not(feature = "embedded-target"))]
const HELLO_TARGET: &str = "binaries/guests/HELLO.EXE";
#[cfg(not(feature = "embedded-target"))]
const HELLO_KERNEL_TARGET: &str = "binaries/kernels/HELLO.BIN";
#[cfg(not(feature = "embedded-target"))]
const BOOT_TARGET: &str = "binaries/boot/OTWBOOT.BIN";
#[cfg(not(feature = "embedded-target"))]
const VGA_BOOT_TARGET: &str = "binaries/boot/OTWVGA.BIN";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let list_imports = args.iter().any(|arg| arg == "--list-imports");
    let run_guest = args.iter().any(|arg| arg == "--run");
    let use_hello = args.iter().any(|arg| arg == "--hello");
    let use_kernel = args.iter().any(|arg| arg == "--kernel");
    let use_boot = args.iter().any(|arg| arg == "--boot");
    let use_vga = args.iter().any(|arg| arg == "--vga");
    let mut use_window = args.iter().any(|arg| arg == "--window");
    let mut use_desktop = args.iter().any(|arg| arg == "--desktop");
    let mut use_explorer = false;
    let mut fat32_image = None;
    let mut list_files = None;

    let mut video_dump = None;
    let mut guest_arguments: Option<String> = None;
    let mut extra_modules = Vec::new();
    let mut positional = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--video-dump" => {
                if index + 1 >= args.len() {
                    eprintln!("--video-dump requires a file path");
                    return ExitCode::FAILURE;
                }
                video_dump = Some(PathBuf::from(&args[index + 1]));
                index += 2;
            }
            "--args" => {
                if index + 1 >= args.len() {
                    eprintln!("--args requires the program's arguments");
                    return ExitCode::FAILURE;
                }
                guest_arguments = Some(args[index + 1].clone());
                index += 2;
            }
            "--module" => {
                if index + 1 >= args.len() {
                    eprintln!("--module requires a DLL path");
                    return ExitCode::FAILURE;
                }
                extra_modules.push(PathBuf::from(&args[index + 1]));
                index += 2;
            }
            "--fat32" => {
                if index + 1 >= args.len() {
                    eprintln!("--fat32 requires a disk image path");
                    return ExitCode::FAILURE;
                }
                fat32_image = Some(PathBuf::from(&args[index + 1]));
                index += 2;
            }
            "--list-files" => {
                let path = args
                    .get(index + 1)
                    .filter(|arg| !arg.starts_with("--"))
                    .map(PathBuf::from);
                let consumed = if path.is_some() { 2 } else { 1 };
                list_files = Some(path.unwrap_or_else(|| PathBuf::from("/")));
                index += consumed;
            }
            "--desktop" => {
                use_desktop = true;
                use_window = true;
                index += 1;
            }
            "--explorer" => {
                use_explorer = true;
                use_window = true;
                index += 1;
            }
            arg if arg.starts_with("--") => index += 1,
            arg => {
                positional.push(PathBuf::from(arg));
                index += 1;
            }
        }
    }
    let explicit_path = positional.into_iter().next();

    if use_boot {
        return run_boot(explicit_path, video_dump, use_vga, use_window);
    }
    if use_kernel {
        return run_kernel(explicit_path);
    }

    let mounted = match fat32_image {
        Some(path) => match fs::read(&path) {
            Ok(bytes) => match Fat32FileSystem::new(bytes) {
                Ok(fs) => {
                    println!(
                        "fat32: {} label={:?} bps={} spc={} clusters={}",
                        path.display(),
                        fs.volume_label(),
                        fs.bytes_per_sector(),
                        fs.sectors_per_cluster(),
                        fs.cluster_count()
                    );
                    Some((path, fs))
                }
                Err(error) => {
                    eprintln!("failed to parse FAT32 image {}: {error}", path.display());
                    return ExitCode::FAILURE;
                }
            },
            Err(error) => {
                eprintln!("failed to read FAT32 image {}: {error}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    if let Some(path) = list_files {
        let Some((_, fs)) = &mounted else {
            eprintln!("--list-files requires --fat32 IMAGE");
            return ExitCode::FAILURE;
        };
        let path_text = path.to_string_lossy();
        match fs.list(&path_text) {
            Ok(entries) => {
                println!("fat32 listing: {path_text}");
                for entry in entries {
                    let kind = if entry.is_directory() {
                        "<DIR>"
                    } else {
                        "     "
                    };
                    let size = if entry.is_directory() { 0 } else { entry.size };
                    println!("{kind} {size:>10}  {}", entry.name);
                }
            }
            Err(error) => {
                eprintln!("failed to list {path_text}: {error}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }

    if use_explorer && mounted.is_none() {
        eprintln!("--explorer requires --fat32 IMAGE");
        return ExitCode::FAILURE;
    }

    let shell_desktop = explicit_path.is_none() && !use_hello && (use_desktop || use_explorer);
    if shell_desktop {
        #[cfg(feature = "display")]
        {
            let explorer_fs = mounted.as_ref().map(|(_, fs)| Rc::new(fs.clone()));
            let programs = mounted
                .as_ref()
                .map(|(_, fs)| scan_executables(fs))
                .unwrap_or_default();
            match &mounted {
                Some((path, _)) => println!(
                    "opening OTW desktop on {} (read-only) with {} program(s)",
                    path.display(),
                    programs.len()
                ),
                None => println!(
                    "opening OTW desktop on drive C: ({})",
                    otw_vm::win32::crt::drive_root().display()
                ),
            }
            let shared = Rc::new(RefCell::new(win32::SharedState::default()));
            let mut gui = match otw_vm::host::display::GuiRuntime::new_with_explorer(
                shared,
                explorer_fs,
                true,
                programs,
            ) {
                Ok(gui) => gui,
                Err(error) => {
                    eprintln!("host display failed: {error}");
                    return ExitCode::FAILURE;
                }
            };
            gui.pump_until_closed();
            return ExitCode::SUCCESS;
        }
        #[cfg(not(feature = "display"))]
        {
            let _ = mounted;
            eprintln!("--desktop requires rebuilding with --features display");
            return ExitCode::FAILURE;
        }
    }

    let (bytes, source) = match explicit_path {
        // RUNDLL32.EXE is built in (the drive holds a placeholder).
        Some(path) if path.file_name().is_some_and(|n| n.eq_ignore_ascii_case("rundll32.exe")) => {
            (otw_vm::vm::stub_program("OTW_RunDll32"), path.display().to_string())
        }
        Some(path) => match fs::read(&path) {
            Ok(bytes) => (bytes, path.display().to_string()),
            Err(error) => {
                eprintln!("failed to read {}: {error}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None if use_hello => match load_hello() {
            Ok(value) => value,
            Err(error) => {
                eprintln!("failed to load hello guest: {error}");
                return ExitCode::FAILURE;
            }
        },
        None => match load_default_target() {
            Ok(value) => value,
            Err(error) => {
                eprintln!("failed to load default target: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    let image = match PeImage::parse(bytes) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("failed to parse {source}: {error}");
            return ExitCode::FAILURE;
        }
    };

    println!("target: {source}");
    println!(
        "PE32 machine=0x{:04x} base={:#010x} entry={:#010x} sections={}",
        image.machine,
        image.image_base,
        image.entry_va(),
        image.sections.len()
    );
    println!(
        "imports: {} functions from {} DLLs",
        image.import_count(),
        image.imports.len()
    );
    for dll in &image.imports {
        println!("  {:<14} {}", dll.dll, dll.names.len());
    }

    if list_imports {
        println!("\nIAT map:");
        for dll in &image.imports {
            for (index, name) in dll.names.iter().enumerate() {
                let slot_rva = dll.first_thunk_rva + index as u32 * iat::THUNK_STRIDE;
                println!(
                    "  {:#010x}  {:<14} {}",
                    image.image_base + slot_rva,
                    dll.dll,
                    name.display()
                );
            }
        }
    }

    let mut inputs = vec![(
        Path::new(&source)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "target.exe".to_owned()),
        image,
    )];
    let mut module_paths = Vec::new();
    for (name, path, bytes) in otw_vm::vm::companion_modules(Path::new(&source), &inputs[0].1) {
        match PeImage::parse(bytes) {
            Ok(dll) => {
                println!("companion DLL: {name} ({})", path.display());
                if let Some(guest) = otw_vm::win32::crt::guest_path_of(&path) {
                    module_paths.push((name.clone(), guest));
                }
                inputs.push((name, dll));
            }
            Err(error) => eprintln!("ignoring {}: {error}", path.display()),
        }
    }
    for path in extra_modules {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "extra.dll".to_owned());
        if inputs
            .iter()
            .any(|(mapped, _)| mapped.eq_ignore_ascii_case(&name))
        {
            continue;
        }
        match fs::read(&path) {
            Ok(bytes) => match PeImage::parse(bytes) {
                Ok(dll) => {
                    println!("extra DLL: {name} ({})", path.display());
                    inputs.push((name, dll));
                }
                Err(error) => eprintln!("ignoring {}: {error}", path.display()),
            },
            Err(error) => eprintln!("ignoring {}: {error}", path.display()),
        }
    }

    // A program outside drive C: sees its own folder as drive D:, as if it
    // ran from a CD.
    if otw_vm::win32::crt::guest_path_of(Path::new(&source)).is_none() && std::env::var_os("OTW_DRIVE_D").is_none() {
        if let Some(folder) = Path::new(&source).canonicalize().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            std::env::set_var("OTW_DRIVE_D", folder);
        }
    }
    let guest_path = otw_vm::win32::crt::guest_path_of(std::path::Path::new(&source));
    let machine = match guest_arguments.as_deref() {
        // The command line starts with the program, as Windows passes it.
        Some(arguments) => {
            let program = guest_path
                .as_ref()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_else(|| inputs[0].0.clone());
            Vm::with_modules_shared(
                inputs,
                std::rc::Rc::new(std::cell::RefCell::new(otw_vm::win32::SharedState::default())),
                format!("\"{program}\" {arguments}"),
            )
        }
        None => Vm::with_modules(inputs),
    };
    let machine = match machine {
        Ok(machine) => machine,
        Err(error) => {
            eprintln!("failed to build VM: {error}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(path) = &guest_path {
        machine.set_program_path(path);
    }
    for (name, path) in &module_paths {
        machine.set_module_path(name, path);
    }
    let implemented = machine.implemented_imports();
    println!(
        "mapped {} bytes; {} IAT thunks installed; {} imports have Rust handlers",
        machine.memory_size(),
        machine.iat().len(),
        implemented
    );
    let missing: Vec<String> = machine
        .iat()
        .entries
        .iter()
        .filter(|entry| entry.slot_va != 0)
        .filter(|entry| !win32::has_implementation(&entry.dll, &entry.name))
        .map(|entry| format!("{}!{}", entry.dll, entry.name.display()))
        .collect();
    if !missing.is_empty() {
        eprintln!("missing implementations: {}", missing.join(", "));
    }

    if !run_guest {
        println!("loader ready; pass --run to execute this PE");
        return ExitCode::SUCCESS;
    }

    let result = if use_window {
        #[cfg(feature = "display")]
        {
            println!("opening host window for the emulated guest");
            let explorer_fs = if use_explorer {
                mounted.map(|(_, fs)| Rc::new(fs))
            } else {
                None
            };
            let gui = match otw_vm::host::display::GuiRuntime::new_with_explorer(
                machine.api_context().shared.clone(),
                explorer_fs,
                false,
                Vec::new(),
            ) {
                Ok(gui) => gui,
                Err(error) => {
                    eprintln!("host display failed: {error}");
                    return ExitCode::FAILURE;
                }
            };
            // Programs it starts show in the same display.
            let gui = Rc::new(RefCell::new(gui));
            let session = gui.borrow().session();
            let child_gui = gui.clone();
            let child_pump: Rc<dyn Fn(bool)> = Rc::new(move |block| {
                if let Ok(mut gui) = child_gui.try_borrow_mut() {
                    gui.pump(std::time::Duration::from_millis(if block { 15 } else { 0 }));
                }
            });
            let display: Rc<dyn otw_vm::vm::ProgramDisplay> = session;
            otw_vm::vm::NestedLauncher::new(Some(child_pump), Some(display))
                .install(&machine.api_context().shared);
            machine.run_with_pump(Box::new(move |block| {
                gui.borrow_mut()
                    .pump(std::time::Duration::from_millis(if block { 15 } else { 0 }))
            }))
        }
        #[cfg(not(feature = "display"))]
        {
            eprintln!("--window requires rebuilding with --features display");
            return ExitCode::FAILURE;
        }
    } else {
        otw_vm::vm::NestedLauncher::new(None, None).install(&machine.api_context().shared);
        machine.run()
    };

    match result {
        Ok(result) => {
            println!(
                "guest exited with status {:#010x} after {} API calls",
                result.exit_code, result.api_calls
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("guest execution failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_kernel(explicit_path: Option<PathBuf>) -> ExitCode {
    let (bytes, source) = match explicit_path {
        Some(path) => match fs::read(&path) {
            Ok(bytes) => (bytes, path.display().to_string()),
            Err(error) => {
                eprintln!("failed to read {}: {error}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None => match load_default_kernel() {
            Ok(value) => value,
            Err(error) => {
                eprintln!("failed to load kernel: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    println!("kernel: {source}");
    match machine::run(&bytes) {
        Ok(result) => {
            if !result.vga.is_empty() {
                println!("VGA text buffer:");
                println!("{}", result.vga);
            }
            if !result.serial.is_empty() {
                println!("serial: {}", result.serial);
            }
            println!("kernel exited with status {:#010x}", result.exit_code);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("kernel execution failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn write_ppm(path: &Path, frame: &machine::VideoFrame) -> Result<(), String> {
    let mut file = fs::File::create(path).map_err(|error| error.to_string())?;
    write!(file, "P6\n{} {}\n255\n", frame.width, frame.height)
        .map_err(|error| error.to_string())?;
    file.write_all(&frame.pixels)
        .map_err(|error| error.to_string())
}

fn run_boot(
    explicit_path: Option<PathBuf>,
    video_dump: Option<PathBuf>,
    use_vga: bool,
    use_window: bool,
) -> ExitCode {
    let (bytes, source) = match explicit_path {
        Some(path) => match fs::read(&path) {
            Ok(bytes) => (bytes, path.display().to_string()),
            Err(error) => {
                eprintln!("failed to read {}: {error}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None if use_vga => match load_default_vga_boot() {
            Ok(value) => value,
            Err(error) => {
                eprintln!("failed to load VGA boot sector: {error}");
                return ExitCode::FAILURE;
            }
        },
        None => match load_default_boot() {
            Ok(value) => value,
            Err(error) => {
                eprintln!("failed to load boot sector: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    println!("boot sector: {source}");
    match machine::run_boot_sector(&bytes) {
        Ok(mut result) => {
            if !result.vga.is_empty() {
                println!("VGA text buffer:");
                println!("{}", result.vga);
            }
            if !result.serial.is_empty() {
                println!("console: {}", result.serial.replace('\r', "\\r"));
            }
            if let Some(frame) = result.video.take() {
                println!("video: {}x{} {:?}", frame.width, frame.height, frame.mode);
                if let Some(path) = video_dump {
                    match write_ppm(&path, &frame) {
                        Ok(()) => println!("video dump: {}", path.display()),
                        Err(error) => {
                            eprintln!("failed to write video dump {}: {error}", path.display());
                            return ExitCode::FAILURE;
                        }
                    }
                } else {
                    println!("pass --video-dump FILE to save this frame");
                }
                if use_window {
                    #[cfg(feature = "display")]
                    {
                        println!("opening host window; close it or press Escape to exit");
                        if let Err(error) = otw_vm::host::display::show_frame(frame) {
                            eprintln!("host display failed: {error}");
                            return ExitCode::FAILURE;
                        }
                    }
                    #[cfg(not(feature = "display"))]
                    {
                        let _ = frame;
                        eprintln!("--window requires rebuilding with --features display");
                        return ExitCode::FAILURE;
                    }
                }
            } else if video_dump.is_some() || use_window {
                println!("no graphics frame to display; use --boot --vga");
            }
            println!("boot sector exited with status {:#010x}", result.exit_code);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("boot sector execution failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "embedded-target")]
fn load_default_vga_boot() -> Result<(Vec<u8>, String), String> {
    Ok((
        otw_vm::assets::VGA_BOOT_BIN.to_vec(),
        "embedded:binaries/boot/OTWVGA.BIN".to_string(),
    ))
}

#[cfg(not(feature = "embedded-target"))]
fn load_default_vga_boot() -> Result<(Vec<u8>, String), String> {
    fs::read(VGA_BOOT_TARGET)
        .map(|bytes| (bytes, VGA_BOOT_TARGET.to_string()))
        .map_err(|error| error.to_string())
}

#[cfg(feature = "embedded-target")]
fn load_default_boot() -> Result<(Vec<u8>, String), String> {
    Ok((
        otw_vm::assets::BOOT_BIN.to_vec(),
        "embedded:binaries/boot/OTWBOOT.BIN".to_string(),
    ))
}

#[cfg(not(feature = "embedded-target"))]
fn load_default_boot() -> Result<(Vec<u8>, String), String> {
    fs::read(BOOT_TARGET)
        .map(|bytes| (bytes, BOOT_TARGET.to_string()))
        .map_err(|error| error.to_string())
}

#[cfg(feature = "embedded-target")]
fn load_default_kernel() -> Result<(Vec<u8>, String), String> {
    Ok((
        otw_vm::assets::HELLO_KERNEL.to_vec(),
        "embedded:binaries/kernels/HELLO.BIN".to_string(),
    ))
}

#[cfg(not(feature = "embedded-target"))]
fn load_default_kernel() -> Result<(Vec<u8>, String), String> {
    fs::read(HELLO_KERNEL_TARGET)
        .map(|bytes| (bytes, HELLO_KERNEL_TARGET.to_string()))
        .map_err(|error| error.to_string())
}

#[cfg(feature = "embedded-target")]
fn load_default_target() -> Result<(Vec<u8>, String), String> {
    Ok((
        otw_vm::assets::NOTEPAD_EXE.to_vec(),
        "embedded:binaries/win95/NOTEPAD.EXE".to_string(),
    ))
}

#[cfg(not(feature = "embedded-target"))]
fn load_default_target() -> Result<(Vec<u8>, String), String> {
    fs::read(DEFAULT_TARGET)
        .map(|bytes| (bytes, DEFAULT_TARGET.to_string()))
        .map_err(|error| {
            format!(
                "{DEFAULT_TARGET}: {error}. Windows 95 programs are not distributed with OTW; \
                 copy your own NOTEPAD.EXE there (see the README), or pass a program path or --hello"
            )
        })
}

#[cfg(feature = "embedded-target")]
fn load_hello() -> Result<(Vec<u8>, String), String> {
    Ok((
        otw_vm::assets::HELLO_EXE.to_vec(),
        "embedded:binaries/guests/HELLO.EXE".to_string(),
    ))
}

#[cfg(not(feature = "embedded-target"))]
fn load_hello() -> Result<(Vec<u8>, String), String> {
    fs::read(HELLO_TARGET)
        .map(|bytes| (bytes, HELLO_TARGET.to_string()))
        .map_err(|error| error.to_string())
}
