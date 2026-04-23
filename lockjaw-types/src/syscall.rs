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
pub const SYS_DEBUG_PUTC: u64 = 0;
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

/// Flag for SYS_ALLOC_PAGES: allocated pages must be physically contiguous.
pub const ALLOC_FLAG_CONTIGUOUS: u64 = 1 << 0;
