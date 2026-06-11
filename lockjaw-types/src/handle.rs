//! Typed handle wrappers for kernel objects.
//!
//! Each kernel object type has its own newtype handle. The compiler
//! prevents passing an EndpointHandle where a PageSetHandle is expected.
//! The raw syscall ABI is still `u64` — these wrappers convert at the
//! boundary.
//!
//! The definitions live in `lockjaw-types` (not in `lockjaw-userlib`)
//! so the kernel can read them directly from user memory through
//! `UserAddressSpace::read<ProcessCreateInfo>`. `repr(transparent)`
//! pins the layout to a bare `u64`, which is what
//! `unsafe impl UserPod` for `PageSetHandle` rests on. The userlib
//! re-exports these types alongside its RAII guards (which depend on
//! syscall wrappers and stay in userlib).

/// PageSet handle — from `sys_alloc_pages`, used with `sys_map_pages`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageSetHandle(pub u64);

/// Endpoint handle — from `sys_create_endpoint`, used with `sys_call`/`receive`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointHandle(pub u64);

/// Notification handle — from `sys_create_notification`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationHandle(pub u64);

/// Reply handle — from `sys_create_reply`, used with `sys_call`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyHandle(pub u64);

// Layout lockdown — `repr(transparent)` over `u64` is what the
// `unsafe impl UserPod for PageSetHandle` below depends on, and what
// the kernel relies on when reading `ProcessCreateInfo` via
// `UserAddressSpace::read`. A future contributor adding a field or
// changing the wrapper would break these asserts at compile time.
const _: () = {
    assert!(core::mem::size_of::<PageSetHandle>() == core::mem::size_of::<u64>());
    assert!(core::mem::align_of::<PageSetHandle>() == core::mem::align_of::<u64>());
    assert!(core::mem::size_of::<EndpointHandle>() == core::mem::size_of::<u64>());
    assert!(core::mem::size_of::<NotificationHandle>() == core::mem::size_of::<u64>());
    assert!(core::mem::size_of::<ReplyHandle>() == core::mem::size_of::<u64>());
};

// SAFETY: PageSetHandle is `repr(transparent)` over u64 — every bit
// pattern is valid; no padding, no niches.
unsafe impl crate::user_pod::UserPod for PageSetHandle {}
