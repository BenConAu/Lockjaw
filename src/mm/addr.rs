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
