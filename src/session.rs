//! A desktop session: several Win32 programs running at once.
//!
//! The kernel [`Scheduler`] runs the processes; the session adds what makes
//! them one desktop. Each program has its own [`SharedState`] (windows,
//! message queue, handles), so programs cannot see each other's state, as
//! with Win32 processes. The session keeps the z-order of programs (the last
//! one is in the foreground) and the clipboard, whose contents are copied
//! between programs around their turns.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use crate::{
    kernel::{
        cpu::RunResult,
        scheduler::{Scheduler, Turn},
    },
    vm::{NestedLauncher, ProgramDisplay, Vm},
    win32::{clipboard::ClipFormat, SharedState},
};

pub struct Program {
    pub pid: u32,
    /// The program file name, e.g. `NOTEPAD.EXE`.
    pub name: String,
    pub shared: Rc<RefCell<SharedState>>,
    /// Clipboard generation this program last exchanged with the session.
    synced: Cell<u64>,
}

/// How a program ended.
pub struct Exit {
    pub pid: u32,
    pub name: String,
    pub result: Result<RunResult, String>,
}

#[derive(Default)]
struct SharedClipboard {
    generation: u64,
    entries: Vec<(ClipFormat, Vec<u8>)>,
}

#[derive(Default)]
pub struct Session {
    scheduler: Scheduler,
    /// Running programs, back to front.
    programs: RefCell<Vec<Rc<Program>>>,
    clipboard: RefCell<SharedClipboard>,
    exits: RefCell<Vec<Exit>>,
    /// Pumps host window-system events (the display); unset when headless.
    host_pump: RefCell<Option<Box<dyn FnMut(bool)>>>,
}

impl Session {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// Install the host event pump, called while programs wait in modal loops.
    pub fn set_host_pump(&self, pump: Box<dyn FnMut(bool)>) {
        *self.host_pump.borrow_mut() = Some(pump);
    }

    /// The running programs, back to front.
    pub fn programs(&self) -> Vec<Rc<Program>> {
        self.programs.borrow().clone()
    }

    pub fn is_empty(&self) -> bool {
        self.programs.borrow().is_empty()
    }

    pub fn foreground(&self) -> Option<Rc<Program>> {
        self.programs.borrow().last().cloned()
    }

    pub fn program(&self, pid: u32) -> Option<Rc<Program>> {
        self.programs
            .borrow()
            .iter()
            .find(|p| p.pid == pid)
            .cloned()
    }

    /// Start a program in the foreground. `load` builds it around the
    /// Win32 state the session gives it.
    pub fn spawn(
        self: &Rc<Self>,
        name: &str,
        screen: (i32, i32),
        load: impl FnOnce(Rc<RefCell<SharedState>>) -> Result<Vm, String>,
    ) -> Result<u32, String> {
        let pid = self.scheduler.allocate_pid();
        let shared = Rc::new(RefCell::new(SharedState::default()));
        let generation = {
            let clipboard = self.clipboard.borrow();
            let mut state = shared.borrow_mut();
            state.gui.screen = screen;
            state.clipboard.entries = clipboard.entries.clone();
            state.clipboard.generation = clipboard.generation;
            clipboard.generation
        };
        let vm = load(shared.clone())?;
        // Programs it starts run inside its calls, pumping as it does.
        let weak = Rc::downgrade(self);
        let pump: Rc<dyn Fn(bool)> = Rc::new(move |block| {
            if let Some(session) = weak.upgrade() {
                session.pump_from(pid, block);
            }
        });
        let display: Rc<dyn ProgramDisplay> = self.clone();
        NestedLauncher::new(Some(pump), Some(display)).install(&shared);
        let session = Rc::downgrade(self);
        let process = vm
            .start(Box::new(move |block| {
                if let Some(session) = session.upgrade() {
                    session.pump_from(pid, block);
                }
            }))
            .map_err(|error| error.to_string())?;
        self.deactivate_foreground();
        self.scheduler.add(pid, process);
        self.programs.borrow_mut().push(Rc::new(Program {
            pid,
            name: name.to_string(),
            shared,
            synced: Cell::new(generation),
        }));
        Ok(pid)
    }

}

impl ProgramDisplay for Session {
    /// Show a program that runs outside the scheduler (a child run inside
    /// its parent's call) on the desktop, in the foreground.
    fn attach(&self, pid: u32, name: &str, shared: Rc<RefCell<SharedState>>) {
        self.deactivate_foreground();
        let generation = self.clipboard.borrow().generation;
        self.programs.borrow_mut().push(Rc::new(Program {
            pid: 0x8000_0000 | pid,
            name: name.to_string(),
            shared,
            synced: Cell::new(generation),
        }));
    }

    /// Take an attached program off the desktop.
    fn detach(&self, pid: u32) {
        if let Some(program) = self.program(0x8000_0000 | pid) {
            self.remove(&program);
        }
    }
}

impl Session {
    /// Tell the foreground program it lost the foreground (the user clicked
    /// another program, the shell, or the desktop).
    pub fn deactivate_foreground(&self) {
        if let Some(program) = self.foreground() {
            let mut shared = program.shared.borrow_mut();
            if shared.gui.active_hwnd != 0 {
                shared.gui.pending_deactivate = true;
            }
        }
    }

    /// Move a program to the front of the z-order. Returns whether it
    /// changed.
    pub fn bring_to_front(&self, pid: u32) -> bool {
        if self.foreground().is_some_and(|p| p.pid == pid) {
            return false;
        }
        self.deactivate_foreground();
        let mut programs = self.programs.borrow_mut();
        let Some(index) = programs.iter().position(|p| p.pid == pid) else {
            return false;
        };
        let program = programs.remove(index);
        programs.push(program);
        true
    }

    /// Programs that ended since the last call.
    pub fn take_exits(&self) -> Vec<Exit> {
        std::mem::take(&mut *self.exits.borrow_mut())
    }

    /// Give every program that is not already running a turn.
    pub fn run_programs(&self) {
        let exits = self.scheduler.run(|pid, turn| {
            if let Some(program) = self.program(pid) {
                match turn {
                    Turn::Before => self.sync_in(&program),
                    Turn::After => self.sync_out(&program),
                }
            }
        });
        for exit in exits {
            let Some(program) = self.program(exit.pid) else {
                continue;
            };
            self.remove(&program);
            self.exits.borrow_mut().push(Exit {
                pid: exit.pid,
                name: program.name.clone(),
                result: exit.result,
            });
        }
    }

    fn remove(&self, program: &Rc<Program>) {
        let was_foreground = self.foreground().is_some_and(|p| p.pid == program.pid);
        self.programs.borrow_mut().retain(|p| p.pid != program.pid);
        if was_foreground {
            // Windows activates the next program's window.
            if let Some(next) = self.foreground() {
                let mut shared = next.shared.borrow_mut();
                if let Some(hwnd) = shared.gui.first_visible_top_level() {
                    shared.gui.pending_activate = Some(hwnd);
                }
            }
        }
    }

    /// A program is waiting: pump host events and, when it may block, let
    /// the other programs run meanwhile.
    fn pump_from(&self, pid: u32, block: bool) {
        let program = self.program(pid);
        if let Some(program) = &program {
            self.sync_out(program);
        }
        if let Ok(mut pump) = self.host_pump.try_borrow_mut() {
            if let Some(pump) = pump.as_mut() {
                pump(block);
            }
        }
        if block {
            self.run_programs();
        }
        if let Some(program) = &program {
            self.sync_in(program);
        }
    }

    /// Bring the session clipboard into a program if another one changed it.
    fn sync_in(&self, program: &Program) {
        let clipboard = self.clipboard.borrow();
        if program.synced.get() == clipboard.generation {
            return;
        }
        let mut shared = program.shared.borrow_mut();
        let local = &mut shared.clipboard;
        local.entries = clipboard.entries.clone();
        local.owner = 0;
        local.handles.clear();
        local.ole_source = 0;
        local.generation = clipboard.generation;
        program.synced.set(clipboard.generation);
    }

    /// Publish a program's clipboard changes to the session.
    fn sync_out(&self, program: &Program) {
        let mut shared = program.shared.borrow_mut();
        let local = &mut shared.clipboard;
        if local.generation == program.synced.get() {
            return;
        }
        let mut clipboard = self.clipboard.borrow_mut();
        clipboard.generation += 1;
        clipboard.entries = local.entries.clone();
        local.generation = clipboard.generation;
        program.synced.set(clipboard.generation);
    }
}
