//! Safe facade for current-thread operations.
//!
//! Syscall handlers repeatedly need to read or write fields of the
//! currently-running thread's TCB. This module provides narrow,
//! per-field accessors that each create a short-lived KernelRef/KernelMut
//! internally — no `&Tcb` or `&mut Tcb` is ever exposed to the caller,
//! so aliasing is impossible regardless of call order or nesting.

use crate::cap::handle_table::HandleTableRef;
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

    /// KVA of the current thread's owning ProcessObject.
    /// ProcessObject lives in the KVM pool — see kernel-vmem-roadmap.md.
    pub fn process_kva() -> lockjaw_types::addr::KernelVa {
        let tcb = Self::ref_();
        lockjaw_types::addr::KernelVa::new(tcb.get().process_kva)
    }

    /// KVA of the current thread's handle table.
    /// Two hops: TCB → process → handle table, via narrow process_ops.
    /// HandleTable lives in the KVM pool (kernel-vmem-roadmap.md).
    pub fn handle_table_kva() -> lockjaw_types::addr::KernelVa {
        crate::cap::process_obj::process_handle_table(Self::process_kva())
    }

    /// Safe typed reference to the current thread's handle table.
    /// Provides lookup/insert/remove without raw KernelVa or unsafe.
    pub fn handle_table() -> HandleTableRef {
        let kva = Self::handle_table_kva();
        // SAFETY: the process's handle_table_kva was set at process
        // creation and always points to a valid HandleTable.
        unsafe { HandleTableRef::from_kva(kva) }
    }

    /// Physical address of the current thread's TTBR0 (user page table).
    /// Two hops: TCB → process → ttbr0, via narrow process_ops.
    pub fn ttbr0() -> PhysAddr {
        let ttbr0 = crate::cap::process_obj::process_ttbr0(Self::process_kva());
        PhysAddr::new(ttbr0)
    }

    /// Safe typed reference to the current thread's user address space.
    /// Returns `None` for kernel threads (ttbr0 == 0).
    pub fn address_space() -> Option<crate::mm::user_access::UserAddressSpace> {
        let ttbr0 = Self::ttbr0();
        if ttbr0.as_u64() == 0 {
            return None;
        }
        // SAFETY: process ttbr0 was set at process creation from a valid
        // L0 page table, and we just checked it's non-zero.
        Some(unsafe { crate::mm::user_access::UserAddressSpace::from_ttbr0(ttbr0) })
    }

    /// The Reply KVA currently bound on this thread (set by sys_receive
    /// when dequeuing a Call waiter; read by sys_export_handle/sys_reply).
    /// Reply objects live in the KVM pool — see kernel-vmem-roadmap.md.
    pub fn current_reply_kva() -> u64 {
        Self::ref_().get().current_reply_kva
    }

    /// Number of valid entries in the wait_objects array (0 = not waiting).
    pub fn wait_count() -> usize {
        Self::ref_().get().wait_count as usize
    }

    /// Read a single wait entry by index (polymorphic u64 address +
    /// object type tag). Caller decodes the address based on the type:
    /// Endpoint → `PhysAddr`; Notification → `KernelVa`.
    /// Caller must ensure `i < wait_count()`.
    pub fn wait_entry(i: usize) -> (u64, u8) {
        let tcb = Self::ref_();
        let t = tcb.get();
        (t.wait_objects[i], t.wait_types[i])
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
    /// `addrs` holds polymorphic u64 values — Endpoint paddrs and
    /// Notification KVAs — decoded by readers based on the matching
    /// `types[i]`.
    pub fn store_wait_state(
        addrs: &[u64],
        thresholds: &[u64],
        types: &[u8],
        count: usize,
    ) {
        let mut tcb = Self::mut_();
        let t = tcb.get_mut();
        for i in 0..count {
            t.wait_objects[i] = addrs[i];
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
