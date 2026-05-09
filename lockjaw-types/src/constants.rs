/// Shared constants used by both kernel and userspace.

/// Stack canary value written at the bottom of every thread stack.
/// Checked on every context switch — if corrupted, the kernel panics.
pub const STACK_CANARY: u64 = 0xDEAD_BEEF_DEAD_BEEF;

/// Stack fill pattern for high-water-mark analysis.
pub const STACK_FILL_PATTERN: u64 = 0xCCCC_CCCC_CCCC_CCCC;

/// Default virtual address for userspace process stacks.
/// Each process gets a stack page mapped here.
pub const USER_STACK_BASE: u64 = 0x0080_0000;

/// Timer tick interval in milliseconds.
pub const TIMER_TICK_MS: u64 = 10;

/// End of user virtual address range. VAs at or above this are kernel-only.
/// Matches the 48-bit split: user space is [0, 0x4000_0000).
pub const USER_VA_END: u64 = 0x4000_0000;

/// Base virtual address of the POSIX personality's mmap region.
/// musl's mmap-backed allocators (malloc above the brk threshold,
/// stdio buffers, etc.) carve from this region upward via the
/// posix-server's bump allocator.
///
/// Layout (POSIX user processes only):
///   0x0000_0000 -- null guard / unused low VA
///   0x0040_0000 -- ELF image (4 MiB anchor)
///                  shared buffer + brk region (grows up; bounded
///                  below USER_STACK_BASE by compute_va_layout)
///   0x0080_0000 -- USER_STACK_BASE (4-page stack from
///                  lockjaw-types::constants)
///   0x0080_4000 -- gap (used as stack guard)
///   0x0100_0000 -- POSIX_MMAP_BASE (this constant)
///                  mmap region grows up; capped well below
///                  USER_VA_END
///   0x4000_0000 -- USER_VA_END
///
/// 16 MiB chosen to leave 8 MiB of headroom above the stack +
/// guard. The compute_va_layout invariant rejects any layout where
/// mmap_base would overlap the stack.
pub const POSIX_MMAP_BASE: u64 = 0x0100_0000;
