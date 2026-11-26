//! A runnable Win32 program: the kernel loads the executable and its
//! genuine DLLs into a process image, and the Win32 subsystem provides the
//! system calls behind its other imports.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::{
    kernel::{
        cpu::{self, RunError, RunResult},
        iat::Iat,
        image::{Image, ImageError},
        loader::MappedModule,
        pe::PeImage,
    },
    win32::{self, ApiContext, Launcher, LauncherSlot, SharedState, Win32Imports},
};

const DEFAULT_COMMAND_LINE: &str = "NOTEPAD.EXE";

pub type VmError = ImageError;

/// A Win32 program ready to run: a kernel process image whose system calls
/// go to the Win32 subsystem.
pub struct Vm {
    image: Image,
    api_ctx: ApiContext,
}

impl Vm {
    /// Map a single PE without companion DLLs.
    pub fn new(image: PeImage) -> Result<Self, VmError> {
        Self::with_modules(vec![("target.exe".to_owned(), image)])
    }

    /// Map an executable plus genuine companion DLLs.
    pub fn with_modules(inputs: Vec<(String, PeImage)>) -> Result<Self, VmError> {
        Self::with_modules_shared(
            inputs,
            Rc::new(RefCell::new(SharedState::default())),
            DEFAULT_COMMAND_LINE.to_owned(),
        )
    }

    /// Map an executable whose Win32 state is `shared` (owned by the
    /// caller, e.g. a desktop session).
    pub fn with_modules_shared(
        inputs: Vec<(String, PeImage)>,
        shared: Rc<RefCell<SharedState>>,
        command_line: String,
    ) -> Result<Self, VmError> {
        win32::crt::prepare_drive();
        crate::kernel::memory::set_idle_wait(|timeout| win32::kobj::wait_change(win32::kobj::generation(), timeout));
        let image = Image::load(inputs, &command_line, &Win32Imports, &win32::entry_points())?;
        let api_ctx = ApiContext::for_image(&image, shared);
        Ok(Self { image, api_ctx })
    }

    /// Where the program lives on drive C: (`C:\\DIR\\APP.EXE`): what
    /// GetModuleFileName reports, and the starting current directory.
    pub fn set_program_path(&self, path: &[u8]) {
        let mut shared = self.api_ctx.shared.borrow_mut();
        shared.program_path = path.to_vec();
        if let Some(end) = path.iter().rposition(|&b| b == b'\\') {
            let dir = &path[..end.max(3).min(path.len())];
            shared.current_dir = if dir.len() < 3 { b"C:\\".to_vec() } else { dir.to_vec() };
        }
    }

    /// Where a DLL mapped at start lives on drive C:, for
    /// GetModuleFileName.
    pub fn set_module_path(&self, name: &str, path: &[u8]) {
        for module in self.api_ctx.modules.borrow_mut().iter_mut() {
            if module.name.eq_ignore_ascii_case(name) {
                module.path = path.to_vec();
            }
        }
    }

    /// The executable.
    pub fn image(&self) -> &PeImage {
        self.image.main()
    }

    /// The kernel's process image.
    pub fn process_image(&self) -> &Image {
        &self.image
    }

    pub fn iat(&self) -> &Iat {
        &self.image.iat
    }

    pub fn api_context(&self) -> &ApiContext {
        &self.api_ctx
    }

    pub fn modules(&self) -> &[MappedModule] {
        &self.image.modules
    }

    /// Imports bound to Win32 functions that are implemented.
    pub fn implemented_imports(&self) -> usize {
        self.image
            .iat
            .entries
            .iter()
            .filter(|entry| {
                entry.slot_va != 0 && win32::has_implementation(&entry.dll, &entry.name)
            })
            .count()
    }

    pub fn memory_size(&self) -> usize {
        self.image.modules[0].memory.len()
    }

    pub fn memory(&self) -> &[u8] {
        &self.image.modules[0].memory
    }

    pub fn read_u32(&self, va: u32) -> Option<u32> {
        self.image.read_u32(va)
    }

    pub fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
        self.image.write_u32(va, value)
    }

    /// Execute the program until it calls `ExitProcess` or the CPU reports
    /// an error.
    pub fn run(&self) -> Result<RunResult, RunError> {
        let result = cpu::run(&self.image, Rc::new(self.api_ctx.clone()), None);
        win32::registry::flush();
        result
    }

    /// Execute with a host callback that pumps window-system events into the
    /// guest message queue before blocking message APIs.
    pub fn run_with_pump<'a>(
        &self,
        pump: Box<dyn FnMut(bool) + 'a>,
    ) -> Result<RunResult, RunError> {
        cpu::run(&self.image, Rc::new(self.api_ctx.clone()), Some(pump))
    }

    /// Load the program as a process that shares the CPU with others: it
    /// yields whenever it waits for messages. Drive it with
    /// [`cpu::Process::resume`].
    pub fn start<'a>(&self, pump: Box<dyn FnMut(bool) + 'a>) -> Result<cpu::Process<'a>, RunError> {
        cpu::Process::new(&self.image, Rc::new(self.api_ctx.clone()), Some(pump), true)
    }
}

/// The genuine DLLs a program imports, and the DLLs those import: next to
/// the program, then in drive C:'s `WINDOWS\SYSTEM` and `WINDOWS` (where
/// the subsystem's own DLLs are never taken from). They are mapped as real
/// modules instead of being replaced by Rust stubs.
pub fn companion_modules(program: &Path, image: &PeImage) -> Vec<(String, PathBuf, Vec<u8>)> {
    let Some(directory) = program.parent() else {
        return Vec::new();
    };
    let drive = win32::crt::drive_root();
    let system_dirs = [drive.join("WINDOWS").join("SYSTEM"), drive.join("WINDOWS")];
    let find_in = |dir: &Path, wanted: &str| -> Option<PathBuf> {
        fs::read_dir(dir).ok()?.flatten().map(|entry| entry.path()).find(|path| {
            path.is_file()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(wanted))
        })
    };
    let mut modules: Vec<(String, PathBuf, Vec<u8>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut pending: VecDeque<String> = image.imports.iter().map(|dll| dll.dll.clone()).collect();
    while let Some(dll) = pending.pop_front() {
        if !seen.insert(dll.to_ascii_lowercase()) || win32::is_rust_backed_dll(&dll) {
            continue;
        }
        let found = find_in(directory, &dll).or_else(|| {
            if win32::module::subsystem_only(&dll) {
                return None;
            }
            system_dirs.iter().find_map(|dir| find_in(dir, &dll))
        });
        let Some(path) = found else {
            continue;
        };
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        match PeImage::parse(bytes.clone()) {
            // A 64-bit program cannot use 32-bit DLLs, nor the reverse.
            Ok(parsed) if parsed.is64 != image.is64 => continue,
            Ok(parsed) => pending.extend(parsed.imports.iter().map(|import| import.dll.clone())),
            Err(_) => {}
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or(dll);
        modules.push((name, path, bytes));
    }
    modules.sort_by(|left, right| left.0.cmp(&right.0));
    modules
}

/// A minimal Windows program whose entry point jumps to the subsystem
/// function `KERNEL32!function` (which never returns): Windows' own small
/// programs, such as RUNDLL32.EXE, that OTW carries out itself.
pub fn stub_program(function: &str) -> Vec<u8> {
    let mut file = vec![0u8; 0x400];
    let put = |file: &mut Vec<u8>, at: usize, bytes: &[u8]| file[at..at + bytes.len()].copy_from_slice(bytes);
    let put32 = |file: &mut Vec<u8>, at: usize, value: u32| file[at..at + 4].copy_from_slice(&value.to_le_bytes());
    let put16 = |file: &mut Vec<u8>, at: usize, value: u16| file[at..at + 2].copy_from_slice(&value.to_le_bytes());
    const BASE: u32 = 0x0040_0000;
    put(&mut file, 0, b"MZ");
    put32(&mut file, 0x3C, 0x40);
    put(&mut file, 0x40, b"PE\0\0");
    // COFF: i386, one section, optional header size, executable 32-bit.
    put16(&mut file, 0x44, 0x14C);
    put16(&mut file, 0x46, 1);
    put16(&mut file, 0x54, 0xE0);
    put16(&mut file, 0x56, 0x0102);
    // Optional header.
    let o = 0x58;
    put16(&mut file, o, 0x10B);
    put32(&mut file, o + 16, 0x1000); // entry point
    put32(&mut file, o + 20, 0x1000);
    put32(&mut file, o + 28, BASE);
    put32(&mut file, o + 32, 0x1000); // section alignment
    put32(&mut file, o + 36, 0x200); // file alignment
    put16(&mut file, o + 40, 4);
    put16(&mut file, o + 48, 4);
    put32(&mut file, o + 56, 0x2000); // size of image
    put32(&mut file, o + 60, 0x200); // size of headers
    put16(&mut file, o + 68, 2); // GUI
    put32(&mut file, o + 72, 0x10_0000);
    put32(&mut file, o + 76, 0x1000);
    put32(&mut file, o + 80, 0x10_0000);
    put32(&mut file, o + 84, 0x1000);
    put32(&mut file, o + 92, 16);
    put32(&mut file, o + 96 + 8, 0x1010); // import directory
    put32(&mut file, o + 96 + 12, 40);
    // The one section.
    let section = o + 0xE0;
    put(&mut file, section, b".text\0\0\0");
    put32(&mut file, section + 8, 0x1000);
    put32(&mut file, section + 12, 0x1000);
    put32(&mut file, section + 16, 0x200);
    put32(&mut file, section + 20, 0x200);
    put32(&mut file, section + 36, 0xE000_0060);
    // Code: jmp [IAT]; imports of KERNEL32!function.
    let text = 0x200;
    put(&mut file, text, &[0xFF, 0x25]);
    put32(&mut file, text + 2, BASE + 0x1050);
    put32(&mut file, text + 0x10, 0x1040); // original first thunk
    put32(&mut file, text + 0x10 + 12, 0x1080); // DLL name
    put32(&mut file, text + 0x10 + 16, 0x1050); // first thunk
    put32(&mut file, text + 0x40, 0x1060);
    put32(&mut file, text + 0x50, 0x1060);
    put(&mut file, text + 0x62, function.as_bytes());
    put(&mut file, text + 0x80, b"KERNEL32.dll");
    file
}

impl Vm {
    /// Load a program from drive C: (`path` like `C:\\DIR\\APP.EXE`) with its
    /// genuine DLLs.
    pub fn from_drive(path: &[u8], command_line: &[u8], shared: Rc<RefCell<SharedState>>) -> Result<Self, String> {
        let host = win32::crt::host_path(path).ok_or("not on drive C:")?;
        let file = String::from_utf8_lossy(path.rsplit(|&b| b == b'\\').next().unwrap_or(path)).to_ascii_uppercase();
        let bytes = if file == "RUNDLL32.EXE" {
            stub_program("OTW_RunDll32")
        } else {
            fs::read(&host).map_err(|error| error.to_string())?
        };
        let image = PeImage::parse(bytes).map_err(|error| error.to_string())?;
        let name = host
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "PROGRAM.EXE".into());
        let mut inputs = vec![(name, image)];
        let mut paths = Vec::new();
        for (dll, dll_host, bytes) in companion_modules(&host, &inputs[0].1) {
            if let Ok(image) = PeImage::parse(bytes) {
                if let Some(guest) = win32::crt::guest_path_of(&dll_host) {
                    paths.push((dll.clone(), guest));
                }
                inputs.push((dll, image));
            }
        }
        let vm = Self::with_modules_shared(inputs, shared, String::from_utf8_lossy(command_line).into_owned())
            .map_err(|error| error.to_string())?;
        vm.set_program_path(path);
        for (dll, guest) in paths {
            vm.set_module_path(&dll, &guest);
        }
        if std::env::var_os("OTW_TRACE_MODULES").is_some() {
            for entry in &vm.image.iat.entries {
                if entry.slot_va != 0 && !win32::has_implementation(&entry.dll, &entry.name) {
                    eprintln!("[module] {}: missing {}!{}", String::from_utf8_lossy(path), entry.dll, entry.name.display());
                }
            }
        }
        Ok(vm)
    }
}

/// Where programs started by others show their windows (a desktop
/// session).
pub trait ProgramDisplay {
    fn attach(&self, pid: u32, name: &str, shared: Rc<RefCell<SharedState>>);
    fn detach(&self, pid: u32);
}

/// Starts the programs a program creates by running each to the end inside
/// the `CreateProcess` call, as a modal child (installers wait for theirs
/// anyway). The child takes over what is left of the scripted input, and
/// shows its windows in the display's session when there is one.
pub struct NestedLauncher {
    /// Pumps host events while the child waits (the parent's pump).
    pump: Option<Rc<dyn Fn(bool)>>,
    /// Where the child's windows are shown.
    display: Option<Rc<dyn ProgramDisplay>>,
    exits: RefCell<HashMap<u32, u32>>,
    me: RefCell<std::rc::Weak<NestedLauncher>>,
}

impl NestedLauncher {
    pub fn new(pump: Option<Rc<dyn Fn(bool)>>, display: Option<Rc<dyn ProgramDisplay>>) -> Rc<Self> {
        let launcher = Rc::new(Self {
            pump,
            display,
            exits: RefCell::new(HashMap::new()),
            me: RefCell::new(std::rc::Weak::new()),
        });
        *launcher.me.borrow_mut() = Rc::downgrade(&launcher);
        launcher
    }

    /// Install into a program's Win32 state.
    pub fn install(self: &Rc<Self>, shared: &Rc<RefCell<SharedState>>) {
        shared.borrow_mut().launcher = LauncherSlot(Some(self.clone()));
    }
}

impl Launcher for NestedLauncher {
    fn start(&self, parent: &ApiContext, path: &[u8], command_line: &[u8], directory: &[u8]) -> Result<u32, String> {
        let shared = Rc::new(RefCell::new(SharedState::default()));
        {
            let mut parent_state = parent.shared.borrow_mut();
            let mut child = shared.borrow_mut();
            child.gui.screen = parent_state.gui.screen;
            child.gui.script = parent_state.gui.script.take();
            child.gui.script_wait = std::mem::take(&mut parent_state.gui.script_wait);
            child.gui.script_until = parent_state.gui.script_until.take();
        }
        if let Some(me) = self.me.borrow().upgrade() {
            me.install(&shared);
        }
        if let Some(environment) = parent.shared.borrow_mut().pending_environment.take() {
            win32::env::set_all(&mut shared.borrow_mut().env, &environment);
        }
        let vm = match Vm::from_drive(path, command_line, shared.clone()) {
            Ok(vm) => vm,
            Err(error) => {
                parent.shared.borrow_mut().gui.script = shared.borrow_mut().gui.script.take();
                return Err(error);
            }
        };
        if !directory.is_empty() {
            shared.borrow_mut().current_dir = directory.to_vec();
        }
        let parent_pid = parent.shared.borrow().pid;
        let pid = win32::kobj::with(|k| k.new_pid(parent_pid, false));
        shared.borrow_mut().pid = pid;
        let name = String::from_utf8_lossy(path.rsplit(|&b| b == b'\\').next().unwrap_or(path)).into_owned();
        if let Some(session) = &self.display {
            session.attach(pid, &name, shared.clone());
        }
        let result = match &self.pump {
            Some(pump) => {
                let pump = pump.clone();
                vm.run_with_pump(Box::new(move |block| pump(block)))
            }
            None => vm.run(),
        };
        if let Some(session) = &self.display {
            session.detach(pid);
        }
        win32::registry::flush();
        // The rest of the script goes back to the parent.
        parent.shared.borrow_mut().gui.script = shared.borrow_mut().gui.script.take();
        parent.shared.borrow_mut().gui.script_until = shared.borrow_mut().gui.script_until.take();
        let code = match result {
            Ok(result) => result.exit_code,
            Err(error) => {
                eprintln!("[process] {name} failed: {error}");
                0xC000_0005
            }
        };
        if std::env::var_os("OTW_TRACE_THREADS").is_some() {
            eprintln!("[process] {name} (process {pid}) exited with {code:#x}");
        }
        self.exits.borrow_mut().insert(pid, code);
        win32::kobj::with(|k| {
            if let Some(process) = k.processes.get_mut(&pid) {
                process.exit.get_or_insert(code);
            }
        });
        Ok(pid)
    }

    fn exit_code(&self, pid: u32) -> Option<u32> {
        self.exits.borrow().get(&pid).copied().or_else(|| win32::kobj::exit_code(pid))
    }

    fn start_with(&self, parent: &ApiContext, path: &[u8], command_line: &[u8], directory: &[u8], options: win32::LaunchOptions) -> Result<u32, String> {
        // A 64-bit program a 64-bit program starts with handles to share
        // runs alongside it (the content processes of a browser talk to
        // theirs through pipes while both run); others run to the end
        // inside the call, in the parent's place on the screen (a program
        // that restarts itself, a helper it waits for).
        let child64 = win32::crt::host_path(path)
            .and_then(|host| fs::read(host).ok())
            .and_then(|bytes| PeImage::parse(bytes).ok())
            .is_some_and(|image| image.is64);
        let parent_pid = parent.shared.borrow().pid;
        let shares = options.inherit
            && match &options.handles {
                Some(handles) => handles.iter().any(|&h| win32::kobj::with(|k| k.owns(parent_pid, h))),
                None => win32::kobj::with(|k| !k.inheritable(parent_pid).is_empty()),
            };
        if !(child64 && parent.shared.borrow().x64 && shares) {
            if std::env::var_os("OTW_TRACE_THREADS").is_some() {
                for entry in options.environment.iter().flatten().filter(|e| e.starts_with("MOZ_") || e.starts_with("XRE_")) {
                    eprintln!("[process]   {entry}");
                }
            }
            if let Some(environment) = options.environment {
                // The child starts from the environment it was given.
                parent.shared.borrow_mut().pending_environment = Some(environment);
            }
            return self.start(parent, path, command_line, directory);
        }
        start_alongside(parent, path, command_line, directory, options)
    }
}

/// Start a program on a host thread of its own, to run alongside the
/// others: it shares only the kernel objects in [`win32::kobj`] with them
/// (and the drive). Returns its process ID.
fn start_alongside(parent: &ApiContext, path: &[u8], command_line: &[u8], directory: &[u8], options: win32::LaunchOptions) -> Result<u32, String> {
    let parent_pid = parent.shared.borrow().pid;
    let pid = win32::kobj::with(|k| {
        let pid = k.new_pid(parent_pid, options.suspended);
        if options.inherit {
            let handles = options.handles.clone().unwrap_or_else(|| k.inheritable(parent_pid));
            for handle in handles {
                // Inherited handles keep their numbers.
                if k.owns(parent_pid, handle) {
                    k.duplicate(parent_pid, handle, pid, Some(handle));
                }
            }
        }
        pid
    });
    let screen = parent.shared.borrow().gui.screen;
    let environment = options.environment.clone();
    let (path, command_line, directory) = (path.to_vec(), command_line.to_vec(), directory.to_vec());
    let name = String::from_utf8_lossy(path.rsplit(|&b| b == b'\\').next().unwrap_or(&path)).into_owned();
    if std::env::var_os("OTW_TRACE_THREADS").is_some() {
        eprintln!("[process] {name} starts as process {pid:#x} alongside {parent_pid:#x}: {}", String::from_utf8_lossy(&command_line));
        for entry in options.environment.iter().flatten().filter(|e| e.starts_with("MOZ_") || e.starts_with("XRE_")) {
            eprintln!("[process]   {entry}");
        }
    }
    std::thread::Builder::new()
        .name(format!("{name} {pid:#x}"))
        .stack_size(256 << 20)
        .spawn(move || {
            win32::registry::set_read_only();
            let shared = Rc::new(RefCell::new(SharedState::default()));
            {
                let mut state = shared.borrow_mut();
                state.pid = pid;
                state.alongside = true;
                state.gui.screen = screen;
                state.gui.script = Some(Default::default());
                if let Some(block) = &environment {
                    win32::env::set_all(&mut state.env, block);
                }
            }
            NestedLauncher::new(None, None).install(&shared);
            let code = match Vm::from_drive(&path, &command_line, shared.clone()) {
                Ok(vm) => {
                    if !directory.is_empty() {
                        shared.borrow_mut().current_dir = directory.clone();
                    }
                    loop {
                        let generation = win32::kobj::generation();
                        if !win32::kobj::with(|k| k.processes.get(&pid).is_some_and(|p| p.suspended && p.exit.is_none())) {
                            break;
                        }
                        win32::kobj::wait_change(generation, std::time::Duration::from_millis(50));
                    }
                    match vm.run() {
                        Ok(result) => result.exit_code,
                        Err(error) => {
                            eprintln!("[process] {name} (process {pid:#x}) failed: {error}");
                            0xC000_0005
                        }
                    }
                }
                Err(error) => {
                    eprintln!("[process] {name} (process {pid:#x}) did not load: {error}");
                    0xC000_0135
                }
            };
            if std::env::var_os("OTW_TRACE_THREADS").is_some() {
                eprintln!("[process] {name} (process {pid:#x}) exited with {code:#x}");
            }
            win32::kobj::with(|k| {
                if let Some(process) = k.processes.get_mut(&pid) {
                    process.exit.get_or_insert(code);
                }
                k.close_all(pid);
            });
            win32::kobj::notify();
        })
        .map_err(|error| error.to_string())?;
    Ok(pid)
}
