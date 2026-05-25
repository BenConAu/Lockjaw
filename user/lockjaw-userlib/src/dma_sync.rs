//! DMA cache-maintenance syscall wrappers.
//!
//! Pairs with the kernel-side `src/arch/aarch64/cache.rs` primitive
//! and `lockjaw-types/src/cache.rs` range math. Drivers call these
//! at device handoff points so a cacheable DMA buffer's data is
//! visible to the right side of the transfer:
//!
//! - After a device DMA write completes and BEFORE the CPU reads
//!   the buffer: `sys_dma_sync_for_cpu(pageset, offset, len)`
//!   invalidates the covering cache lines so subsequent CPU loads
//!   read fresh DRAM rather than a stale cache line.
//! - Before a device DMA read starts: `sys_dma_sync_for_device(...)`
//!   cleans (writes back) the covering cache lines so the device
//!   sees the CPU's writes.
//!
//! At C0 these syscalls return `SyscallError::NOT_SUPPORTED` —
//! the ABI surface is reserved, no behaviour yet. C1 lands the
//! real handlers along with the substrate flip (DmaPool cacheable +
//! kernel direct-map inclusion). See
//! `docs/cacheable-dma-migration-plan.md`.
//!
//! Both syscalls accept any handle the kernel can validate as a
//! DmaPool-origin PageSet. Buddy-origin Cacheable PageSets (used
//! by virtio-blk on QEMU virt) currently work without sync because
//! QEMU's DMA model is coherent; if non-coherent Buddy-origin DMA
//! ever appears, the syscall surface here will be extended rather
//! than duplicated.

use core::arch::asm;
use lockjaw_types::syscall::{SyscallError, SYS_DMA_SYNC_FOR_CPU, SYS_DMA_SYNC_FOR_DEVICE};

use crate::handle::PageSetHandle;

/// Invalidate the cache lines covering `[offset, offset+len)`
/// within `pageset` so subsequent CPU loads read fresh DRAM.
/// Call AFTER a device's DMA write has completed and BEFORE the
/// caller reads the buffer.
///
/// Returns `SyscallError::OK` on success, `INVALID_HANDLE` if the
/// pageset isn't a valid DmaPool-origin handle, `INVALID_PARAMETER`
/// if `offset+len` exceeds the pageset's byte size, or
/// `NOT_SUPPORTED` at C0 (handler not yet implemented).
pub fn sys_dma_sync_for_cpu(pageset: PageSetHandle, offset: u64, len: u64) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") pageset.0 => err,
            in("x1") offset,
            in("x2") len,
            in("x8") SYS_DMA_SYNC_FOR_CPU,
        );
    }
    SyscallError(err)
}

/// Clean (write back) the cache lines covering `[offset, offset+len)`
/// within `pageset` so a subsequent device DMA read sees the CPU's
/// writes. Call BEFORE kicking off the device transfer.
///
/// Returns `SyscallError::OK` on success, error otherwise (same
/// shape as `sys_dma_sync_for_cpu`).
pub fn sys_dma_sync_for_device(pageset: PageSetHandle, offset: u64, len: u64) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") pageset.0 => err,
            in("x1") offset,
            in("x2") len,
            in("x8") SYS_DMA_SYNC_FOR_DEVICE,
        );
    }
    SyscallError(err)
}
