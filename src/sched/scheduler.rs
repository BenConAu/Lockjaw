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

/// Register a thread in the run queue. The first thread registered (index 0)
/// is the idle thread — its saved_sp will be filled on the first context switch.
pub unsafe fn add_thread(tcb_paddr: PhysAddr) {
    let idx = THREAD_COUNT;
    assert!(idx < MAX_THREADS, "too many threads");
    THREADS[idx] = Some(tcb_paddr);
    THREAD_COUNT += 1;
}

/// Start the scheduler. Must be called after all initial threads are added.
pub unsafe fn start() {
    ACTIVE = true;
}

/// Called from the timer tick handler. Picks the next ready thread and
/// context-switches to it.
pub unsafe fn tick() {
    if !ACTIVE || THREAD_COUNT < 2 {
        return;
    }

    schedule();
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

unsafe fn schedule() {
    let old_idx = CURRENT;
    let old_paddr = THREADS[old_idx].unwrap();
    let old_tcb = tcb_ptr_mut(old_paddr);

    // Mark old thread as Ready (it was Running)
    (*old_tcb).state = ThreadState::Ready;

    // Find next Ready thread (round-robin)
    let mut next_idx = (old_idx + 1) % THREAD_COUNT;
    loop {
        let paddr = THREADS[next_idx].unwrap();
        let tcb = tcb_ptr(paddr);
        if (*tcb).state == ThreadState::Ready {
            break;
        }
        next_idx = (next_idx + 1) % THREAD_COUNT;
        if next_idx == old_idx {
            // No other ready thread — stay on current
            (*old_tcb).state = ThreadState::Running;
            return;
        }
    }

    let new_paddr = THREADS[next_idx].unwrap();
    let new_tcb = tcb_ptr_mut(new_paddr);

    (*new_tcb).state = ThreadState::Running;
    CURRENT = next_idx;

    // Check stack canary of the thread we're switching away from
    check_thread_canary(old_tcb);

    // Context switch: save old SP, load new SP, swap callee-saved regs
    let old_sp_ptr = &mut (*old_tcb).saved_sp as *mut u64;
    let new_sp_ptr = &(*new_tcb).saved_sp as *const u64;
    context_switch(old_sp_ptr, new_sp_ptr);
}

unsafe fn tcb_ptr(paddr: PhysAddr) -> *const Tcb {
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *const Tcb
}

unsafe fn tcb_ptr_mut(paddr: PhysAddr) -> *mut Tcb {
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *mut Tcb
}

/// Check the stack canary for a thread.
unsafe fn check_thread_canary(tcb: *const Tcb) {
    let canary_ptr = (*tcb).stack_base as *const u64;
    let value = ptr::read_volatile(canary_ptr);
    if value != 0xDEAD_BEEF_DEAD_BEEF {
        panic!(
            "Thread stack canary corrupted! Expected {:#018x}, got {:#018x}",
            0xDEAD_BEEF_DEAD_BEEFu64, value
        );
    }
}
