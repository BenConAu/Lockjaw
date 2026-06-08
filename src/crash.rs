/// Shared crash diagnostic helpers used by both the exception handler
/// and the panic handler.
///
/// IMPORTANT: These functions must never panic. They are called from
/// the panic handler. We write directly to the UART via puts/putc.
/// No core::fmt, no vtables, no .unwrap() on any fallible path.

use crate::sched::tcb::Tcb;
use crate::print::{KPrint, Hex};

// syscall_name lives in lockjaw-types/src/syscall.rs (host-testable,
// alongside the syscall number constants). Re-export for kernel use.
pub use lockjaw_types::syscall::syscall_name;

/// Print thread ID and syscall breadcrumb. Must never panic — a panic
/// here causes infinite recursion (panic → print_thread_context → panic)
/// which overflows the stack into the guard page.
///
/// All pointer access uses raw pointer reads with no alignment checks.
/// Debug builds insert alignment assertions on &T dereferences, so we
/// avoid creating references to Tcb fields entirely.
///
/// Re-entry guard: if a panic occurs inside this function (e.g., from
/// a debug-mode alignment check the compiler inserts), the guard
/// prevents infinite recursion (panic → print_thread_context → panic).
pub fn print_thread_context(prefix: &str) {
    use core::sync::atomic::{AtomicBool, Ordering};
    static IN_CRASH: AtomicBool = AtomicBool::new(false);
    if IN_CRASH.swap(true, Ordering::Relaxed) {
        return; // already in crash handler — don't recurse
    }
    let uart = crate::arch::aarch64::pl011::Pl011::new();

    unsafe {
        let thread_idx = crate::sched::scheduler::current_thread_index();

        if let Some(tcb_kva) = crate::sched::scheduler::try_current_tcb_kva() {
            // Read TCB fields via raw pointer arithmetic — no &Tcb reference,
            // no alignment checks that could panic in debug builds.
            // TCBs live in the KVM pool; the KVA is the dereferenceable
            // pointer directly (no KERNEL_VA_OFFSET translation).
            let tcb_va = tcb_kva.as_u64();
            // SAFETY: tcb_kva is from the scheduler; mapped in KVM.
            let tcb_ptr = tcb_va as *const u8;

            // Read the name field (offset = offset_of Tcb::name).
            // Use byte-by-byte read to avoid any alignment requirement.
            let name_offset = core::mem::offset_of!(Tcb, name);
            let mut name_buf = [0u8; 16];
            for i in 0..16 {
                name_buf[i] = core::ptr::read_volatile(tcb_ptr.add(name_offset + i));
            }
            let name_len = name_buf.iter().position(|&b| b == 0).unwrap_or(16);
            let name = core::str::from_utf8_unchecked(&name_buf[..name_len]);
            uart.puts(prefix);
            uart.puts("  Thread: #");
            KPrint::kprint(&thread_idx);
            if name_len > 0 {
                uart.puts(" (");
                uart.puts(name);
                uart.puts(")");
            }
            uart.puts("\n");

            // Read the current_syscall field (u64) byte-by-byte.
            let sc_offset = core::mem::offset_of!(Tcb, current_syscall);
            let mut sc_bytes = [0u8; 8];
            for i in 0..8 {
                sc_bytes[i] = core::ptr::read_volatile(tcb_ptr.add(sc_offset + i));
            }
            let sc = u64::from_ne_bytes(sc_bytes);

            if sc != u64::MAX {
                let args_offset = core::mem::offset_of!(Tcb, current_syscall_args);
                let mut args = [0u64; 4];
                for a in 0..4 {
                    let mut arg_bytes = [0u8; 8];
                    for i in 0..8 {
                        arg_bytes[i] = core::ptr::read_volatile(tcb_ptr.add(args_offset + a * 8 + i));
                    }
                    args[a] = u64::from_ne_bytes(arg_bytes);
                }
                uart.puts(prefix);
                uart.puts("  During syscall: ");
                uart.puts(syscall_name(sc));
                uart.puts(" (x0=");
                KPrint::kprint(&Hex(args[0]));
                uart.puts(", x1=");
                KPrint::kprint(&Hex(args[1]));
                uart.puts(", x2=");
                KPrint::kprint(&Hex(args[2]));
                uart.puts(", x3=");
                KPrint::kprint(&Hex(args[3]));
                uart.puts(")\n");
            }
        } else {
            uart.puts(prefix);
            uart.puts("  Thread: #");
            KPrint::kprint(&thread_idx);
            uart.puts("\n");
        }
    }
}
