//! Kernel objects several processes share: processes started to run
//! alongside their parent, sections backed by the paging file, and pipes.
//! Programs reach them through handles; a handle belongs to one process,
//! and `DuplicateHandle` or inheritance gives another process a handle of
//! its own to the same object.
//!
//! Processes that run alongside others each have a host thread, so this
//! state is behind a lock. Handles to these objects are numbered from
//! `FIRST_HANDLE` up, apart from the ones each process numbers itself.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use crate::kernel::memory::SharedMemory;

/// Handles to shared objects start here (a process's own handles stay
/// far below).
pub const FIRST_HANDLE: u32 = 0x00A0_0000;

#[derive(Clone)]
pub enum Object {
    Section { memory: Arc<SharedMemory>, size: u64 },
    /// One end of a pipe (0 the server's, 1 the client's).
    PipeEnd { pipe: u64, side: usize },
    /// A process (a handle another process was given to it).
    Process { pid: u32 },
    /// An open file another process passed on: the receiving process
    /// takes it into its own file table when it first uses the handle.
    File { file: Arc<std::fs::File>, path: Vec<u8> },
}

impl std::fmt::Debug for Object {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Object::Section { size, .. } => write!(f, "Section({size:#x})"),
            Object::PipeEnd { pipe, side } => write!(f, "PipeEnd({pipe}, {side})"),
            Object::Process { pid } => write!(f, "Process({pid})"),
            Object::File { path, .. } => write!(f, "File({})", String::from_utf8_lossy(path)),
        }
    }
}

#[derive(Debug, Default)]
pub struct Pipe {
    pub name: String,
    /// Bytes waiting to be read by each side.
    pub inbound: [VecDeque<u8>; 2],
    /// Handles open to each side.
    pub handles: [u32; 2],
    /// A client has opened it.
    pub connected: bool,
    /// Ends that were opened and have since been closed for good.
    pub closed: [bool; 2],
}

/// A process running alongside the others.
#[derive(Debug, Default)]
pub struct Process {
    pub exit: Option<u32>,
    /// Created suspended, not yet resumed.
    pub suspended: bool,
    pub parent: u32,
    /// Another process ended it (TerminateProcess).
    pub terminated: bool,
}

#[derive(Default)]
pub struct Kernel {
    objects: HashMap<u64, Object>,
    next_object: u64,
    /// (process, handle) to object.
    handles: HashMap<(u32, u32), u64>,
    /// Handles marked inheritable, by process.
    inheritable: std::collections::HashSet<(u32, u32)>,
    next_handle: u32,
    pub pipes: HashMap<u64, Pipe>,
    pub processes: HashMap<u32, Process>,
    next_pid: u32,
    /// Named sections, by lower-case name.
    names: HashMap<String, u64>,
}

fn kernel() -> MutexGuard<'static, Kernel> {
    static KERNEL: OnceLock<Mutex<Kernel>> = OnceLock::new();
    KERNEL
        .get_or_init(|| Mutex::new(Kernel::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Work on the shared state.
pub fn with<R>(f: impl FnOnce(&mut Kernel) -> R) -> R {
    f(&mut kernel())
}

impl Kernel {
    fn new_handle(&mut self) -> u32 {
        self.next_handle += 4;
        FIRST_HANDLE + self.next_handle
    }

    /// A new object and `pid`'s first handle to it.
    pub fn create(&mut self, pid: u32, object: Object) -> u32 {
        self.next_object += 1;
        let id = self.next_object;
        if let Object::PipeEnd { pipe, side } = &object {
            if let Some(p) = self.pipes.get_mut(pipe) {
                p.handles[*side] += 1;
            }
        }
        self.objects.insert(id, object);
        let handle = self.new_handle();
        self.handles.insert((pid, handle), id);
        handle
    }

    pub fn get(&self, pid: u32, handle: u32) -> Option<&Object> {
        self.objects.get(self.handles.get(&(pid, handle))?)
    }

    pub fn owns(&self, pid: u32, handle: u32) -> bool {
        self.handles.contains_key(&(pid, handle))
    }

    /// Give `target` a handle to the object behind `pid`'s `handle`
    /// (numbered `value` when given, as an inherited handle keeps its
    /// number).
    pub fn duplicate(&mut self, pid: u32, handle: u32, target: u32, value: Option<u32>) -> Option<u32> {
        let id = *self.handles.get(&(pid, handle))?;
        let new = value.unwrap_or_else(|| self.new_handle());
        if let Some(Object::PipeEnd { pipe, side }) = self.objects.get(&id) {
            if let Some(p) = self.pipes.get_mut(pipe) {
                p.handles[*side] += 1;
            }
        }
        self.handles.insert((target, new), id);
        Some(new)
    }

    /// Close `pid`'s handle. Returns whether it was one of these.
    pub fn close(&mut self, pid: u32, handle: u32) -> bool {
        let Some(id) = self.handles.remove(&(pid, handle)) else {
            return false;
        };
        self.inheritable.remove(&(pid, handle));
        if let Some(Object::PipeEnd { pipe, side }) = self.objects.get(&id) {
            if let Some(p) = self.pipes.get_mut(pipe) {
                p.handles[*side] = p.handles[*side].saturating_sub(1);
                if p.handles[*side] == 0 {
                    p.closed[*side] = true;
                }
            }
        }
        if !self.handles.values().any(|&other| other == id) {
            self.objects.remove(&id);
        }
        true
    }

    pub fn set_inheritable(&mut self, pid: u32, handle: u32, inherit: bool) {
        if inherit {
            self.inheritable.insert((pid, handle));
        } else {
            self.inheritable.remove(&(pid, handle));
        }
    }

    /// `pid`'s handles a child it starts inheriting all of may have.
    pub fn inheritable(&self, pid: u32) -> Vec<u32> {
        self.inheritable.iter().filter(|(p, _)| *p == pid).map(|&(_, h)| h).collect()
    }

    /// Give `pid` a handle to the named object, if there is one.
    pub fn named(&mut self, pid: u32, name: &str) -> Option<u32> {
        let id = *self.names.get(name)?;
        if !self.objects.contains_key(&id) {
            self.names.remove(name);
            return None;
        }
        let handle = self.new_handle();
        self.handles.insert((pid, handle), id);
        Some(handle)
    }

    /// Name the object behind `pid`'s handle.
    pub fn name(&mut self, pid: u32, handle: u32, name: &str) {
        if let Some(&id) = self.handles.get(&(pid, handle)) {
            self.names.insert(name.to_string(), id);
        }
    }

    /// A handle in `owner` to process `pid`.
    pub fn process_handle(&mut self, owner: u32, pid: u32) -> u32 {
        self.create(owner, Object::Process { pid })
    }

    /// Close every handle of a process that ended.
    pub fn close_all(&mut self, pid: u32) {
        let handles: Vec<u32> = self.handles.keys().filter(|(p, _)| *p == pid).map(|&(_, h)| h).collect();
        for handle in handles {
            self.close(pid, handle);
        }
    }

    pub fn new_pipe(&mut self, name: &str) -> u64 {
        self.next_object += 1;
        let id = self.next_object;
        self.pipes.insert(id, Pipe { name: name.to_ascii_lowercase(), ..Pipe::default() });
        id
    }

    /// A pipe of that name waiting for a client.
    pub fn listening_pipe(&self, name: &str) -> Option<u64> {
        let name = name.to_ascii_lowercase();
        self.pipes.iter().find(|(_, p)| p.name == name && !p.connected && !p.closed[0]).map(|(&id, _)| id)
    }

    pub fn pipe_exists(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.pipes.values().any(|p| p.name == name && !p.closed[0])
    }

    /// A process id for a new process.
    pub fn new_pid(&mut self, parent: u32, suspended: bool) -> u32 {
        self.next_pid += 4;
        let pid = 0x1000 + self.next_pid;
        self.processes.insert(pid, Process { exit: None, suspended, parent, terminated: false });
        pid
    }
}

/// The exit code of a process running alongside others, once it ended.
pub fn exit_code(pid: u32) -> Option<u32> {
    with(|k| k.processes.get(&pid).and_then(|p| p.exit))
}

/// Whether a process id names a process of these (alive or ended).
pub fn known_process(pid: u32) -> bool {
    with(|k| k.processes.contains_key(&pid))
}

/// Whether another process ended this one.
pub fn terminated(pid: u32) -> bool {
    pid != 1 && with(|k| k.processes.get(&pid).is_some_and(|p| p.terminated))
}
