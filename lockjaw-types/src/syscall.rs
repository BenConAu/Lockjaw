/// Syscall error codes shared between kernel and userspace.
///
/// Returned in x0 when a syscall fails. Success is always 0.
/// Userspace can match on these to diagnose failures.

/// Syscall succeeded.
pub const SYS_OK: u64 = 0;

/// The handle index is invalid or the slot is empty.
pub const SYS_ERR_INVALID_HANDLE: u64 = 1;

/// The handle does not have the required rights for this operation.
pub const SYS_ERR_INSUFFICIENT_RIGHTS: u64 = 2;

/// The page allocator has no free pages.
pub const SYS_ERR_OUT_OF_MEMORY: u64 = 3;

/// A parameter is invalid (wrong type, out of range, etc).
pub const SYS_ERR_INVALID_PARAMETER: u64 = 4;

/// The endpoint is busy (e.g. already has a sender/receiver blocked).
pub const SYS_ERR_ENDPOINT_BUSY: u64 = 5;

/// No caller is waiting for a reply on this endpoint.
pub const SYS_ERR_NO_CALLER: u64 = 6;

/// The scheduler's run queue is full.
pub const SYS_ERR_QUEUE_FULL: u64 = 7;

/// The notification value is not monotonically increasing.
pub const SYS_ERR_NOT_MONOTONIC: u64 = 8;

/// A thread is already waiting on this notification (single-waiter limit).
pub const SYS_ERR_ALREADY_WAITING: u64 = 9;

/// An unknown or unrecoverable error occurred.
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
pub const SYS_SIGNAL_NOTIFICATION: u64 = 10;
pub const SYS_WAIT_NOTIFICATION: u64 = 11;
pub const SYS_BIND_IRQ: u64 = 12;
