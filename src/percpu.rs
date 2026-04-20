use crate::arch::aarch64::platform::MAX_CPUS;
use core::arch::asm;
use core::cell::UnsafeCell;

// ---------------------------------------------------------------------------
// Per-CPU data structure
// ---------------------------------------------------------------------------

/// Per-CPU kernel state, accessed via TPIDR_EL1. Each core stores a pointer
/// to its own PerCpu in TPIDR_EL1 during init. Reading TPIDR_EL1 is one
/// `mrs` instruction — no memory access, no TLB walk.
///
/// Fields are not `pub` — access goes through narrow per-field accessors
/// (same pattern as CurrentThread) to prevent safe code from obtaining
/// two `&mut PerCpu` references to the same slot.
#[repr(C)]
struct PerCpu {
    cpu_id: u32,
    current_thread_idx: usize,
}

// ---------------------------------------------------------------------------
// Static per-CPU array
// ---------------------------------------------------------------------------

struct PerCpuArray([UnsafeCell<PerCpu>; MAX_CPUS]);

/// SAFETY: Each CPU accesses only its own slot (indexed by cpu_id stored
/// in TPIDR_EL1). No cross-CPU access to the same slot occurs.
unsafe impl Sync for PerCpuArray {}

static PERCPU_DATA: PerCpuArray = PerCpuArray([
    UnsafeCell::new(PerCpu { cpu_id: 0, current_thread_idx: 0 }),
    UnsafeCell::new(PerCpu { cpu_id: 1, current_thread_idx: 0 }),
    UnsafeCell::new(PerCpu { cpu_id: 2, current_thread_idx: 0 }),
    UnsafeCell::new(PerCpu { cpu_id: 3, current_thread_idx: 0 }),
]);

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initialize per-CPU data for this core and store the pointer in TPIDR_EL1.
/// Must be called exactly once per CPU, early in boot (after higher-half
/// mapping is active so the pointer is in kernel VA space).
pub fn init_percpu(cpu_id: u32) {
    assert!((cpu_id as usize) < MAX_CPUS, "cpu_id out of range");
    let ptr = PERCPU_DATA.0[cpu_id as usize].get();
    unsafe {
        (*ptr).cpu_id = cpu_id;
        (*ptr).current_thread_idx = 0;
        asm!(
            "msr TPIDR_EL1, {ptr}",
            ptr = in(reg) ptr as u64,
        );
    }
}

// ---------------------------------------------------------------------------
// Narrow per-field accessors (same pattern as CurrentThread)
// ---------------------------------------------------------------------------
// No &mut PerCpu is ever exposed to safe code. Each accessor reads
// TPIDR_EL1, dereferences the pointer for one field, and drops it
// immediately. This prevents the aliasing bug where two callers
// hold simultaneous &mut references to the same PerCpu.

/// Read this core's CPU ID.
#[inline]
pub fn cpu_id() -> u32 {
    // SAFETY: TPIDR_EL1 set by init_percpu; read-only field after init.
    unsafe { (*raw_percpu()).cpu_id }
}

/// Read this core's current thread index.
#[inline]
pub fn current_thread_idx() -> usize {
    // SAFETY: TPIDR_EL1 set by init_percpu; only this core writes this field.
    unsafe { (*raw_percpu()).current_thread_idx }
}

/// Set this core's current thread index. Called by the scheduler under
/// the GKL after a scheduling decision.
#[inline]
pub fn set_current_thread_idx(idx: usize) {
    // SAFETY: TPIDR_EL1 set by init_percpu; only this core writes this field.
    unsafe { (*raw_percpu()).current_thread_idx = idx; }
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// Read TPIDR_EL1 and return the raw pointer. Not public — callers use
/// the narrow accessors above.
#[inline]
fn raw_percpu() -> *mut PerCpu {
    let ptr: u64;
    unsafe {
        asm!(
            "mrs {ptr}, TPIDR_EL1",
            ptr = out(reg) ptr,
        );
    }
    // SAFETY: TPIDR_EL1 was set by init_percpu to a valid PerCpu pointer
    ptr as *mut PerCpu
}
