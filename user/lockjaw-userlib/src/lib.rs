#![no_std]

pub mod syscall;
pub mod print;
pub mod process;

// Re-export the ELF parser from lockjaw-types (no duplicate).
pub use lockjaw_types::elf;

// Re-export shared constants and types from lockjaw-types.
pub use lockjaw_types::addr::PAGE_SIZE;
pub use lockjaw_types::vmem::MAP_FLAG_DEVICE;
pub use lockjaw_types::syscall::SyscallError;
pub use lockjaw_types::wait::WaitEntry;
pub use lockjaw_types::device::{PL011_HASH, CMD_CLAIM_DEVICE};

pub use syscall::*;
pub use print::*;
pub use process::{ProcessMapping, FLAG_EXECUTABLE};

/// Zero a page at the given virtual address.
/// Unsafe: caller must ensure the VA points to a valid mapped page.
pub unsafe fn zero_page_at_va(va: u64) {
    core::ptr::write_bytes(va as *mut u8, 0, PAGE_SIZE as usize);
}
