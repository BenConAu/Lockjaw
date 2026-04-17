//! Safe facade for current-thread operations.
//!
//! Syscall handlers repeatedly need to read or write fields of the
//! currently-running thread's TCB. This module provides narrow,
//! per-field accessors that each create a short-lived KernelRef/KernelMut
//! internally — no `&Tcb` or `&mut Tcb` is ever exposed to the caller,
//! so aliasing is impossible regardless of call order or nesting.

use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::{KernelMut, KernelRef};
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;

/// Facade for accessing the currently-running thread's TCB.
/// Every method is safe; each internally constructs a KernelRef/KernelMut,
/// performs one operation, and drops it before returning.
pub struct CurrentThread;

impl CurrentThread {
    // --- Identity ---

    /// Physical address of the current thread's TCB.
    /// Useful when passing to IPC functions that take a TCB paddr.
    pub fn tcb_paddr() -> PhysAddr {
        scheduler::current_tcb_paddr()
    }

    // --- Read-only field accessors ---

    /// Physical address of the current thread's handle table.
    pub fn handle_table_paddr() -> PhysAddr {
        let tcb = Self::ref_();
        PhysAddr::new(tcb.get().handle_table_paddr)
    }

    /// Physical address of the current thread's TTBR0 (user page table).
    pub fn ttbr0() -> PhysAddr {
        let tcb = Self::ref_();
        PhysAddr::new(tcb.get().ttbr0_paddr)
    }

    /// The Reply paddr currently bound on this thread (set by sys_receive
    /// when dequeuing a Call waiter; read by sys_export_handle/sys_reply).
    pub fn current_reply_paddr() -> u64 {
        Self::ref_().get().current_reply_paddr
    }

    /// Number of valid entries in the wait_objects array (0 = not waiting).
    pub fn wait_count() -> usize {
        Self::ref_().get().wait_count as usize
    }

    /// Read a single wait entry by index (paddr + object type tag).
    /// Caller must ensure `i < wait_count()`.
    pub fn wait_entry(i: usize) -> (PhysAddr, u8) {
        let tcb = Self::ref_();
        let t = tcb.get();
        (PhysAddr::new(t.wait_objects[i]), t.wait_types[i])
    }

    // --- Write operations ---

    /// Record which syscall is currently executing (for crash diagnostics).
    pub fn set_breadcrumb(syscall: u64, args: [u64; 4]) {
        let mut tcb = Self::mut_();
        let t = tcb.get_mut();
        t.current_syscall = syscall;
        t.current_syscall_args = args;
    }

    /// Clear the syscall breadcrumb after the handler returns.
    pub fn clear_breadcrumb() {
        Self::mut_().get_mut().current_syscall = u64::MAX;
    }

    /// Store the sys_wait_any state into the TCB before blocking.
    pub fn store_wait_state(
        paddrs: &[PhysAddr],
        thresholds: &[u64],
        types: &[u8],
        count: usize,
    ) {
        let mut tcb = Self::mut_();
        let t = tcb.get_mut();
        for i in 0..count {
            t.wait_objects[i] = paddrs[i].as_u64();
            t.wait_thresholds[i] = thresholds[i];
            t.wait_types[i] = types[i];
        }
        t.wait_count = count as u8;
    }

    /// Clear the wait count after post-wake cleanup.
    pub fn clear_wait_count() {
        Self::mut_().get_mut().wait_count = 0;
    }

    // --- Internal helpers (not pub — prevent external nesting) ---

    fn ref_() -> KernelRef<'static, Tcb> {
        let paddr = scheduler::current_tcb_paddr();
        // SAFETY: scheduler guarantees current_tcb_paddr is a valid,
        // live TCB in a kernel-owned page.
        unsafe { KernelRef::from_paddr(paddr) }
    }

    fn mut_() -> KernelMut<'static, Tcb> {
        let paddr = scheduler::current_tcb_paddr();
        // SAFETY: same as ref_. Single-core execution ensures no
        // concurrent access.
        unsafe { KernelMut::from_paddr(paddr) }
    }
}
