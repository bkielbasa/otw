//! Cooperative scheduling of processes on one host thread.
//!
//! Each [`Process`] has its own CPU. [`Scheduler::run`] gives every process
//! a turn: it runs until it waits for messages again (it yields) or exits.
//! Turns may nest — a process waiting inside a modal loop cannot yield, so
//! its waits call `run` to let the others proceed — and a process is never
//! resumed from inside itself.

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use super::cpu::{Process, RunResult};

/// Before or after a process's turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    Before,
    After,
}

/// A process that ended.
pub struct Exit {
    pub pid: u32,
    pub result: Result<RunResult, String>,
}

type Slot = Rc<RefCell<Option<Process<'static>>>>;

#[derive(Default)]
pub struct Scheduler {
    /// In the order they get turns.
    processes: RefCell<Vec<(u32, Slot)>>,
    next_pid: Cell<u32>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve a process id (before the process exists, so its system
    /// calls can know it).
    pub fn allocate_pid(&self) -> u32 {
        let pid = self.next_pid.get() + 1;
        self.next_pid.set(pid);
        pid
    }

    pub fn add(&self, pid: u32, process: Process<'static>) {
        self.processes
            .borrow_mut()
            .push((pid, Rc::new(RefCell::new(Some(process)))));
    }

    pub fn len(&self) -> usize {
        self.processes.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.processes.borrow().is_empty()
    }

    /// Give every process not already running a turn. `around` is called
    /// before and after each turn. Returns the processes that ended.
    pub fn run(&self, mut around: impl FnMut(u32, Turn)) -> Vec<Exit> {
        let snapshot: Vec<(u32, Slot)> = self.processes.borrow().clone();
        let mut exits = Vec::new();
        for (pid, slot) in snapshot {
            let Ok(mut process) = slot.try_borrow_mut() else {
                continue; // running further up the stack
            };
            let Some(running) = process.as_mut() else {
                continue;
            };
            around(pid, Turn::Before);
            let outcome = running.resume();
            around(pid, Turn::After);
            let result = match outcome {
                Ok(None) => continue,
                Ok(Some(result)) => Ok(result),
                Err(error) => Err(error.to_string()),
            };
            *process = None;
            drop(process);
            self.processes.borrow_mut().retain(|(id, _)| *id != pid);
            exits.push(Exit { pid, result });
        }
        exits
    }
}
