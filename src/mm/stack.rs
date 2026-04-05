use core::ptr;

const CANARY_VALUE: u64 = 0xDEAD_BEEF_DEAD_BEEF;
const FILL_PATTERN: u64 = 0xCCCC_CCCC_CCCC_CCCC;

extern "C" {
    static __stack_bottom: u8;
    static __stack_top: u8;
}

/// Write the stack canary at `__stack_bottom` and fill the rest of the stack
/// with a known pattern for high-water-mark analysis.
///
/// # Safety
/// Must be called after MMU and guard page are set up, but before any deep
/// call chains. The stack must not have grown past `__stack_bottom + 8`.
pub unsafe fn init_canary() {
    let bottom = &raw const __stack_bottom as u64;
    let top = &raw const __stack_top as u64;

    // Write canary at the very bottom of the stack (first 8 bytes)
    ptr::write_volatile(bottom as *mut u64, CANARY_VALUE);

    // Fill remaining stack with pattern.
    // Leave headroom below current SP for this function's own frame.
    let mut addr = bottom + 8;
    let safe_limit = top - 256;
    while addr < safe_limit {
        ptr::write_volatile(addr as *mut u64, FILL_PATTERN);
        addr += 8;
    }
}

/// Check that the stack canary is intact. Panics if corrupted.
/// Intended to be called periodically (e.g. on every context switch in Phase 5).
pub fn check_canary() {
    unsafe {
        let canary_ptr = &raw const __stack_bottom as *const u64;
        let value = ptr::read_volatile(canary_ptr);
        if value != CANARY_VALUE {
            panic!(
                "Stack canary corrupted! Expected {:#018x}, got {:#018x}",
                CANARY_VALUE, value
            );
        }
    }
}
