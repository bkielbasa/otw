//! Tasks: what runs on the CPUs. Each task has a kernel stack of its own;
//! a CPU switches to another task when the one it runs waits (`sleep_us`),
//! gives way (`yield_now`) or ends, and every CPU takes tasks from one
//! queue. A program's threads are tasks; so is the desktop (task 0, the
//! first CPU's start), and each CPU has an idle task that halts it when
//! there is nothing to do.
//!
//! Switches happen only in ordinary kernel code (never inside an interrupt
//! handler): the timer makes a program's code give way by returning to the
//! kernel (see `user`), which then calls `yield_now`.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::apic::{self, MAX_CPUS};
use crate::sync::SpinLock;

const STACK_SIZE: usize = 512 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Running,
    Ready,
    Sleeping,
    Dead,
}

pub struct Task {
    pub id: u64,
    /// The kernel stack pointer while switched out.
    rsp: u64,
    /// Its kernel stack (none for a CPU's own start), freed with it.
    #[allow(dead_code)]
    stack: Option<Box<[u8]>>,
    state: State,
    /// When a sleeping task is due (0: at once; set by other CPUs too).
    wake_at: AtomicU64,
    start: Option<Box<dyn FnOnce()>>,
    /// The innermost user-mode entry's kernel frame (`user`).
    pub entry_rsp: u64,
    /// The program's address space and thread block while it runs program
    /// code (0: the kernel's).
    pub cr3: u64,
    pub fs_base: u64,
    pub gs_base: u64,
    /// 32-bit program code: FS holds a data selector.
    pub compat: bool,
    /// What kernel work it does, for OTW_PROFILE (`profile::tag`; 0: none).
    pub tag: usize,
}

/// Tasks are handed between CPUs: only one CPU uses one at a time.
#[derive(Clone, Copy)]
struct TaskPtr(*mut Task);
unsafe impl Send for TaskPtr {}

static READY: SpinLock<VecDeque<TaskPtr>> = SpinLock::new(VecDeque::new());
static SLEEPING: SpinLock<Vec<TaskPtr>> = SpinLock::new(Vec::new());
static CURRENT: [AtomicPtr<Task>; MAX_CPUS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS];
static IDLE: [AtomicPtr<Task>; MAX_CPUS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS];
/// The task a CPU just switched away from, to queue (or free) once off its
/// stack.
static PREVIOUS: [AtomicPtr<Task>; MAX_CPUS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS];
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

// otw_switch(save: *mut u64 /* rdi */, load: u64 /* rsi */): save the
// callee-saved registers and the stack pointer, continue on the other
// stack.
global_asm!(
    ".global otw_switch",
    "otw_switch:",
    "push rbx", "push rbp", "push r12", "push r13", "push r14", "push r15",
    "mov [rdi], rsp",
    "mov rsp, rsi",
    "pop r15", "pop r14", "pop r13", "pop r12", "pop rbp", "pop rbx",
    "ret",
    "",
    // A new task starts here, its Task in r12.
    ".global otw_task_entry",
    "otw_task_entry:",
    "mov rdi, r12",
    "call {main}",
    "ud2",
    main = sym task_main,
);

extern "sysv64" {
    fn otw_switch(save: *mut u64, load: u64);
    fn otw_task_entry();
}

fn cpu() -> usize {
    apic::cpu_index().min(MAX_CPUS - 1)
}

fn new_task(stack: Option<Box<[u8]>>) -> *mut Task {
    Box::into_raw(Box::new(Task {
        id: NEXT_ID.fetch_add(1, Ordering::SeqCst),
        rsp: 0,
        stack,
        state: State::Running,
        wake_at: AtomicU64::new(0),
        start: None,
        entry_rsp: 0,
        cr3: 0,
        fs_base: 0,
        gs_base: 0,
        compat: false,
        tag: 0,
    }))
}

/// The running task.
pub fn current() -> &'static mut Task {
    unsafe { &mut *CURRENT[cpu()].load(Ordering::SeqCst) }
}

pub fn current_id() -> u64 {
    let task = CURRENT[cpu()].load(Ordering::SeqCst);
    if task.is_null() {
        0
    } else {
        unsafe { (*task).id }
    }
}

/// For a profile sample, from an interrupt: whether this CPU is idle, its
/// task's id, tag and address space.
pub fn sample_current() -> (bool, u64, usize, u64) {
    let index = cpu();
    let task = CURRENT[index].load(Ordering::Relaxed);
    if task.is_null() || task == IDLE[index].load(Ordering::Relaxed) {
        return (true, 0, 0, 0);
    }
    unsafe { (false, (*task).id, (*task).tag, (*task).cr3) }
}

/// Tag the running task's kernel work (OTW_PROFILE) until the guard goes.
pub fn tagged(tag: usize) -> Tagged {
    let task = current();
    let previous = core::mem::replace(&mut task.tag, tag);
    Tagged(previous)
}

pub struct Tagged(usize);

impl Drop for Tagged {
    fn drop(&mut self) {
        current().tag = self.0;
    }
}

/// This CPU's start becomes a task (the first CPU's: the desktop), and it
/// gets an idle task.
pub fn init_this_cpu(idle_loop_here: bool) {
    let index = cpu();
    let me = new_task(None);
    CURRENT[index].store(me, Ordering::SeqCst);
    if idle_loop_here {
        IDLE[index].store(me, Ordering::SeqCst);
    } else {
        let idle = spawn_task(Box::new(|| idle_loop()), false);
        IDLE[index].store(idle, Ordering::SeqCst);
    }
}

/// A task that runs `start` (queued if `ready`).
fn spawn_task(start: Box<dyn FnOnce()>, ready: bool) -> *mut Task {
    let stack = alloc::vec![0u8; STACK_SIZE].into_boxed_slice();
    let top = (stack.as_ptr() as u64 + STACK_SIZE as u64) & !15;
    let task = new_task(Some(stack));
    unsafe {
        // What otw_switch pops: r15, r14, r13, r12 (the task), rbp, rbx,
        // then returns to the entry; the call there leaves the stack
        // aligned as the ABI wants.
        let frame = (top - 8 * 8) as *mut u64;
        let values = [0, 0, 0, task as u64, 0, 0, otw_task_entry as *const () as u64, 0];
        for (i, value) in values.iter().enumerate() {
            *frame.add(i) = *value;
        }
        (*task).rsp = frame as u64;
        (*task).start = Some(start);
        (*task).state = State::Ready;
    }
    if ready {
        READY.lock().push_back(TaskPtr(task));
    }
    task
}

/// Start `f` as a task of its own; returns its id.
pub fn spawn(f: impl FnOnce() + 'static) -> u64 {
    let task = spawn_task(Box::new(f), true);
    wake_others();
    unsafe { (*task).id }
}

/// Wake the other CPUs (halted ones look for ready tasks).
fn wake_others() {
    let me = cpu();
    for index in (0..apic::cpu_count()).filter(|&i| i != me) {
        if let Some(id) = apic::apic_id_of(index) {
            apic::send_ipi(id, crate::arch::WAKE_VECTOR as u32);
        }
    }
}

extern "sysv64" fn task_main(task: *mut Task) -> ! {
    finish_switch();
    crate::arch::enable_interrupts();
    let start = unsafe { (*task).start.take() };
    if let Some(start) = start {
        start();
    }
    exit()
}

/// After a switch, on the new stack: queue the task switched away from
/// (or put it to sleep, or free it).
fn finish_switch() {
    let previous = PREVIOUS[cpu()].swap(core::ptr::null_mut(), Ordering::SeqCst);
    if previous.is_null() {
        return;
    }
    let state = unsafe { (*previous).state };
    match state {
        State::Ready => READY.lock().push_back(TaskPtr(previous)),
        State::Sleeping => {
            // Woken (or due) while it was still switching away: ready now.
            if unsafe { (*previous).wake_at.load(Ordering::SeqCst) } <= apic::now_us() {
                forget_waits(&[TaskPtr(previous)]);
                unsafe { (*previous).state = State::Ready };
                READY.lock().push_back(TaskPtr(previous));
            } else {
                SLEEPING.lock().push(TaskPtr(previous));
            }
        }
        State::Dead => drop(unsafe { Box::from_raw(previous) }),
        State::Running => {}
    }
}

/// Switch to `next`, leaving the current task in `state`.
fn switch_to(next: *mut Task, state: State) {
    let index = cpu();
    let me = CURRENT[index].load(Ordering::SeqCst);
    if me == next {
        unsafe { (*me).state = State::Running };
        return;
    }
    let idle = IDLE[index].load(Ordering::SeqCst);
    unsafe {
        (*me).state = if me == idle && state != State::Dead { State::Running } else { state };
        // The idle task is never queued: each CPU switches back to its own.
        let queue_me = me != idle;
        PREVIOUS[index].store(if queue_me { me } else { core::ptr::null_mut() }, Ordering::SeqCst);
        (*next).state = State::Running;
        CURRENT[index].store(next, Ordering::SeqCst);
        crate::arch::disable_interrupts();
        // The next task's address space (the kernel's for a task without a
        // program): a CPU must not stay in a program's tables, which go
        // when the program ends.
        let wanted = if (*next).cr3 != 0 { (*next).cr3 } else { crate::paging::page_table_root() };
        let current: u64;
        core::arch::asm!("mov {}, cr3", out(reg) current);
        if current & !0xFFF != wanted {
            core::arch::asm!("mov cr3, {}", in(reg) wanted);
        }
        otw_switch(&mut (*me).rsp, (*next).rsp);
    }
    // Back on this task (possibly on another CPU).
    finish_switch();
    crate::arch::enable_interrupts();
}

fn next_ready() -> Option<*mut Task> {
    READY.lock().pop_front().map(|t| t.0)
}

/// Let another ready task run (if there is one); this one stays ready.
pub fn yield_now() {
    wake_sleepers();
    if let Some(next) = next_ready() {
        switch_to(next, State::Ready);
    } else {
        core::hint::spin_loop();
    }
}

/// Whether tasks are waiting for a CPU.
pub fn others_ready() -> bool {
    !READY.lock().is_empty()
}

/// Sleep for `us` microseconds, letting other tasks run.
pub fn sleep_us(us: u64) {
    let index = cpu();
    let me = CURRENT[index].load(Ordering::SeqCst);
    if me.is_null() || me == IDLE[index].load(Ordering::SeqCst) {
        crate::apic::delay_us(us);
        return;
    }
    unsafe { (*me).wake_at.store(apic::now_us().saturating_add(us), Ordering::SeqCst) };
    let next = next_ready().unwrap_or(IDLE[index].load(Ordering::SeqCst));
    switch_to(next, State::Sleeping);
}

/// Tasks blocked in `wait_on`, by the address of the word they wait on.
static WAITERS: SpinLock<Vec<(usize, TaskPtr)>> = SpinLock::new(Vec::new());
/// CPUs halted for want of work (a bit each).
static HALTED: AtomicU64 = AtomicU64::new(0);

/// Block while `word` holds `expected`, until `wake(word)` or `timeout_us`
/// passes. May return early.
pub fn wait_on(word: &core::sync::atomic::AtomicU32, expected: u32, timeout_us: u64) {
    let index = cpu();
    let me = CURRENT[index].load(Ordering::SeqCst);
    let key = word as *const _ as usize;
    if me.is_null() || me == IDLE[index].load(Ordering::SeqCst) {
        // Not a task that can sleep: look again and again, a while.
        let start = apic::now_us();
        while word.load(Ordering::SeqCst) == expected && apic::now_us() - start < timeout_us.min(1000) {
            core::hint::spin_loop();
        }
        return;
    }
    {
        let mut waiters = WAITERS.lock();
        if word.load(Ordering::SeqCst) != expected {
            return;
        }
        unsafe { (*me).wake_at.store(apic::now_us().saturating_add(timeout_us), Ordering::SeqCst) };
        waiters.push((key, TaskPtr(me)));
    }
    let next = next_ready().unwrap_or(IDLE[index].load(Ordering::SeqCst));
    switch_to(next, State::Sleeping);
    // Woken, or the time is up: no longer waiting.
    let mut waiters = WAITERS.lock();
    if let Some(i) = waiters.iter().position(|(_, t)| t.0 == me) {
        waiters.remove(i);
    }
}

/// Wake one task blocked on `word` (the one waiting longest), or all.
/// (Tasks whose wait timed out are no longer listed: the wake goes to
/// one still waiting.)
pub fn wake(word: &core::sync::atomic::AtomicU32, all: bool) {
    let key = word as *const _ as usize;
    let mut woke = false;
    {
        let mut waiters = WAITERS.lock();
        if waiters.is_empty() {
            return;
        }
        waiters.retain(|&(k, t)| {
            if k == key && (all || !woke) {
                unsafe { (*t.0).wake_at.store(0, Ordering::SeqCst) };
                woke = true;
                false
            } else {
                true
            }
        });
    }
    if woke {
        wake_sleepers();
        wake_a_halted_cpu();
    }
}

/// Tasks woken by time: no longer waiting on a word (a wake must go to
/// one still waiting).
fn forget_waits(tasks: &[TaskPtr]) {
    let mut waiters = WAITERS.lock();
    if waiters.is_empty() {
        return;
    }
    waiters.retain(|(_, t)| !tasks.iter().any(|w| w.0 == t.0));
}

/// A task became ready: a halted CPU (if any) should take it now rather
/// than at its next timer interrupt.
fn wake_a_halted_cpu() {
    let halted = HALTED.load(Ordering::SeqCst) & !(1 << cpu());
    if halted == 0 {
        return;
    }
    let index = halted.trailing_zeros() as usize;
    if let Some(id) = apic::apic_id_of(index) {
        apic::send_ipi(id, crate::arch::WAKE_VECTOR as u32);
    }
}

/// End the running task.
pub fn exit() -> ! {
    let index = cpu();
    let next = next_ready().unwrap_or(IDLE[index].load(Ordering::SeqCst));
    switch_to(next, State::Dead);
    unreachable!("a dead task ran again")
}

fn wake_sleepers() {
    let now = apic::now_us();
    let mut sleeping = SLEEPING.lock();
    if sleeping.is_empty() {
        return;
    }
    let mut woken = Vec::new();
    sleeping.retain(|t| {
        let due = unsafe { (*t.0).wake_at.load(Ordering::SeqCst) <= now };
        if due {
            woken.push(*t);
        }
        !due
    });
    drop(sleeping);
    forget_waits(&woken);
    let mut ready = READY.lock();
    for t in woken {
        unsafe { (*t.0).state = State::Ready };
        ready.push_back(t);
    }
}

/// What a CPU does when no task wants it: wake sleepers, run what is
/// ready, else halt until the next interrupt (the timer, 1000 times a
/// second, or another CPU).
pub fn idle_loop() -> ! {
    let bit = 1u64 << cpu();
    loop {
        wake_sleepers();
        match next_ready() {
            Some(next) => switch_to(next, State::Running),
            None => {
                // Halted (until an interrupt), and known to be: a CPU that
                // readies a task wakes it. Interrupts stay off from the last
                // look at the queue to the halt, so a wake is not missed.
                crate::arch::disable_interrupts();
                HALTED.fetch_or(bit, Ordering::SeqCst);
                if READY.lock().is_empty() {
                    crate::arch::wait_for_interrupt();
                } else {
                    crate::arch::enable_interrupts();
                }
                HALTED.fetch_and(!bit, Ordering::SeqCst);
            }
        }
    }
}
