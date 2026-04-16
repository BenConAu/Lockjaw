use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use crate::sched::tcb::Tcb;
use crate::sched::context::context_switch;
use core::cell::UnsafeCell;
use core::ptr;
use lockjaw_types::scheduler::{
    SchedDecision, SchedReason, SchedState, SchedThreadState,
};

const MAX_THREADS: usize = 8;

/// Physical addresses of TCBs in the run queue. None = empty slot.
/// Indexed by SchedState.current. This is the concrete mapping from
/// abstract thread indices to TCB physical pages.
static mut THREADS: [Option<PhysAddr>; MAX_THREADS] = [None; MAX_THREADS];

/// The scheduler's abstract state — the model owns thread states and
/// current thread index. All scheduler operations (block, unblock,
/// transitions) go through this.
struct SyncSchedState(UnsafeCell<SchedState>);
unsafe impl Sync for SyncSchedState {}
static SCHED_STATE: SyncSchedState = SyncSchedState(UnsafeCell::new(SchedState::new_const()));

/// Whether the scheduler is active (set after init + first threads added).
static mut ACTIVE: bool = false;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Register a thread in the run queue.
///
/// The first thread registered (index 0) is the idle/boot thread — its
/// saved_sp will be filled on the first context switch away from it, and
/// its state is marked Running (since it's already executing).
/// Subsequent threads are added as Ready.
/// Returns false if the run queue is full (MAX_THREADS = 8 reached).
pub unsafe fn add_thread(tcb_paddr: PhysAddr) -> bool {
    let state = &mut *SCHED_STATE.0.get();
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
    THREADS[idx] = Some(tcb_paddr);
    true
}

/// Activate the scheduler. After this call, timer ticks trigger scheduling.
/// Must be called after all initial threads are registered via add_thread().
pub unsafe fn start() {
    ACTIVE = true;
}

/// Called from the timer tick handler every TIMER_TICK_MS milliseconds.
/// Uses the round-robin model (from lockjaw-types) to pick the next Ready
/// thread, swaps TTBR0 if needed, checks the stack canary, and performs
/// the context switch.
pub unsafe fn tick() {
    let state = &*SCHED_STATE.0.get();
    if !ACTIVE || state.thread_count() < 2 {
        return;
    }
    schedule(SchedReason::Preempt);
}

/// Return the physical address of the currently running thread's TCB.
/// Used by syscall handlers to look up the caller's handle table and TTBR0.
pub unsafe fn current_tcb_paddr() -> PhysAddr {
    let state = &*SCHED_STATE.0.get();
    THREADS[state.current].unwrap()
}

/// Return the index of the currently running thread in the run queue.
/// Used by crash diagnostics to identify the faulting thread.
pub unsafe fn current_thread_index() -> usize {
    let state = &*SCHED_STATE.0.get();
    state.current
}

/// Like current_tcb_paddr but returns None instead of panicking.
/// Safe to call from the panic handler without risk of re-entrant panic.
/// Uses raw pointer reads to avoid bounds-check panics and static-mut-ref warnings.
pub unsafe fn try_current_tcb_paddr() -> Option<PhysAddr> {
    // SAFETY: raw pointer to static
    let state_ptr = SCHED_STATE.0.get() as *const SchedState;
    let idx = (*state_ptr).current;
    if idx >= MAX_THREADS { return None; }
    // SAFETY: raw pointer to static
    let ptr = (&raw const THREADS as *const Option<PhysAddr>).add(idx);
    core::ptr::read_volatile(ptr)
}

/// Block the current thread and schedule away.
/// Loops until another thread context-switches back to us.
/// If no thread is Ready, halts via wfi until an interrupt wakes something.
/// Saves/restores the DAIF mask around wfi to preserve the kernel's
/// "single-threaded during syscall" invariant.
pub unsafe fn block_current() {
    let state = &mut *SCHED_STATE.0.get();
    state.block_current().expect("block_current: not Running");
    loop {
        schedule(SchedReason::Block);
        // Re-read: schedule may have switched us out and back in.
        // If we're Running again, an unblock_thread + schedule decided
        // to pick us, so we resume.
        let state = &*SCHED_STATE.0.get();
        if state.get(state.current) == Some(SchedThreadState::Running) {
            return;
        }
        // All blocked. Wait for an IRQ, preserving DAIF around wfi so
        // we return to the caller with the same IRQ mask we had on entry.
        core::arch::asm!(
            "mrs x0, DAIF",            // Save current IRQ mask
            "msr DAIFClr, #2",         // Unmask IRQ (bit 1) so wfi can wake
            "wfi",                      // Halt until an IRQ arrives
            "msr DAIF, x0",            // Restore original mask
            out("x0") _,
        );
    }
}

/// Unblock a thread by setting its state to Ready.
/// The thread will be picked up by the next scheduling round.
/// Typically called from IPC endpoint code when a partner arrives.
/// Takes a TCB paddr (what IPC code has) and looks up the thread index.
pub unsafe fn unblock_thread(tcb_paddr: PhysAddr) {
    let idx = match thread_index_for(tcb_paddr) {
        Some(i) => i,
        None => return, // unknown thread — ignore
    };
    let state = &mut *SCHED_STATE.0.get();
    let _ = state.unblock(idx);
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// Find the thread index for a given TCB paddr. O(N) but N is small.
unsafe fn thread_index_for(paddr: PhysAddr) -> Option<usize> {
    for i in 0..MAX_THREADS {
        if THREADS[i] == Some(paddr) {
            return Some(i);
        }
    }
    None
}

unsafe fn schedule(reason: SchedReason) {
    let state = &mut *SCHED_STATE.0.get();
    let old_idx = state.current;
    let old_paddr = THREADS[old_idx].unwrap();
    let old_tcb = tcb_ptr_mut(old_paddr);

    let decision = state.decide(reason);

    let next_idx = match decision {
        SchedDecision::SwitchTo(idx) => idx,
        SchedDecision::StayOnCurrent | SchedDecision::WaitForInterrupt => {
            // No context switch. Model's apply_decision validates.
            let _ = state.apply_decision(reason, decision);
            return;
        }
    };

    // Apply the transition in the model. This validates the move:
    // demotes old Running -> Ready (on Preempt), promotes next Ready -> Running.
    state.apply_decision(reason, decision).expect("invalid scheduler transition");

    let new_paddr = THREADS[next_idx].unwrap();
    let new_tcb = tcb_ptr_mut(new_paddr);

    // Check stack canary of the thread we're switching away from
    check_thread_canary(old_tcb);

    // Swap TTBR0 if the new thread has a different address space.
    // TTBR0 is irrelevant during kernel execution (all kernel code
    // accessed via TTBR1), so swapping before context_switch is safe.
    // When the new thread eventually erets to EL0, TTBR0 is already set.
    let new_ttbr0 = (*new_tcb).ttbr0_paddr;
    if new_ttbr0 != 0 {
        core::arch::asm!(
            "msr TTBR0_EL1, {val}",           // Install new process page table
            "dsb ish",                          // Ensure TTBR0 write completes
            "tlbi vmalle1is",                   // Flush TLB (all entries, inner shareable)
            "dsb ish",                          // Ensure TLB flush completes
            "isb",                              // Sync pipeline
            val = in(reg) new_ttbr0,
        );
    }

    // Context switch: save old SP, load new SP, swap callee-saved regs
    // SAFETY: context switch register pointers
    let old_sp_ptr = &mut (*old_tcb).saved_sp as *mut u64;
    // SAFETY: context switch register pointers
    let new_sp_ptr = &(*new_tcb).saved_sp as *const u64;
    context_switch(old_sp_ptr, new_sp_ptr);
}

unsafe fn tcb_ptr_mut(paddr: PhysAddr) -> *mut Tcb {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *mut Tcb
}

/// Check the stack canary for a thread.
unsafe fn check_thread_canary(tcb: *const Tcb) {
    // SAFETY: kernel stack address
    let canary_ptr = (*tcb).stack_base as *const u64;
    let value = ptr::read_volatile(canary_ptr);
    if value != lockjaw_types::constants::STACK_CANARY {
        panic!(
            "Thread stack canary corrupted! Expected {:#018x}, got {:#018x}",
            lockjaw_types::constants::STACK_CANARY, value
        );
    }
}
