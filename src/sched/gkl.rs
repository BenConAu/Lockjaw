/// Giant Kernel Lock — serializes all kernel-mode execution across cores.
///
/// Wraps a TicketLock from lockjaw-types (host-testable). Every kernel
/// entry point (syscall handler, IRQ handler) acquires the GKL before
/// touching kernel state. The lock is released on kernel exit (eret to
/// EL0) or before wfi in block_current.
///
/// The GKL is only held with IRQs masked. Code that needs IRQs (wfi,
/// EL0) must release the GKL first. This prevents deadlock: a timer
/// tick on a core holding the GKL would re-lock and spin forever.
///
/// Context switch transfers the lock obligation: the old thread stops
/// running (its handler frame stays on its kernel stack), the new thread
/// resumes inside its handler and eventually releases the lock.
///
/// Proof obligations (one per kernel path):
///   Path 1 — Syscall, no block: lock → dispatch → unlock. 1:1.
///   Path 2 — Syscall with block: lock → block_current (unlock+unmask
///            → wfi → mask+lock → schedule → context_switch) → resumed
///            thread unlocks. 1:1 across threads.
///   Path 3 — Timer preemption: lock → tick → schedule → context_switch
///            → resumed thread unlocks. 1:1 across threads.
///   Path 4 — Thread first run: thread_entry inherits lock+masked →
///            entry fn decides: process_entry unlocks before eret,
///            kernel threads run under lock until they block. CPU 0's
///            boot TCB never executes its entry fn (becomes init via
///            process_kva re-point + direct drop_to_el0); secondary
///            CPUs hold no TCB at all (see Path 5).
///   Path 5 — Idle CPU timer preemption (scheduler-refactor Stage 2+,
///            currently dead-code): CPU is parked in `idle_wait` with
///            no current thread (NO GKL held, IRQs unmasked). IRQ →
///            SAVE_REGS on boot stack → gkl_lock → tick →
///            schedule_from_idle → context_switch into picked thread
///            → resumed thread unlocks. The abandoned IRQ frame on the
///            boot stack (~256 bytes ExceptionContext) is never popped;
///            next entry into idle_wait starts from a fresh boot-stack
///            SP set by the IRQ entry path, overwriting it. Worst-case
///            unrecoverable boot-stack usage: one frame. Per-CPU guard
///            pages catch real overflow.

use lockjaw_types::ticket_lock::TicketLock;

static GKL: TicketLock = TicketLock::new();

/// Acquire the Giant Kernel Lock.
#[inline]
pub fn gkl_lock() {
    GKL.lock();
}

/// Release the Giant Kernel Lock.
#[inline]
pub fn gkl_unlock() {
    GKL.unlock();
}
