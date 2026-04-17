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
///
/// The `UserPod` bound guarantees T is safe to construct from arbitrary
/// bytes — no niches, no references, no enums with restricted discriminants.
/// This closes the soundness hole in the original `T: Copy` bound, where
/// e.g. `bool` (valid bit patterns 0 or 1 only) would be UB to read from
/// untrusted memory.
pub unsafe fn copy_from_user<T: lockjaw_types::user_pod::UserPod>(ttbr0_paddr: PhysAddr, user_va: u64) -> Option<T> {
    // Validate the user VA is in the user range and doesn't cross a page boundary.
    // A cross-page read would continue into whatever physical page is adjacent
    // in kernel VA space, not the user's next page.
    let size = mem::size_of::<T>() as u64;
    if !lockjaw_types::wait::validate_user_buffer(user_va, size) {
        return None;
    }
    if !lockjaw_types::vmem::validate_intra_page(user_va, size) {
        return None;
    }

    // Translate through the caller's page table
    let kernel_va = translate_user_va(ttbr0_paddr, user_va)?;

    // Read through TTBR1 — immune to TTBR0 changes
    // SAFETY: translated via page table walk (TTBR1)
    Some(core::ptr::read(kernel_va as *const T))
}
