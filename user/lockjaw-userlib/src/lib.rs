#![no_std]

pub mod syscall;
pub mod print;
pub mod process;
pub mod virtual_memory;
pub mod block;
pub mod clock;
pub mod cprman;
pub mod devmgr;
pub mod dma;
pub mod dma_sync;
pub mod dma_transfer;
pub mod display;
pub mod driver_runtime;
pub mod fs;
pub mod fwcfg;
pub mod handle;
pub mod irq;
pub mod pl011;
pub mod sdhci;
pub mod time;
pub mod virtio;
pub mod virtio_blk;
pub mod virtqueue;

// Re-export the ELF parser and load planner from lockjaw-types
// (no duplicate; userspace loaders use these directly).
pub use lockjaw_types::elf;
pub use lockjaw_types::elf_loader;
pub use lockjaw_types::fdt;

// Re-export shared constants and types from lockjaw-types.
pub use lockjaw_types::addr::PAGE_SIZE;
pub use lockjaw_types::vmem::MapMemoryAttribute;
pub use lockjaw_types::syscall::SyscallError;
pub use lockjaw_types::wait::WaitEntry;
pub use lockjaw_types::process::ProcessCreateInfo;
pub use lockjaw_types::device::{
    PL011_HASH, FW_CFG_HASH, BCM2711_EMMC2_HASH,
    CMD_CLAIM_DEVICE, CLAIM_OK, CLAIM_ERR,
};

// Selective re-export — only the driver-regime allowlist surfaces at
// the crate root. Forbidden `sys_*` wrappers are reachable only via the
// explicit `lockjaw_userlib::syscall::` module path (or an aliased
// `use ...::syscall::sys_x as y`, which still names the path in the
// import). A driver writing `use lockjaw_userlib::*` therefore cannot
// pull any forbidden syscall into scope -- the regime's construction
// half. The follow-up `check-driver-unsafe` xtask adds a textual
// `syscall::` scan over driver source that catches the explicit-path
// escape too.
//
// Non-driver userspace crates (init, servers, tests) `use
// lockjaw_userlib::syscall::*;` for the full surface; everything they
// need beyond the allowlist (BootInfo, SchedTelemetry, IRQ_FLAG_EDGE,
// park_forever, the rest of sys_*) lives there.
pub use syscall::{sys_exit, sys_debug_puts};
pub use print::*;
pub use process::{ProcessMapping, FLAG_EXECUTABLE};
pub use lockjaw_types::process::PROCESS_MAPPINGS_PER_PAGE;
pub use virtual_memory::{unmap_pages_tracked, VaUnmapped, VMEM};
pub use handle::*;

/// Zero a page at the given virtual address.
/// Unsafe: caller must ensure the VA points to a valid mapped page.
pub unsafe fn zero_page_at_va(va: u64) {
    core::ptr::write_bytes(va as *mut u8, 0, PAGE_SIZE as usize);
}
