/// Safe user memory access via page table walk + TTBR1.
///
/// The kernel must never dereference user pointers directly (via TTBR0)
/// because a context switch can change TTBR0 to a different process's
/// page table mid-access. Instead, we walk the caller's page table to
/// translate the user VA to a physical address, then read via the
/// kernel's direct map (TTBR1), which never changes.

use crate::arch::aarch64::vmem::translate_user_va;
use crate::mm::addr::PhysAddr;
use core::mem;

/// Read a value of type T from a user virtual address.
///
/// Walks the caller's page table (identified by ttbr0_paddr) to
/// translate user_va to a kernel VA, then reads T through TTBR1.
/// Returns None if the user VA is not mapped.
///
/// # Safety
/// `ttbr0_paddr` must be a valid L0 page table.
/// T must be safe to read from arbitrary bytes (no references, no padding invariants).
pub unsafe fn copy_from_user<T: Copy>(ttbr0_paddr: PhysAddr, user_va: u64) -> Option<T> {
    // Validate the user VA is in the user range
    let size = mem::size_of::<T>() as u64;
    if !lockjaw_types::wait::validate_user_buffer(user_va, size) {
        return None;
    }

    // Translate through the caller's page table
    let kernel_va = translate_user_va(ttbr0_paddr, user_va)?;

    // Read through TTBR1 — immune to TTBR0 changes
    Some(core::ptr::read(kernel_va as *const T))
}
