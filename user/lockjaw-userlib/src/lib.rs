#![no_std]

pub mod syscall;
pub mod print;
pub mod process;
pub mod virtual_memory;
pub mod block;
pub mod clock;
pub mod display;
pub mod fs;
pub mod handle;
pub mod time;
pub mod virtqueue;

// Re-export the ELF parser and load planner from lockjaw-types
// (no duplicate; userspace loaders use these directly).
pub use lockjaw_types::elf;
pub use lockjaw_types::elf_loader;

// Re-export shared constants and types from lockjaw-types.
pub use lockjaw_types::addr::PAGE_SIZE;
pub use lockjaw_types::vmem::MAP_FLAG_DEVICE;
pub use lockjaw_types::syscall::SyscallError;
pub use lockjaw_types::wait::WaitEntry;
pub use lockjaw_types::device::{PL011_HASH, FW_CFG_HASH, CMD_CLAIM_DEVICE, CLAIM_OK, CLAIM_ERR};

pub use syscall::*;
pub use print::*;
pub use process::{ProcessMapping, FLAG_EXECUTABLE};
pub use lockjaw_types::process::PROCESS_MAPPINGS_PER_PAGE;
pub use virtual_memory::VMEM;
pub use handle::*;

/// Zero a page at the given virtual address.
/// Unsafe: caller must ensure the VA points to a valid mapped page.
pub unsafe fn zero_page_at_va(va: u64) {
    core::ptr::write_bytes(va as *mut u8, 0, PAGE_SIZE as usize);
}
