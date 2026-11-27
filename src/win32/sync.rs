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

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

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
    /// A waitable timer: signalled from its due time (in VM ticks); an
    /// auto-reset one is taken by a wait and, if periodic, set again.
    Timer { manual: bool, due: Option<u64>, period: u64 },
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
    /// WaitOnAddress: the address and the value it had; a different value
    /// ends the wait.
    address: u32,
    compare: Vec<u8>,
    /// The timeout the wait was made with (to tell the same call made
    /// again from another).
    timeout: u32,
    /// A slim reader/writer lock to take (shared or exclusive).
    srw: u32,
    srw_exclusive: bool,
    /// A wake-up on this address (condition variable, WakeByAddress*)
    /// ends the wait: the address's wake count when it began.
    wake_key: u32,
    wake_count: u64,
}

/// Wake-ups of a condition variable or address: all waiters (a count),
/// or single ones not yet taken.
#[derive(Clone, Copy, Debug, Default)]
struct Wakes {
    all: u64,
    single: u32,
}

fn address_changed<M: GuestMemory + ?Sized>(memory: &mut M, wait: &Wait) -> bool {
    wait.address != 0 && memory.read_bytes(wait.address, wait.compare.len()).is_some_and(|now| now != wait.compare)
}

/// WaitOnAddress: wait until the value at `address` differs from
/// `compare` or the timeout passes. Returns whether it changed (or the
/// thread came off the CPU, to make the call again).
pub fn wait_on_address<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, address: u32, compare: Vec<u8>, timeout: u32) -> bool {
    let wake_count = ctx.shared.borrow().sync.wakes.get(&address).map(|w| w.all).unwrap_or(0);
    let spec = Wait { address, compare, wake_key: address, wake_count, ..Wait::default() };
    !matches!(wait(ctx, memory, spec, timeout), Outcome::Timeout)
}

/// Threads and synchronisation objects of a process.
#[derive(Debug, Default)]
pub struct SyncState {
    pub objects: HashMap<u32, Object>,
    /// The time waits are checked at (VM ticks), for timers.
    now: u64,
    /// Named objects (`CreateEvent(..., "name")`), by lower-case name.
    names: HashMap<String, u32>,
    /// Waits of threads that are off the CPU.
    waits: HashMap<u32, Wait>,
    /// Threads created suspended, not yet resumed.
    suspended: HashSet<u32>,
    /// Wake-ups by address.
    wakes: HashMap<u32, Wakes>,
    /// Thread-local storage of threads other than the first, by (thread, slot).
    tls: HashMap<(u32, u32), u32>,
    /// Start routines of new threads, by start token.
    starts: HashMap<u32, (u32, u32)>,
    next_start: u32,
    /// Modules that asked for no thread notifications.
    pub quiet_modules: HashSet<u32>,
    /// When the running thread got the CPU (64-bit processes are
    /// time-sliced at API calls).
    slice_start: Option<std::time::Instant>,
    /// The main thread is letting others run.
    preempting: bool,
    /// The last error of each thread off the CPU.
    last_errors: HashMap<u32, u32>,
    /// The TEB of each thread of a 64-bit process that made a call (which
    /// hold their TLS slots).
    pub tebs: HashMap<u32, u32>,
    /// The number of threads waiting in the subsystem for each slim
    /// reader/writer lock (while any do, the lock's waiters bit is set).
    srw_waiters: HashMap<u32, u32>,
}

impl SyncState {
    /// A thread's last error.
    pub fn last_error_of(&self, thread: u32) -> u32 {
        self.last_errors.get(&thread).copied().unwrap_or(0)
    }

    pub fn set_last_error_of(&mut self, thread: u32, error: u32) {
        self.last_errors.insert(thread, error);
    }
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
/// The thread id behind a thread handle (or the current thread's pseudo
/// handle).
pub fn thread_id_of(ctx: &ApiContext, memory: &dyn GuestMemory, handle: u32) -> Option<u32> {
    if handle == 0xFFFF_FFFE {
        return Some(memory.current_thread());
    }
    match ctx.shared.borrow().sync.objects.get(&handle) {
        Some(Object::Thread { id, .. }) => Some(*id),
        _ => None,
    }
}

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
        Some(Object::Timer { due, .. }) => due.is_some_and(|due| due <= sync.now),
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
        Some(Object::Timer { manual: false, due, period }) => {
            *due = (*period > 0).then(|| due.unwrap_or(0).max(0) + *period);
        }
        _ => {}
    }
}

/// CreateWaitableTimer: a timer not yet set.
pub fn create_timer(ctx: &ApiContext, manual: bool) -> u32 {
    create(ctx, None, Object::Timer { manual, due: None, period: 0 })
}

/// SetWaitableTimer: due in `delay` milliseconds, then every `period`.
pub fn set_timer(ctx: &ApiContext, handle: u32, delay: u64, period: u64) -> bool {
    let mut shared = ctx.shared.borrow_mut();
    let now = shared.ticks();
    match shared.sync.objects.get_mut(&handle) {
        Some(Object::Timer { due, period: every, .. }) => {
            *due = Some(now + delay);
            *every = period;
            true
        }
        _ => false,
    }
}

/// CancelWaitableTimer.
pub fn cancel_timer(ctx: &ApiContext, handle: u32) -> bool {
    match ctx.shared.borrow_mut().sync.objects.get_mut(&handle) {
        Some(Object::Timer { due, .. }) => {
            *due = None;
            true
        }
        _ => false,
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

/// Take a slim reader/writer lock if it is free. The lock's word is
/// shared with the program's own code for it (native.rs): bit 0 an
/// exclusive owner, bit 1 waiters here, readers from bit 2.
fn srw_take<M: GuestMemory + ?Sized>(memory: &mut M, lock: u32, exclusive: bool) -> bool {
    use crate::win32::native::{SRW_EXCLUSIVE, SRW_READER};
    let mut current = memory.read_u32(lock).unwrap_or(0);
    loop {
        if !srw_word_free(current, exclusive) {
            return false;
        }
        let new = if exclusive { current | SRW_EXCLUSIVE } else { current.wrapping_add(SRW_READER) };
        match memory.compare_exchange_u32(lock, current, new) {
            Some(Ok(_)) => return true,
            Some(Err(found)) => current = found,
            None => return false,
        }
    }
}

fn srw_word_free(word: u32, exclusive: bool) -> bool {
    use crate::win32::native::{SRW_EXCLUSIVE, SRW_READER};
    word & SRW_EXCLUSIVE == 0 && (!exclusive || word / SRW_READER == 0)
}

fn srw_free<M: GuestMemory + ?Sized>(memory: &mut M, lock: u32, exclusive: bool) -> bool {
    srw_word_free(memory.read_u32(lock).unwrap_or(0), exclusive)
}

/// One more (`true`) or one fewer thread waits here for a lock: the
/// lock's waiters bit is set while any do.
fn srw_waiting<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, lock: u32, more: bool) {
    use crate::win32::native::SRW_WAITERS;
    let any = {
        let mut shared = ctx.shared.borrow_mut();
        let count = shared.sync.srw_waiters.entry(lock).or_default();
        if more {
            *count += 1;
        } else {
            *count = count.saturating_sub(1);
        }
        let any = *count > 0;
        if !any {
            shared.sync.srw_waiters.remove(&lock);
        }
        any
    };
    let _ = memory.update_u32(lock, &mut |word| if any { word | SRW_WAITERS } else { word & !SRW_WAITERS });
}

/// Whether a wake-up came for a wait (taking a single one).
fn woken(sync: &mut SyncState, wait: &Wait) -> bool {
    if wait.wake_key == 0 {
        return false;
    }
    let Some(wakes) = sync.wakes.get_mut(&wait.wake_key) else {
        return false;
    };
    if wakes.all != wait.wake_count {
        return true;
    }
    if wakes.single > 0 {
        wakes.single -= 1;
        return true;
    }
    false
}

/// AcquireSRWLock*: take the lock, waiting while another holds it.
pub fn srw_acquire<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, lock: u32, exclusive: bool) -> bool {
    if srw_take(memory, lock, exclusive) {
        return true;
    }
    // Set the waiters bit first, so that the holder's release (in the
    // program) comes here to wake this thread; the wait looks at the lock
    // again before it sleeps.
    srw_waiting(ctx, memory, lock, true);
    let spec = Wait { srw: lock, srw_exclusive: exclusive, ..Wait::default() };
    let taken = matches!(wait(ctx, memory, spec, INFINITE), Outcome::Signaled(_));
    srw_waiting(ctx, memory, lock, false);
    taken
}

/// TryAcquireSRWLock*.
pub fn srw_try<M: GuestMemory + ?Sized>(memory: &mut M, lock: u32, exclusive: bool) -> bool {
    srw_take(memory, lock, exclusive)
}

/// ReleaseSRWLock*.
pub fn srw_release<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, lock: u32, exclusive: bool) {
    use crate::win32::native::{SRW_EXCLUSIVE, SRW_READER};
    let _ = memory.update_u32(lock, &mut |word| {
        if exclusive {
            word & !SRW_EXCLUSIVE
        } else if word / SRW_READER > 0 {
            word - SRW_READER
        } else {
            word
        }
    });
    if ctx.shared.borrow().sync.srw_waiters.contains_key(&lock) {
        wake_key(ctx, crate::win32::kobj::KEY_SRW | lock as u64);
    }
}

/// WakeConditionVariable / WakeByAddressSingle (`all` false) and the
/// wake-all forms.
pub fn wake(ctx: &ApiContext, key: u32, all: bool) {
    {
        let mut shared = ctx.shared.borrow_mut();
        let wakes = shared.sync.wakes.entry(key).or_default();
        if all {
            wakes.all += 1;
            wakes.single = 0;
        } else {
            wakes.single = wakes.single.saturating_add(1).min(64);
        }
    }
    wake_key(ctx, crate::win32::kobj::KEY_WAKE | key as u64);
}

/// Sleep on a condition variable (or address) until woken or the timeout
/// passes. Returns whether it was woken.
pub fn sleep_on<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, key: u32, timeout: u32) -> bool {
    let wake_count = ctx.shared.borrow().sync.wakes.get(&key).map(|w| w.all).unwrap_or(0);
    let spec = Wait { wake_key: key, wake_count, ..Wait::default() };
    !matches!(wait(ctx, memory, spec, timeout), Outcome::Timeout)
}

fn section_free<M: GuestMemory + ?Sized>(memory: &mut M, section: u32, thread: u32) -> bool {
    let owner = memory.read_u32(section + crate::win32::native::CS_OWNER).unwrap_or(0);
    owner == 0 || owner == thread
}

fn messages_waiting(ctx: &ApiContext, thread: u32) -> bool {
    let shared = ctx.shared.borrow();
    let now = shared.ticks();
    crate::win32::user::messages_for(&shared.gui, now, thread)
}

/// Thread-local storage of a 64-bit process: in each thread's TEB, where
/// the program's own TlsGetValue and TlsSetValue keep it (native.rs).
/// `teb` is the calling thread's.
pub fn tls64(ctx: &ApiContext, memory: &mut dyn GuestMemory, teb: u32, name: &str, args: &crate::kernel::syscall::Args) -> Option<ApiOutcome> {
    use crate::win32::native::{TEB_TLS_SLOTS, TLS_SLOTS};
    let count = args.len();
    let index = args.wide(0) as u32;
    let slot = |teb: u32| (index < TLS_SLOTS).then(|| teb + TEB_TLS_SLOTS + index * 8);
    let invalid = || {
        ctx.shared.borrow_mut().last_error = 87; // ERROR_INVALID_PARAMETER
        Some(ApiOutcome::new(0, count))
    };
    match name {
        "TlsGetValue" | "FlsGetValue" => {
            let Some(at) = slot(teb) else { return invalid() };
            let value = memory.read_u64(at).unwrap_or(0);
            ctx.shared.borrow_mut().last_error = 0;
            Some(ApiOutcome::wide(value, count))
        }
        "TlsSetValue" | "FlsSetValue" => {
            let Some(at) = slot(teb) else { return invalid() };
            let _ = memory.write_u64(at, args.wide(1));
            Some(ApiOutcome::new(1, count))
        }
        "TlsAlloc" | "FlsAlloc" => {
            let (index, tebs) = {
                let mut shared = ctx.shared.borrow_mut();
                let Some(index) = shared.tls_slots.iter().position(Option::is_none) else {
                    shared.last_error = 8; // ERROR_NOT_ENOUGH_MEMORY
                    return Some(ApiOutcome::new(u32::MAX, count));
                };
                shared.tls_slots[index] = Some(0);
                shared.last_error = 0;
                (index as u32, shared.sync.tebs.values().copied().chain([teb]).collect::<Vec<_>>())
            };
            // A new slot starts empty in every thread.
            for teb in tebs {
                let _ = memory.write_u64(teb + TEB_TLS_SLOTS + index * 8, 0);
            }
            Some(ApiOutcome::new(index, count))
        }
        "TlsFree" | "FlsFree" => {
            let mut shared = ctx.shared.borrow_mut();
            match shared.tls_slots.get_mut(index as usize) {
                Some(slot @ Some(_)) => {
                    *slot = None;
                    Some(ApiOutcome::new(1, count))
                }
                _ => {
                    drop(shared);
                    invalid()
                }
            }
        }
        _ => None,
    }
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
    if wait.srw != 0 {
        return srw_free(memory, wait.srw, wait.srw_exclusive);
    }
    if wait.wake_key != 0 {
        let wakes = ctx.shared.borrow().sync.wakes.get(&wait.wake_key).copied().unwrap_or_default();
        if wakes.all != wait.wake_count || wakes.single > 0 {
            return true;
        }
    }
    if wait.address != 0 {
        return address_changed(memory, &wait);
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
    // Pipe reads whose data came from another process.
    crate::win32::ipc::poll(ctx, memory);
    let now = ctx.shared.borrow().ticks();
    ctx.shared.borrow_mut().sync.now = now;
    for thread in memory.waiting_threads() {
        if !ready(ctx, memory, thread, now) {
            continue;
        }
        // Each thread keeps its own last error.
        let last_error = {
            let mut shared = ctx.shared.borrow_mut();
            let own = shared.sync.last_errors.get(&thread).copied().unwrap_or(0);
            std::mem::replace(&mut shared.last_error, own)
        };
        let outcome = memory.run_thread(thread);
        {
            let mut shared = ctx.shared.borrow_mut();
            let own = std::mem::replace(&mut shared.last_error, last_error);
            shared.sync.last_errors.insert(thread, own);
        }
        if std::env::var_os("OTW_TRACE_THREADS").is_some() {
            let wait = ctx.shared.borrow().sync.waits.get(&thread).cloned();
            eprintln!("[thread] {thread} ran: {outcome:?}, now waits for {wait:?}");
        }
        match outcome {
            Some(ThreadRun::Exited(code)) => {
                thread_exited(ctx, thread, code);
                ran = true;
            }
            Some(ThreadRun::Waiting) => ran = true,
            None => {}
        }
    }
    ran
}

/// A thread ended: its handle is signalled, its waits and thread-local
/// storage go, mutexes it held are abandoned.
pub fn thread_exited(ctx: &ApiContext, thread: u32, code: u32) {
    {
        let mut shared = ctx.shared.borrow_mut();
        shared.sync.waits.remove(&thread);
        shared.sync.tls.retain(|(t, _), _| *t != thread);
        shared.sync.last_errors.remove(&thread);
        shared.sync.tebs.remove(&thread);
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
    }
    let handles: Vec<u32> = ctx
        .shared
        .borrow()
        .sync
        .objects
        .iter()
        .filter(|(_, o)| match o {
            Object::Thread { id, .. } => *id == thread,
            // Mutexes it held are abandoned.
            Object::Mutex { .. } => true,
            _ => false,
        })
        .map(|(&h, _)| h)
        .collect();
    for handle in handles {
        wake_handle(ctx, handle);
    }
}

/// Whether a thread was created suspended and is not yet resumed.
pub fn thread_suspended(ctx: &ApiContext, thread: u32) -> bool {
    ctx.shared.borrow().sync.suspended.contains(&thread)
}

/// Note the exit codes of started programs that have ended.
fn refresh_processes(ctx: &ApiContext) {
    let Some(launcher) = ctx.shared.try_borrow().ok().map(|s| s.launcher.0.clone()) else { return };
    // (A caller further up may be looking at the state: the next look
    // catches up.)
    let Ok(mut shared) = ctx.shared.try_borrow_mut() else { return };
    let mut ended = Vec::new();
    for (&handle, object) in shared.sync.objects.iter_mut() {
        if let Object::Process { pid, exit } = object {
            if exit.is_none() {
                *exit = launcher.as_ref().and_then(|l| l.exit_code(*pid)).or_else(|| crate::win32::kobj::exit_code(*pid));
                if exit.is_some() {
                    ended.push(handle);
                }
            }
        }
    }
    drop(shared);
    // The threads waiting for them (the thread that looked may be another).
    for handle in ended {
        wake_handle(ctx, handle);
    }
}

/// Time-slicing for the threads of a 64-bit process, which otherwise only
/// change places when one waits: a thread that has had the CPU for a
/// while gives it up at its next API call. A worker comes off the CPU
/// (returns true: the call is made again when it next runs); the main
/// thread lets the threads that can go on run first.
pub fn preempt(ctx: &ApiContext, memory: &mut dyn GuestMemory) -> bool {
    const SLICE: std::time::Duration = std::time::Duration::from_millis(15);
    {
        let mut shared = ctx.shared.borrow_mut();
        let now = std::time::Instant::now();
        let started = *shared.sync.slice_start.get_or_insert(now);
        if now.duration_since(started) < SLICE || shared.sync.preempting {
            return false;
        }
        shared.sync.slice_start = Some(now);
    }
    if memory.suspend_thread() {
        return true;
    }
    if !memory.callback_levels().is_empty() || memory.waiting_threads().is_empty() {
        // Not from inside a callback the subsystem made (a window
        // procedure it is running), only from the thread's own code.
        return false;
    }
    ctx.shared.borrow_mut().sync.preempting = true;
    run_ready_threads(ctx, memory);
    let mut shared = ctx.shared.borrow_mut();
    shared.sync.preempting = false;
    shared.sync.slice_start = Some(std::time::Instant::now());
    false
}

/// Wake the threads of this process waiting for `key` (kobj::KEY_*).
pub fn wake_key(ctx: &ApiContext, key: u64) {
    let pid = ctx.shared.borrow().pid;
    crate::win32::kobj::wake(pid, key);
}

/// Wake the threads waiting on a handle.
pub fn wake_handle(ctx: &ApiContext, handle: u32) {
    wake_key(ctx, crate::win32::kobj::KEY_HANDLE | handle as u64);
}

/// Signal an event and wake its waiters.
pub fn signal(ctx: &ApiContext, handle: u32) {
    set_event(&mut ctx.shared.borrow_mut().sync, handle);
    wake_handle(ctx, handle);
}

/// Signal an event (another part of the subsystem finished something a
/// thread may wait for).
pub fn set_event(sync: &mut SyncState, handle: u32) {
    if let Some(Object::Event { signaled, .. }) = sync.objects.get_mut(&handle) {
        *signaled = true;
    }
}

/// The process a process handle of this process names.
pub fn process_id(ctx: &ApiContext, handle: u32) -> Option<u32> {
    if let Some(Object::Process { pid, .. }) = ctx.shared.borrow().sync.objects.get(&handle) {
        return Some(*pid);
    }
    let me = ctx.shared.borrow().pid;
    crate::win32::kobj::with(|k| match k.get(me, handle) {
        Some(crate::win32::kobj::Object::Process { pid }) => Some(*pid),
        _ => None,
    })
}

/// A handle to another process (OpenProcess).
pub fn open_process(ctx: &ApiContext, pid: u32) -> u32 {
    create(ctx, None, Object::Process { pid, exit: None })
}

/// What `CreateProcess` asks for besides the program: the environment,
/// the handles to inherit (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST` in an
/// extended STARTUPINFO), whether to start suspended.
fn launch_options<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, args: &[u32], wide: bool) -> crate::win32::LaunchOptions {
    let flags = args[5];
    let environment = (args[6] != 0).then(|| {
        let unicode = flags & 0x400 != 0;
        let mut strings = Vec::new();
        let mut at = args[6];
        loop {
            let text = if unicode {
                let units = crate::win32::unicode::read_wstr(memory, at);
                at += 2 * (units.len() as u32 + 1);
                String::from_utf16_lossy(&units)
            } else {
                let bytes = crate::win32::read_cstr(memory, at);
                at += bytes.len() as u32 + 1;
                String::from_utf8_lossy(&bytes).into_owned()
            };
            if text.is_empty() || strings.len() > 4096 {
                break;
            }
            strings.push(text);
        }
        strings
    });
    let mut handles = None;
    if flags & 0x8_0000 != 0 && args[8] != 0 {
        // STARTUPINFOEX: the attribute list follows STARTUPINFO.
        let x64 = memory.ptr_size() == 8;
        let list = memory.read_ptr(args[8] + if x64 { 104 } else { 68 }).unwrap_or(0) as u32;
        if list != 0 && memory.read_u32(list) == Some(0x4C57_544F) {
            let entries = memory.read_u32(list + 4).unwrap_or(0).min(16);
            for index in 0..entries {
                let entry = list + 8 + index * 24;
                let attribute = memory.read_u64(entry).unwrap_or(0);
                let value = memory.read_u64(entry + 8).unwrap_or(0) as u32;
                let size = memory.read_u64(entry + 16).unwrap_or(0) as u32;
                if attribute == 0x2_0002 {
                    let width = memory.ptr_size();
                    let list: Vec<u32> = (0..size / width).filter_map(|i| memory.read_ptr(value + i * width).map(|h| h as u32)).collect();
                    handles = Some(list);
                }
            }
        }
    }
    let _ = wide;
    // Without one of its own, a child gets the parent's environment as it
    // now is.
    let environment = environment.or_else(|| Some(crate::win32::env::block(ctx)));
    crate::win32::LaunchOptions { environment, handles, inherit: args[4] != 0, suspended: flags & 4 != 0 }
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
    // (Only the same wait: a call that came back for another reason, such
    // as a step of a longer wait the subsystem makes, must not pass its
    // deadline on to the next wait.)
    let previous = ctx
        .shared
        .borrow_mut()
        .sync
        .waits
        .remove(&thread)
        .filter(|p| {
            p.handles == wait.handles
                && p.messages == wait.messages
                && p.section == wait.section
                && p.address == wait.address
                && p.srw == wait.srw
                && p.wake_key == wait.wake_key
                && p.timeout == timeout
        });
    wait.timeout = timeout;
    wait.deadline = match previous {
        Some(previous) => previous.deadline,
        None if timeout == INFINITE => None,
        None => Some(ctx.shared.borrow().ticks() + timeout as u64),
    };
    let began = std::time::Instant::now();
    let mut reported = false;
    // What wakes this wait (registered before each look at the state).
    let keys = {
        use crate::win32::kobj::*;
        let mut keys: Vec<u64> = wait.handles.iter().map(|&h| KEY_HANDLE | h as u64).collect();
        if wait.section != 0 {
            keys.push(KEY_SECTION | wait.section as u64);
        }
        if wait.srw != 0 {
            keys.push(KEY_SRW | wait.srw as u64);
        }
        if wait.wake_key != 0 {
            keys.push(KEY_WAKE | wait.wake_key as u64);
        }
        if wait.messages {
            keys.push(KEY_MESSAGES);
        }
        // Data from another process (pipes), or another process's end.
        let shared_objects = crate::win32::ipc::has_pending(ctx)
            || wait.handles.iter().any(|h| matches!(ctx.shared.borrow().sync.objects.get(h), Some(crate::win32::sync::Object::Process { .. })));
        if shared_objects {
            keys.push(KEY_SHARED);
        }
        keys
    };
    let pid = ctx.shared.borrow().pid;
    loop {
        let doing = std::doing("wait: interest");
        let _interest = crate::win32::kobj::interest(pid, keys.clone());
        drop(doing);
        if !reported && began.elapsed().as_secs() >= 20 && std::env::var_os("OTW_TRACE_WAITS").is_some() {
            reported = true;
            let pid = ctx.shared.borrow().pid;
            let owner = if wait.section != 0 { memory.read_u32(wait.section + 12).unwrap_or(0) } else { 0 };
            eprintln!(
                "[waits] process {pid:#x} thread {thread} has waited 20 s for {:?} (address {:#x} = {:?} waiting while {:?}, wakes {:?} from {}, section {:#x} owned by {owner}, messages {})",
                wait.handles,
                wait.address,
                memory.read_bytes(wait.address, wait.compare.len()),
                wait.compare,
                ctx.shared.borrow().sync.wakes.get(&wait.wake_key).copied(),
                wait.wake_count,
                wait.section,
                wait.messages
            );
            for (other, w) in &ctx.shared.borrow().sync.waits {
                eprintln!(
                    "[waits]   thread {other}: handles {:?} address {:#x} section {:#x} deadline {:?}",
                    w.handles, w.address, w.section, w.deadline
                );
            }
        }
        let generation = crate::win32::kobj::generation();
        let doing = std::doing("wait: processes");
        refresh_processes(ctx);
        drop(doing);
        let doing = std::doing("wait: pipes");
        crate::win32::ipc::poll(ctx, memory);
        drop(doing);
        let doing = std::doing("wait: objects");
        let me = ctx.shared.borrow().pid;
        if crate::win32::kobj::terminated(me) {
            // Another process ended this one: stop here for good.
            loop {
                crate::kernel::lock::unlocked(|| std::thread::sleep(std::time::Duration::from_secs(1)));
            }
        }
        {
            let mut shared = ctx.shared.borrow_mut();
            shared.sync.now = shared.ticks();
            if let Some(index) = try_wait(&mut shared.sync, &wait, thread) {
                return Outcome::Signaled(index);
            }
        }
        if wait.section != 0 && section_free(memory, wait.section, thread) {
            return Outcome::Signaled(0);
        }
        if wait.srw != 0 && srw_take(memory, wait.srw, wait.srw_exclusive) {
            return Outcome::Signaled(0);
        }
        if woken(&mut ctx.shared.borrow_mut().sync, &wait) {
            return Outcome::Signaled(0);
        }
        if address_changed(memory, &wait) {
            return Outcome::Signaled(0);
        }
        if wait.messages && !memory.has_host() && !memory.is_worker() && ctx.shared.borrow().x64 {
            // Headless, scripted input (OTW_INPUT) stands in for the
            // host's, also while other threads keep running. (Only for
            // 64-bit programs, which wait in MsgWaitForMultipleObjects
            // rather than GetMessage; fed here too, the script would run
            // ahead of the older programs it was timed for.)
            let due = ctx.shared.borrow().gui.script_until.is_none_or(|until| std::time::Instant::now() >= until);
            if due {
                crate::win32::user::feed_script(ctx);
            }
        }
        if wait.messages && messages_waiting(ctx, thread) {
            return Outcome::Signaled(wait.handles.len() as u32);
        }
        let now = ctx.shared.borrow().ticks();
        if wait.deadline.is_some_and(|deadline| now >= deadline) {
            return Outcome::Timeout;
        }
        drop(doing);
        if memory.suspend_thread() {
            ctx.shared.borrow_mut().sync.waits.insert(thread, wait);
            return Outcome::Suspended;
        }
        let doing = std::doing("wait: other threads");
        if run_ready_threads(ctx, memory) {
            continue;
        }
        drop(doing);
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
        if memory.waiting_threads().is_empty() && wait.deadline.is_none() && !wait.messages && !memory.others_running() {
            if std::env::var_os("OTW_TRACE_THREADS").is_some() {
                eprintln!("[thread] {thread} wait on {:?}: nothing else can run; it succeeds", wait.handles);
            }
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
            // With threads on other CPUs, the changes that end waits are
            // announced (kobj::notify): wait longer between looks.
            let cap = if memory.others_running() { 50 } else { 10 };
            let remaining = wait.deadline.map(|d| d.saturating_sub(now)).unwrap_or(cap).min(cap);
            // Until another process changes something, or a while.
            let _ = generation;
            let doing = std::doing("wait: park");
            crate::kernel::lock::unlocked(|| {
                // Keep up with other threads' memory changes meanwhile.
                memory.sync_memory();
                crate::win32::kobj::park(std::time::Duration::from_millis(remaining.max(1)))
            });
            drop(doing);
            memory.sync_memory();
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
                    if name != "ResetEvent" {
                        wake_handle(ctx, args[0]);
                    }
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
                        drop(shared);
                        wake_handle(ctx, args[0]);
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
            wake_handle(ctx, args[0]);
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
            // HANDLEs: pointer-sized.
            let size = memory.ptr_size();
            let handles: Vec<u32> = (0..args[0].min(64)).map(|i| memory.read_ptr(args[1] + i * size).unwrap_or(0) as u32).collect();
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
                crate::kernel::lock::unlocked(std::thread::yield_now);
                return ok(0);
            }
            let spec = Wait::default();
            let timeout = args[0];
            if memory.waiting_threads().is_empty() && memory.current_thread() == 1 && !memory.others_running() {
                // Alone: sleep a little, as before threads.
                crate::kernel::lock::unlocked(|| std::thread::sleep(std::time::Duration::from_millis(timeout.min(50) as u64)));
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
            // The parameter: on the stack (stdcall), or in RCX (x64).
            let regs = if memory.ptr_size() == 8 {
                Some([0, 0, parameter, 0, 0, 0])
            } else {
                let _ = memory.write_u32(registers.esp + 4, parameter);
                None
            };
            memory.request_resume(crate::win32::Resume {
                esp: registers.esp,
                ebp: 0,
                eip: start,
                regs,
            });
            ok(0)
        }
        "OTW_RunDll32" => {
            // RUNDLL32.EXE's program: its command line names the DLL call.
            let line = crate::win32::read_cstr(memory, ctx.command_line_va);
            let code = rundll32(ctx, memory, b"", &line).unwrap_or(1);
            Some(ApiOutcome { value: 0, wide: None, arg_count: count, exit_code: Some(code) })
        }
        "ExitThread" => {
            if memory.end_thread(args[0]) {
                return ok(0);
            }
            Some(ApiOutcome { value: 0, wide: None, arg_count: count, exit_code: Some(args[0]) })
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
                    crate::win32::x64::write_struct(memory, &crate::win32::x64::PROCESS_INFORMATION, args[9], &[process, main_thread, pid, pid]);
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
            let options = launch_options(ctx, memory, args, wide);
            match launcher.start_with(ctx, &path, &command_line, &directory, options) {
                Ok(pid) => {
                    let process = create(ctx, None, Object::Process { pid, exit: None });
                    let main_thread = create(ctx, None, Object::Process { pid, exit: None });
                    refresh_processes(ctx);
                    if args[9] != 0 {
                        crate::win32::x64::write_struct(memory, &crate::win32::x64::PROCESS_INFORMATION, args[9], &[process, main_thread, pid, pid]);
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
                Some(Object::Process { pid, .. }) => {
                    // The main thread of a process started alongside this one.
                    let pid = *pid;
                    let was = crate::win32::kobj::with(|k| {
                        let process = k.processes.get_mut(&pid)?;
                        let was = process.suspended;
                        process.suspended = name == "SuspendThread";
                        Some(was)
                    });
                    crate::win32::kobj::notify();
                    return ok(u32::from(was.unwrap_or(false)));
                }
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
            let mut section = vec![0u8; if memory.ptr_size() == 8 { 40 } else { 24 }];
            section[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
            let _ = memory.write_bytes(args[0], &section);
            ok(1)
        }
        "DeleteCriticalSection" => ok(0),
        // The owner is taken and given back at once (compare-exchange):
        // 64-bit programs do both in their own code (native.rs), coming
        // here only to wait, or to wake the threads waiting here (counted
        // at +16).
        "EnterCriticalSection" | "TryEnterCriticalSection" => {
            use crate::win32::native::{cs_waiters, CS_OWNER, CS_RECURSION};
            let section = args[0];
            let waiters = section + cs_waiters(memory.ptr_size());
            loop {
                match memory.compare_exchange_u32(section + CS_OWNER, 0, thread) {
                    Some(Ok(_)) => {
                        let _ = memory.write_u32(section + CS_RECURSION, 1);
                        return ok(1);
                    }
                    Some(Err(owner)) if owner == thread => {
                        let recursion = memory.read_u32(section + CS_RECURSION).unwrap_or(0);
                        let _ = memory.write_u32(section + CS_RECURSION, recursion + 1);
                        return ok(1);
                    }
                    Some(Err(_)) if name == "EnterCriticalSection" => {}
                    _ => return ok(0),
                }
                // Counted as waiting before looking again, so that the
                // owner giving it back (in the program) has this wake us.
                let _ = memory.update_u32(waiters, &mut |n| n + 1);
                let spec = Wait { section, ..Wait::default() };
                let outcome = wait(ctx, memory, spec, INFINITE);
                let _ = memory.update_u32(waiters, &mut |n| n.saturating_sub(1));
                if let Outcome::Suspended = outcome {
                    return ok(0);
                }
            }
        }
        "LeaveCriticalSection" => {
            use crate::win32::native::{cs_waiters, CS_OWNER, CS_RECURSION};
            let section = args[0];
            let mut released = false;
            if memory.read_u32(section + CS_OWNER).unwrap_or(0) == thread {
                let recursion = memory.read_u32(section + CS_RECURSION).unwrap_or(1).saturating_sub(1);
                let _ = memory.write_u32(section + CS_RECURSION, recursion);
                if recursion == 0 {
                    let _ = memory.compare_exchange_u32(section + CS_OWNER, thread, 0);
                    released = true;
                }
            } else {
                // The program gave it back itself and asks for the wake-up.
                released = true;
            }
            let waiting = memory.read_u32(section + cs_waiters(memory.ptr_size())).unwrap_or(0) > 0;
            if released && waiting {
                wake_key(ctx, crate::win32::kobj::KEY_SECTION | section as u64);
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

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;
    use crate::win32::native::{SRW_EXCLUSIVE, SRW_READER, SRW_WAITERS};

    /// A 64-bit process's memory, as thread `.1` sees it (TEB at `.2`).
    struct Flat(Vec<u8>, u32, u32);
    impl GuestMemory for Flat {
        fn read_bytes(&mut self, va: u32, len: usize) -> Option<Vec<u8>> {
            self.0.get(va as usize..va as usize + len).map(<[u8]>::to_vec)
        }
        fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Option<()> {
            self.0.get_mut(va as usize..va as usize + bytes.len())?.copy_from_slice(bytes);
            Some(())
        }
        fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
            self.write_bytes(va, &value.to_le_bytes())
        }
        fn ptr_size(&self) -> u32 {
            8
        }
        fn current_thread(&self) -> u32 {
            self.1
        }
        fn current_tib(&self) -> u32 {
            self.2
        }
    }

    #[test]
    fn slim_locks_share_their_word_with_the_program() {
        let ctx = ApiContext::default();
        let mut memory = Flat(vec![0; 0x100], 1, 0);
        let lock = 0x40;
        assert!(srw_acquire(&ctx, &mut memory, lock, false));
        assert!(srw_try(&mut memory, lock, false));
        assert_eq!(memory.read_u32(lock), Some(2 * SRW_READER));
        assert!(!srw_try(&mut memory, lock, true));
        srw_release(&ctx, &mut memory, lock, false);
        srw_release(&ctx, &mut memory, lock, false);
        assert_eq!(memory.read_u32(lock), Some(0));
        // Taken in the program: not free here.
        memory.write_u32(lock, SRW_EXCLUSIVE).unwrap();
        assert!(!srw_try(&mut memory, lock, false));
        // A waiter sets the waiters bit, the last one clears it; the bit
        // does not hold the lock.
        srw_waiting(&ctx, &mut memory, lock, true);
        srw_waiting(&ctx, &mut memory, lock, true);
        assert_eq!(memory.read_u32(lock), Some(SRW_EXCLUSIVE | SRW_WAITERS));
        srw_release(&ctx, &mut memory, lock, true);
        assert_eq!(memory.read_u32(lock), Some(SRW_WAITERS));
        assert!(srw_try(&mut memory, lock, true));
        assert_eq!(memory.read_u32(lock), Some(SRW_EXCLUSIVE | SRW_WAITERS));
        srw_waiting(&ctx, &mut memory, lock, false);
        assert_eq!(memory.read_u32(lock), Some(SRW_EXCLUSIVE | SRW_WAITERS));
        srw_waiting(&ctx, &mut memory, lock, false);
        assert_eq!(memory.read_u32(lock), Some(SRW_EXCLUSIVE));
    }

    #[test]
    fn critical_sections_keep_owner_and_recursion_where_the_program_does() {
        let ctx = ApiContext::default();
        let mut memory = Flat(vec![0; 0x100], 3, 0);
        let section = 0x40;
        dispatch(&ctx, &mut memory, "InitializeCriticalSection", &[section]).unwrap();
        assert_eq!(dispatch(&ctx, &mut memory, "EnterCriticalSection", &[section]).unwrap().value, 1);
        assert_eq!(dispatch(&ctx, &mut memory, "TryEnterCriticalSection", &[section]).unwrap().value, 1);
        assert_eq!(memory.read_u32(section + 8), Some(2));
        assert_eq!(memory.read_u32(section + 12), Some(3));
        // Another thread cannot have it.
        memory.1 = 4;
        assert_eq!(dispatch(&ctx, &mut memory, "TryEnterCriticalSection", &[section]).unwrap().value, 0);
        dispatch(&ctx, &mut memory, "LeaveCriticalSection", &[section]).unwrap();
        assert_eq!(memory.read_u32(section + 8), Some(2), "only the owner leaves it");
        memory.1 = 3;
        dispatch(&ctx, &mut memory, "LeaveCriticalSection", &[section]).unwrap();
        dispatch(&ctx, &mut memory, "LeaveCriticalSection", &[section]).unwrap();
        assert_eq!((memory.read_u32(section + 8), memory.read_u32(section + 12)), (Some(0), Some(0)));
    }

    #[test]
    fn tls_of_64_bit_threads_is_in_their_tebs() {
        use crate::kernel::syscall::Args;
        use crate::win32::native::TEB_TLS_SLOTS;
        let ctx = ApiContext::default();
        let (first, second) = (0x1000, 0x3000);
        let mut memory = Flat(vec![0; 0x5000], 1, first);
        ctx.shared.borrow_mut().sync.tebs.insert(2, second);
        let index = tls64(&ctx, &mut memory, first, "TlsAlloc", &Args::new(vec![], true)).unwrap().value;
        assert!(index < 64);
        let value = 0x7_0000_1234;
        tls64(&ctx, &mut memory, first, "TlsSetValue", &Args::new(vec![index as u64, value], true)).unwrap();
        assert_eq!(memory.read_u64(first + TEB_TLS_SLOTS + index * 8), Some(value));
        assert_eq!(tls64(&ctx, &mut memory, first, "FlsGetValue", &Args::new(vec![index as u64], true)).unwrap().wide, Some(value));
        assert_eq!(tls64(&ctx, &mut memory, second, "TlsGetValue", &Args::new(vec![index as u64], true)).unwrap().wide, Some(0));
        // A slot given back and taken again starts empty in every thread.
        memory.write_u64(second + TEB_TLS_SLOTS + index * 8, 5).unwrap();
        assert_eq!(tls64(&ctx, &mut memory, first, "TlsFree", &Args::new(vec![index as u64], true)).unwrap().value, 1);
        assert_eq!(tls64(&ctx, &mut memory, first, "TlsAlloc", &Args::new(vec![], true)).unwrap().value, index);
        assert_eq!(memory.read_u64(first + TEB_TLS_SLOTS + index * 8), Some(0));
        assert_eq!(memory.read_u64(second + TEB_TLS_SLOTS + index * 8), Some(0));
        assert_eq!(tls64(&ctx, &mut memory, first, "TlsGetValue", &Args::new(vec![64], true)).unwrap().value, 0);
        assert_eq!(ctx.shared.borrow().last_error, 87);
    }
}
