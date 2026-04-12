use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// Global tick counter — incremented by the timer IRQ handler.
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

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

/// Initialize the virtual timer for periodic 10ms ticks.
///
/// # Safety
/// GIC must be initialized and exceptions must be installed.
/// Interrupts must be unmasked after this call for ticks to fire.
pub unsafe fn init() {
    let freq = timer_freq();
    crate::kprintln!("  Timer frequency: {} Hz", freq);
    arm_timer(lockjaw_types::constants::TIMER_TICK_MS);
    crate::kprintln!("  Timer armed (10ms interval)");
}

/// Called from the IRQ handler when INTID 27 fires.
/// Increments the tick counter, rearms the timer, and triggers the scheduler.
pub fn handle_tick() {
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);
    unsafe {
        arm_timer(lockjaw_types::constants::TIMER_TICK_MS);
        crate::sched::scheduler::tick();
    }
}

/// Read the current tick count.
pub fn tick_count() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}
