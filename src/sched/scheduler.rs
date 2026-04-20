use crate::mm::addr::{PhysAddr, PhysPage, KERNEL_VA_OFFSET};
use crate::mm::kernel_ptr::{KernelMut, KernelRef};
use crate::mm::page_alloc;
use crate::sched::tcb::Tcb;
use crate::sched::context::context_switch;
use core::cell::UnsafeCell;
use core::ptr;
use lockjaw_types::scheduler::{
    SchedDecision, SchedReason, SchedState, SchedThreadState,
};
use lockjaw_types::object::HandleTableHeader;
use lockjaw_types::process::ProcessLifecycle;

const MAX_THREADS: usize = 8;

// ---------------------------------------------------------------------------
// Pending exit — typed record for deferred thread cleanup
// ---------------------------------------------------------------------------

/// Captures the identity of an exited thread for deferred cleanup.
/// Stored after exit_current() and consumed by finish_exit() on the
/// next schedule() call from a different thread.
struct PendingExit {
    thread_idx: usize,
    tcb_paddr: PhysAddr,
}

/// Cleanup slot for the most recently exited thread.
/// Invariant: must be None before exit_current() stores a new value.
/// finish_exit() drains this at the start of every schedule() call.
struct PendingExitSlot(UnsafeCell<Option<PendingExit>>);
/// SAFETY: single-core, IRQs masked during kernel execution.
unsafe impl Sync for PendingExitSlot {}

static PENDING_EXIT: PendingExitSlot = PendingExitSlot(UnsafeCell::new(None));

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
    threads: UnsafeCell<[Option<PhysAddr>; MAX_THREADS]>,
    active: UnsafeCell<bool>,
}

/// SAFETY: single-core kernel. Kernel entry (exception vectors) masks
/// IRQs before touching scheduler state; the only preemption point is
/// the timer tick handler, which calls `tick()` — a leaf that does not
/// re-enter any scheduler method. When SMP lands, replace this with a
/// proper SpinMutex.
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

    fn threads_ptr(&self) -> *mut [Option<PhysAddr>; MAX_THREADS] {
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
/// The first thread registered (index 0) is the idle/boot thread — its
/// saved_sp will be filled on the first context switch away from it, and
/// its state is marked Running (since it's already executing).
/// Subsequent threads are added as Ready.
/// Returns `false` if the run queue is full (MAX_THREADS = 8 reached).
pub fn add_thread(tcb_paddr: PhysAddr) -> bool {
    // SAFETY: single-core, IRQs masked — exclusive access to state + threads.
    unsafe {
        let state = &mut *SCHEDULER.state_ptr();
        let idx = match state.add_thread() {
            Some(i) => i,
            None => return false,
        };
        // First add_thread is the boot/init thread — model puts it in Ready
        // but it's actually already running. Mark it Running.
        if idx == 0 {
            state.mark_initial_running();
        }
        if idx >= MAX_THREADS {
            return false;
        }
        (*SCHEDULER.threads_ptr())[idx] = Some(tcb_paddr);
        true
    }
}

/// Activate the scheduler. After this call, timer ticks trigger scheduling.
/// Must be called after all initial threads are registered via add_thread().
pub fn start() {
    // SAFETY: single-core, called once during boot.
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
    // SAFETY: single-core, IRQs masked — exclusive access.
    unsafe {
        let active = *SCHEDULER.active.get();
        let state = &*SCHEDULER.state_ptr();
        if !active || state.thread_count() < 2 {
            return;
        }
        let current_state = state.get(state.current);
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

/// Return the physical address of the currently running thread's TCB.
/// Used by syscall handlers to look up the caller's handle table and TTBR0.
pub fn current_tcb_paddr() -> PhysAddr {
    // SAFETY: single-core, IRQs masked — read-only access, no aliasing.
    unsafe {
        let idx = (*SCHEDULER.state_ptr()).current;
        (*SCHEDULER.threads_ptr())[idx].unwrap()
    }
}

/// Return the index of the currently running thread in the run queue.
/// Used by crash diagnostics to identify the faulting thread.
pub fn current_thread_index() -> usize {
    // SAFETY: single-core, IRQs masked — read-only access.
    unsafe { (*SCHEDULER.state_ptr()).current }
}

/// Like current_tcb_paddr but returns None instead of panicking.
/// Safe to call from the panic handler without risk of re-entrant panic.
/// Uses raw pointer reads to avoid bounds-check panics.
pub fn try_current_tcb_paddr() -> Option<PhysAddr> {
    // SAFETY: raw pointer reads for crash-robustness (no bounds checks).
    unsafe {
        let state_ptr = SCHEDULER.state.get() as *const SchedState;
        let idx = (*state_ptr).current;
        if idx >= MAX_THREADS { return None; }
        // SAFETY: raw pointer to UnsafeCell interior for crash-safe volatile read
        let threads_ptr = SCHEDULER.threads.get() as *const [Option<PhysAddr>; MAX_THREADS];
        // SAFETY: raw pointer to array element — avoids slice bounds check
        let ptr = (threads_ptr as *const Option<PhysAddr>).add(idx);
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
    // SAFETY: single-core, IRQs masked. Each &mut borrow is scoped to
    // avoid overlapping — we re-derive from the raw pointer after each
    // context switch because schedule() invalidates prior references.
    unsafe {
        (&mut *SCHEDULER.state_ptr()).block_current().expect("block_current: not Running");
    }
    loop {
        // schedule() performs context_switch (asm) — inherently unsafe.
        unsafe { schedule(SchedReason::Block); }
        // Re-read: schedule may have switched us out and back in.
        // If we're Running again, an unblock_thread + schedule decided
        // to pick us, so we resume.
        unsafe {
            let state = &*SCHEDULER.state_ptr();
            if state.get(state.current) == Some(SchedThreadState::Running) {
                return;
            }
        }
        // All blocked. Wait for an IRQ, preserving DAIF around wfi so
        // we return to the caller with the same IRQ mask we had on entry.
        unsafe {
            core::arch::asm!(
                "mrs x0, DAIF",            // Save current IRQ mask
                "msr DAIFClr, #2",         // Unmask IRQ (bit 1) so wfi can wake
                "wfi",                      // Halt until an IRQ arrives
                "msr DAIF, x0",            // Restore original mask
                out("x0") _,
            );
        }
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
pub fn unblock_thread(tcb_paddr: PhysAddr) {
    let idx = thread_index_for(tcb_paddr)
        .expect("unblock_thread: TCB paddr not registered in scheduler");
    // SAFETY: single-core, IRQs masked — exclusive access.
    unsafe {
        (&mut *SCHEDULER.state_ptr()).unblock(idx)
            .expect("unblock_thread: thread not in Blocked state");
    }
}

/// Exit the current thread permanently. Never returns.
///
/// 1. Asserts no pending exit (cleanup slot empty)
/// 2. Calls step(Exit) which transitions Running → Exited and picks next
/// 3. Stores PendingExit record for deferred cleanup by finish_exit()
/// 4. Context-switches away, then loops in wfi forever
pub fn exit_current() -> ! {
    // SAFETY: single-core, IRQs masked — exclusive access.
    unsafe {
        // Invariant: no pending exit waiting for cleanup
        assert!(
            (*PENDING_EXIT.0.get()).is_none(),
            "exit_current: previous exit not yet cleaned up"
        );

        let state = &mut *SCHEDULER.state_ptr();
        let decision = state.step(SchedReason::Exit);

        match decision {
            SchedDecision::ExitAndSwitch { exited, next } => {
                let tcb_paddr = (*SCHEDULER.threads_ptr())[exited].unwrap();
                *PENDING_EXIT.0.get() = Some(PendingExit { thread_idx: exited, tcb_paddr });

                let new_paddr = (*SCHEDULER.threads_ptr())[next].unwrap();
                let new_tcb = KernelMut::<Tcb>::from_paddr(new_paddr);
                let new_ttbr0 = crate::cap::process_obj::process_ttbr0(
                    PhysAddr::new(new_tcb.get().process_paddr)
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
                let mut old_tcb = KernelMut::<Tcb>::from_paddr(old_paddr);
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
                let tcb_paddr = (*SCHEDULER.threads_ptr())[exited].unwrap();
                *PENDING_EXIT.0.get() = Some(PendingExit { thread_idx: exited, tcb_paddr });

                loop {
                    schedule(SchedReason::Block);
                    // If we're somehow still here (no Ready thread yet),
                    // wfi until the next interrupt.
                    core::arch::asm!(
                        "mrs x0, DAIF",
                        "msr DAIFClr, #2",
                        "wfi",
                        "msr DAIF, x0",
                        out("x0") _,
                    );
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
    // SAFETY: single-core, IRQs masked — exclusive access.
    let pending = unsafe { &*PENDING_EXIT.0.get() };
    let pending = match pending {
        Some(p) => p,
        None => return, // common path — no pending exit
    };

    // Cannot clean up if the exited thread is still current (ExitAndHalt
    // path — we haven't context-switched to a different thread yet).
    // Cleanup will happen later when an interrupt unblocks a thread and
    // schedule() runs from that thread's context.
    let current_idx = unsafe { (*SCHEDULER.state_ptr()).current };
    if pending.thread_idx == current_idx {
        return;
    }

    // Safe to proceed — take ownership of the pending record.
    let pending = unsafe { (*PENDING_EXIT.0.get()).take().unwrap() };

    // Step 1: Read everything we need from the exiting TCB
    let tcb = unsafe { KernelRef::<Tcb>::from_paddr(pending.tcb_paddr) };
    // Invariant: stack_base is a direct-map VA of the kernel stack page
    // (stack_base = phys + KERNEL_VA_OFFSET). This is set by create_tcb.
    let stack_paddr = PhysAddr::new(tcb.get().stack_base - KERNEL_VA_OFFSET);
    let process_paddr = PhysAddr::new(tcb.get().process_paddr);

    // Step 2: Remove from scheduler array + model
    unsafe {
        (&mut *SCHEDULER.state_ptr()).remove_thread(pending.thread_idx)
            .expect("finish_exit: remove_thread failed");
        (*SCHEDULER.threads_ptr())[pending.thread_idx] = None;
    }

    // Step 3: Free per-thread resources (kernel stack, TCB)
    page_alloc::dealloc_page(PhysPage::containing(stack_paddr));
    // Free TCB page (after reading all fields we need from it)
    page_alloc::dealloc_page(PhysPage::containing(pending.tcb_paddr));

    // Step 4: Decrement process thread count via narrow op + pure model
    let lifecycle = crate::cap::process_obj::process_dec_thread_count(process_paddr);
    let mut pages_freed = 2u32; // stack + TCB

    match lifecycle {
        ProcessLifecycle::LastThread => {
            // Last thread in this process — free process resources
            let ht_paddr = crate::cap::process_obj::process_handle_table(process_paddr);
            let ht_page_count = if ht_paddr.as_u64() != 0 {
                let ht = unsafe { KernelRef::<HandleTableHeader>::from_paddr(ht_paddr) };
                ht.get().header.page_count as usize
            } else {
                0
            };
            for i in 0..ht_page_count {
                let page = PhysAddr::new(ht_paddr.as_u64() + (i as u64) * crate::mm::addr::PAGE_SIZE);
                page_alloc::dealloc_page(PhysPage::containing(page));
            }
            pages_freed += ht_page_count as u32;
            // Free ProcessObject page (last)
            page_alloc::dealloc_page(PhysPage::containing(process_paddr));
            pages_freed += 1;
        }
        ProcessLifecycle::ThreadsRemaining(_) | ProcessLifecycle::Immortal(_) => {
            // Process stays alive — other threads still running, or immortal
        }
    }

    crate::kprintln!("[EXIT] Thread {} cleaned up ({} pages freed)",
        pending.thread_idx, pages_freed);
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

fn thread_index_for(paddr: PhysAddr) -> Option<usize> {
    // SAFETY: single-core, IRQs masked — read-only access.
    let threads = unsafe { &*SCHEDULER.threads_ptr() };
    for i in 0..MAX_THREADS {
        if threads[i] == Some(paddr) {
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

    let state = &mut *SCHEDULER.state_ptr();
    let old_idx = state.current;
    let old_paddr = (*SCHEDULER.threads_ptr())[old_idx].unwrap();
    let mut old_tcb = KernelMut::<Tcb>::from_paddr(old_paddr);

    // step() validates preconditions, computes the decision, applies it,
    // and returns the action for the kernel to execute. No separate
    // decide/apply_decision calls — the model owns the transition.
    let decision = state.step(reason);

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
    let new_tcb = KernelMut::<Tcb>::from_paddr(new_paddr);

    // Check stack canary of the thread we're switching away from
    check_thread_canary(old_tcb.get());

    // Swap TTBR0 if the new thread is in a different process.
    // Same-process switches skip the TLB flush (threads share address space).
    let old_process = old_tcb.get().process_paddr;
    let new_process = new_tcb.get().process_paddr;
    let new_ttbr0 = crate::cap::process_obj::process_ttbr0(PhysAddr::new(new_process));
    if new_ttbr0 != 0 && new_process != old_process {
        core::arch::asm!(
            "msr TTBR0_EL1, {val}",           // Install new process page table
            "dsb ish",                          // Ensure TTBR0 write completes
            "tlbi vmalle1is",                   // Flush TLB (all entries, inner shareable)
            "dsb ish",                          // Ensure TLB flush completes
            "isb",                              // Sync pipeline
            val = in(reg) new_ttbr0,
        );
    }

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
    // SAFETY: kernel stack address — Tcb guarantees stack_base points
    // at the base of a kernel-owned page whose first u64 is the canary.
    let value = unsafe { ptr::read_volatile(tcb.stack_base as *const u64) };
    if value != lockjaw_types::constants::STACK_CANARY {
        panic!(
            "Thread stack canary corrupted! Expected {:#018x}, got {:#018x}",
            lockjaw_types::constants::STACK_CANARY, value
        );
    }
}
