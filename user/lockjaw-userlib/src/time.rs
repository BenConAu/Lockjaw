//! Userspace time helpers — monotonic counter reads and sleep.
//!
//! `monotonic_now` and `cntfreq_hz` read EL0-accessible AArch64
//! architectural registers directly via `mrs`. Both are gated on
//! the kernel's `CNTKCTL_EL1.EL0VCTEN | EL0PCTEN` bits set in
//! `enable_el0_counter_reads` at boot — without that, these `mrs`
//! instructions trap synchronously to EL1.
//!
//! Sleep is implemented on top of `sys_wait_any` with `count == 0`
//! and the deadline in x2. The kernel's per-tick scan in
//! `handle_tick` wakes the thread when `now >= deadline`.
//!
//! Wakeup latency is tick-quantized: `sleep_for(N ns)` returns no
//! sooner than the deadline, and in the worst case up to ~two
//! scheduler-tick periods (~20 ms with a 10 ms tick) past the
//! deadline — one tick to align the request with the next tick
//! boundary, plus one tick for the deadline scan that runs on the
//! tick *after* deadline expiry. With S3's wake-before-schedule
//! ordering in `handle_tick` the second tick is headroom in
//! practice; the integration test in `tests/qemu_integration.sh`
//! pins the resulting envelope at [50 ms, 70 ms] for a 50 ms
//! request. This is a *lower-bounded* sleep — good enough for any
//! spec requirement stated as a minimum ("wait at least N"), not a
//! high-resolution facility.

use core::arch::asm;
use lockjaw_types::syscall::SyscallError;

// Re-export the pure types so callers only need `lockjaw_userlib::time`.
pub use lockjaw_types::time::{MonoTicks, Nanos, TickFreq, nanos_to_ticks, ticks_to_nanos};

/// Read the current monotonic counter via `mrs CNTVCT_EL0`.
///
/// Constant-cost (single instruction). The kernel must have set
/// `CNTKCTL_EL1.EL0VCTEN` at boot — see kernel timer init. Without
/// that bit, this call traps synchronously and the thread dies.
pub fn monotonic_now() -> MonoTicks {
    let ticks: u64;
    unsafe {
        asm!(
            "mrs {val}, CNTVCT_EL0",            // EL0-readable virtual counter
            val = out(reg) ticks,
        );
    }
    MonoTicks(ticks)
}

/// Read the counter frequency via `mrs CNTFRQ_EL0`.
///
/// The value is constant for the lifetime of the boot, but we
/// re-read on every call rather than caching: `mrs` is single-cycle,
/// caching would buy nothing measurable, and an `OnceLock` would add
/// initialization-order surface for no real benefit.
pub fn cntfreq_hz() -> TickFreq {
    let hz: u64;
    unsafe {
        asm!(
            "mrs {val}, CNTFRQ_EL0",            // EL0-readable counter frequency
            val = out(reg) hz,
        );
    }
    TickFreq(hz)
}

/// Sleep until the absolute monotonic deadline `deadline` (in
/// CNTVCT_EL0 ticks). Returns once `now >= deadline`, possibly up
/// to ~two scheduler-tick periods (~20 ms) later — see module doc
/// for the latency-envelope rationale.
///
/// Internally calls `sys_wait_any(NULL, 0, deadline)` — the
/// pure-sleep form of the syscall. The mask returned is always 0
/// (timeout encoding) and is discarded.
pub fn sleep_until(deadline: MonoTicks) -> Result<(), SyscallError> {
    crate::syscall::sys_wait_any_until(&[], deadline).map(|_mask| ())
}

/// Sleep for at least `nanos` nanoseconds from now. Wake is
/// tick-quantized — actual elapsed time is in `[nanos, nanos + ~2
/// scheduler-tick periods]` (see module doc). Saturates at
/// `NO_DEADLINE - 1` for absurdly large inputs (guards against u64
/// wrap turning the deadline into the past).
pub fn sleep_for(nanos: Nanos) -> Result<(), SyscallError> {
    // deadline_in saturates at NO_DEADLINE-1 to keep the sentinel
    // reserved, so absurdly large `nanos` cannot wrap into "the past"
    // and trigger an immediate-return.
    let deadline = monotonic_now().deadline_in(nanos, cntfreq_hz());
    sleep_until(deadline)
}

/// Returned by [`spin_until_or_deadline`] when the deadline elapsed
/// before the predicate ever returned true. Unit struct because the
/// only fact the primitive knows is "we ran out of time" — the
/// operation context is the caller's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineExpired;

/// Spin until `check()` returns true, or until `deadline` expires.
///
/// Uses `MonoTicks::has_expired(monotonic_now())` so the
/// `NO_DEADLINE` sentinel is honored (a `NO_DEADLINE` deadline never
/// expires — equivalent to "spin forever until check passes").
pub fn spin_until_or_deadline<F: FnMut() -> bool>(
    mut check: F,
    deadline: MonoTicks,
) -> Result<(), DeadlineExpired> {
    loop {
        if check() {
            return Ok(());
        }
        if deadline.has_expired(monotonic_now()) {
            return Err(DeadlineExpired);
        }
        core::hint::spin_loop();
    }
}
