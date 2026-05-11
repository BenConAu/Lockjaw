use core::fmt;

pub const PAGE_SIZE: u64 = 4096;
pub const PAGE_SHIFT: u64 = 12;

// ---------------------------------------------------------------------------
// PhysAddr
// ---------------------------------------------------------------------------

/// A physical memory address. Wraps a `u64` to prevent raw pointer arithmetic
/// on untyped integers outside this module.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct PhysAddr(u64);

impl PhysAddr {
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Round down to the containing 4 KB page.
    pub const fn containing_page(self) -> PhysPage {
        PhysPage(self.0 >> PAGE_SHIFT)
    }
}

impl fmt::Debug for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysAddr({:#x})", self.0)
    }
}

impl fmt::LowerHex for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

// ---------------------------------------------------------------------------
// PhysPage
// ---------------------------------------------------------------------------

/// A 4 KB-aligned physical page, identified by its page number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct PhysPage(u64);

impl PhysPage {
    /// Return the page containing the given physical address.
    pub const fn containing(addr: PhysAddr) -> Self {
        addr.containing_page()
    }

    /// Physical address of the first byte in this page.
    pub const fn start_addr(self) -> PhysAddr {
        PhysAddr(self.0 << PAGE_SHIFT)
    }
}

impl fmt::Debug for PhysPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysPage(#{}, {:#x})", self.0, self.start_addr().as_u64())
    }
}

// ---------------------------------------------------------------------------
// KernelVa
// ---------------------------------------------------------------------------

/// A virtual address in the kernel VA pool (KVM region). Distinct
/// from `PhysAddr` and from the linear higher-half map's
/// `paddr + KERNEL_VA_OFFSET` view: code that obtained one of these
/// has gone through `kvm::alloc_kernel_pages`, which stitched
/// independently-allocated physical frames into a virtually-
/// contiguous range. A `KernelVa` and a `PhysAddr` cannot be
/// converted back and forth — the two address regimes coexist and
/// must not be conflated.
///
/// The separation is enforced at compile time. The following
/// snippets must not compile:
///
/// ```compile_fail
/// use lockjaw_types::addr::{KernelVa, PhysAddr};
/// let kva = KernelVa::new(0xFFFF_8000_0000_0000);
/// let _: PhysAddr = kva; // PhysAddr ≠ KernelVa
/// ```
///
/// ```compile_fail
/// use lockjaw_types::addr::{KernelVa, PhysAddr};
/// let paddr = PhysAddr::new(0x4020_0000);
/// let _: KernelVa = paddr; // KernelVa ≠ PhysAddr
/// ```
///
/// ```compile_fail
/// use lockjaw_types::addr::{KernelVa, PhysAddr};
/// let kva = KernelVa::new(0xFFFF_8000_0000_0000);
/// let paddr = PhysAddr::new(0x4020_0000);
/// let _ = kva == paddr; // not comparable
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct KernelVa(u64);

impl KernelVa {
    pub const fn new(va: u64) -> Self {
        Self(va)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Advance by `pages` 4 KB pages.
    pub const fn add_pages(self, pages: usize) -> Self {
        Self(self.0 + (pages as u64) * PAGE_SIZE)
    }

    /// Byte difference between two KVAs (self minus other), in pages.
    /// Caller must ensure self >= other.
    pub const fn pages_since(self, other: Self) -> usize {
        ((self.0 - other.0) >> PAGE_SHIFT) as usize
    }
}

impl fmt::Debug for KernelVa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KernelVa({:#x})", self.0)
    }
}

impl fmt::LowerHex for KernelVa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

// ---------------------------------------------------------------------------
// KernelImageVa
// ---------------------------------------------------------------------------

/// A virtual address derived from a kernel-image linker symbol.
/// Distinct from `KernelVa` (KVM allocator pool) and `PhysAddr`
/// (physical memory) because the kernel image's address regime is
/// independent of both: linker symbols resolve to a fixed
/// higher-half VA chosen at link time, not to a paddr-shifted view
/// of the linear map and not to a runtime-allocated KVM pool slot.
///
/// The relink work (see `docs/relink-notes.md`,
/// `~/.claude/plans/nifty-rolling-naur.md`) moves the kernel image
/// into a dedicated L0[1] region; until that lands, the runtime
/// values still happen to coincide with `paddr + KERNEL_VA_OFFSET`
/// via the linear map. The newtype's *meaning* is "an address
/// derived from a kernel image linker symbol" regardless of where
/// in the higher half that symbol actually resolves at any given
/// boot stage. The PA-recovery helper
/// (`mmu::kernel_image_kva_to_pa`) is what hides the regime
/// difference at a single point.
///
/// The separation is enforced at compile time. The following
/// snippets must not compile:
///
/// ```compile_fail
/// use lockjaw_types::addr::{KernelImageVa, PhysAddr};
/// let iva = KernelImageVa::new(0xFFFF_0080_0000_0000);
/// let _: PhysAddr = iva; // PhysAddr ≠ KernelImageVa
/// ```
///
/// ```compile_fail
/// use lockjaw_types::addr::{KernelImageVa, KernelVa};
/// let iva = KernelImageVa::new(0xFFFF_0080_0000_0000);
/// let _: KernelVa = iva; // KernelVa ≠ KernelImageVa
/// ```
///
/// ```compile_fail
/// use lockjaw_types::addr::{KernelImageVa, KernelVa};
/// let iva = KernelImageVa::new(0xFFFF_0080_0000_0000);
/// let kva = KernelVa::new(0xFFFF_8000_0000_0000);
/// let _ = iva == kva; // not comparable
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct KernelImageVa(u64);

impl KernelImageVa {
    pub const fn new(va: u64) -> Self {
        Self(va)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for KernelImageVa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KernelImageVa({:#x})", self.0)
    }
}

impl fmt::LowerHex for KernelImageVa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phys_addr_round_trip() {
        let addr = PhysAddr::new(0x4008_1234);
        assert_eq!(addr.as_u64(), 0x4008_1234);
    }

    #[test]
    fn containing_page_strips_low_bits() {
        let addr = PhysAddr::new(0x4008_1FFF);
        let page = addr.containing_page();
        assert_eq!(page.start_addr().as_u64(), 0x4008_1000);
    }

    #[test]
    fn containing_page_preserves_aligned() {
        let addr = PhysAddr::new(0x4008_0000);
        let page = addr.containing_page();
        assert_eq!(page.start_addr().as_u64(), 0x4008_0000);
    }

    #[test]
    fn phys_page_start_addr_round_trip() {
        let addr = PhysAddr::new(0x4000_0000);
        let page = PhysPage::containing(addr);
        assert_eq!(page.start_addr(), addr);
    }

    #[test]
    fn page_constants() {
        assert_eq!(PAGE_SIZE, 4096);
        assert_eq!(PAGE_SHIFT, 12);
        assert_eq!(1u64 << PAGE_SHIFT, PAGE_SIZE);
    }

    #[test]
    fn phys_addr_ordering() {
        let a = PhysAddr::new(0x1000);
        let b = PhysAddr::new(0x2000);
        assert!(a < b);
        assert!(b > a);
        assert_eq!(a, PhysAddr::new(0x1000));
    }

    #[test]
    fn zero_address() {
        let addr = PhysAddr::new(0);
        assert_eq!(addr.containing_page().start_addr().as_u64(), 0);
    }

    #[test]
    fn kernel_va_round_trip() {
        let kva = KernelVa::new(0xFFFF_8000_0010_0000);
        assert_eq!(kva.as_u64(), 0xFFFF_8000_0010_0000);
    }

    #[test]
    fn kernel_va_add_pages() {
        let base = KernelVa::new(0xFFFF_8000_0000_0000);
        assert_eq!(base.add_pages(0).as_u64(), 0xFFFF_8000_0000_0000);
        assert_eq!(base.add_pages(1).as_u64(), 0xFFFF_8000_0000_1000);
        assert_eq!(base.add_pages(33).as_u64(), 0xFFFF_8000_0002_1000);
    }

    #[test]
    fn kernel_va_pages_since() {
        let base = KernelVa::new(0xFFFF_8000_0000_0000);
        let advanced = base.add_pages(7);
        assert_eq!(advanced.pages_since(base), 7);
    }

    // Type-separation assertions (KernelVa ≠ PhysAddr) are
    // `compile_fail` doctests on `KernelVa` itself — the test
    // harness compiles each snippet and asserts it errors. See
    // `lockjaw-types/src/addr.rs::KernelVa` for the snippets.
}
