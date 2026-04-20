/// Giant Kernel Lock — serializes all kernel-mode execution across cores.
///
/// Wraps a TicketLock from lockjaw-types (host-testable). Every kernel
/// entry point (syscall handler, IRQ handler) acquires the GKL before
/// touching kernel state. The lock is released on kernel exit (eret to
/// EL0) or before wfi in block_current.
///
/// Context switch transfers the lock obligation: the old thread stops
/// running (its handler frame stays on its kernel stack), the new thread
/// resumes inside its handler and eventually releases the lock.
///
/// Proof obligations (one per kernel path):
///   Path 1 — Syscall, no block: lock → dispatch → unlock. 1:1.
///   Path 2 — Syscall with block: lock → block_current (unlock → wfi →
///            lock → schedule → context_switch) → resumed thread unlocks.
///   Path 3 — Timer preemption: lock → tick → schedule → context_switch →
///            resumed thread unlocks. 1:1 across threads.
///   Path 4 — Thread first run: thread_entry inherits lock → entry fn →
///            (user: unlock before eret) or (kernel: unlock via block_current).

use lockjaw_types::ticket_lock::TicketLock;

static GKL: TicketLock = TicketLock::new();

/// Acquire the Giant Kernel Lock.
#[inline]
#[allow(dead_code)] // Used when handler integration lands (commit 4)
pub fn gkl_lock() {
    GKL.lock();
}

/// Release the Giant Kernel Lock.
#[inline]
#[allow(dead_code)] // Used when handler integration lands (commit 4)
pub fn gkl_unlock() {
    GKL.unlock();
}

/// C-callable unlock for the thread_entry assembly trampoline.
#[no_mangle]
pub extern "C" fn gkl_unlock_from_trampoline() {
    gkl_unlock();
}
