/// Syscall error type — distinct from return values at the type level.
///
/// Returned in x1 on syscall exit. x1 = 0 means success, nonzero = error.
/// The return VALUE (if any) is in x0 and is only meaningful when x1 = 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct SyscallError(pub u64);

impl SyscallError {
    /// No error — syscall succeeded.
    pub const OK: Self = SyscallError(0);

    /// The handle index is invalid or the slot is empty.
    pub const INVALID_HANDLE: Self = SyscallError(1);

    /// The handle does not have the required rights for this operation.
    pub const INSUFFICIENT_RIGHTS: Self = SyscallError(2);

    /// The page allocator has no free pages.
    pub const OUT_OF_MEMORY: Self = SyscallError(3);

    /// A parameter is invalid (wrong type, out of range, etc).
    pub const INVALID_PARAMETER: Self = SyscallError(4);

    /// The endpoint is busy (e.g. already has a sender/receiver blocked).
    pub const ENDPOINT_BUSY: Self = SyscallError(5);

    /// No caller is waiting for a reply on this endpoint.
    pub const NO_CALLER: Self = SyscallError(6);

    /// The scheduler's run queue is full.
    pub const QUEUE_FULL: Self = SyscallError(7);

    /// The notification value is not monotonically increasing.
    pub const NOT_MONOTONIC: Self = SyscallError(8);

    /// A thread is already waiting on this notification (single-waiter limit).
    pub const ALREADY_WAITING: Self = SyscallError(9);

    /// The operation would block but non-blocking was requested.
    pub const WOULD_BLOCK: Self = SyscallError(10);

    /// A Reply object passed to sys_call is already bound to an outstanding
    /// call. Each client thread uses one Reply at a time — the previous call
    /// must complete (receive reply) before reusing.
    pub const REPLY_BOUND: Self = SyscallError(11);

    /// The caller's handle table is full. Distinct from OUT_OF_MEMORY
    /// (which means the page allocator is exhausted).
    pub const HANDLE_TABLE_FULL: Self = SyscallError(12);

    /// The syscall exists in the ABI but the kernel-side handler is
    /// not yet implemented on this build. Used by the cacheable-DMA
    /// migration's Phase C0 (`sys_dma_sync_for_cpu` / `for_device`
    /// stubs) to surface the "ABI surface reserved; no behaviour
    /// yet" state cleanly. Callers are expected to treat this as a
    /// fatal error during migration; promoted to OK at C1 when the
    /// real handlers go live.
    pub const NOT_SUPPORTED: Self = SyscallError(13);

    /// The kernel's pre-allocated PageSet header pool is exhausted
    /// (`MAX_PAGESETS` slots all claimed), or the PageSet table is
    /// full. Distinct from `OUT_OF_MEMORY` (the page allocator
    /// itself) and from `HANDLE_TABLE_FULL` (the caller's handle
    /// table). Bounded resource: userspace must close PageSet
    /// handles before retrying. Introduced by NK2 of the
    /// no-kernel-alloc migration; see
    /// `docs/architecture/no-kernel-alloc.md`.
    pub const OUT_OF_PAGE_SETS: Self = SyscallError(14);

    /// An unknown or unrecoverable error occurred.
    pub const UNKNOWN: Self = SyscallError(u64::MAX);

    /// Check if this represents success (no error).
    pub fn is_ok(self) -> bool {
        self.0 == 0
    }
}

// Keep the old SYS_ERR_* constants as aliases during migration.
// TODO: Remove once all kernel code uses SyscallError directly.
pub const SYS_OK: u64 = 0;
pub const SYS_ERR_INVALID_HANDLE: u64 = 1;
pub const SYS_ERR_INSUFFICIENT_RIGHTS: u64 = 2;
pub const SYS_ERR_OUT_OF_MEMORY: u64 = 3;
pub const SYS_ERR_INVALID_PARAMETER: u64 = 4;
pub const SYS_ERR_ENDPOINT_BUSY: u64 = 5;
pub const SYS_ERR_NO_CALLER: u64 = 6;
pub const SYS_ERR_QUEUE_FULL: u64 = 7;
pub const SYS_ERR_NOT_MONOTONIC: u64 = 8;
pub const SYS_ERR_ALREADY_WAITING: u64 = 9;
pub const SYS_ERR_WOULD_BLOCK: u64 = 10;
pub const SYS_ERR_UNKNOWN: u64 = u64::MAX;

/// Syscall numbers.
pub const SYS_DEBUG_PUTS: u64 = 0;
pub const SYS_YIELD: u64 = 1;
pub const SYS_SEND: u64 = 2;
pub const SYS_RECEIVE: u64 = 3;
pub const SYS_CALL: u64 = 4;
pub const SYS_REPLY: u64 = 5;
pub const SYS_ALLOC_PAGES: u64 = 6;
pub const SYS_MAP_PAGES: u64 = 7;
pub const SYS_CREATE_PROCESS: u64 = 8;
pub const SYS_CREATE_NOTIFICATION: u64 = 9;
pub const SYS_SIGNAL_NOTIFICATION: u64 = 10;
pub const SYS_WAIT_NOTIFICATION: u64 = 11;
pub const SYS_BIND_IRQ: u64 = 12;
pub const SYS_CREATE_ENDPOINT: u64 = 13;
pub const SYS_RECV_NB: u64 = 14;
pub const SYS_WAIT_ANY: u64 = 15;
pub const SYS_EXPORT_HANDLE: u64 = 16;
pub const SYS_GET_BOOT_INFO: u64 = 17;
pub const SYS_REGISTER_DEVICE_PAGE: u64 = 18;
pub const SYS_QUERY_PAGESET_PHYS: u64 = 19;
pub const SYS_CREATE_REPLY: u64 = 20;
pub const SYS_EXIT: u64 = 21;
pub const SYS_CREATE_THREAD: u64 = 22;
pub const SYS_QUERY_MAPPING: u64 = 23;
pub const SYS_CLOSE_HANDLE: u64 = 24;
pub const SYS_UNMAP_PAGES: u64 = 25;
pub const SYS_QUERY_CALLER_TOKEN: u64 = 26;
pub const SYS_ALLOC_DMA_PAGES: u64 = 27;
pub const SYS_SCHED_TELEMETRY: u64 = 28;
/// Make DMA writes visible to the CPU. Invalidates the cache lines
/// covering `[offset, offset+len)` within the named DmaPool-origin
/// PageSet so a subsequent CPU load reads fresh DRAM rather than a
/// stale cache line. See `docs/history/cacheable-dma-migration-plan.md`.
/// Returns `NOT_SUPPORTED` until C1 lands the real handler.
pub const SYS_DMA_SYNC_FOR_CPU: u64 = 29;
/// Make pending CPU writes visible to the device. Cleans
/// (write-back) the cache lines covering `[offset, offset+len)`
/// within the named DmaPool-origin PageSet so the device's
/// subsequent DMA read sees what the CPU wrote. Returns
/// `NOT_SUPPORTED` until C1 lands the real handler.
pub const SYS_DMA_SYNC_FOR_DEVICE: u64 = 30;
/// Re-enable a previously masked level-triggered IRQ in the GIC.
/// Required after handling a level-triggered IRQ: the kernel masks
/// the intid in `irq_dispatch` so the level-still-asserted source
/// can't refire immediately after EOIR; userspace clears the source
/// (e.g. writes 1 to NORMAL_INT_STATUS) and then calls this to
/// re-enable. No-op for edge-triggered IRQs (they aren't masked).
/// (Was SYS_UNMASK_IRQ = 29 on m7-irq-experiment branch; renumbered
/// to 31 here because 29/30 were taken by the cacheable-DMA syscalls
/// landed in C0 of the migration after the branch diverged.)
pub const SYS_UNMASK_IRQ: u64 = 31;

/// Flag for SYS_ALLOC_PAGES: allocated pages must be physically contiguous.
pub const ALLOC_FLAG_CONTIGUOUS: u64 = 1 << 0;

/// Decode a syscall number to its name. Used by crash diagnostics.
pub fn syscall_name(num: u64) -> &'static str {
    match num {
        SYS_DEBUG_PUTS => "sys_debug_puts",
        SYS_YIELD => "sys_yield",
        SYS_SEND => "sys_send",
        SYS_RECEIVE => "sys_receive",
        SYS_CALL => "sys_call",
        SYS_REPLY => "sys_reply",
        SYS_ALLOC_PAGES => "sys_alloc_pages",
        SYS_MAP_PAGES => "sys_map_pages",
        SYS_CREATE_PROCESS => "sys_create_process",
        SYS_CREATE_NOTIFICATION => "sys_create_notification",
        SYS_SIGNAL_NOTIFICATION => "sys_signal_notification",
        SYS_WAIT_NOTIFICATION => "sys_wait_notification",
        SYS_BIND_IRQ => "sys_bind_irq",
        SYS_UNMASK_IRQ => "sys_unmask_irq",
        SYS_CREATE_ENDPOINT => "sys_create_endpoint",
        SYS_RECV_NB => "sys_recv_nb",
        SYS_WAIT_ANY => "sys_wait_any",
        SYS_EXPORT_HANDLE => "sys_export_handle",
        SYS_GET_BOOT_INFO => "sys_get_boot_info",
        SYS_ALLOC_DMA_PAGES => "sys_alloc_dma_pages",
        SYS_REGISTER_DEVICE_PAGE => "sys_register_device_page",
        SYS_QUERY_PAGESET_PHYS => "sys_query_pageset_phys",
        SYS_CREATE_REPLY => "sys_create_reply",
        SYS_EXIT => "sys_exit",
        SYS_CREATE_THREAD => "sys_create_thread",
        SYS_QUERY_MAPPING => "sys_query_mapping",
        SYS_CLOSE_HANDLE => "sys_close_handle",
        SYS_UNMAP_PAGES => "sys_unmap_pages",
        SYS_QUERY_CALLER_TOKEN => "sys_query_caller_token",
        SYS_SCHED_TELEMETRY => "sys_sched_telemetry",
        SYS_DMA_SYNC_FOR_CPU => "sys_dma_sync_for_cpu",
        SYS_DMA_SYNC_FOR_DEVICE => "sys_dma_sync_for_device",
        _ => "unknown",
    }
}
