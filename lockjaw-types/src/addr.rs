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
}
