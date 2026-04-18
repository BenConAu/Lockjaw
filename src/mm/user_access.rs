/// Safe user memory access via page table walk + TTBR1.
///
/// The kernel must never dereference user pointers directly (via TTBR0)
/// because a context switch can change TTBR0 to a different process's
/// page table mid-access. Instead, we walk the caller's page table to
/// translate the user VA to a physical address, then read via the
/// kernel's direct map (TTBR1), which never changes.

use crate::arch::aarch64::vmem;
use crate::mm::addr::PhysAddr;
use core::mem;

// ---------------------------------------------------------------------------
// UserAddressSpace — safe wrapper over user memory access
// ---------------------------------------------------------------------------

/// A validated reference to a user address space (TTBR0 page table).
/// Constructed from `CurrentThread::address_space()`, which proves the
/// TTBR0 paddr came from a scheduler-registered TCB.
///
/// All methods are safe — the TTBR0 validity is established at
/// construction time, and the `UserPod` bound on `read` ensures the
/// result type is safe to construct from arbitrary bytes.
pub struct UserAddressSpace(PhysAddr);

impl UserAddressSpace {
    /// Wrap a known-valid TTBR0 physical address.
    ///
    /// # Safety
    /// `ttbr0_paddr` must point to a valid L0 page table.
    pub unsafe fn from_ttbr0(ttbr0_paddr: PhysAddr) -> Self {
        UserAddressSpace(ttbr0_paddr)
    }

    /// Read a value of type T from a user virtual address.
    ///
    /// Walks the caller's page table to translate user_va to a kernel VA,
    /// then reads T through TTBR1. Returns None if the user VA is not
    /// mapped or crosses a page boundary.
    ///
    /// The `UserPod` bound guarantees T is safe to construct from arbitrary
    /// bytes — no niches, no references, no enums with restricted
    /// discriminants.
    pub fn read<T: lockjaw_types::user_pod::UserPod>(&self, user_va: u64) -> Option<T> {
        let size = mem::size_of::<T>() as u64;
        // Validate the user VA is in the user range and doesn't cross a page
        // boundary. A cross-page read would continue into whatever physical
        // page is adjacent in kernel VA space, not the user's next page.
        if !lockjaw_types::wait::validate_user_buffer(user_va, size) {
            return None;
        }
        if !lockjaw_types::vmem::validate_intra_page(user_va, size) {
            return None;
        }
        // SAFETY: self.0 was validated at construction as a live L0 table.
        let kernel_va = unsafe { vmem::translate_user_va(self.0, user_va) }?;
        // SAFETY: translated via page table walk (TTBR1)
        Some(unsafe { core::ptr::read(kernel_va as *const T) })
    }

    /// The underlying TTBR0 physical address, for operations that need
    /// the raw paddr (e.g. map_pages_in_existing, create_process).
    pub fn ttbr0(&self) -> PhysAddr {
        self.0
    }
}

