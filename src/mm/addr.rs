// Re-export pure types from lockjaw-types.
pub use lockjaw_types::addr::*;

// Kernel-specific constants (not in lockjaw-types).

/// QEMU virt RAM: 128 MB starting at 0x4000_0000.
pub const RAM_START: PhysAddr = PhysAddr::new(0x4000_0000);
pub const RAM_END: PhysAddr = PhysAddr::new(0x4800_0000);
pub const RAM_SIZE: u64 = 0x0800_0000;
pub const TOTAL_PAGES: usize = (RAM_SIZE / PAGE_SIZE) as usize; // 32768

/// Offset added to physical addresses to produce kernel virtual addresses.
pub const KERNEL_VA_OFFSET: u64 = 0xFFFF_0000_0000_0000;

/// A physical page that has been donated to the kernel for object
/// initialization. This newtype exists so factory functions like
/// `create_endpoint` can take a `DonatedPage` instead of a raw
/// `PhysAddr`, encoding the "this page is owned and ready" invariant
/// in the type system. Only kernel code that has consumed a PageSet
/// (or allocated a page internally) can construct one.
pub struct DonatedPage(PhysAddr);

impl DonatedPage {
    /// Wrap a physical address as a donated page.
    ///
    /// # Safety
    /// The page at `paddr` must be (a) kernel-owned (not mapped by
    /// userspace), (b) not referenced by any other `DonatedPage` or
    /// live kernel object, and (c) either zeroed or about to be
    /// fully overwritten by the factory function that receives this.
    pub unsafe fn new(paddr: PhysAddr) -> Self {
        DonatedPage(paddr)
    }

    /// The underlying physical address.
    pub fn paddr(&self) -> PhysAddr {
        self.0
    }
}
