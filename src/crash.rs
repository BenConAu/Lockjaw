/// Shared crash diagnostic helpers used by both the exception handler
/// and the panic handler.
///
/// IMPORTANT: These functions must never panic. They are called from
/// the panic handler. We write directly to the UART via core::fmt::Write
/// and discard errors with `let _ =`. No .unwrap() on any fallible path.

use crate::mm::kernel_ptr::KernelRef;
use crate::sched::tcb::Tcb;
use core::fmt::Write;

/// Decode a syscall number to its name.
pub fn syscall_name(num: u64) -> &'static str {
    match num {
        0  => "sys_debug_putc",
        1  => "sys_yield",
        2  => "sys_send",
        3  => "sys_receive",
        4  => "sys_call",
        5  => "sys_reply",
        6  => "sys_alloc_pages",
        7  => "sys_map_pages",
        8  => "sys_create_process",
        9  => "sys_create_notification",
        10 => "sys_signal_notification",
        11 => "sys_wait_notification",
        12 => "sys_bind_irq",
        13 => "sys_create_endpoint",
        14 => "sys_recv_nb",
        15 => "sys_wait_any",
        16 => "sys_export_handle",
        17 => "sys_get_boot_info",
        18 => "sys_register_device_page",
        19 => "sys_query_pageset_phys",
        _  => "unknown",
    }
}

/// Print thread ID and syscall breadcrumb. Never panics.
/// Uses try_current_tcb_paddr (returns Option) and writes to UART
/// directly with `let _ =` to discard write errors.
pub fn print_thread_context(prefix: &str) {
    let mut uart = crate::arch::aarch64::uart::Uart::new();

    unsafe {
        let thread_idx = crate::sched::scheduler::current_thread_index();

        if let Some(tcb_paddr) = crate::sched::scheduler::try_current_tcb_paddr() {
            let tcb = KernelRef::<Tcb>::from_paddr(tcb_paddr);
            let tcb_ref = tcb.get();

            // volatile reads: crash path must not assume Tcb is well-formed
            let name_bytes = core::ptr::read_volatile(&tcb_ref.name);
            let name_len = name_bytes.iter().position(|&b| b == 0).unwrap_or(16);
            let name = core::str::from_utf8_unchecked(&name_bytes[..name_len]);
            if name_len > 0 {
                let _ = writeln!(uart, "{}  Thread: #{} ({})", prefix, thread_idx, name);
            } else {
                let _ = writeln!(uart, "{}  Thread: #{}", prefix, thread_idx);
            }

            let sc = core::ptr::read_volatile(&tcb_ref.current_syscall);
            if sc != u64::MAX {
                let [a0, a1, a2, a3] = core::ptr::read_volatile(&tcb_ref.current_syscall_args);
                let _ = writeln!(uart, "{}  During syscall: {} (x0={:#x}, x1={:#x}, x2={:#x}, x3={:#x})",
                    prefix, syscall_name(sc), a0, a1, a2, a3);
            }
        } else {
            let _ = writeln!(uart, "{}  Thread: #{}", prefix, thread_idx);
        }
    }
}
