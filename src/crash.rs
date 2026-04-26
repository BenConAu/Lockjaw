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
        20 => "sys_create_reply",
        21 => "sys_exit",
        22 => "sys_create_thread",
        23 => "sys_query_mapping",
        24 => "sys_close_handle",
        25 => "sys_unmap_pages",
        _  => "unknown",
    }
}

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
    let mut uart = crate::arch::aarch64::uart::Uart::new();

    unsafe {
        let thread_idx = crate::sched::scheduler::current_thread_index();

        if let Some(tcb_paddr) = crate::sched::scheduler::try_current_tcb_paddr() {
            // Read TCB fields via raw pointer arithmetic — no &Tcb reference,
            // no alignment checks that could panic in debug builds.
            let tcb_va = tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET;
            // SAFETY: tcb_paddr is from the scheduler; kernel VA via KERNEL_VA_OFFSET
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
            if name_len > 0 {
                let _ = writeln!(uart, "{}  Thread: #{} ({})", prefix, thread_idx, name);
            } else {
                let _ = writeln!(uart, "{}  Thread: #{}", prefix, thread_idx);
            }

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
                let _ = writeln!(uart, "{}  During syscall: {} (x0={:#x}, x1={:#x}, x2={:#x}, x3={:#x})",
                    prefix, syscall_name(sc), args[0], args[1], args[2], args[3]);
            }
        } else {
            let _ = writeln!(uart, "{}  Thread: #{}", prefix, thread_idx);
        }
    }
}
