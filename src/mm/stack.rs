use core::ptr;

use lockjaw_types::constants::{STACK_CANARY, STACK_FILL_PATTERN};

extern "C" {
    static __stack_bottom_0: u8;
    static __stack_top_0: u8;
    static __per_cpu_stacks: u8;
}

/// Per-CPU block stride: guard page (4 KB) + stack (8 KB) = 12 KB.
/// Must match linker.ld and boot.rs secondary stack calculation.
#[allow(dead_code)]
const PER_CPU_STACK_STRIDE: u64 = 12288;

/// Usable stack size per CPU (excluding guard page).
#[allow(dead_code)]
const PER_CPU_STACK_SIZE: u64 = 8192;

/// Write the stack canary at the bottom of CPU 0's stack and fill the rest
/// with a known pattern for high-water-mark analysis.
///
/// # Safety
/// Must be called after MMU and guard pages are set up, but before any deep
/// call chains. The stack must not have grown past `__stack_bottom_0 + 8`.
pub unsafe fn init_canary() {
    init_canary_at(
        &raw const __stack_bottom_0 as u64,
        &raw const __stack_top_0 as u64,
    );
}

/// Write the stack canary for a secondary CPU's stack.
///
/// # Safety
/// Must be called after the secondary's guard page is unmapped and MMU is
/// active. The secondary must not be using its stack yet (or only minimally).
#[allow(dead_code)] // Used by secondary core boot (commit 2)
pub unsafe fn init_canary_for_cpu(cpu_id: u32) {
    let base = &raw const __per_cpu_stacks as u64;
    let bottom = base + (cpu_id as u64) * PER_CPU_STACK_STRIDE + 4096; // skip guard
    let top = bottom + PER_CPU_STACK_SIZE;
    init_canary_at(bottom, top);
}

/// Write canary and fill pattern for a stack starting at `bottom`.
/// Reads SP to determine how far up the stack is safe to fill.
unsafe fn init_canary_at(bottom: u64, _top: u64) {
    // Write canary at the very bottom of the stack (first 8 bytes)
    // SAFETY: kernel stack address
    ptr::write_volatile(bottom as *mut u64, STACK_CANARY);

    // Read the current stack pointer so we never overwrite live frames.
    // Extra 256-byte margin for this function's own locals and any
    // compiler-inserted spills.
    let sp: u64;
    core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack));
    let safe_limit = sp - 256;

    // Fill from just above the canary up to below the live stack region.
    let mut addr = bottom + 8;
    while addr < safe_limit {
        // SAFETY: kernel stack address below any live frame
        ptr::write_volatile(addr as *mut u64, STACK_FILL_PATTERN);
        addr += 8;
    }
}

/// Check that the CPU 0 stack canary is intact. Panics if corrupted.
/// Called on context switch and from the panic handler.
pub fn check_canary() {
    unsafe {
        // SAFETY: linker symbol
        let canary_ptr = &raw const __stack_bottom_0 as *const u64;
        let value = ptr::read_volatile(canary_ptr);
        if value != STACK_CANARY {
            use crate::print::{KPrint, Addr};
            crate::arch::aarch64::pl011::Pl011::new().puts("[PANIC] Stack canary corrupted!\n");
            crate::arch::aarch64::pl011::Pl011::new().puts("  Expected: ");
            KPrint::kprint(&Addr(STACK_CANARY));
            crate::arch::aarch64::pl011::Pl011::new().puts("\n  Got:      ");
            KPrint::kprint(&Addr(value));
            crate::arch::aarch64::pl011::Pl011::new().puts("\n");
            panic!("stack canary corrupted");
        }
    }
}

/// Check the kernel stack canary and report status without panicking.
/// Used by crash diagnostics — if the canary is corrupted during a crash,
/// we want to print that fact, not trigger a second panic.
/// Writes directly to UART (not kprintln) to avoid re-entrant panics.
pub fn check_canary_report(prefix: &str) {
    let uart = crate::arch::aarch64::pl011::Pl011::new();
    unsafe {
        // SAFETY: linker symbol
        let canary_ptr = &raw const __stack_bottom_0 as *const u64;
        let value = ptr::read_volatile(canary_ptr);
        uart.puts(prefix);
        if value == STACK_CANARY {
            uart.puts("  Kernel stack canary: INTACT\n");
        } else {
            uart.puts("  Kernel stack canary: *** CORRUPTED *** — register dump may be unreliable\n");
        }
    }
}
