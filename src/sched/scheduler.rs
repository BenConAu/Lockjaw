use crate::mm::addr::{PhysAddr, PhysPage};
use crate::mm::kernel_ptr::{KernelMut, KernelRef};
use crate::mm::page_alloc;
use crate::sched::tcb::Tcb;
use crate::sched::context::context_switch;
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use lockjaw_types::scheduler::{
    SchedDecision, SchedReason, SchedState, SchedThreadState,
};
use lockjaw_types::object::HandleTableHeader;
use lockjaw_types::process::ProcessLifecycle;

/// Slot capacity for the scheduler's TCB-pointer table. Imported
/// from `lockjaw_types::scheduler::MAX_THREADS` so the pure
/// scheduler model and the kernel's concrete storage stay in sync
/// by construction — single source of truth, no drift.
use lockjaw_types::scheduler::MAX_THREADS;

// ---------------------------------------------------------------------------
// Scheduler observability counters
// ---------------------------------------------------------------------------
// AtomicU32 (not UnsafeCell) — clean bridge into SMP. Relaxed ordering
// is sufficient for observability-only counters on single-core.

static CONTEXT_SWITCH_COUNT: AtomicU32 = AtomicU32::new(0);
static TTBR0_WRITE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Read the scheduler counters: (context_switches, ttbr0_writes).
pub fn scheduler_stats() -> (u32, u32) {
    (
        CONTEXT_SWITCH_COUNT.load(Ordering::Relaxed),
        TTBR0_WRITE_COUNT.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// Pending exit — typed record for deferred thread cleanup
// ---------------------------------------------------------------------------

/// Captures the identity of an exited thread for deferred cleanup.
/// Stored after exit_current() and consumed by finish_exit() on the
/// next schedule() call from a different thread.
struct PendingExit {
    thread_idx: usize,
    tcb_kva: lockjaw_types::addr::KernelVa,
}

/// Per-CPU cleanup slots for exited threads. Each CPU has its own slot
/// so two CPUs can have independent pending exits. A CPU only drains
/// its own slot in finish_exit() — no cross-CPU cleanup.
/// Invariant: a CPU's slot must be None before exit_current() stores
/// a new value. finish_exit() drains the slot at the start of every
/// schedule() call.
struct PendingExitSlots(UnsafeCell<[Option<PendingExit>; MAX_CPUS]>);
/// SAFETY: GKL held during all access. Per-CPU indexing prevents
/// cross-CPU slot corruption even without the lock.
unsafe impl Sync for PendingExitSlots {}

use lockjaw_types::scheduler::MAX_CPUS;

static PENDING_EXITS: PendingExitSlots = PendingExitSlots(UnsafeCell::new(
    [None, None, None, None]
));

/// Per-CPU scratch slot for `context_switch`'s `old_sp_ptr` argument when
/// an idle CPU (no current TCB to save into) switches into a thread via
/// `schedule_from_idle`.
///
/// **Write-only by contract.** The asm in
/// [`crate::sched::context::context_switch`] stores to `*old_sp_ptr`
/// then loads from `*new_sp_ptr`; it never reads the old slot back, and
/// has no failure path that "returns to the old SP." So `IDLE_SP[cpu]`
/// captures the abandoned IRQ-frame SP for that one switch and is never
/// resumed-into.
///
/// On the next entry into `idle_wait` (after the picked thread blocks
/// or exits and yields the CPU again), the path goes:
/// IRQ → SAVE_REGS on boot stack (SP_EL1 = boot stack top) → gkl_lock
/// → tick → schedule_from_idle. The fresh boot-stack SP overwrites
/// the orphaned frame; one frame (~256 bytes ExceptionContext) of
/// unrecoverable boot-stack space per idle-wake. Per-CPU guard pages
/// catch pathological growth.
///
/// See `src/sched/gkl.rs` Path 5 for the GKL transfer reasoning.
struct IdleSpSlots(UnsafeCell<[u64; MAX_CPUS]>);
/// SAFETY: per-CPU indexed slot, GKL held during all access. Write-only:
/// callers must never read these back (see contract above).
unsafe impl Sync for IdleSpSlots {}

static IDLE_SP: IdleSpSlots = IdleSpSlots(UnsafeCell::new([0; MAX_CPUS]));

// ---------------------------------------------------------------------------
// Scheduler singleton — wraps all mutable globals behind safe methods
// ---------------------------------------------------------------------------

/// The kernel scheduler. Wraps three pieces of global mutable state
/// (the abstract SchedState model, the TCB-paddr table, and the
/// active flag) in a single struct with safe methods. Internal access
/// uses `UnsafeCell`; the SAFETY justification for `impl Sync` lives
/// in one place rather than at every call site.
pub struct Scheduler {
    state: UnsafeCell<SchedState>,
    /// TCB addresses for each scheduled thread. TCBs live in the
    /// KVM pool, addressed through KernelVa. See kernel-vmem-roadmap.md.
    threads: UnsafeCell<[Option<lockjaw_types::addr::KernelVa>; MAX_THREADS]>,
    active: UnsafeCell<bool>,
}

/// SAFETY: All scheduler state access is serialized by the Giant Kernel
/// Lock (GKL). The GKL is held during exception handlers and kernel
/// thread execution. Per-CPU fields (current_per_cpu, PENDING_EXITS)
/// are additionally partitioned by cpu_id.
unsafe impl Sync for Scheduler {}

impl Scheduler {
    const fn new() -> Self {
        Scheduler {
            state: UnsafeCell::new(SchedState::new_const()),
            threads: UnsafeCell::new([None; MAX_THREADS]),
            active: UnsafeCell::new(false),
        }
    }

    /// Raw pointer to the scheduler state. Callers must create `&mut`
    /// references in scoped blocks that never overlap — the Rust
    /// aliasing model forbids two `&mut` to the same UnsafeCell even
    /// on single-core. Never return `&mut` from a `&self` method.
    fn state_ptr(&self) -> *mut SchedState {
        self.state.get()
    }

    fn threads_ptr(&self) -> *mut [Option<lockjaw_types::addr::KernelVa>; MAX_THREADS] {
        self.threads.get()
    }
}

pub static SCHEDULER: Scheduler = Scheduler::new();

// ---------------------------------------------------------------------------
// Public API — safe wrappers around SCHEDULER methods
// ---------------------------------------------------------------------------
// Callers keep using `scheduler::add_thread(p)` — the only change is
// they no longer need an `unsafe { }` block around the call.

/// Register a thread in the run queue.
///
/// The first thread registered (index 0) is the CPU 0 boot TCB
/// (later becomes init) — its saved_sp will be filled on the first
/// context switch away from it, and its state is marked Running
/// (since it's already executing). Subsequent threads are added as Ready.
/// Returns `false` if the run queue is full (every `MAX_THREADS`
/// slot occupied). Caller maps to `SyscallError::QUEUE_FULL`, not
/// OUT_OF_MEMORY — exhaustion of the scheduler's slot table is a
/// distinct failure class from physical-memory OOM.
pub fn add_thread(tcb_kva: lockjaw_types::addr::KernelVa) -> bool {
    // SAFETY: GKL held — exclusive access to state + threads.
    unsafe {
        let state = &mut *SCHEDULER.state_ptr();
        let idx = match state.add_thread() {
            Some(i) => i,
            None => return false,
        };
        // First add_thread is the boot thread on CPU 0. add_thread sets
        // it Ready; set_initial_current transitions Ready → Running and
        // assigns it as CPU 0's current.
        if idx == 0 {
            state.set_initial_current(0, idx);
        }
        if idx >= MAX_THREADS {
            return false;
        }
        (*SCHEDULER.threads_ptr())[idx] = Some(tcb_kva);
        true
    }
}

/// Read-only check: would the next `add_thread` succeed?
///
/// Used by `sys_create_process` as a precondition during the validate
/// phase of the new transactional restructure: if the run queue is
/// full, the caller returns an error before any destructive consume
/// work runs in the apply phase. The answer is stable across the
/// remainder of the syscall because GKL is held throughout — no other
/// thread can mutate the run queue between has_room() and add_thread().
pub fn has_room() -> bool {
    // SAFETY: GKL held — exclusive access to state.
    unsafe {
        let state = &*SCHEDULER.state_ptr();
        state.has_free_slot()
    }
}

/// Read-only walk over every registered TCB. Used by handle revocation
/// to enumerate live processes whose handle tables may hold a handle
/// to the object being revoked.
///
/// `f` is called once per Some(tcb_kva) slot in the run queue.
/// Slots may share a process (multiple threads of one process); the
/// caller is responsible for any deduplication needed. GKL must be
/// held — the run queue must not change between the walk and any
/// follow-up action keyed on the visited TCBs.
pub fn for_each_tcb(mut f: impl FnMut(lockjaw_types::addr::KernelVa)) {
    // SAFETY: GKL held — read-only access to threads array.
    unsafe {
        let threads = &*SCHEDULER.threads_ptr();
        for i in 0..MAX_THREADS {
            if let Some(kva) = threads[i] {
                f(kva);
            }
        }
    }
}

/// Activate the scheduler. After this call, timer ticks trigger scheduling.
/// Must be called after all initial threads are registered via add_thread().
pub fn start() {
    // SAFETY: called once during boot, before secondaries are active.
    unsafe { *SCHEDULER.active.get() = true; }
}

/// Called from the timer tick handler every TIMER_TICK_MS milliseconds.
/// Uses the round-robin model (from lockjaw-types) to pick the next Ready
/// thread, swaps TTBR0 if needed, checks the stack canary, and performs
/// the context switch.
///
/// If the current thread is Blocked (e.g., sitting in block_current's
/// wfi loop), a Preempt step is invalid — there's nothing to preempt.
/// We return and let the block_current loop re-check on its next
/// iteration. IPC unblock operations (from IRQ handlers that signal
/// notifications or deliver messages) set other threads to Ready, and
/// the block_current loop picks them up via step(Block).
pub fn tick() {
    // SAFETY: GKL held — exclusive access to scheduler state.
    unsafe {
        let cpu_id = crate::percpu::cpu_id() as usize;
        // Read scheduler snapshot then drop the borrow before
        // dispatching: schedule() and schedule_from_idle() both
        // re-borrow state mutably.
        let (active, thread_count, current_opt, current_state) = {
            let state = &*SCHEDULER.state_ptr();
            let active = *SCHEDULER.active.get();
            let tc = state.thread_count();
            let cur = state.try_current_for(cpu_id);
            let cs = cur.and_then(|idx| state.get(idx));
            (active, tc, cur, cs)
        };
        if !active || thread_count < 2 {
            return;
        }
        // Idle CPU (no current thread) — route through the no-current
        // scheduler path. If no Ready thread exists, schedule_from_idle
        // is a no-op; idle_wait re-enters wfi on return from this IRQ.
        if current_opt.is_none() {
            schedule_from_idle(cpu_id);
            return;
        }
        if current_state == Some(SchedThreadState::Exited) {
            // ExitAndHalt case: current thread exited but no Ready thread
            // was available. An interrupt may have just unblocked a thread.
            // Use Block reason — step(Block) accepts Exited as current.
            schedule(SchedReason::Block);
            return;
        }
        // Can only Preempt a Running thread. If current is Blocked (idle
        // loop in block_current), the timer tick has no work — return and
        // the block_current loop will re-evaluate.
        if current_state != Some(SchedThreadState::Running) {
            return;
        }
        // schedule() performs context_switch (asm) — inherently unsafe.
        schedule(SchedReason::Preempt);
    }
}

/// Return the KVA of the currently running thread's TCB.
/// TCBs live in the KVM pool (kernel-vmem-roadmap.md). Used by
/// syscall handlers to look up the caller's handle table and TTBR0.
///
/// **Precondition**: this CPU has a current thread. Holds in syscall
/// handlers (the caller's thread is current), IRQ handlers entered from
/// a running thread (preempted thread is current), and any kernel path
/// executing on a CPU it was scheduled onto. Idle CPUs (no thread
/// assigned, parked in `idle_wait` — added in the scheduler refactor)
/// MUST NOT reach here; use `try_current_tcb_kva` if a caller might
/// run in a no-current context.
///
/// Panics on a missing current — treat as a contract violation, not a
/// runtime condition. All current callers (syscall handlers, IPC paths,
/// process.rs, exception/data-abort handler) were audited during the
/// scheduler refactor plan and satisfy the precondition.
pub fn current_tcb_kva() -> lockjaw_types::addr::KernelVa {
    // SAFETY: GKL held — exclusive access to scheduler state.
    unsafe {
        let cpu_id = crate::percpu::cpu_id() as usize;
        let idx = (*SCHEDULER.state_ptr()).current_for(cpu_id);
        (*SCHEDULER.threads_ptr())[idx]
            .unwrap_or_else(|| panic!("current_tcb_kva: no TCB for current thread"))
    }
}

/// Return the index of the currently running thread in the run queue.
/// Used by crash diagnostics — must never panic (called from panic handler).
pub fn current_thread_index() -> usize {
    // SAFETY: raw pointer reads for crash-robustness (no bounds checks).
    unsafe {
        let cpu_id = crate::percpu::cpu_id() as usize;
        // SAFETY: UnsafeCell interior pointer for crash-safe read
        let state_ptr = SCHEDULER.state.get() as *const SchedState;
        match (*state_ptr).current_per_cpu.get(cpu_id) {
            Some(Some(idx)) => *idx,
            _ => 0,
        }
    }
}

/// Like current_tcb_kva but returns None instead of panicking.
/// Safe to call from the panic handler without risk of re-entrant panic.
/// Uses raw pointer reads to avoid bounds-check panics.
pub fn try_current_tcb_kva() -> Option<lockjaw_types::addr::KernelVa> {
    // SAFETY: raw pointer reads for crash-robustness (no bounds checks).
    unsafe {
        let cpu_id = crate::percpu::cpu_id() as usize;
        // SAFETY: UnsafeCell interior pointer for crash-safe volatile read
        let state_ptr = SCHEDULER.state.get() as *const SchedState;
        let idx = match (*state_ptr).current_per_cpu.get(cpu_id) {
            Some(Some(i)) => *i,
            _ => return None,
        };
        if idx >= MAX_THREADS { return None; }
        // SAFETY: raw pointer to UnsafeCell interior for crash-safe volatile read
        let threads_ptr = SCHEDULER.threads.get()
            as *const [Option<lockjaw_types::addr::KernelVa>; MAX_THREADS];
        // SAFETY: raw pointer to array element — avoids slice bounds check
        let ptr = (threads_ptr as *const Option<lockjaw_types::addr::KernelVa>).add(idx);
        core::ptr::read_volatile(ptr)
    }
}

/// Block the current thread and schedule away.
/// Loops until another thread context-switches back to us.
/// If no thread is Ready, halts via wfi until an interrupt wakes something.
/// Saves/restores the DAIF mask around wfi to preserve the kernel's
/// "single-threaded during syscall" invariant.
// ---------------------------------------------------------------------------
// BlockToken — compile-time enforcement of scoped borrows before blocking
// ---------------------------------------------------------------------------

/// Zero-sized token that enforces the scoped-borrow discipline for blocking
/// IPC paths. All temporary `&mut T` references to shared kernel objects
/// must be derived via `scoped_mut(ptr, &mut token)`, which borrows the
/// token. `block_current(token)` consumes the token by value. Rust's borrow
/// checker prevents moving the token while it is borrowed, so any `&mut T`
/// derived from it must be dropped before `block_current` can be called.
///
/// This turns the "no &mut references alive across block_current" invariant
/// from a comment into a compiler error.
pub struct BlockToken(());

impl BlockToken {
    /// Create a new token at the start of a potentially blocking path.
    pub fn new() -> Self {
        BlockToken(())
    }
}

/// Derive a scoped `&mut T` from a raw pointer, tied to the BlockToken's
/// lifetime. The returned reference borrows the token, so the token cannot
/// be moved (consumed by `block_current`) until the reference is dropped.
///
/// # Safety
/// `ptr` must point to a live, properly-initialized `T` in kernel memory.
#[inline]
pub unsafe fn scoped_mut<'a, T>(ptr: *mut T, _token: &'a mut BlockToken) -> &'a mut T {
    &mut *ptr
}

pub fn block_current(_token: BlockToken) {
    let cpu_id = crate::percpu::cpu_id() as usize;
    // SAFETY: GKL held — exclusive access to scheduler state.
    // We re-derive references from the raw pointer after each context
    // switch because schedule() invalidates prior references (the
    // context switch may have mutated scheduler state on another path).
    unsafe {
        (&mut *SCHEDULER.state_ptr()).block_current(cpu_id)
            .unwrap_or_else(|_| panic!("block_current: not Running"));
    }
    // schedule() performs context_switch — when we resume here, another
    // thread ran and eventually switched back to us. If we're Running
    // again, an unblock_thread + schedule decided to pick us, so we
    // return to the caller. Otherwise we loop and wait again.
    loop {
        unsafe { schedule(SchedReason::Block); }
        unsafe {
            let state = &*SCHEDULER.state_ptr();
            let current = state.current_for(cpu_id);
            if state.get(current) == Some(SchedThreadState::Running) {
                return;
            }
        }
        // All blocked. Release GKL + unmask IRQs so other cores can run
        // and this core can receive the interrupt that will unblock a
        // thread. Re-mask + re-acquire after waking.
        crate::sched::gkl::gkl_unlock();
        unsafe {
            core::arch::asm!(
                "msr DAIFClr, #2",         // Unmask IRQ so wfi can wake
                "wfi",                      // Halt until an IRQ arrives
                "msr DAIFSet, #2",         // Re-mask IRQ
            );
        }
        crate::sched::gkl::gkl_lock();
    }
}

/// Unblock a thread by setting its state to Ready.
/// The thread will be picked up by the next scheduling round.
/// Typically called from IPC endpoint code when a partner arrives.
/// Takes a TCB paddr (what IPC code has) and looks up the thread index.
///
/// Panics if the TCB is not registered (kernel bug — IPC code should
/// only unblock threads it knows about) or if the thread is not in
/// the Blocked state (kernel bug — only Blocked threads should be
/// unblocked, and the IPC state machine shouldn't have them otherwise).
pub fn unblock_thread(tcb_kva: lockjaw_types::addr::KernelVa) {
    let idx = thread_index_for(tcb_kva)
        .unwrap_or_else(|| panic!("unblock_thread: TCB KVA not registered in scheduler"));
    // SAFETY: GKL held — exclusive access to scheduler state.
    unsafe {
        (&mut *SCHEDULER.state_ptr()).unblock(idx)
            .unwrap_or_else(|_| panic!("unblock_thread: thread not in Blocked state"));
    }
}

/// Wake every Blocked thread whose `wait_deadline` has expired
/// relative to `now` (a CNTVCT_EL0 reading).
///
/// Called from the timer tick handler before `scheduler::tick()`
/// so that a just-expired sleeper participates in the same tick's
/// scheduling decision (see `handle_tick` for the ordering
/// rationale).
///
/// O(N) walk over the run queue under the GKL. N is bounded by
/// `MAX_THREADS` (currently 1024). Each visited TCB is two loads
/// (deadline + scheduler state) and a compare; if a future workload
/// pushes wake latency or contention, switch to a sorted per-CPU
/// deadline heap (out of scope today).
///
/// Three guards each block a class of bug:
/// - `is_no_deadline` skips TCBs with no outstanding sleep, so the
///   page-zeroed default for never-slept threads (NO_DEADLINE) is
///   correctly ignored.
/// - `has_expired` skips not-yet-due deadlines.
/// - `state.get(idx) == Blocked` skips threads that have already
///   been woken by an IPC unblock but whose syscall return path
///   hasn't yet cleared `wait_deadline` (a brief Running window
///   with a stale deadline). Without this guard, calling
///   `unblock` on a Ready/Running thread would panic.
///
/// We clear `wait_deadline` to NO_DEADLINE before unblocking so a
/// repeat scan on a later tick (before the woken thread's syscall
/// return path runs) cannot try to unblock it again.
pub fn wake_expired_deadlines(now: lockjaw_types::time::MonoTicks) {
    use lockjaw_types::scheduler::SchedThreadState;
    use lockjaw_types::time::MonoTicks;
    use lockjaw_types::thread::Tcb;

    // SAFETY: GKL held during timer tick — exclusive access to scheduler
    // state and to every live TCB (single-core mutation per CPU; the
    // tick walk on this CPU does not race other CPUs' tick walks because
    // each holds the GKL while running).
    unsafe {
        let threads = &*SCHEDULER.threads_ptr();
        let state = &mut *SCHEDULER.state_ptr();
        for idx in 0..MAX_THREADS {
            let Some(kva) = threads[idx] else { continue };
            // SAFETY: live KVM-mapped TCB; GKL held — no concurrent mutation. (See module doc above.)
            let tcb_ptr = kva.as_u64() as *mut Tcb;
            let deadline = MonoTicks((*tcb_ptr).wait_deadline);
            if deadline.is_no_deadline() { continue; }
            if !deadline.has_expired(now) { continue; }
            // Only Blocked threads can be unblocked. A Ready/Running
            // thread with a stale deadline got woken by IPC and is
            // racing the syscall return path; let it clear itself.
            if state.get(idx) != Some(SchedThreadState::Blocked) { continue; }
            // Clear deadline first so a re-scan can't double-unblock.
            (*tcb_ptr).wait_deadline = MonoTicks::NO_DEADLINE.0;
            state.unblock(idx).unwrap_or_else(|_| {
                panic!("wake_expired_deadlines: unblock failed for idx {}", idx)
            });
        }
    }
}

/// Exit the current thread permanently. Never returns.
///
/// 1. Asserts no pending exit (cleanup slot empty)
/// 2. Calls step(Exit) which transitions Running → Exited and picks next
/// 3. Stores PendingExit record for deferred cleanup by finish_exit()
/// 4. Context-switches away, then loops in wfi forever
pub fn exit_current() -> ! {
    // SAFETY: GKL held — exclusive access to scheduler state.
    unsafe {
        // Clean up any previous exit on this CPU before proceeding.
        // This handles the case where two threads exit on the same CPU
        // without an intervening schedule() (which normally calls finish_exit).
        finish_exit();

        let cpu_id = crate::percpu::cpu_id() as usize;
        // Invariant: this CPU's pending exit slot must be empty
        assert!(
            (*PENDING_EXITS.0.get())[cpu_id].is_none(),
            "exit_current: previous exit not yet cleaned up"
        );
        let state = &mut *SCHEDULER.state_ptr();
        let decision = state.step(cpu_id, SchedReason::Exit);

        match decision {
            SchedDecision::ExitAndSwitch { exited, next } => {
                let tcb_kva = (*SCHEDULER.threads_ptr())[exited].unwrap();
                (*PENDING_EXITS.0.get())[cpu_id] = Some(PendingExit { thread_idx: exited, tcb_kva });

                let new_paddr = (*SCHEDULER.threads_ptr())[next].unwrap();
                let new_tcb = KernelMut::<Tcb>::from_kva(new_paddr);
                let new_ttbr0 = crate::cap::process_obj::process_ttbr0(
                    lockjaw_types::addr::KernelVa::new(new_tcb.get().process_kva)
                );

                // Swap TTBR0 if the new thread has a user address space
                if new_ttbr0 != 0 {
                    core::arch::asm!(
                        "msr TTBR0_EL1, {val}",
                        "dsb ish",
                        "tlbi vmalle1is",
                        "dsb ish",
                        "isb",
                        val = in(reg) new_ttbr0,
                    );
                }

                // We don't need to save our old SP properly since we
                // will never be switched back to. But context_switch
                // writes it anyway (harmless — the TCB page will be freed).
                let old_paddr = (*SCHEDULER.threads_ptr())[exited].unwrap();
                let mut old_tcb = KernelMut::<Tcb>::from_kva(old_paddr);
                // SAFETY: old_tcb is live; field reference is unique (exiting thread)
                let old_sp_ptr = &mut old_tcb.get_mut().saved_sp as *mut u64;
                // SAFETY: new_tcb is live; shared field reference
                let new_sp_ptr = &new_tcb.get().saved_sp as *const u64;
                context_switch(old_sp_ptr, new_sp_ptr);

                // If we somehow get here, something is very wrong.
                // The exited thread should never be scheduled again.
                unreachable!("exit_current: exited thread resumed");
            }
            SchedDecision::ExitAndHalt { exited } => {
                // No Ready thread right now, but Blocked threads may be
                // woken by interrupts. Store PENDING_EXIT and loop like
                // block_current: wfi until an interrupt unblocks a thread,
                // then schedule(Block) picks it up. finish_exit() at the
                // start of schedule() will clean up this thread's resources
                // once we switch to the newly-Ready thread.
                let tcb_kva = (*SCHEDULER.threads_ptr())[exited].unwrap();
                (*PENDING_EXITS.0.get())[cpu_id] = Some(PendingExit { thread_idx: exited, tcb_kva });

                loop {
                    schedule(SchedReason::Block);
                    // Release GKL + unmask IRQs around wfi (same as
                    // block_current). Re-acquire after waking.
                    crate::sched::gkl::gkl_unlock();
                    core::arch::asm!(
                        "msr DAIFClr, #2",
                        "wfi",
                        "msr DAIFSet, #2",
                    );
                    crate::sched::gkl::gkl_lock();
                }
            }
            _ => unreachable!("step(Exit) returned unexpected decision"),
        }
    }
}

/// Clean up the most recently exited thread's resources.
/// Called at the start of schedule() before any state transitions.
/// Common path (no pending exit) is a single pointer read.
///
/// Cleanup order (documented explicitly):
///   1. Read all TCB fields we need before freeing anything
///   2. Remove from scheduler (prevents re-selection during cleanup)
///   3. Free handle table page(s) (extent from ObjectHeader.page_count)
///   4. Free kernel stack page
///   5. Free TCB page (last — we were reading from it)
fn finish_exit() {
    let cpu_id = crate::percpu::cpu_id() as usize;
    // SAFETY: GKL held — exclusive access. Only drain this CPU's slot.
    let pending = unsafe { &(*PENDING_EXITS.0.get())[cpu_id] };
    let pending = match pending {
        Some(p) => p,
        None => return, // common path — no pending exit on this CPU
    };

    // Cannot clean up if the exited thread is still current on this CPU
    // (ExitAndHalt path — haven't context-switched to a different thread).
    //
    // Bootstrap-only safety on the None branch: before any thread has
    // run on this CPU, current_per_cpu[cpu] is None — but in that window
    // a pending-exit slot can't exist (no thread has been scheduled to
    // exit), so a None current here is unreachable for the duration of
    // this refactor's scope. Treating None as "skip" is correct now;
    // if a future refactor makes None a true steady-state idle marker,
    // finish_exit must drain pending exits on the now-idle CPU (cross-
    // CPU drain or lazy-drain on next schedule). See
    // docs/plans/scheduler-refactor.md Risk #2.
    let current_idx = match unsafe { (*SCHEDULER.state_ptr()).try_current_for(cpu_id) } {
        Some(idx) => idx,
        None => return,
    };
    if pending.thread_idx == current_idx {
        return;
    }

    // Safe to proceed — take ownership of this CPU's pending record.
    let pending = unsafe { (*PENDING_EXITS.0.get())[cpu_id].take().unwrap() };

    // Step 1: Read everything we need from the exiting TCB
    let tcb = unsafe { KernelRef::<Tcb>::from_kva(pending.tcb_kva) };
    let stack_base = tcb.get().stack_base;
    let process_kva = lockjaw_types::addr::KernelVa::new(tcb.get().process_kva);

    // Step 2: Remove from scheduler array + model
    unsafe {
        (&mut *SCHEDULER.state_ptr()).remove_thread(pending.thread_idx)
            .unwrap_or_else(|_| panic!("finish_exit: remove_thread failed"));
        (*SCHEDULER.threads_ptr())[pending.thread_idx] = None;
    }

    // Step 3: Free per-thread resources (kernel stack, TCB).
    // Stack regime determines the free path:
    // - Pool: KVM-allocated dynamic stack — free via kvm.
    // - Image: linker-reserved boot stack — never freed (only the
    //   CPU 0 boot TCB uses Image and it becomes init; exiting init
    //   would mean exiting the kernel). Reaching this branch is an
    //   invariant violation.
    // SAFETY: stack and tcb pages came from prior allocations at
    // thread create time; finish_exit holds the GKL and no live
    // references into either page exist by this point.
    use lockjaw_types::thread::KernelStackBase;
    let stack_kva = match stack_base {
        KernelStackBase::Pool(kva) => kva,
        KernelStackBase::Image(_) => {
            panic!("finish_exit: boot TCB exited (Image-region stack must not be freed)");
        }
    };
    unsafe {
        crate::mm::kvm::free_kernel_pages(
            crate::mm::kvm::OwnedKvmRange { kva: stack_kva, pages: 1 }
        );
        crate::mm::kvm::free_kernel_pages(
            crate::mm::kvm::OwnedKvmRange { kva: pending.tcb_kva, pages: 1 }
        );
    }

    // Step 4: Decrement process thread count via narrow op + pure model
    let lifecycle = crate::cap::process_obj::process_dec_thread_count(process_kva);
    let mut pages_freed = 2u32; // stack + TCB

    match lifecycle {
        ProcessLifecycle::LastThread => {
            // Last thread — teardown plan owns the sequence.
            use lockjaw_types::process::{TeardownStep, build_teardown_plan};
            use lockjaw_types::object::{CloseHandleResult, decide_close_handle};

            let ttbr0 = crate::cap::process_obj::process_ttbr0(process_kva);
            let ht_kva = crate::cap::process_obj::process_handle_table(process_kva);
            let ht_page_count = if ht_kva.as_u64() != 0 {
                let ht = unsafe { KernelRef::<HandleTableHeader>::from_kva(ht_kva) };
                ht.get().header.page_count as u8
            } else {
                0
            };

            let plan = build_teardown_plan(
                crate::cap::process_obj::process_owned_page_count(process_kva),
                ttbr0 != 0,
                ht_kva.as_u64() != 0,
                ht_page_count,
            );

            for step in plan.iter() {
                match step {
                    TeardownStep::FreeOwnedPages { count } => {
                        for i in 0..*count as usize {
                            if let Some(paddr) = crate::cap::process_obj::process_owned_page(process_kva, i) {
                                page_alloc::dealloc_page(PhysPage::containing(PhysAddr::new(paddr)));
                                pages_freed += 1;
                            }
                        }
                    }
                    TeardownStep::FreeAddressSpace => {
                        unsafe { crate::arch::aarch64::vmem::free_address_space(PhysAddr::new(ttbr0)); }
                        pages_freed += 3; // approximate — L3 count varies
                    }
                    TeardownStep::CleanupHandleEntriesPtesGone => {
                        // PTEs already freed by FreeAddressSpace.
                        let ht = unsafe {
                            crate::cap::handle_table::HandleTableRef::from_kva(ht_kva)
                        };
                        ht.for_each_entry(|entry| {
                            match decide_close_handle(Some(entry)) {
                                CloseHandleResult::RemoveAndDecRef { header_kva } => {
                                    crate::cap::pageset_table::dec_refcount_and_maybe_free(header_kva);
                                }
                                CloseHandleResult::UnmapThenRemove { header_kva, .. } => {
                                    // PTEs already gone — skip unmap, dec both.
                                    crate::cap::pageset_table::dec_both_and_maybe_free(header_kva);
                                }
                                _ => {}
                            }
                        });
                    }
                    TeardownStep::CleanupHandleEntriesNoAddressSpace => {
                        // Kernel process: narrower decision type,
                        // no unmap variant exists.
                        use lockjaw_types::object::{TeardownHandleAction, decide_teardown_handle};
                        let ht = unsafe {
                            crate::cap::handle_table::HandleTableRef::from_kva(ht_kva)
                        };
                        ht.for_each_entry(|entry| {
                            match decide_teardown_handle(entry) {
                                TeardownHandleAction::DecRef { header_kva } => {
                                    crate::cap::pageset_table::dec_refcount_and_maybe_free(header_kva);
                                }
                                TeardownHandleAction::Skip => {}
                            }
                        });
                    }
                    TeardownStep::FreeHandleTable { page_count } => {
                        // HandleTable lives in the KVM pool; tear down
                        // the KVA range, returning the backing frames
                        // to page_alloc and the VA to the KVM free list.
                        // SAFETY: ht_kva came from a prior
                        // kvm::alloc_kernel_pages(N) at process create
                        // time; no live references into the pages now.
                        unsafe {
                            crate::mm::kvm::free_kernel_pages(
                                crate::mm::kvm::OwnedKvmRange { kva: ht_kva, pages: *page_count as usize }
                            );
                        }
                        pages_freed += *page_count as u32;
                    }
                    TeardownStep::FreeProcessPage => {
                        // ProcessObject lives in the KVM pool; tear down
                        // the KVA range, returning the backing frame to
                        // page_alloc and the VA to the KVM free list.
                        // SAFETY: process_kva came from a prior
                        // kvm::alloc_kernel_pages(1) at process create
                        // time; no live KernelMut/Ref into the page now.
                        unsafe {
                            crate::mm::kvm::free_kernel_pages(
                                crate::mm::kvm::OwnedKvmRange { kva: process_kva, pages: 1 }
                            );
                        }
                        pages_freed += 1;
                    }
                }
            }
        }
        ProcessLifecycle::ThreadsRemaining(_) | ProcessLifecycle::Immortal(_) => {
            // Process stays alive — other threads still running, or immortal
        }
    }

    crate::kprintln!("[EXIT] Thread ", pending.thread_idx, " cleaned up (", pages_freed, " pages freed)");
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

fn thread_index_for(kva: lockjaw_types::addr::KernelVa) -> Option<usize> {
    // SAFETY: GKL held — read-only access.
    let threads = unsafe { &*SCHEDULER.threads_ptr() };
    for i in 0..MAX_THREADS {
        if threads[i] == Some(kva) {
            return Some(i);
        }
    }
    None
}

/// The actual context-switch path. Unsafe because it calls the asm
/// context_switch trampoline and touches hardware registers (TTBR0).
unsafe fn schedule(reason: SchedReason) {
    // Clean up any previously exited thread before doing anything else.
    finish_exit();

    let cpu_id = crate::percpu::cpu_id() as usize;
    let state = &mut *SCHEDULER.state_ptr();
    // Schedule is called from syscall handlers, voluntary yield, and the
    // tick path — all entered via a currently-running thread. An idle
    // CPU (no current thread, parked in `idle_wait`) must NOT call
    // schedule(); it routes through `schedule_from_idle` instead. The
    // panic message is the contract — no caller triggers it today.
    let old_idx = match state.try_current_for(cpu_id) {
        Some(idx) => idx,
        None => panic!(
            "schedule called with no current thread — idle CPUs must use schedule_from_idle"
        ),
    };
    let old_paddr = (*SCHEDULER.threads_ptr())[old_idx].unwrap();
    let mut old_tcb = KernelMut::<Tcb>::from_kva(old_paddr);

    let decision = state.step(cpu_id, reason);

    let next_idx = match decision {
        SchedDecision::SwitchTo(idx) => idx,
        SchedDecision::StayOnCurrent | SchedDecision::WaitForInterrupt => {
            // No context switch needed. State has already been validated
            // and (for StayOnCurrent/WaitForInterrupt) not mutated.
            return;
        }
        SchedDecision::ExitAndSwitch { .. } | SchedDecision::ExitAndHalt { .. } => {
            // Exit decisions are handled by exit_current(), not schedule().
            unreachable!("schedule() should never see Exit decisions");
        }
    };

    let new_paddr = (*SCHEDULER.threads_ptr())[next_idx].unwrap();
    let new_tcb = KernelMut::<Tcb>::from_kva(new_paddr);

    // Check stack canary of the thread we're switching away from
    check_thread_canary(old_tcb.get());

    // Swap TTBR0 if the new thread is in a different process.
    // TTBR0 is irrelevant during kernel execution (all kernel code
    // accessed via TTBR1), so swapping before context_switch is safe —
    // when the new thread eventually erets to EL0, TTBR0 is already set.
    // Same-process switches skip the TLB flush (threads share address space).
    let old_process = old_tcb.get().process_kva;
    let new_process = new_tcb.get().process_kva;
    let new_ttbr0 = crate::cap::process_obj::process_ttbr0(lockjaw_types::addr::KernelVa::new(new_process));
    if new_ttbr0 != 0 && new_process != old_process {
        TTBR0_WRITE_COUNT.fetch_add(1, Ordering::Relaxed);
        core::arch::asm!(
            "msr TTBR0_EL1, {val}",           // Install new process page table
            "dsb ish",                          // Ensure TTBR0 write completes
            "tlbi vmalle1is",                   // Flush TLB (all entries, inner shareable)
            "dsb ish",                          // Ensure TLB flush completes
            "isb",                              // Sync pipeline
            val = in(reg) new_ttbr0,
        );
    }

    CONTEXT_SWITCH_COUNT.fetch_add(1, Ordering::Relaxed);

    // Context switch: save old SP, load new SP, swap callee-saved regs.
    // context_switch is extern "C" and takes raw pointers; coerce the
    // typed references here, with single SAFETY annotations.
    // SAFETY: old_tcb is live; field reference is unique for the duration
    let old_sp_ptr = &mut old_tcb.get_mut().saved_sp as *mut u64;
    // SAFETY: new_tcb is live; shared field reference
    let new_sp_ptr = &new_tcb.get().saved_sp as *const u64;
    context_switch(old_sp_ptr, new_sp_ptr);
}

/// Check the stack canary for a thread.
fn check_thread_canary(tcb: &Tcb) {
    // Tcb guarantees stack_base points at the base of a kernel-owned
    // page whose first u64 is the canary. The regime (Image vs Pool)
    // doesn't affect the dereference; both variants wrap a valid VA.
    // SAFETY: stack_base points at a valid kernel-owned stack page.
    let canary_ptr = tcb.stack_base.as_u64() as *const u64;
    let value = unsafe { ptr::read_volatile(canary_ptr) };
    if value != lockjaw_types::constants::STACK_CANARY {
        use crate::print::{KPrint, Addr};
        let uart = crate::arch::aarch64::uart::Uart::new();
        uart.puts("[PANIC] Thread stack canary corrupted!\n");
        uart.puts("  Expected: ");
        KPrint::kprint(&Addr(lockjaw_types::constants::STACK_CANARY));
        uart.puts("\n  Got:      ");
        KPrint::kprint(&Addr(value));
        uart.puts("\n");
        panic!("thread stack canary corrupted");
    }
}

// ---------------------------------------------------------------------------
// Scheduler-refactor Stage 2: idle-CPU primitives (dead code, no callers yet)
// ---------------------------------------------------------------------------
//
// These primitives let the kernel handle a CPU with no current thread:
// the CPU parks in `idle_wait` (kernel-owned wfi loop on its boot stack)
// and the tick handler calls `schedule_from_idle` to pick up work via
// the pure model's `step_from_idle`. Stage 3 wires the live callers.

/// Schedule from an idle CPU — counterpart to `schedule()` for the case
/// where `current_per_cpu[cpu_id]` is `None`.
///
/// Drains any pending exit on this CPU (no-op when idle, since
/// `PendingExitSlots` requires a Running current to be populated — see
/// the bootstrap-only-safety note on `finish_exit`), then calls
/// `step_from_idle` to pick a Ready thread. If one is found, performs
/// the TTBR0 swap (if the new thread's process is non-kernel) and
/// `context_switch`es into it via `IDLE_SP[cpu_id]` as the save-side
/// pointer. If no Ready thread exists, returns (caller re-enters wfi).
///
/// # Safety
/// GKL must be held. Must be called only when this CPU has no current
/// thread (idle). Asserts.
unsafe fn schedule_from_idle(cpu_id: usize) {
    // Drain any pending exit first — matches the schedule() prologue
    // ordering. No-op on idle CPU today (see `finish_exit` note).
    finish_exit();

    let state = &mut *SCHEDULER.state_ptr();
    assert!(
        state.try_current_for(cpu_id).is_none(),
        "schedule_from_idle: CPU {} already has a current thread — use schedule() instead",
        cpu_id
    );

    let decision = match state.step_from_idle(cpu_id) {
        Some(d) => d,
        None => return, // No work — caller re-enters idle_wait's wfi.
    };

    let next_idx = match decision {
        SchedDecision::SwitchTo(idx) => idx,
        // `step_from_idle` only returns SwitchTo or None today; any
        // other variant is a model bug.
        _ => unreachable!("step_from_idle returned non-SwitchTo decision"),
    };

    let new_paddr = (*SCHEDULER.threads_ptr())[next_idx].unwrap();
    let new_tcb = KernelMut::<Tcb>::from_kva(new_paddr);

    // TTBR0 swap. No old process to compare against — always swap when
    // the new thread's process is non-kernel.
    let new_process = new_tcb.get().process_kva;
    let new_ttbr0 = crate::cap::process_obj::process_ttbr0(
        lockjaw_types::addr::KernelVa::new(new_process),
    );
    if new_ttbr0 != 0 {
        TTBR0_WRITE_COUNT.fetch_add(1, Ordering::Relaxed);
        core::arch::asm!(
            "msr TTBR0_EL1, {val}",
            "dsb ish",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            val = in(reg) new_ttbr0,
        );
    }

    CONTEXT_SWITCH_COUNT.fetch_add(1, Ordering::Relaxed);

    // SAFETY: IDLE_SP[cpu_id] is per-CPU scratch (see IdleSpSlots doc);
    // GKL held. The store is the only access — never read back.
    let old_sp_ptr = (IDLE_SP.0.get() as *mut u64).add(cpu_id);
    // SAFETY: new_tcb is live; shared field reference.
    let new_sp_ptr = &new_tcb.get().saved_sp as *const u64;
    context_switch(old_sp_ptr, new_sp_ptr);
}

/// Park this CPU in a kernel-owned wfi loop. The CPU has no current
/// thread (`current_per_cpu[cpu_id] == None`); ticks will eventually
/// pick up work via `schedule_from_idle`.
///
/// Caller must NOT hold the GKL — wfi blocks IRQ delivery if DAIF.I is
/// set, and even with IRQs unmasked, holding the GKL across wfi would
/// deadlock the next tick (which acquires GKL on entry). Secondaries
/// boot fresh from PSCI without the GKL; this is the only legal caller
/// today.
///
/// # Safety
/// GKL must NOT be held. IRQs must be in any state — this unmasks them.
pub fn idle_wait(_cpu_id: usize) -> ! {
    // SAFETY: unmasking IRQs is required for the wfi loop to be
    // interruptible. The caller's contract is "no GKL held."
    unsafe { core::arch::asm!("msr DAIFClr, #2") };
    loop {
        // SAFETY: wfi is a hint instruction; safe in any context with
        // IRQs unmasked.
        unsafe { core::arch::asm!("wfi") };
    }
}
