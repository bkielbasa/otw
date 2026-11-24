//! A runnable Win32 program: the kernel loads the executable and its
//! genuine DLLs into a process image, and the Win32 subsystem provides the
//! system calls behind its other imports.

use std::{cell::RefCell, rc::Rc};

use crate::{
    kernel::{
        cpu::{self, RunError, RunResult},
        iat::Iat,
        image::{Image, ImageError},
        loader::MappedModule,
        pe::PeImage,
    },
    win32::{self, ApiContext, SharedState, Win32Imports},
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
        let image = Image::load(inputs, &command_line, &Win32Imports, &win32::entry_points())?;
        let api_ctx = ApiContext::for_image(&image, shared);
        Ok(Self { image, api_ctx })
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
        cpu::run(&self.image, Rc::new(self.api_ctx.clone()), None)
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
