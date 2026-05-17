use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// Global tick counter — incremented by the timer IRQ handler.
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Tick-handler self-timing in CNTVCT ticks. Updated atomically each
/// tick; codex flagged that a multi-ms tick handler could mimic
/// SD-card stalls, so we keep these as a permanent monitor. Both
/// counters are read by `sys_sched_telemetry` so userspace can
/// correlate "how long did the tick handler take last/at peak" with
/// observed measurement variance. `LAST` is the most recent
/// completed tick's elapsed ticks; `MAX` is the high-water mark.
static TICK_LAST_CYCLES: AtomicU64 = AtomicU64::new(0);
static TICK_MAX_CYCLES: AtomicU64 = AtomicU64::new(0);

/// Read the timer frequency from CNTFRQ_EL0.
fn timer_freq() -> u64 {
    let freq: u64;
    unsafe {
        asm!(
            "mrs {val}, CNTFRQ_EL0",            // Read Counter-timer Frequency Register
            val = out(reg) freq,
        );
    }
    freq
}

/// Kernel-side read of the monotonic counter (CNTVCT_EL0).
///
/// Used by the per-tick deadline scan in `handle_tick` and by
/// `sys_wait_any`'s deadline-already-past fast path. EL1 always
/// has access to the counter regardless of CNTKCTL_EL1 — that
/// register only gates EL0 reads (see `enable_el0_counter_reads`).
pub fn kernel_now() -> lockjaw_types::time::MonoTicks {
    let ticks: u64;
    unsafe {
        asm!(
            "mrs {val}, CNTVCT_EL0",            // Read virtual counter (EL1 access never traps)
            val = out(reg) ticks,
        );
    }
    lockjaw_types::time::MonoTicks(ticks)
}

/// Arm the virtual timer to fire after `duration_ms` milliseconds.
unsafe fn arm_timer(duration_ms: u64) {
    let ticks = timer_freq() / 1000 * duration_ms;
    asm!(
        "msr CNTV_TVAL_EL0, {val}",             // Set countdown value (ticks until fire)
        val = in(reg) ticks,
    );
    asm!(
        "msr CNTV_CTL_EL0, {val}",              // Enable timer, unmask interrupt
        val = in(reg) 1u64,                      // bit 0 = ENABLE, bit 1 = IMASK (0 = unmasked)
    );
}

/// Allow EL0 to read CNTVCT_EL0 and CNTPCT_EL0 directly via `mrs`.
///
/// CNTKCTL_EL1.EL0VCTEN (bit 1) and EL0PCTEN (bit 0): 0 = the
/// counter read traps to EL1, 1 = EL0 reads succeed. The reset
/// value of CNTKCTL_EL1 is architecturally UNKNOWN and on QEMU /
/// most implementations boots zero, so without this write a
/// userspace `mrs CNTVCT_EL0` would synchronously fault.
///
/// We expose both VCTEN and PCTEN even though Lockjaw uses only
/// the virtual counter: granting CNTPCT_EL0 read costs nothing
/// (it's already side-effect-free hardware) and avoids surprising
/// any future userspace that prefers the physical counter.
///
/// Read-modify-write: CNTKCTL_EL1 also carries EVNTEN/EVNTDIR/
/// EVNTI (event-stream config) and EL0PTEN/EL0VTEN (EL0 access to
/// the timer compare/control regs). We only intend to grant
/// counter-read access here; clobbering the rest with a bare write
/// would silently change unrelated policy. So OR our two bits in.
///
/// CNTKCTL_EL1 is per-CPU, so this runs on the boot CPU here and
/// on each secondary in `init_secondary` below.
unsafe fn enable_el0_counter_reads() {
    let mut val: u64;
    asm!(
        "mrs {val}, CNTKCTL_EL1",   // Read current control bits (event-stream, EL0PTEN, EL0VTEN, …)
        val = out(reg) val,
    );
    val |= 0x3;                       // Set bit 0 (EL0PCTEN) and bit 1 (EL0VCTEN); leave others intact
    asm!(
        "msr CNTKCTL_EL1, {val}",   // Write back: EL0 counter reads now succeed, other policy unchanged
        val = in(reg) val,
    );
}

/// Initialize the virtual timer for periodic 10ms ticks.
///
/// # Safety
/// GIC must be initialized and exceptions must be installed.
/// Interrupts must be unmasked after this call for ticks to fire.
pub unsafe fn init() {
    let freq = timer_freq();
    crate::kprintln!("  Timer frequency: ", freq, " Hz");
    enable_el0_counter_reads();
    crate::kprintln!("  CNTKCTL_EL1: EL0 counter reads enabled");
    arm_timer(lockjaw_types::constants::TIMER_TICK_MS);
    crate::kprintln!("  Timer armed (10ms interval)");
}

/// Arm the timer on a secondary CPU. No printing — UART is not
/// serialized during secondary bring-up (GKL not yet held).
pub unsafe fn init_secondary() {
    enable_el0_counter_reads();
    arm_timer(lockjaw_types::constants::TIMER_TICK_MS);
}

/// Called from the IRQ handler when INTID 27 fires.
/// Increments the tick counter, rearms the timer, wakes any
/// deadline-expired sleepers, and triggers the scheduler.
///
/// Wake-before-schedule ordering matters: a TCB whose deadline
/// just expired must transition Blocked→Ready *before*
/// `scheduler::tick()` so it can be a candidate for *this* tick's
/// scheduling decision. Calling `unblock_thread` after `tick()`
/// would leave the just-expired sleeper Blocked through the
/// current scheduling pass — the woken thread wouldn't actually
/// run until the *next* tick, doubling the worst-case wakeup
/// latency for no reason and breaking the [50ms, 70ms] tolerance
/// the integration test pins.
pub fn handle_tick() {
    let start = kernel_now();
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);
    unsafe {
        arm_timer(lockjaw_types::constants::TIMER_TICK_MS);
        crate::sched::scheduler::wake_expired_deadlines(kernel_now());
        crate::sched::scheduler::tick();
    }
    let elapsed = kernel_now().0.saturating_sub(start.0);
    TICK_LAST_CYCLES.store(elapsed, Ordering::Relaxed);
    // Atomic max via fetch_max (Relaxed is fine: a single read-write
    // race may lose an update, but the high-water mark trends right
    // over many ticks).
    TICK_MAX_CYCLES.fetch_max(elapsed, Ordering::Relaxed);
}

/// Read the current tick count.
pub fn tick_count() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

/// Read last/max tick-handler elapsed cycles (CNTVCT ticks). Used
/// by `sys_sched_telemetry` so userspace can correlate measurement
/// variance with tick-handler cost.
pub fn tick_self_timing() -> (u64, u64) {
    (
        TICK_LAST_CYCLES.load(Ordering::Relaxed),
        TICK_MAX_CYCLES.load(Ordering::Relaxed),
    )
}
