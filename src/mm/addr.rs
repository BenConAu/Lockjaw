// Re-export pure types from lockjaw-types.
pub use lockjaw_types::addr::*;

// Kernel-specific constants (not in lockjaw-types).

/// RAM layout — read from platform discovery at runtime.
/// These functions replace the old compile-time constants.
pub fn ram_start() -> PhysAddr {
    PhysAddr::new(crate::arch::aarch64::platform::info().ram_base)
}

pub fn ram_size() -> u64 {
    crate::arch::aarch64::platform::info().ram_size
}

pub fn ram_end() -> PhysAddr {
    PhysAddr::new(crate::arch::aarch64::platform::info().ram_base
                + crate::arch::aarch64::platform::info().ram_size)
}

pub fn total_pages() -> usize {
    (ram_size() / PAGE_SIZE) as usize
}


/// Offset added to physical addresses to produce kernel virtual addresses.
pub const KERNEL_VA_OFFSET: u64 = 0xFFFF_0000_0000_0000;

/// A validated kernel-owned physical page intended for object
/// initialization. This newtype exists so factory functions like
/// `create_endpoint` can take a `ObjectInitPage` instead of a raw
/// `PhysAddr`, encoding the "this page is kernel-owned and validated"
/// invariant in the type system.
///
/// A `ObjectInitPage` does NOT imply that the backing PageSet has been
/// consumed yet — consumption typically happens after the factory
/// succeeds, to preserve rollback semantics. The invariant is:
/// "this paddr came from a validated, kernel-allocated page that is
/// safe to write into." The caller is responsible for calling
/// `consume_pageset` after successful use to prevent double-use
/// through the PageSet table.
///
/// If a second lifecycle path appears that needs a stronger
/// "ownership committed" guarantee, split this into
/// `PreparedObjectPage` (pre-consume) and `ObjectInitPage` (post-consume).
pub struct ObjectInitPage(PhysAddr);

impl ObjectInitPage {
    /// Wrap a physical address as a validated kernel object page.
    ///
    /// # Safety
    /// The page at `paddr` must be (a) kernel-owned (allocated by the
    /// page allocator, not mapped by userspace), (b) not referenced by
    /// any other `ObjectInitPage` or live kernel object, and (c) either
    /// zeroed or about to be fully overwritten by the factory function
    /// that receives this.
    pub unsafe fn new(paddr: PhysAddr) -> Self {
        ObjectInitPage(paddr)
    }

    /// The underlying physical address.
    pub fn paddr(&self) -> PhysAddr {
        self.0
    }
}

/// Derive the physical address of a kernel object from a raw pointer.
/// The pointer must point into the kernel's direct-mapped VA region
/// (i.e. obtained via KernelMut::raw_ptr()). Reverses the
/// `paddr + KERNEL_VA_OFFSET` cast that KernelMut::from_paddr performs.
///
/// Takes `*const T` (not `&T`) because the primary callers are blocking
/// IPC functions that hold objects as `*mut T` to avoid Stacked Borrows
/// violations across context switches.
#[inline]
pub(crate) fn paddr_of_raw<T>(ptr: *const T) -> PhysAddr {
    // SAFETY: ptr points into the kernel's direct-mapped VA region; subtracting
    // KERNEL_VA_OFFSET reverses the KernelMut::from_paddr cast.
    PhysAddr::new(ptr as usize as u64 - KERNEL_VA_OFFSET)
}
