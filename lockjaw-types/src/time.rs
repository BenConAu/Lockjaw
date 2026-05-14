//! Monotonic time and deadline math for the sys_wait_any timeout
//! extension. Pure logic — no syscalls, no MMIO, no unsafe.
//!
//! The kernel reads CNTVCT_EL0 and userspace reads CNTVCT_EL0 +
//! CNTFRQ_EL0 directly via `mrs` (ARMv8 lets EL0 access both once
//! CNTKCTL_EL1.EL0VCTEN is set). This module owns the type wrappers
//! and the unit conversions; the actual register reads live in the
//! kernel timer module and `lockjaw-userlib::time`.
//!
//! Three newtypes:
//!   - [`MonoTicks`] — raw CNTVCT_EL0 reading (counter ticks).
//!   - [`Nanos`]     — duration / instant expressed in nanoseconds.
//!   - [`TickFreq`]  — counter frequency from CNTFRQ_EL0 (Hz).
//!
//! Rounding discipline:
//!   - `nanos_to_ticks` rounds *up* — used to build a deadline that
//!     guarantees "wait at least N nanoseconds elapsed". An off-by-
//!     one short would silently violate the SD-spec-style minimum.
//!   - `ticks_to_nanos` rounds *down* — natural integer division.
//!     Callers reading "how long did I actually wait?" can compare
//!     `elapsed_ns >= target_ns` and get the right answer.
//!
//! `MonoTicks::NO_DEADLINE` (u64::MAX) is the sentinel passed in the
//! `deadline` argument of sys_wait_any to mean "no timeout". A real
//! CNTVCT_EL0 reading takes ~10,800 years at 54 MHz to reach that
//! value, so collision is not a practical concern.

/// Counter-timer ticks (CNTVCT_EL0). Monotonic across a boot.
///
/// Ordering and equality are derived: `MonoTicks(a) < MonoTicks(b)`
/// iff `a < b`. Arithmetic uses [`MonoTicks::deadline_in`]
/// rather than raw `+` to keep the [`NO_DEADLINE`](Self::NO_DEADLINE)
/// sentinel reserved.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonoTicks(pub u64);

impl MonoTicks {
    /// Sentinel passed to sys_wait_any meaning "no deadline".
    pub const NO_DEADLINE: MonoTicks = MonoTicks(u64::MAX);

    /// Build a deadline that fires no sooner than `nanos` from
    /// `self`. Saturating — clamps at `NO_DEADLINE - 1` so the
    /// sentinel stays uniquely "no deadline".
    pub const fn deadline_in(self, nanos: Nanos, freq: TickFreq) -> MonoTicks {
        let delta = nanos_to_ticks(nanos, freq).0;
        let raw = self.0.saturating_add(delta);
        // Reserve u64::MAX for the sentinel.
        if raw == u64::MAX { MonoTicks(u64::MAX - 1) } else { MonoTicks(raw) }
    }

    /// True iff this deadline has expired relative to `now`.
    /// `NO_DEADLINE.has_expired(_)` is always false — the sentinel
    /// represents "no deadline at all" rather than "deadline in the
    /// infinite future".
    pub const fn has_expired(self, now: MonoTicks) -> bool {
        if self.0 == u64::MAX { false } else { now.0 >= self.0 }
    }

    /// True iff this MonoTicks is the NO_DEADLINE sentinel.
    pub const fn is_no_deadline(self) -> bool {
        self.0 == u64::MAX
    }
}

/// Duration or instant in nanoseconds.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Nanos(pub u64);

impl Nanos {
    pub const fn from_nanos(n: u64) -> Nanos { Nanos(n) }
    pub const fn from_micros(us: u64) -> Nanos { Nanos(us.saturating_mul(1_000)) }
    pub const fn from_millis(ms: u64) -> Nanos { Nanos(ms.saturating_mul(1_000_000)) }
    pub const fn from_secs(s: u64)    -> Nanos { Nanos(s.saturating_mul(1_000_000_000)) }
}

/// Counter frequency from CNTFRQ_EL0, in Hz. Constant for the boot.
/// Typical values: 10 MHz (QEMU virt), 54 MHz (Pi 4B), 1 GHz on
/// some server-class parts.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TickFreq(pub u64);

const NS_PER_SEC: u128 = 1_000_000_000;

/// Convert ticks → nanoseconds. Truncating (rounds down).
///
/// Saturates at `Nanos(u64::MAX)` if the result wouldn't fit; not a
/// real concern at typical frequencies (u64 nanos ≈ 584 years).
pub const fn ticks_to_nanos(ticks: MonoTicks, freq: TickFreq) -> Nanos {
    if freq.0 == 0 { return Nanos(0); }
    let prod = (ticks.0 as u128).saturating_mul(NS_PER_SEC);
    let ns = prod / (freq.0 as u128);
    if ns > u64::MAX as u128 { Nanos(u64::MAX) } else { Nanos(ns as u64) }
}

/// Convert nanoseconds → ticks. Ceiling — rounds up to guarantee
/// "wait at least N nanoseconds" semantics.
///
/// Saturates at `MonoTicks(u64::MAX)` if the result wouldn't fit.
pub const fn nanos_to_ticks(nanos: Nanos, freq: TickFreq) -> MonoTicks {
    if freq.0 == 0 { return MonoTicks(0); }
    let prod = (nanos.0 as u128).saturating_mul(freq.0 as u128);
    // Ceiling division: (a + b - 1) / b for b > 0.
    let ticks = (prod + NS_PER_SEC - 1) / NS_PER_SEC;
    if ticks > u64::MAX as u128 { MonoTicks(u64::MAX) } else { MonoTicks(ticks as u64) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // QEMU virt's typical CNTFRQ_EL0.
    const QEMU_FREQ: TickFreq = TickFreq(10_000_000);
    // Pi 4B's CNTFRQ_EL0.
    const PI4B_FREQ: TickFreq = TickFreq(54_000_000);

    // ---- MonoTicks ordering ----

    #[test]
    fn mono_ticks_orders_by_inner_u64() {
        assert!(MonoTicks(0) < MonoTicks(1));
        assert!(MonoTicks(100) < MonoTicks(u64::MAX));
        assert_eq!(MonoTicks(42), MonoTicks(42));
    }

    // ---- NO_DEADLINE sentinel ----

    #[test]
    fn no_deadline_never_expires() {
        assert!(!MonoTicks::NO_DEADLINE.has_expired(MonoTicks(0)));
        assert!(!MonoTicks::NO_DEADLINE.has_expired(MonoTicks(u64::MAX - 1)));
        // Even comparing against u64::MAX itself: NO_DEADLINE means
        // "no deadline", not "the largest possible deadline". The
        // contract is "never expires" regardless of `now`.
        assert!(!MonoTicks::NO_DEADLINE.has_expired(MonoTicks(u64::MAX)));
    }

    #[test]
    fn no_deadline_is_no_deadline() {
        assert!(MonoTicks::NO_DEADLINE.is_no_deadline());
        assert!(!MonoTicks(0).is_no_deadline());
        assert!(!MonoTicks(u64::MAX - 1).is_no_deadline());
    }

    // ---- has_expired ordinary cases ----

    #[test]
    fn deadline_not_expired_when_now_before() {
        assert!(!MonoTicks(100).has_expired(MonoTicks(99)));
    }

    #[test]
    fn deadline_expired_when_now_equal() {
        assert!(MonoTicks(100).has_expired(MonoTicks(100)));
    }

    #[test]
    fn deadline_expired_when_now_after() {
        assert!(MonoTicks(100).has_expired(MonoTicks(200)));
    }

    // ---- deadline_in: typical & boundary cases ----

    #[test]
    fn deadline_in_advances_by_converted_ticks() {
        // 1 µs at 10 MHz = 10 ticks.
        let now = MonoTicks(1000);
        let dl = now.deadline_in(Nanos::from_micros(1), QEMU_FREQ);
        assert_eq!(dl, MonoTicks(1010));
    }

    #[test]
    fn deadline_in_at_pi4b_freq() {
        // 200 µs at 54 MHz = ceil(200_000 * 54 / 1_000_000_000 * 1e9) =
        // ceil(10800) = 10800 ticks.
        let now = MonoTicks(0);
        let dl = now.deadline_in(Nanos::from_micros(200), PI4B_FREQ);
        assert_eq!(dl, MonoTicks(10_800));
    }

    #[test]
    fn deadline_in_saturates_at_u64_max_minus_one() {
        // Adding any positive duration to a near-MAX baseline must
        // not produce the NO_DEADLINE sentinel.
        let now = MonoTicks(u64::MAX - 5);
        let dl = now.deadline_in(Nanos::from_secs(60), QEMU_FREQ);
        assert_eq!(dl, MonoTicks(u64::MAX - 1));
        // And it must not equal the sentinel.
        assert!(!dl.is_no_deadline());
    }

    #[test]
    fn deadline_in_with_zero_duration_returns_now() {
        let now = MonoTicks(12345);
        let dl = now.deadline_in(Nanos::from_nanos(0), QEMU_FREQ);
        assert_eq!(dl, now);
    }

    // ---- nanos_to_ticks ceiling behavior ----

    #[test]
    fn nanos_to_ticks_rounds_up_short_duration() {
        // 50 ns at 10 MHz: exact = 0.5 ticks → ceiling = 1 tick.
        // (Wait at least 50 ns means hold for 1 tick = 100 ns.)
        let t = nanos_to_ticks(Nanos::from_nanos(50), QEMU_FREQ);
        assert_eq!(t, MonoTicks(1));
    }

    #[test]
    fn nanos_to_ticks_exact_multiple() {
        // 100 ns at 10 MHz = exactly 1 tick.
        let t = nanos_to_ticks(Nanos::from_nanos(100), QEMU_FREQ);
        assert_eq!(t, MonoTicks(1));
    }

    #[test]
    fn nanos_to_ticks_zero() {
        // Exactly 0 ns must convert to 0 ticks (no off-by-one
        // pretending we waited a tick we didn't).
        let t = nanos_to_ticks(Nanos::from_nanos(0), QEMU_FREQ);
        assert_eq!(t, MonoTicks(0));
    }

    #[test]
    fn nanos_to_ticks_zero_freq_returns_zero() {
        let t = nanos_to_ticks(Nanos::from_millis(50), TickFreq(0));
        assert_eq!(t, MonoTicks(0));
    }

    // ---- ticks_to_nanos truncation behavior ----

    #[test]
    fn ticks_to_nanos_exact() {
        // 100 ticks at 10 MHz = 10 µs = 10_000 ns.
        let n = ticks_to_nanos(MonoTicks(100), QEMU_FREQ);
        assert_eq!(n, Nanos(10_000));
    }

    #[test]
    fn ticks_to_nanos_truncates() {
        // 1 tick at 10 MHz = exactly 100 ns; 0 ticks = 0.
        // Test: 1 tick at 3 Hz → 333 333 333 ns (truncates 0.333…).
        let n = ticks_to_nanos(MonoTicks(1), TickFreq(3));
        assert_eq!(n, Nanos(333_333_333));
    }

    #[test]
    fn ticks_to_nanos_zero_freq_returns_zero() {
        let n = ticks_to_nanos(MonoTicks(1000), TickFreq(0));
        assert_eq!(n, Nanos(0));
    }

    // ---- round-trip ----

    #[test]
    fn ticks_to_nanos_to_ticks_roundtrip_at_10mhz() {
        // 1234 ticks at 10 MHz: 1234 * 100 = 123_400 ns.
        // Back: ceil(123_400 / 100) = 1234 ticks. Exact round-trip.
        let t0 = MonoTicks(1234);
        let n = ticks_to_nanos(t0, QEMU_FREQ);
        let t1 = nanos_to_ticks(n, QEMU_FREQ);
        assert_eq!(t0, t1);
    }

    #[test]
    fn nanos_to_ticks_to_nanos_at_pi4b_overshoots_within_one_tick() {
        // 200 µs at 54 MHz: ceil(200_000 * 54 / 1_000) = 10_800 ticks.
        // Back to ns: 10_800 * 1_000_000_000 / 54_000_000 = 200_000 ns.
        // Exact round-trip when the ceiling didn't actually round up.
        let n0 = Nanos::from_micros(200);
        let t = nanos_to_ticks(n0, PI4B_FREQ);
        let n1 = ticks_to_nanos(t, PI4B_FREQ);
        assert_eq!(n0, n1);
    }

    #[test]
    fn nanos_to_ticks_to_nanos_overshoots_when_rounded() {
        // 51 ns at 10 MHz: ceil = 1 tick → back to ns = 100 ns.
        // Round-up bias is intentional ("wait at least N nanos").
        let n0 = Nanos::from_nanos(51);
        let t = nanos_to_ticks(n0, QEMU_FREQ);
        let n1 = ticks_to_nanos(t, QEMU_FREQ);
        assert!(n1 >= n0, "rounded-up tick must back-convert to ≥ original ns");
        assert_eq!(n1, Nanos(100));
    }

    // ---- Nanos constructors ----

    #[test]
    fn nanos_constructors() {
        assert_eq!(Nanos::from_nanos(7), Nanos(7));
        assert_eq!(Nanos::from_micros(7), Nanos(7_000));
        assert_eq!(Nanos::from_millis(7), Nanos(7_000_000));
        assert_eq!(Nanos::from_secs(7), Nanos(7_000_000_000));
    }

    #[test]
    fn nanos_constructors_saturate() {
        assert_eq!(Nanos::from_secs(u64::MAX), Nanos(u64::MAX));
        assert_eq!(Nanos::from_millis(u64::MAX), Nanos(u64::MAX));
        assert_eq!(Nanos::from_micros(u64::MAX), Nanos(u64::MAX));
    }

    // ---- Sentinel doesn't collide with realistic ticks ----

    #[test]
    fn no_deadline_value_is_distinct_from_realistic_uptime() {
        // u64::MAX ticks at 54 MHz = u64::MAX / 54_000_000 seconds
        // = ~341 billion seconds = ~10,800 years. The sentinel cannot
        // collide with any real CNTVCT_EL0 reading on a Lockjaw boot.
        let years = (u64::MAX / 54_000_000) / (365 * 24 * 60 * 60);
        assert!(years > 10_000);
    }
}
