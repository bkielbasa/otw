//! What 64-bit processes share with each other: named pipes (with
//! overlapped I/O and completion ports), sections backed by the paging
//! file, and handles passed between processes (`DuplicateHandle`, process
//! handles). The objects themselves are in [`kobj`]; this is the API.
//!
//! Overlapped reads of a pipe that has no data wait in the process that
//! made them: whenever its threads wait, [`poll`] finishes the reads whose
//! data has come (the writer, in another process, cannot reach this
//! process's memory).

use std::collections::{HashMap, HashSet};

use crate::win32::{
    kobj::{self, Object},
    sync, ApiContext, ApiOutcome, GuestMemory,
};

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_BROKEN_PIPE: u32 = 109;
const ERROR_NO_DATA: u32 = 232;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_OPERATION_ABORTED: u32 = 995;
const ERROR_IO_INCOMPLETE: u32 = 996;
const ERROR_IO_PENDING: u32 = 997;
const ERROR_ALREADY_EXISTS: u32 = 183;

const STATUS_SUCCESS: u64 = 0;
const STATUS_PENDING: u64 = 0x103;
const STATUS_PIPE_BROKEN: u64 = 0xC000_014B;
const STATUS_CANCELLED: u64 = 0xC000_0120;

#[derive(Clone, Debug)]
enum Op {
    Read { buffer: u32, len: u32 },
    Connect,
}

#[derive(Clone, Debug)]
struct Pending {
    handle: u32,
    overlapped: u32,
    op: Op,
}

/// A process's side of these objects.
#[derive(Debug, Default)]
pub struct State {
    /// Completion port and key each handle reports to.
    ports: HashMap<u32, (u32, u64)>,
    /// Handles whose operations that finish at once post no packet.
    skip_on_success: HashSet<u32>,
    pending: Vec<Pending>,
    /// Section views, by address.
    views: HashSet<u32>,
    /// Named sections, by lower-case name (this process's handle).
    trace: Option<bool>,
}

fn trace(ctx: &ApiContext) -> bool {
    let mut shared = ctx.shared.borrow_mut();
    *shared.ipc.trace.get_or_insert_with(|| std::env::var_os("OTW_TRACE_IPC").is_some())
}

fn pid(ctx: &ApiContext) -> u32 {
    ctx.shared.borrow().pid
}

fn set_error(ctx: &ApiContext, code: u32) {
    ctx.shared.borrow_mut().last_error = code;
}

pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "CreateNamedPipeW" | "CreateNamedPipeA" => 8,
        "ConnectNamedPipe" | "CancelIoEx" | "GetHandleInformation" | "SetFileCompletionNotificationModes" | "WaitNamedPipeW" => 2,
        "TerminateProcess" | "UnmapViewOfFileEx" => 2,
        "DisconnectNamedPipe" | "CancelIo" | "CloseHandle" | "UnmapViewOfFile" | "GetFileType" | "GetProcessId" => 1,
        "ReadFile" | "WriteFile" | "MapViewOfFile" => 5,
        "GetOverlappedResult" | "SetHandleInformation" | "OpenProcess" | "OpenFileMappingW" => 3,
        "GetOverlappedResultEx" | "CreateIoCompletionPort" | "SetNamedPipeHandleState" => 4,
        "GetQueuedCompletionStatus" | "GetNamedPipeInfo" | "NtQueryObject" | "NtQuerySection" => 5,
        "GetQueuedCompletionStatusEx" | "PeekNamedPipe" | "CreateFileMappingW" | "MapViewOfFileEx" => 6,
        "CreateFileW" | "DuplicateHandle" => 7,
        _ => return None,
    })
}

fn pipe_name(memory: &mut dyn GuestMemory, va: u32) -> Option<String> {
    if va == 0 {
        return None;
    }
    let name = String::from_utf16_lossy(&crate::win32::unicode::read_wstr(memory, va));
    let lower = name.to_ascii_lowercase();
    (lower.starts_with("\\\\.\\pipe\\") || lower.starts_with("\\\\?\\pipe\\")).then_some(lower)
}

/// SECURITY_ATTRIBUTES.bInheritHandle.
fn inherits(memory: &mut dyn GuestMemory, attributes: u32) -> bool {
    let at = if memory.ptr_size() == 8 { 16 } else { 8 };
    attributes != 0 && memory.read_u32(attributes + at).unwrap_or(0) != 0
}

/// The OVERLAPPED fields: Internal, InternalHigh, hEvent.
fn overlapped_fields(memory: &dyn GuestMemory, overlapped: u32) -> (u32, u32, u32) {
    let size = memory.ptr_size();
    (overlapped, overlapped + size, overlapped + 2 * size + 8)
}

/// What a pipe end has for a reader wanting `len` bytes: data, the end of
/// the pipe (`Err`), or nothing yet.
fn take(pipe: u64, side: usize, len: u32, peek: bool) -> Option<Result<Vec<u8>, u32>> {
    kobj::with(|k| {
        let p = k.pipes.get_mut(&pipe)?;
        let inbound = &mut p.inbound[side];
        if !inbound.is_empty() {
            let n = (len as usize).min(inbound.len());
            let bytes: Vec<u8> = if peek { inbound.iter().take(n).copied().collect() } else { inbound.drain(..n).collect() };
            return Some(Ok(bytes));
        }
        if p.closed[1 - side] || p.closed[side] {
            return Some(Err(ERROR_BROKEN_PIPE));
        }
        None
    })
}

fn pipe_end(ctx: &ApiContext, handle: u32) -> Option<(u64, usize)> {
    let pid = pid(ctx);
    kobj::with(|k| match k.get(pid, handle) {
        Some(Object::PipeEnd { pipe, side }) => Some((*pipe, *side)),
        _ => None,
    })
}

/// Queue a completion packet on a port of this process and wake a waiter.
fn post(ctx: &ApiContext, port: u32, bytes: u32, key: u64, overlapped: u64) {
    let mut shared = ctx.shared.borrow_mut();
    if let Some(queue) = shared.nt.ports.get_mut(&port) {
        queue.push_back((bytes, key, overlapped));
    }
    sync::set_event(&mut shared.sync, port);
}

/// Finish an overlapped operation: its status and byte count, its event,
/// and a packet for the handle's completion port.
fn complete(ctx: &ApiContext, memory: &mut dyn GuestMemory, handle: u32, overlapped: u32, status: u64, bytes: u32, at_once: bool) {
    if overlapped == 0 {
        return;
    }
    let (internal, high, event_at) = overlapped_fields(memory, overlapped);
    let _ = memory.write_ptr(internal, status);
    let _ = memory.write_ptr(high, bytes as u64);
    let event = memory.read_ptr(event_at).unwrap_or(0) as u32;
    if event & !1 != 0 {
        sync::set_event(&mut ctx.shared.borrow_mut().sync, event & !1);
    }
    // An event handle with its low bit set asks for no packet.
    if event & 1 != 0 {
        return;
    }
    let skip = at_once && status == STATUS_SUCCESS && ctx.shared.borrow().ipc.skip_on_success.contains(&handle);
    let port = ctx.shared.borrow().ipc.ports.get(&handle).copied();
    if let (Some((port, key)), false) = (port, skip) {
        post(ctx, port, bytes, key, overlapped as u64);
    }
}

/// Finish the overlapped operations whose data or peer has come. Returns
/// whether any finished.
pub fn poll<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) -> bool {
    if ctx.shared.borrow().ipc.pending.is_empty() {
        return false;
    }
    let pending = std::mem::take(&mut ctx.shared.borrow_mut().ipc.pending);
    let mut keep = Vec::new();
    let mut done = Vec::new();
    for op in pending {
        let Some((pipe, side)) = pipe_end(ctx, op.handle) else {
            done.push((op, STATUS_CANCELLED, Vec::new()));
            continue;
        };
        match op.op {
            Op::Connect => {
                if kobj::with(|k| k.pipes.get(&pipe).is_some_and(|p| p.connected)) {
                    done.push((op, STATUS_SUCCESS, Vec::new()));
                } else {
                    keep.push(op);
                }
            }
            Op::Read { len, .. } => match take(pipe, side, len, false) {
                Some(Ok(bytes)) => done.push((op, STATUS_SUCCESS, bytes)),
                Some(Err(_)) => done.push((op, STATUS_PIPE_BROKEN, Vec::new())),
                None => keep.push(op),
            },
        }
    }
    ctx.shared.borrow_mut().ipc.pending.extend(keep);
    let finished = !done.is_empty();
    let mut wide = Wide(memory);
    for (op, status, bytes) in done {
        if let Op::Read { buffer, .. } = op.op {
            let _ = wide.0.write_bytes(buffer, &bytes);
        }
        if trace(ctx) {
            eprintln!("[ipc {}] {:?} on {:#x} done: {status:#x}, {} bytes", pid(ctx), op.op, op.handle, bytes.len());
        }
        complete(ctx, &mut wide, op.handle, op.overlapped, status, bytes.len() as u32, false);
    }
    finished
}

/// A `dyn GuestMemory` over any guest memory.
struct Wide<'m, M: GuestMemory + ?Sized>(&'m mut M);

impl<M: GuestMemory + ?Sized> GuestMemory for Wide<'_, M> {
    fn read_bytes(&mut self, va: u32, len: usize) -> Option<Vec<u8>> {
        self.0.read_bytes(va, len)
    }
    fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Option<()> {
        self.0.write_bytes(va, bytes)
    }
    fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
        self.0.write_u32(va, value)
    }
    fn ptr_size(&self) -> u32 {
        self.0.ptr_size()
    }
}

/// Whether there are overlapped operations waiting.
pub fn has_pending(ctx: &ApiContext) -> bool {
    !ctx.shared.borrow().ipc.pending.is_empty()
}

/// Wait a moment for another process (the thread gives way to others).
fn nap(ctx: &ApiContext, memory: &mut dyn GuestMemory) {
    crate::win32::dispatch_with_memory(ctx, memory, "KERNEL32.dll", &crate::kernel::pe::ImportName::ByName("Sleep".into()), &[1], 0);
}

/// The process a process handle names.
fn process_of(ctx: &ApiContext, handle: u32) -> Option<u32> {
    if handle == u32::MAX {
        return Some(pid(ctx));
    }
    sync::process_id(ctx, handle)
}

fn cancel(ctx: &ApiContext, memory: &mut dyn GuestMemory, handle: u32, overlapped: Option<u32>) -> bool {
    let cancelled: Vec<Pending> = {
        let mut shared = ctx.shared.borrow_mut();
        let (gone, keep) = std::mem::take(&mut shared.ipc.pending)
            .into_iter()
            .partition(|p| p.handle == handle && overlapped.is_none_or(|o| o == p.overlapped));
        shared.ipc.pending = keep;
        gone
    };
    for op in &cancelled {
        complete(ctx, memory, op.handle, op.overlapped, STATUS_CANCELLED, 0, false);
    }
    !cancelled.is_empty()
}

/// The kernel's name for the type of object a handle names.
fn object_type(ctx: &ApiContext, handle: u32) -> &'static str {
    let me = pid(ctx);
    let shared = kobj::with(|k| match k.get(me, handle) {
        Some(Object::Section { .. }) => Some("Section"),
        Some(Object::PipeEnd { .. }) => Some("File"),
        Some(Object::Process { .. }) => Some("Process"),
        None => None,
    });
    if let Some(name) = shared {
        return name;
    }
    if handle == u32::MAX {
        return "Process";
    }
    if handle == u32::MAX - 1 {
        return "Thread";
    }
    let shared = ctx.shared.borrow();
    match shared.sync.objects.get(&handle) {
        Some(sync::Object::Event { .. }) => "Event",
        Some(sync::Object::Mutex { .. }) => "Mutant",
        Some(sync::Object::Semaphore { .. }) => "Semaphore",
        Some(sync::Object::Thread { .. }) => "Thread",
        Some(sync::Object::Process { .. }) => "Process",
        Some(sync::Object::Timer { .. }) => "Timer",
        None => "File",
    }
}

pub fn call(ctx: &ApiContext, memory: &mut dyn GuestMemory, name: &str, a: &[u64]) -> Option<ApiOutcome> {
    let count = arg_count(name)?;
    let arg = |i: usize| a.get(i).copied().unwrap_or(0);
    let p = |i: usize| arg(i) as u32;
    let ok = |value: u64| Some(ApiOutcome::wide(value, count));
    let fail = |code: u32, value: u64| {
        set_error(ctx, code);
        Some(ApiOutcome::wide(value, count))
    };
    let me = pid(ctx);
    let ours = |handle: u32| kobj::with(|k| k.owns(me, handle));
    match name {
        // ── Pipes ──
        "CreateNamedPipeW" => {
            // (name, open mode, pipe mode, instances, out size, in size, timeout, attributes)
            let Some(pipe_name) = pipe_name(memory, p(0)) else {
                return fail(123, u32::MAX as u64);
            };
            let first = p(1) & 0x8_0000 != 0;
            let handle = kobj::with(|k| {
                if first && k.pipe_exists(&pipe_name) {
                    return None;
                }
                let pipe = k.new_pipe(&pipe_name);
                Some(k.create(me, Object::PipeEnd { pipe, side: 0 }))
            });
            let Some(handle) = handle else {
                return fail(ERROR_ACCESS_DENIED, u32::MAX as u64);
            };
            if inherits(memory, p(7)) {
                kobj::with(|k| k.set_inheritable(me, handle, true));
            }
            if trace(ctx) {
                eprintln!("[ipc {me}] CreateNamedPipe {pipe_name} = {handle:#x}");
            }
            set_error(ctx, 0);
            ok(handle as u64)
        }
        "CreateFileW" => {
            // (name, access, share, attributes, disposition, flags, template)
            let pipe_name = pipe_name(memory, p(0))?;
            let handle = kobj::with(|k| {
                let pipe = k.listening_pipe(&pipe_name)?;
                k.pipes.get_mut(&pipe)?.connected = true;
                Some(k.create(me, Object::PipeEnd { pipe, side: 1 }))
            });
            let Some(handle) = handle else {
                let exists = kobj::with(|k| k.pipe_exists(&pipe_name));
                return fail(if exists { ERROR_PIPE_BUSY } else { ERROR_FILE_NOT_FOUND }, u32::MAX as u64);
            };
            if inherits(memory, p(3)) {
                kobj::with(|k| k.set_inheritable(me, handle, true));
            }
            if trace(ctx) {
                eprintln!("[ipc {me}] open pipe {pipe_name} = {handle:#x}");
            }
            set_error(ctx, 0);
            ok(handle as u64)
        }
        "ConnectNamedPipe" => {
            let (pipe, _) = pipe_end(ctx, p(0))?;
            let connected = kobj::with(|k| k.pipes.get(&pipe).is_some_and(|p| p.connected));
            if connected {
                return fail(ERROR_PIPE_CONNECTED, 0);
            }
            if p(1) != 0 {
                let (internal, _, _) = overlapped_fields(memory, p(1));
                let _ = memory.write_ptr(internal, STATUS_PENDING);
                ctx.shared.borrow_mut().ipc.pending.push(Pending { handle: p(0), overlapped: p(1), op: Op::Connect });
                return fail(ERROR_IO_PENDING, 0);
            }
            while !kobj::with(|k| k.pipes.get(&pipe).is_some_and(|p| p.connected)) {
                nap(ctx, memory);
            }
            ok(1)
        }
        "DisconnectNamedPipe" | "SetNamedPipeHandleState" => {
            pipe_end(ctx, p(0))?;
            ok(1)
        }
        "WaitNamedPipeW" => {
            let pipe_name = pipe_name(memory, p(0))?;
            let ready = kobj::with(|k| k.listening_pipe(&pipe_name).is_some());
            if ready {
                ok(1)
            } else {
                fail(ERROR_FILE_NOT_FOUND, 0)
            }
        }
        "GetNamedPipeInfo" => {
            pipe_end(ctx, p(0))?;
            for (i, value) in [(1, 0u32), (2, 0x1000), (3, 0x1000), (4, 1)] {
                if p(i) != 0 {
                    let _ = memory.write_u32(p(i), value);
                }
            }
            ok(1)
        }
        "PeekNamedPipe" => {
            // (pipe, buffer, size, &read, &available, &left)
            let (pipe, side) = pipe_end(ctx, p(0))?;
            let available = kobj::with(|k| k.pipes.get(&pipe).map(|p| p.inbound[side].len() as u32)).unwrap_or(0);
            let bytes = match take(pipe, side, p(2), true) {
                Some(Ok(bytes)) => bytes,
                Some(Err(code)) => return fail(code, 0),
                None => Vec::new(),
            };
            if p(1) != 0 {
                let _ = memory.write_bytes(p(1), &bytes);
            }
            for (i, value) in [(3, bytes.len() as u32), (4, available), (5, 0)] {
                if p(i) != 0 {
                    let _ = memory.write_u32(p(i), value);
                }
            }
            ok(1)
        }
        "GetFileType" => {
            let (_, _) = pipe_end(ctx, p(0))?;
            ok(3) // FILE_TYPE_PIPE
        }
        "ReadFile" => {
            // (handle, buffer, length, &read, overlapped)
            let (pipe, side) = pipe_end(ctx, p(0))?;
            loop {
                match take(pipe, side, p(2), false) {
                    Some(Ok(bytes)) => {
                        let _ = memory.write_bytes(p(1), &bytes);
                        if p(3) != 0 {
                            let _ = memory.write_u32(p(3), bytes.len() as u32);
                        }
                        complete(ctx, memory, p(0), p(4), STATUS_SUCCESS, bytes.len() as u32, true);
                        set_error(ctx, 0);
                        return ok(1);
                    }
                    Some(Err(code)) => {
                        if p(3) != 0 {
                            let _ = memory.write_u32(p(3), 0);
                        }
                        return fail(code, 0);
                    }
                    None if p(4) != 0 => {
                        let (internal, _, _) = overlapped_fields(memory, p(4));
                        let _ = memory.write_ptr(internal, STATUS_PENDING);
                        ctx.shared.borrow_mut().ipc.pending.push(Pending {
                            handle: p(0),
                            overlapped: p(4),
                            op: Op::Read { buffer: p(1), len: p(2) },
                        });
                        return fail(ERROR_IO_PENDING, 0);
                    }
                    None => nap(ctx, memory),
                }
            }
        }
        "WriteFile" => {
            // (handle, buffer, length, &written, overlapped)
            let (pipe, side) = pipe_end(ctx, p(0))?;
            let bytes = memory.read_bytes(p(1), p(2) as usize).unwrap_or_default();
            let written = kobj::with(|k| {
                let p = k.pipes.get_mut(&pipe)?;
                if p.closed[1 - side] {
                    return None;
                }
                p.inbound[1 - side].extend(bytes.iter().copied());
                Some(bytes.len() as u32)
            });
            let Some(written) = written else {
                return fail(ERROR_NO_DATA, 0);
            };
            if p(3) != 0 {
                let _ = memory.write_u32(p(3), written);
            }
            complete(ctx, memory, p(0), p(4), STATUS_SUCCESS, written, true);
            set_error(ctx, 0);
            ok(1)
        }
        "GetOverlappedResult" | "GetOverlappedResultEx" => {
            // (handle, overlapped, &bytes, wait | timeout[, alertable])
            if pipe_end(ctx, p(0)).is_none() {
                return None;
            }
            let (internal, high, _) = overlapped_fields(memory, p(1));
            let wait = if name == "GetOverlappedResult" { p(3) != 0 } else { p(3) != 0 };
            loop {
                poll(ctx, memory);
                let status = memory.read_ptr(internal).unwrap_or(0);
                if status != STATUS_PENDING {
                    let bytes = memory.read_ptr(high).unwrap_or(0) as u32;
                    if p(2) != 0 {
                        let _ = memory.write_u32(p(2), bytes);
                    }
                    return match status {
                        STATUS_SUCCESS => ok(1),
                        STATUS_CANCELLED => fail(ERROR_OPERATION_ABORTED, 0),
                        _ => fail(ERROR_BROKEN_PIPE, 0),
                    };
                }
                if !wait {
                    return fail(ERROR_IO_INCOMPLETE, 0);
                }
                nap(ctx, memory);
            }
        }
        "CancelIo" | "CancelIoEx" => {
            if pipe_end(ctx, p(0)).is_none() {
                return None;
            }
            let overlapped = (name == "CancelIoEx" && p(1) != 0).then_some(p(1));
            if cancel(ctx, memory, p(0), overlapped) {
                ok(1)
            } else {
                fail(1168, 0) // ERROR_NOT_FOUND
            }
        }
        // ── Completion ports ──
        "CreateIoCompletionPort" => {
            // (file, existing port, key, threads)
            if !ours(p(0)) {
                return None;
            }
            let port = if p(1) != 0 {
                p(1)
            } else {
                crate::win32::nt::call(ctx, memory, "CreateIoCompletionPort", &[u32::MAX as u64, 0, 0, arg(3)])?.value as u32
            };
            ctx.shared.borrow_mut().ipc.ports.insert(p(0), (port, arg(2)));
            ok(port as u64)
        }
        "SetFileCompletionNotificationModes" => {
            if !ours(p(0)) {
                return None;
            }
            let mut shared = ctx.shared.borrow_mut();
            if p(1) & 1 != 0 {
                shared.ipc.skip_on_success.insert(p(0));
            } else {
                shared.ipc.skip_on_success.remove(&p(0));
            }
            ok(1)
        }
        "GetQueuedCompletionStatus" => {
            // (port, &bytes, &key, &overlapped, timeout)
            poll(ctx, memory);
            let outcome = crate::win32::nt::call(ctx, memory, "GetQueuedCompletionStatus", a)?;
            let overlapped = memory.read_ptr(p(3)).unwrap_or(0) as u32;
            if outcome.value != 0 && overlapped != 0 {
                // A finished operation that failed.
                let status = memory.read_ptr(overlapped).unwrap_or(0);
                if status == STATUS_CANCELLED {
                    return fail(ERROR_OPERATION_ABORTED, 0);
                }
                if status != STATUS_SUCCESS && status != STATUS_PENDING && status & 0xC000_0000 == 0xC000_0000 {
                    return fail(ERROR_BROKEN_PIPE, 0);
                }
            }
            Some(ApiOutcome::wide(outcome.value as u64, count))
        }
        "GetQueuedCompletionStatusEx" => {
            // (port, entries, count, &removed, timeout, alertable):
            // OVERLAPPED_ENTRY is key, overlapped, internal, bytes.
            poll(ctx, memory);
            let size = memory.ptr_size();
            let scratch = ctx.shared.borrow_mut().heap.alloc(32).unwrap_or(0);
            let got = crate::win32::nt::call(ctx, memory, "GetQueuedCompletionStatus", &[arg(0), scratch as u64, (scratch + 8) as u64, (scratch + 16) as u64, arg(4)])?;
            let mut removed = 0u32;
            let mut first = got.value != 0 || memory.read_ptr(scratch + 16).unwrap_or(0) != 0;
            while first && removed < p(2) {
                let entry = p(1) + removed * (3 * size + 8);
                let bytes = memory.read_u32(scratch).unwrap_or(0);
                let key = memory.read_ptr(scratch + 8).unwrap_or(0);
                let overlapped = memory.read_ptr(scratch + 16).unwrap_or(0);
                let _ = memory.write_ptr(entry, key);
                let _ = memory.write_ptr(entry + size, overlapped);
                let _ = memory.write_ptr(entry + 2 * size, 0);
                let _ = memory.write_u32(entry + 3 * size, bytes);
                removed += 1;
                let more = ctx.shared.borrow().nt.ports.get(&p(0)).is_some_and(|q| !q.is_empty());
                if !more {
                    break;
                }
                let next = crate::win32::nt::call(ctx, memory, "GetQueuedCompletionStatus", &[arg(0), scratch as u64, (scratch + 8) as u64, (scratch + 16) as u64, 0])?;
                first = next.value != 0 || memory.read_ptr(scratch + 16).unwrap_or(0) != 0;
            }
            ctx.shared.borrow_mut().heap.free(scratch);
            if p(3) != 0 {
                let _ = memory.write_u32(p(3), removed);
            }
            if removed == 0 {
                return fail(258, 0); // WAIT_TIMEOUT
            }
            ok(1)
        }
        // ── Sections ──
        "CreateFileMappingW" => {
            // (file, attributes, protect, size high, size low, name)
            if p(0) != u32::MAX || memory.ptr_size() != 8 {
                return None;
            }
            let size = (arg(3) << 32) | arg(4) & 0xFFFF_FFFF;
            let section_name = (p(5) != 0).then(|| String::from_utf16_lossy(&crate::win32::unicode::read_wstr(memory, p(5))).to_ascii_lowercase());
            if let Some(existing) = section_name.as_deref().and_then(|n| kobj::with(|k| k.named(me, n))) {
                set_error(ctx, ERROR_ALREADY_EXISTS);
                return ok(existing as u64);
            }
            let Some(shared_memory) = crate::kernel::memory::SharedMemory::new(size as usize) else {
                return fail(8, 0);
            };
            let handle = kobj::with(|k| {
                let handle = k.create(me, Object::Section { memory: shared_memory, size });
                if let Some(n) = &section_name {
                    k.name(me, handle, n);
                }
                handle
            });
            if inherits(memory, p(1)) {
                kobj::with(|k| k.set_inheritable(me, handle, true));
            }
            if trace(ctx) {
                eprintln!("[ipc {me}] CreateFileMapping {size:#x} {section_name:?} = {handle:#x}");
            }
            set_error(ctx, 0);
            ok(handle as u64)
        }
        "OpenFileMappingW" => {
            // (access, inherit, name)
            let section_name = String::from_utf16_lossy(&crate::win32::unicode::read_wstr(memory, p(2))).to_ascii_lowercase();
            match kobj::with(|k| k.named(me, &section_name)) {
                Some(handle) => ok(handle as u64),
                None => None,
            }
        }
        "MapViewOfFile" | "MapViewOfFileEx" => {
            // (section, access, offset high, offset low, bytes[, address])
            let section = kobj::with(|k| match k.get(me, p(0)) {
                Some(Object::Section { memory, size }) => Some((memory.clone(), *size)),
                _ => None,
            })?;
            let (shared_memory, size) = section;
            let offset = (arg(2) << 32) | (arg(3) & 0xFFFF_FFFF);
            let length = if arg(4) != 0 { arg(4) } else { size.saturating_sub(offset) };
            let preferred = if name == "MapViewOfFileEx" && p(5) != 0 { p(5) } else { 0x2000_0000 };
            let Some(view) = memory.map_section(shared_memory, offset as usize, length as u32, preferred) else {
                return fail(8, 0);
            };
            if name == "MapViewOfFileEx" && p(5) != 0 && view != p(5) {
                memory.unmap_section(view);
                return fail(487, 0); // ERROR_INVALID_ADDRESS
            }
            ctx.shared.borrow_mut().ipc.views.insert(view);
            if trace(ctx) {
                eprintln!("[ipc {me}] MapViewOfFile {:#x} at {view:#x} ({length:#x} bytes)", p(0));
            }
            ok(view as u64)
        }
        "UnmapViewOfFile" | "UnmapViewOfFileEx" => {
            let view = p(0);
            if !ctx.shared.borrow_mut().ipc.views.remove(&view) {
                return None;
            }
            memory.unmap_section(view);
            ok(1)
        }
        // ── Handles ──
        "CloseHandle" => {
            if !ours(p(0)) {
                return None;
            }
            cancel(ctx, memory, p(0), None);
            {
                let mut shared = ctx.shared.borrow_mut();
                shared.ipc.ports.remove(&p(0));
                shared.ipc.skip_on_success.remove(&p(0));
            }
            kobj::with(|k| k.close(me, p(0)));
            if trace(ctx) {
                eprintln!("[ipc {me}] CloseHandle {:#x}", p(0));
            }
            ok(1)
        }
        "SetHandleInformation" => {
            // (handle, mask, flags): HANDLE_FLAG_INHERIT is 1.
            if !ours(p(0)) {
                return None;
            }
            if p(1) & 1 != 0 {
                kobj::with(|k| k.set_inheritable(me, p(0), p(2) & 1 != 0));
            }
            ok(1)
        }
        "GetHandleInformation" => {
            if !ours(p(0)) {
                return None;
            }
            let inherit = kobj::with(|k| k.inheritable(me).contains(&p(0)));
            let _ = memory.write_u32(p(1), u32::from(inherit));
            ok(1)
        }
        "DuplicateHandle" => {
            // (source process, handle, target process, &new, access, inherit, options)
            let source = process_of(ctx, p(0));
            let target = process_of(ctx, p(2));
            let (Some(source), Some(target)) = (source, target) else {
                return None;
            };
            let close_source = p(6) & 1 != 0;
            if kobj::with(|k| k.owns(source, p(1))) {
                let new = kobj::with(|k| {
                    let new = k.duplicate(source, p(1), target, None);
                    if let Some(new) = new {
                        if p(5) != 0 {
                            k.set_inheritable(target, new, true);
                        }
                    }
                    if close_source {
                        k.close(source, p(1));
                    }
                    new
                });
                if trace(ctx) {
                    eprintln!("[ipc {me}] DuplicateHandle {:#x} of {source} into {target} = {new:x?}", p(1));
                }
                if p(3) != 0 {
                    let _ = memory.write_ptr(p(3), new.unwrap_or(0) as u64);
                }
                return ok(u64::from(new.is_some()));
            }
            if source == me && target == me {
                return None;
            }
            if source == me && p(1) == u32::MAX {
                // This process, for another: a handle to it there.
                let new = kobj::with(|k| k.process_handle(target, me));
                if p(3) != 0 {
                    let _ = memory.write_ptr(p(3), new as u64);
                }
                return ok(1);
            }
            eprintln!("[ipc {me}] DuplicateHandle {:#x} of {source} into {target}: not shareable", p(1));
            if source == me && close_source {
                return None;
            }
            fail(ERROR_INVALID_HANDLE, 0)
        }
        // ── Processes ──
        "OpenProcess" => {
            // (access, inherit, id)
            let wanted = p(2);
            if wanted == me {
                return None;
            }
            if wanted != 1 && !kobj::known_process(wanted) {
                return fail(87, 0);
            }
            ok(sync::open_process(ctx, wanted) as u64)
        }
        "GetProcessId" => ok(process_of(ctx, p(0)).unwrap_or(0) as u64),
        "NtQuerySection" => {
            // (section, class, buffer, length, &returned):
            // SECTION_BASIC_INFORMATION is base, attributes, size.
            let size = kobj::with(|k| match k.get(me, p(0)) {
                Some(Object::Section { size, .. }) => Some(*size),
                _ => None,
            })?;
            if p(1) != 0 || p(3) < 24 {
                return ok(0xC000_0003); // STATUS_INVALID_INFO_CLASS
            }
            let _ = memory.write_u64(p(2), 0);
            let _ = memory.write_u64(p(2) + 8, 0x800_0000); // SEC_COMMIT
            let _ = memory.write_u64(p(2) + 16, size);
            if p(4) != 0 {
                let _ = memory.write_u32(p(4), 24);
            }
            ok(0)
        }
        "NtQueryObject" => {
            // (handle, class, buffer, length, &returned): the type name
            // (ObjectTypeInformation) only.
            if p(1) != 2 {
                return None;
            }
            let type_name = object_type(ctx, p(0));
            let units: Vec<u16> = type_name.encode_utf16().collect();
            let size = memory.ptr_size();
            let header = 2 * size + 22 * 4;
            let needed = header + 2 * (units.len() as u32 + 1);
            if p(4) != 0 {
                let _ = memory.write_u32(p(4), needed);
            }
            if p(3) < needed {
                return ok(0xC000_0004); // STATUS_INFO_LENGTH_MISMATCH
            }
            let _ = memory.write_bytes(p(2), &vec![0u8; header as usize]);
            let length = 2 * units.len() as u16;
            let _ = memory.write_bytes(p(2), &[length.to_le_bytes(), (length + 2).to_le_bytes()].concat());
            let _ = memory.write_ptr(p(2) + size, (p(2) + header) as u64);
            crate::win32::unicode::write_wide(memory, p(2) + header, &units, true);
            ok(0)
        }
        "TerminateProcess" => {
            let target = process_of(ctx, p(0))?;
            if target == me {
                return None;
            }
            kobj::with(|k| {
                if let Some(process) = k.processes.get_mut(&target) {
                    process.terminated = true;
                    process.exit.get_or_insert(p(1));
                }
            });
            ok(1)
        }
        _ => None,
    }
}
