use core::fmt;

pub const PAGE_SIZE: u64 = 4096;
pub const PAGE_SHIFT: u64 = 12;

/// QEMU virt RAM: 128 MB starting at 0x4000_0000.
pub const RAM_START: PhysAddr = PhysAddr(0x4000_0000);
pub const RAM_END: PhysAddr = PhysAddr(0x4800_0000);
pub const RAM_SIZE: u64 = 0x0800_0000;
pub const TOTAL_FRAMES: usize = (RAM_SIZE / PAGE_SIZE) as usize; // 32768

/// Offset added to physical addresses to produce kernel virtual addresses.
pub const KERNEL_VA_OFFSET: u64 = 0xFFFF_0000_0000_0000;

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

    /// Round down to the containing 4 KB frame.
    pub const fn containing_frame(self) -> PhysFrame {
        PhysFrame(self.0 >> PAGE_SHIFT)
    }

    /// True if this address is 4 KB-aligned.
    pub const fn is_page_aligned(self) -> bool {
        self.0 & (PAGE_SIZE - 1) == 0
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
// VirtAddr
// ---------------------------------------------------------------------------

/// A virtual memory address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct VirtAddr(u64);

impl VirtAddr {
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Extract the 4-level page table indices for 4 KB granule.
    ///   L0 index = bits [47:39]   (each entry covers 512 GB)
    ///   L1 index = bits [38:30]   (each entry covers 1 GB)
    ///   L2 index = bits [29:21]   (each entry covers 2 MB)
    ///   L3 index = bits [20:12]   (each entry covers 4 KB)
    pub const fn page_indices(self) -> (usize, usize, usize, usize) {
        let addr = self.0;
        let l0 = ((addr >> 39) & 0x1FF) as usize;
        let l1 = ((addr >> 30) & 0x1FF) as usize;
        let l2 = ((addr >> 21) & 0x1FF) as usize;
        let l3 = ((addr >> 12) & 0x1FF) as usize;
        (l0, l1, l2, l3)
    }

    /// Offset within the 4 KB page (bits [11:0]).
    pub const fn page_offset(self) -> u64 {
        self.0 & (PAGE_SIZE - 1)
    }

    pub const fn is_page_aligned(self) -> bool {
        self.0 & (PAGE_SIZE - 1) == 0
    }
}

impl fmt::Debug for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtAddr({:#x})", self.0)
    }
}

impl fmt::LowerHex for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

// ---------------------------------------------------------------------------
// PhysFrame
// ---------------------------------------------------------------------------

/// A 4 KB-aligned physical frame, identified by its frame number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct PhysFrame(u64);

impl PhysFrame {
    /// Create from a frame number (not an address).
    pub const fn from_number(n: u64) -> Self {
        Self(n)
    }

    /// Return the frame containing the given physical address.
    pub const fn containing(addr: PhysAddr) -> Self {
        addr.containing_frame()
    }

    /// Frame number.
    pub const fn number(self) -> u64 {
        self.0
    }

    /// Physical address of the first byte in this frame.
    pub const fn start_addr(self) -> PhysAddr {
        PhysAddr(self.0 << PAGE_SHIFT)
    }
}

impl fmt::Debug for PhysFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysFrame(#{}, {:#x})", self.0, self.start_addr().as_u64())
    }
}
