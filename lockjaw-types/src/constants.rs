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
