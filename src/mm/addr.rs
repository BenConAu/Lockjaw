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
// PhysFrame
// ---------------------------------------------------------------------------

/// A 4 KB-aligned physical frame, identified by its frame number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct PhysFrame(u64);

impl PhysFrame {
    /// Return the frame containing the given physical address.
    pub const fn containing(addr: PhysAddr) -> Self {
        addr.containing_frame()
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
