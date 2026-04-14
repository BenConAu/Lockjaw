#![no_std]

pub mod syscall;
pub mod print;
pub mod process;

// Re-export the ELF parser from lockjaw-types (no duplicate).
pub use lockjaw_types::elf;

// Re-export shared constants from lockjaw-types.
pub use lockjaw_types::addr::PAGE_SIZE;
pub use lockjaw_types::vmem::MAP_FLAG_DEVICE;
pub use lockjaw_types::syscall::SYS_ERR_WOULD_BLOCK;

pub use syscall::*;
pub use print::*;
pub use process::{ProcessMapping, FLAG_EXECUTABLE};
