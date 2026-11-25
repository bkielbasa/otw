//! Threads and synchronisation: `CreateThread`, events, mutexes,
//! semaphores, critical sections, thread-local storage and the wait
//! functions.
//!
//! The kernel runs a process's threads one at a time and switches only when
//! the running one waits. A thread that waits on something not yet
//! signalled comes off the CPU (its wait is made again when it next runs);
//! the first thread instead gives the CPU to the threads that can go on,
//! in turn, until its wait is over. A new thread first sends
//! `DLL_THREAD_ATTACH` to the loaded DLLs, as Windows does.

use std::collections::{HashMap, HashSet};

use crate::win32::{ApiContext, ApiOutcome, GuestMemory, ThreadRun};

const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x102;
const INFINITE: u32 = u32::MAX;
const STILL_ACTIVE: u32 = 0x103;
const CREATE_SUSPENDED: u32 = 0x4;

/// A waitable object.
#[derive(Clone, Debug)]
pub enum Object {
    Event { manual: bool, signaled: bool },
    Mutex { owner: u32, count: u32 },
    Semaphore { count: u32, max: u32 },
    Thread { id: u32, exit: Option<u32> },
    /// A program started with CreateProcess (its process or main thread).
    Process { pid: u32, exit: Option<u32> },
}

/// What a thread that came off the CPU waits for.
#[derive(Clone, Debug, Default)]
pub struct Wait {
    handles: Vec<u32>,
    all: bool,
    /// A critical section to own.
    section: u32,
    /// Ticks when it gives up waiting.
    deadline: Option<u64>,
    /// Messages also end the wait.
    messages: bool,
}

/// Threads and synchronisation objects of a process.
#[derive(Debug, Default)]
pub struct SyncState {
    pub objects: HashMap<u32, Object>,
    /// Named objects (`CreateEvent(..., "name")`), by lower-case name.
    names: HashMap<String, u32>,
    /// Waits of threads that are off the CPU.
    waits: HashMap<u32, Wait>,
    /// Threads created suspended, not yet resumed.
    suspended: HashSet<u32>,
    /// Thread-local storage of threads other than the first, by (thread, slot).
    tls: HashMap<(u32, u32), u32>,
    /// Start routines of new threads, by start token.
    starts: HashMap<u32, (u32, u32)>,
    next_start: u32,
    /// Modules that asked for no thread notifications.
    pub quiet_modules: HashSet<u32>,
}

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "GetCurrentThreadId" => 0,
        "SetEvent" | "ResetEvent" | "PulseEvent" | "ReleaseMutex" | "Sleep" | "ExitThread"
        | "ResumeThread" | "SuspendThread" | "EnterCriticalSection" | "LeaveCriticalSection"
        | "TryEnterCriticalSection" | "InitializeCriticalSection" | "DeleteCriticalSection"
        | "ReinitializeCriticalSection" | "OTW_ThreadStart" | "TlsGetValue"
        | "DisableThreadLibraryCalls" => 1,
        "OTW_RunDll32" => 0,
        "WaitForSingleObject" | "SleepEx" | "GetExitCodeThread" | "TlsSetValue" | "TerminateThread"
        | "GetExitCodeProcess"
        | "InitializeCriticalSectionAndSpinCount" => 2,
        "CreateMutexA" | "CreateMutexW" | "OpenMutexA" | "OpenMutexW" | "OpenEventA" | "OpenEventW"
        | "OpenSemaphoreA" | "OpenSemaphoreW" | "ReleaseSemaphore" | "WaitForSingleObjectEx" => 3,
        "CreateEventA" | "CreateEventW" | "CreateSemaphoreA" | "CreateSemaphoreW"
        | "WaitForMultipleObjects" => 4,
        "WaitForMultipleObjectsEx" | "MsgWaitForMultipleObjects" => 5,
        "CreateThread" => 6,
        "CreateProcessA" | "CreateProcessW" => 10,
        _ => return None,
    })
}

fn name_arg<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, wide: bool) -> Option<String> {
    if va == 0 {
        return None;
    }
    let bytes = if wide {
        crate::win32::unicode::wide_to_ansi(&crate::win32::unicode::read_wstr(memory, va))
    } else {
        crate::win32::read_cstr(memory, va)
    };
    Some(String::from_utf8_lossy(&bytes).to_ascii_lowercase())
}

/// A new object, or the existing one of that name.
fn create(ctx: &ApiContext, name: Option<String>, object: Object) -> u32 {
    let mut shared = ctx.shared.borrow_mut();
    if let Some(name) = &name {
        if let Some(&handle) = shared.sync.names.get(name) {
            shared.last_error = 183; // ERROR_ALREADY_EXISTS
            return handle;
        }
    }
    let handle = shared.alloc_handle();
    shared.sync.objects.insert(handle, object);
    if let Some(name) = name {
        shared.sync.names.insert(name, handle);
    }
    shared.last_error = 0;
    handle
}

fn open(ctx: &ApiContext, name: Option<String>) -> u32 {
    let mut shared = ctx.shared.borrow_mut();
    match name.and_then(|n| shared.sync.names.get(&n).copied()) {
        Some(handle) => handle,
        None => {
            shared.last_error = 2;
            0
        }
    }
}

/// Whether `handle` is signalled for thread `thread`. Handles that are not
/// synchronisation objects (files, processes) count as signalled.
fn signaled(sync: &SyncState, handle: u32, thread: u32) -> bool {
    match sync.objects.get(&handle) {
        Some(Object::Event { signaled, .. }) => *signaled,
        Some(Object::Mutex { owner, .. }) => *owner == 0 || *owner == thread,
        Some(Object::Semaphore { count, .. }) => *count > 0,
        Some(Object::Thread { exit, .. }) | Some(Object::Process { exit, .. }) => exit.is_some(),
        None => true,
    }
}

/// Take what a satisfied wait takes (an auto-reset event, a mutex, a
/// semaphore count).
fn acquire(sync: &mut SyncState, handle: u32, thread: u32) {
    match sync.objects.get_mut(&handle) {
        Some(Object::Event { manual: false, signaled }) => *signaled = false,
        Some(Object::Mutex { owner, count }) => {
            *owner = thread;
            *count += 1;
        }
        Some(Object::Semaphore { count, .. }) => *count -= 1,
        _ => {}
    }
}

/// The index of the handle that ends the wait (any; or 0 when all are
/// signalled for a wait on all), taking it.
fn try_wait(sync: &mut SyncState, wait: &Wait, thread: u32) -> Option<u32> {
    if wait.handles.is_empty() {
        return None;
    }
    if wait.all {
        if wait.handles.iter().all(|&h| signaled(sync, h, thread)) {
            for &handle in &wait.handles {
                acquire(sync, handle, thread);
            }
            return Some(0);
        }
        return None;
    }
    let index = wait.handles.iter().position(|&h| signaled(sync, h, thread))?;
    acquire(sync, wait.handles[index], thread);
    Some(index as u32)
}

fn section_free<M: GuestMemory + ?Sized>(memory: &mut M, section: u32, thread: u32) -> bool {
    let owner = memory.read_u32(section + 12).unwrap_or(0);
    owner == 0 || owner == thread
}

fn messages_waiting(ctx: &ApiContext, thread: u32) -> bool {
    let shared = ctx.shared.borrow();
    let now = shared.ticks();
    crate::win32::user::messages_for(&shared.gui, now, thread)
}

/// Whether a thread that is off the CPU can go on.
fn ready<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, thread: u32, now: u64) -> bool {
    let wait = {
        let shared = ctx.shared.borrow();
        if shared.sync.suspended.contains(&thread) {
            return false;
        }
        match shared.sync.waits.get(&thread) {
            None => {
                drop(shared);
                // A thread in WinSock's `select` goes on when a socket is
                // ready or its time is up.
                return crate::win32::winsock::select_ready(&mut ctx.shared.borrow_mut(), thread).unwrap_or(true);
            }
            Some(wait) => wait.clone(),
        }
    };
    if wait.deadline.is_some_and(|deadline| now >= deadline) {
        return true;
    }
    if wait.section != 0 {
        return section_free(memory, wait.section, thread);
    }
    if wait.messages && messages_waiting(ctx, thread) {
        return true;
    }
    let shared = ctx.shared.borrow();
    if wait.all {
        !wait.handles.is_empty() && wait.handles.iter().all(|&h| signaled(&shared.sync, h, thread))
    } else {
        wait.handles.iter().any(|&h| signaled(&shared.sync, h, thread))
    }
}

/// Give the CPU in turn to the threads that can go on. Returns whether any
/// ran.
pub fn run_ready_threads<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) -> bool {
    let mut ran = false;
    let now = ctx.shared.borrow().ticks();
    for thread in memory.waiting_threads() {
        if !ready(ctx, memory, thread, now) {
            continue;
        }
        // Each thread keeps its own last error.
        let last_error = ctx.shared.borrow().last_error;
        let outcome = memory.run_thread(thread);
        ctx.shared.borrow_mut().last_error = last_error;
        if std::env::var_os("OTW_TRACE_THREADS").is_some() {
            let wait = ctx.shared.borrow().sync.waits.get(&thread).cloned();
            eprintln!("[thread] {thread} ran: {outcome:?}, now waits for {wait:?}");
        }
        match outcome {
            Some(ThreadRun::Exited(code)) => {
                let mut shared = ctx.shared.borrow_mut();
                shared.sync.waits.remove(&thread);
                shared.sync.tls.retain(|(t, _), _| *t != thread);
                for object in shared.sync.objects.values_mut() {
                    match object {
                        Object::Thread { id, exit } if *id == thread => *exit = Some(code),
                        // A mutex its owner left behind is abandoned (free).
                        Object::Mutex { owner, count } if *owner == thread => {
                            *owner = 0;
                            *count = 0;
                        }
                        _ => {}
                    }
                }
                ran = true;
            }
            Some(ThreadRun::Waiting) => ran = true,
            None => {}
        }
    }
    ran
}

/// Note the exit codes of started programs that have ended.
fn refresh_processes(ctx: &ApiContext) {
    let launcher = ctx.shared.borrow().launcher.0.clone();
    let Some(launcher) = launcher else {
        return;
    };
    let mut shared = ctx.shared.borrow_mut();
    for object in shared.sync.objects.values_mut() {
        if let Object::Process { pid, exit } = object {
            if exit.is_none() {
                *exit = launcher.exit_code(*pid);
            }
        }
    }
}

/// The program a `CreateProcess` call names: its application name, or the
/// start of its command line (quoted, or the shortest run of words that
/// names a file), found as Windows searches: the current directory, the
/// program's directory, `C:\WINDOWS\SYSTEM`, `C:\WINDOWS`.
fn program_path(ctx: &ApiContext, application: &[u8], command_line: &[u8]) -> Option<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = Vec::new();
    if !application.is_empty() {
        names.push(application.to_vec());
    } else if command_line.first() == Some(&b'"') {
        let end = command_line[1..].iter().position(|&b| b == b'"').map(|i| i + 1).unwrap_or(command_line.len());
        names.push(command_line[1..end].to_vec());
    } else {
        for (index, &byte) in command_line.iter().enumerate() {
            if byte == b' ' {
                names.push(command_line[..index].to_vec());
            }
        }
        names.push(command_line.to_vec());
    }
    let (program_dir, current_dir) = {
        let shared = ctx.shared.borrow();
        let program = &shared.program_path;
        let dir = program.iter().rposition(|&b| b == b'\\').map(|end| program[..end].to_vec()).unwrap_or_default();
        (dir, shared.current_dir.clone())
    };
    for name in names {
        let name: Vec<u8> = name.iter().copied().filter(|&b| b != b'"').collect();
        let file = name.rsplit(|&b| b == b'\\').next().unwrap_or(&name);
        let mut name = name.clone();
        if !file.contains(&b'.') {
            name.extend_from_slice(b".exe");
        }
        let qualified = name.contains(&b':') || name.first() == Some(&b'\\');
        let mut candidates = vec![crate::win32::crt::absolute(ctx, &name)];
        if !qualified {
            for dir in [current_dir.clone(), program_dir.clone(), b"C:\\WINDOWS\\SYSTEM".to_vec(), b"C:\\WINDOWS".to_vec()] {
                if !dir.is_empty() {
                    candidates.push([dir.as_slice(), b"\\", &name].concat());
                }
            }
        }
        for candidate in candidates {
            if crate::win32::crt::host_path(&candidate).is_some_and(|host| host.is_file()) {
                return Some(candidate);
            }
        }
    }
    None
}

/// `rundll32 dll,entry arguments`: load the DLL and call
/// `entry(window, instance, arguments, show)`. Returns `None` when the
/// command is another program.
fn rundll32<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, application: &[u8], command_line: &[u8]) -> Option<u32> {
    let line = String::from_utf8_lossy(command_line).into_owned();
    let line = line.trim();
    let (program, rest) = if let Some(quoted) = line.strip_prefix('"') {
        let end = quoted.find('"')?;
        (&quoted[..end], quoted[end + 1..].trim_start())
    } else {
        line.split_once(' ').unwrap_or((line, ""))
    };
    let program = if application.is_empty() { program.to_owned() } else { String::from_utf8_lossy(application).into_owned() };
    let file = program.rsplit(['\\', '/']).next().unwrap_or(&program).to_ascii_lowercase();
    if file != "rundll32" && file != "rundll32.exe" {
        return None;
    }
    let (dll, call) = rest.split_once(',')?;
    let (entry, arguments) = call.trim().split_once(' ').unwrap_or((call.trim(), ""));
    if std::env::var_os("OTW_TRACE_SETUP").is_some() || std::env::var_os("OTW_TRACE_THREADS").is_some() {
        eprintln!("[process] rundll32 {dll} {entry} {arguments:?}");
    }
    let Ok(module) = crate::win32::module::load_library(ctx, memory, dll.trim(), 0) else {
        return Some(1);
    };
    let find = |memory: &mut M, name: &str| {
        crate::win32::module::proc_address(ctx, memory, module, &crate::kernel::pe::ImportName::ByName(name.into()), 0)
    };
    let function = find(memory, entry).or_else(|| find(memory, &format!("{entry}A")));
    let Some(function) = function else {
        return Some(1);
    };
    let text = ctx.shared.borrow_mut().heap.alloc(arguments.len() as u32 + 1).unwrap_or(0);
    let _ = memory.write_bytes(text, &[arguments.as_bytes(), &[0]].concat());
    memory.call_guest(function, &[0, module, text, 1]);
    ctx.shared.borrow_mut().heap.free(text);
    Some(0)
}

/// How a wait ended.
enum Outcome {
    /// The object at this index (or a message, one past the last).
    Signaled(u32),
    Timeout,
    /// The thread came off the CPU; the call is made again later.
    Suspended,
}

/// Wait as `wait` says (its deadline in milliseconds from now, or none).
fn wait<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, mut wait: Wait, timeout: u32) -> Outcome {
    let thread = memory.current_thread();
    // A wait made again after the thread came off the CPU keeps its deadline.
    let previous = ctx.shared.borrow_mut().sync.waits.remove(&thread);
    wait.deadline = match previous {
        Some(previous) => previous.deadline,
        None if timeout == INFINITE => None,
        None => Some(ctx.shared.borrow().ticks() + timeout as u64),
    };
    loop {
        refresh_processes(ctx);
        {
            let mut shared = ctx.shared.borrow_mut();
            if let Some(index) = try_wait(&mut shared.sync, &wait, thread) {
                return Outcome::Signaled(index);
            }
        }
        if wait.section != 0 && section_free(memory, wait.section, thread) {
            return Outcome::Signaled(0);
        }
        if wait.messages && messages_waiting(ctx, thread) {
            return Outcome::Signaled(wait.handles.len() as u32);
        }
        let now = ctx.shared.borrow().ticks();
        if wait.deadline.is_some_and(|deadline| now >= deadline) {
            return Outcome::Timeout;
        }
        if memory.suspend_thread() {
            ctx.shared.borrow_mut().sync.waits.insert(thread, wait);
            return Outcome::Suspended;
        }
        if run_ready_threads(ctx, memory) {
            continue;
        }
        // Nothing else can run.
        let on_programs = {
            let shared = ctx.shared.borrow();
            wait.handles.iter().any(|h| matches!(shared.sync.objects.get(h), Some(Object::Process { exit: None, .. })))
        };
        if on_programs {
            // Started programs run while the host is pumped.
            memory.pump(true);
            continue;
        }
        if memory.waiting_threads().is_empty() && wait.deadline.is_none() && !wait.messages {
            // With no other thread, nothing could ever signal it: do as the
            // single-threaded VM always did and let the wait succeed.
            return Outcome::Signaled(0);
        }
        if memory.stopping() {
            return Outcome::Timeout;
        }
        if wait.messages {
            memory.pump(true);
        } else {
            let remaining = wait.deadline.map(|d| d.saturating_sub(now)).unwrap_or(10);
            std::thread::sleep(std::time::Duration::from_millis(remaining.clamp(1, 10)));
        }
    }
}

fn result_of(outcome: Outcome) -> u32 {
    match outcome {
        Outcome::Signaled(index) => WAIT_OBJECT_0 + index,
        Outcome::Timeout => WAIT_TIMEOUT,
        Outcome::Suspended => 0,
    }
}

pub fn dispatch<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    let ok = |value: u32| Some(ApiOutcome::new(value, count));
    let wide = name.ends_with('W');
    let thread = memory.current_thread();
    match name {
        "GetCurrentThreadId" => ok(thread),
        "CreateEventA" | "CreateEventW" => {
            // (security, manual reset, initially signalled, name)
            let name = name_arg(memory, args[3], wide);
            ok(create(ctx, name, Object::Event { manual: args[1] != 0, signaled: args[2] != 0 }))
        }
        "CreateMutexA" | "CreateMutexW" => {
            // (security, initially owned, name)
            let name = name_arg(memory, args[2], wide);
            let owned = args[1] != 0;
            ok(create(
                ctx,
                name,
                Object::Mutex { owner: if owned { thread } else { 0 }, count: u32::from(owned) },
            ))
        }
        "CreateSemaphoreA" | "CreateSemaphoreW" => {
            // (security, initial count, maximum, name)
            let name = name_arg(memory, args[3], wide);
            ok(create(ctx, name, Object::Semaphore { count: args[1], max: args[2].max(1) }))
        }
        "OpenEventA" | "OpenEventW" | "OpenMutexA" | "OpenMutexW" | "OpenSemaphoreA" | "OpenSemaphoreW" => {
            let name = name_arg(memory, args[2], wide);
            ok(open(ctx, name))
        }
        "SetEvent" | "ResetEvent" | "PulseEvent" => {
            let mut shared = ctx.shared.borrow_mut();
            match shared.sync.objects.get_mut(&args[0]) {
                Some(Object::Event { signaled, .. }) => {
                    *signaled = name == "SetEvent";
                    drop(shared);
                    if name == "PulseEvent" {
                        // Releases the threads already waiting: let them run.
                        if let Some(Object::Event { signaled, .. }) = ctx.shared.borrow_mut().sync.objects.get_mut(&args[0]) {
                            *signaled = true;
                        }
                        run_ready_threads(ctx, memory);
                        if let Some(Object::Event { signaled, .. }) = ctx.shared.borrow_mut().sync.objects.get_mut(&args[0]) {
                            *signaled = false;
                        }
                    }
                    ok(1)
                }
                _ => ok(1),
            }
        }
        "ReleaseMutex" => {
            let mut shared = ctx.shared.borrow_mut();
            match shared.sync.objects.get_mut(&args[0]) {
                Some(Object::Mutex { owner, count }) if *owner == thread => {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        *owner = 0;
                    }
                    ok(1)
                }
                Some(Object::Mutex { .. }) => {
                    shared.last_error = 288; // ERROR_NOT_OWNER
                    ok(0)
                }
                _ => ok(1),
            }
        }
        "ReleaseSemaphore" => {
            // (semaphore, release count, &previous)
            let previous = {
                let mut shared = ctx.shared.borrow_mut();
                match shared.sync.objects.get_mut(&args[0]) {
                    Some(Object::Semaphore { count, max }) => {
                        let previous = *count;
                        if previous + args[1] > *max {
                            shared.last_error = 298; // ERROR_TOO_MANY_POSTS
                            return ok(0);
                        }
                        *count += args[1];
                        previous
                    }
                    _ => 0,
                }
            };
            if args[2] != 0 {
                let _ = memory.write_u32(args[2], previous);
            }
            ok(1)
        }
        "WaitForSingleObject" | "WaitForSingleObjectEx" => {
            let spec = Wait { handles: vec![args[0]], ..Wait::default() };
            ok(result_of(wait(ctx, memory, spec, args[1])))
        }
        "WaitForMultipleObjects" | "WaitForMultipleObjectsEx" | "MsgWaitForMultipleObjects" => {
            // (count, handles, wait all, timeout[, wake mask or alertable])
            let handles: Vec<u32> = (0..args[0].min(64)).map(|i| memory.read_u32(args[1] + i * 4).unwrap_or(0)).collect();
            let spec = Wait {
                handles,
                all: args[2] != 0,
                messages: name == "MsgWaitForMultipleObjects",
                ..Wait::default()
            };
            ok(result_of(wait(ctx, memory, spec, args[3])))
        }
        "Sleep" | "SleepEx" => {
            if args[0] == 0 {
                run_ready_threads(ctx, memory);
                return ok(0);
            }
            let spec = Wait::default();
            let timeout = args[0];
            if memory.waiting_threads().is_empty() && memory.current_thread() == 1 {
                // Alone: sleep a little, as before threads.
                std::thread::sleep(std::time::Duration::from_millis(timeout.min(50) as u64));
                return ok(0);
            }
            let _ = wait(ctx, memory, spec, timeout);
            ok(0)
        }
        "CreateThread" => {
            // (security, stack size, start, parameter, flags, &thread id)
            let token = {
                let mut shared = ctx.shared.borrow_mut();
                shared.sync.next_start += 1;
                let token = shared.sync.next_start;
                shared.sync.starts.insert(token, (args[2], args[3]));
                token
            };
            let start = ctx.builtin_proc("OTW_ThreadStart");
            let stack = if args[1] == 0 { 0x10_0000 } else { args[1] };
            let Some(id) = memory.create_thread(start, token, stack) else {
                ctx.shared.borrow_mut().last_error = 8;
                return ok(0);
            };
            let handle = create(ctx, None, Object::Thread { id, exit: None });
            if args[4] & CREATE_SUSPENDED != 0 {
                ctx.shared.borrow_mut().sync.suspended.insert(id);
            }
            if args[5] != 0 {
                let _ = memory.write_u32(args[5], id);
            }
            if std::env::var_os("OTW_TRACE_THREADS").is_some() {
                eprintln!("[thread] {id} created: start {:#x}({:#x})", args[2], args[3]);
            }
            ok(handle)
        }
        "OTW_ThreadStart" => {
            // The new thread's first code: DLL_THREAD_ATTACH to the loaded
            // DLLs, then on to the start routine with the return address the
            // kernel put on the stack.
            let (start, parameter) = ctx.shared.borrow_mut().sync.starts.remove(&args[0]).unwrap_or((0, 0));
            let modules: Vec<(u32, u32)> = ctx
                .modules
                .borrow()
                .iter()
                .filter(|m| !m.path.is_empty())
                .map(|m| (m.image_base, m.entry))
                .collect();
            for (base, entry) in modules {
                if entry != 0 && !ctx.shared.borrow().sync.quiet_modules.contains(&base) {
                    memory.call_guest(entry, &[base, 2, 0]);
                }
            }
            let registers = memory.registers()?;
            let _ = memory.write_u32(registers.esp + 4, parameter);
            memory.request_resume(crate::win32::Resume {
                esp: registers.esp,
                ebp: 0,
                eip: start,
                regs: None,
            });
            ok(0)
        }
        "OTW_RunDll32" => {
            // RUNDLL32.EXE's program: its command line names the DLL call.
            let line = crate::win32::read_cstr(memory, ctx.command_line_va);
            let code = rundll32(ctx, memory, b"", &line).unwrap_or(1);
            Some(ApiOutcome { value: 0, arg_count: count, exit_code: Some(code) })
        }
        "ExitThread" => {
            if memory.end_thread(args[0]) {
                return ok(0);
            }
            Some(ApiOutcome { value: 0, arg_count: count, exit_code: Some(args[0]) })
        }
        "TerminateThread" => ok(0),
        "CreateProcessA" | "CreateProcessW" => {
            // (application, command line, process and thread security,
            //  inherit, flags, environment, directory, STARTUPINFO,
            //  PROCESS_INFORMATION*)
            let read = |memory: &mut M, va: u32| {
                if va == 0 {
                    Vec::new()
                } else if wide {
                    crate::win32::unicode::wide_to_ansi(&crate::win32::unicode::read_wstr(memory, va))
                } else {
                    crate::win32::read_cstr(memory, va)
                }
            };
            let application = read(memory, args[0]);
            let command_line = read(memory, args[1]);
            let directory = read(memory, args[7]);
            let launcher = ctx.shared.borrow().launcher.0.clone();
            if std::env::var_os("OTW_TRACE_THREADS").is_some() {
                eprintln!(
                    "[process] CreateProcess {:?} {:?}",
                    String::from_utf8_lossy(&application),
                    String::from_utf8_lossy(&command_line)
                );
            }
            // Windows' own rundll32 runs as a program of its own when
            // programs can be started; otherwise here, in this process.
            let in_place = launcher.is_none();
            if let Some(code) = in_place.then(|| rundll32(ctx, memory, &application, &command_line)).flatten() {
                // Windows' own rundll32 runs here, in this process.
                let pid = 0x7000 + ctx.shared.borrow_mut().alloc_handle();
                let process = create(ctx, None, Object::Process { pid, exit: Some(code) });
                let main_thread = create(ctx, None, Object::Process { pid, exit: Some(code) });
                if args[9] != 0 {
                    for (offset, value) in [(0, process), (4, main_thread), (8, pid), (12, pid)] {
                        let _ = memory.write_u32(args[9] + offset, value);
                    }
                }
                return ok(1);
            }
            let Some(launcher) = launcher else {
                eprintln!("[process] cannot start {:?}: no launcher", String::from_utf8_lossy(&command_line));
                ctx.shared.borrow_mut().last_error = 8;
                return ok(0);
            };
            let Some(path) = program_path(ctx, &application, &command_line) else {
                ctx.shared.borrow_mut().last_error = 2; // ERROR_FILE_NOT_FOUND
                return ok(0);
            };
            let command_line = if command_line.is_empty() {
                [b"\"".as_slice(), &path, b"\""].concat()
            } else {
                command_line
            };
            let directory = if directory.is_empty() {
                ctx.shared.borrow().current_dir.clone()
            } else {
                crate::win32::crt::absolute(ctx, &directory)
            };
            match launcher.start(ctx, &path, &command_line, &directory) {
                Ok(pid) => {
                    let process = create(ctx, None, Object::Process { pid, exit: None });
                    let main_thread = create(ctx, None, Object::Process { pid, exit: None });
                    refresh_processes(ctx);
                    if args[9] != 0 {
                        for (offset, value) in [(0, process), (4, main_thread), (8, pid), (12, pid)] {
                            let _ = memory.write_u32(args[9] + offset, value);
                        }
                    }
                    ok(1)
                }
                Err(error) => {
                    eprintln!("[process] {} did not start: {error}", String::from_utf8_lossy(&path));
                    ctx.shared.borrow_mut().last_error = 193; // ERROR_BAD_EXE_FORMAT
                    ok(0)
                }
            }
        }
        "GetExitCodeProcess" => {
            refresh_processes(ctx);
            let code = match ctx.shared.borrow().sync.objects.get(&args[0]) {
                Some(Object::Process { exit, .. }) => exit.unwrap_or(STILL_ACTIVE),
                _ => STILL_ACTIVE,
            };
            let _ = memory.write_u32(args[1], code);
            ok(1)
        }
        "GetExitCodeThread" => {
            let code = match ctx.shared.borrow().sync.objects.get(&args[0]) {
                Some(Object::Thread { exit, .. }) => exit.unwrap_or(STILL_ACTIVE),
                _ => STILL_ACTIVE,
            };
            let _ = memory.write_u32(args[1], code);
            ok(1)
        }
        "ResumeThread" | "SuspendThread" => {
            let id = match ctx.shared.borrow().sync.objects.get(&args[0]) {
                Some(Object::Thread { id, .. }) => *id,
                _ => return ok(0),
            };
            let mut shared = ctx.shared.borrow_mut();
            let was = u32::from(shared.sync.suspended.contains(&id));
            if name == "ResumeThread" {
                shared.sync.suspended.remove(&id);
            } else {
                shared.sync.suspended.insert(id);
            }
            ok(was)
        }
        "DisableThreadLibraryCalls" => {
            ctx.shared.borrow_mut().sync.quiet_modules.insert(args[0]);
            ok(1)
        }
        // CRITICAL_SECTION: debug info, lock count, recursion count, owning
        // thread, lock semaphore, spin count.
        "InitializeCriticalSection" | "InitializeCriticalSectionAndSpinCount" | "ReinitializeCriticalSection" => {
            let mut section = vec![0u8; 24];
            section[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
            let _ = memory.write_bytes(args[0], &section);
            ok(1)
        }
        "DeleteCriticalSection" => ok(0),
        "EnterCriticalSection" | "TryEnterCriticalSection" => {
            let section = args[0];
            if !section_free(memory, section, thread) {
                if name == "TryEnterCriticalSection" {
                    return ok(0);
                }
                let spec = Wait { section, ..Wait::default() };
                if let Outcome::Suspended = wait(ctx, memory, spec, INFINITE) {
                    return ok(0);
                }
            }
            let recursion = memory.read_u32(section + 8).unwrap_or(0);
            let _ = memory.write_u32(section + 8, recursion + 1);
            let _ = memory.write_u32(section + 12, thread);
            ok(1)
        }
        "LeaveCriticalSection" => {
            let section = args[0];
            if memory.read_u32(section + 12).unwrap_or(0) == thread {
                let recursion = memory.read_u32(section + 8).unwrap_or(1).saturating_sub(1);
                let _ = memory.write_u32(section + 8, recursion);
                if recursion == 0 {
                    let _ = memory.write_u32(section + 12, 0);
                }
            }
            ok(0)
        }
        "TlsGetValue" => {
            let mut shared = ctx.shared.borrow_mut();
            let value = if thread == 1 {
                shared.tls_slots.get(args[0] as usize).and_then(|slot| *slot).unwrap_or(0)
            } else {
                shared.sync.tls.get(&(thread, args[0])).copied().unwrap_or(0)
            };
            shared.last_error = 0;
            ok(value)
        }
        "TlsSetValue" => {
            let mut shared = ctx.shared.borrow_mut();
            let allocated = shared.tls_slots.get(args[0] as usize).is_some_and(Option::is_some);
            if !allocated {
                shared.last_error = 87;
                return ok(0);
            }
            if thread == 1 {
                shared.tls_slots[args[0] as usize] = Some(args[1]);
            } else {
                shared.sync.tls.insert((thread, args[0]), args[1]);
            }
            ok(1)
        }
        _ => None,
    }
}
