use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use crate::sched::tcb::{Tcb, ThreadState};
use crate::sched::context::context_switch;
use core::ptr;

const MAX_THREADS: usize = 8;

/// Physical addresses of TCBs in the run queue. None = empty slot.
static mut THREADS: [Option<PhysAddr>; MAX_THREADS] = [None; MAX_THREADS];

/// Index of the currently running thread.
static mut CURRENT: usize = 0;

/// Number of registered threads.
static mut THREAD_COUNT: usize = 0;

/// Whether the scheduler is active (set after init + first threads added).
static mut ACTIVE: bool = false;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Register a thread in the run queue.
///
/// The first thread registered (index 0) is the idle/boot thread — its
/// saved_sp will be filled on the first context switch away from it.
/// Returns false if the run queue is full (MAX_THREADS = 8 reached).
pub unsafe fn add_thread(tcb_paddr: PhysAddr) -> bool {
    let idx = THREAD_COUNT;
    if idx >= MAX_THREADS {
        return false;
    }
    THREADS[idx] = Some(tcb_paddr);
    THREAD_COUNT += 1;
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
    if !ACTIVE || THREAD_COUNT < 2 {
        return;
    }

    schedule();
}

/// Return the physical address of the currently running thread's TCB.
/// Used by syscall handlers to look up the caller's handle table and TTBR0.
pub unsafe fn current_tcb_paddr() -> PhysAddr {
    THREADS[CURRENT].unwrap()
}

/// Return the index of the currently running thread in the run queue.
/// Used by crash diagnostics to identify the faulting thread.
pub unsafe fn current_thread_index() -> usize {
    CURRENT
}

/// Like current_tcb_paddr but returns None instead of panicking.
/// Safe to call from the panic handler without risk of re-entrant panic.
/// Uses raw pointer read to avoid bounds-check panics and static-mut-ref warnings.
pub unsafe fn try_current_tcb_paddr() -> Option<PhysAddr> {
    let idx = CURRENT;
    if idx >= MAX_THREADS { return None; }
    // SAFETY: raw pointer to static
    let ptr = (&raw const THREADS as *const Option<PhysAddr>).add(idx);
    core::ptr::read_volatile(ptr)
}

/// Block the current thread and schedule away to the next Ready thread.
/// Sets the thread's state to Blocked. The caller must have already set
/// the TCB's ipc_blocked_on field. This function does not return until
/// the thread is unblocked by another thread calling unblock_thread().
pub unsafe fn block_current() {
    let paddr = THREADS[CURRENT].unwrap();
    let tcb = tcb_ptr_mut(paddr);
    (*tcb).state = ThreadState::Blocked;
    schedule();
}

/// Unblock a thread by setting its state back to Ready.
/// The thread will be picked up by the next scheduling round.
/// Typically called from IPC endpoint code when a partner arrives.
pub unsafe fn unblock_thread(tcb_paddr: PhysAddr) {
    let tcb = tcb_ptr_mut(tcb_paddr);
    (*tcb).state = ThreadState::Ready;
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

unsafe fn schedule() {
    use lockjaw_types::scheduler::{select_next, SchedThreadState, SchedDecision};

    let old_idx = CURRENT;
    let old_paddr = THREADS[old_idx].unwrap();
    let old_tcb = tcb_ptr_mut(old_paddr);

    // Only mark old thread as Ready if it was Running (not if it was just Blocked)
    if (*old_tcb).state == ThreadState::Running {
        (*old_tcb).state = ThreadState::Ready;
    }

    // Ask the model (tested on host) which thread to run next
    let decision = select_next(old_idx, THREAD_COUNT, |i| {
        let paddr = THREADS[i].unwrap();
        let tcb = tcb_ptr(paddr);
        match (*tcb).state {
            ThreadState::Ready => SchedThreadState::Ready,
            ThreadState::Running => SchedThreadState::Running,
            ThreadState::Blocked => SchedThreadState::Blocked,
        }
    });

    let next_idx = match decision {
        SchedDecision::SwitchTo(idx) => idx,
        SchedDecision::StayOnCurrent => {
            (*old_tcb).state = ThreadState::Running;
            return;
        }
    };

    let new_paddr = THREADS[next_idx].unwrap();
    let new_tcb = tcb_ptr_mut(new_paddr);

    (*new_tcb).state = ThreadState::Running;
    CURRENT = next_idx;

    // Check stack canary of the thread we're switching away from
    check_thread_canary(old_tcb);

    // Swap TTBR0 if the new thread has a different address space.
    // TTBR0 is irrelevant during kernel execution (all kernel code
    // accessed via TTBR1), so swapping before context_switch is safe.
    // When the new thread eventually erets to EL0, TTBR0 is already set.
    let new_ttbr0 = (*new_tcb).ttbr0_paddr;
    if new_ttbr0 != 0 {
        core::arch::asm!(
            "msr TTBR0_EL1, {val}",         // Install new process page table
            "dsb ish",                        // Ensure TTBR0 write completes
            "tlbi vmalle1is",                 // Flush TLB (all entries, inner shareable)
            "dsb ish",                        // Ensure TLB flush completes
            "isb",                            // Sync pipeline
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

unsafe fn tcb_ptr(paddr: PhysAddr) -> *const Tcb {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *const Tcb
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
