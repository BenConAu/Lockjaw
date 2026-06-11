//! Typed handle wrappers for kernel objects, with userlib-side RAII
//! guards.
//!
//! The bare handle newtypes (`PageSetHandle` etc.) now live in
//! `lockjaw-types::handle` so the kernel can read them directly from
//! user memory via `UserAddressSpace::read<ProcessCreateInfo>`. We
//! re-export them here so existing userlib consumers see no change.
//! The RAII guards (`PageSetGuard`, `NotificationGuard`) and the
//! `Exportable` trait stay in userlib because they reference syscall
//! wrappers that the kernel does not.

pub use lockjaw_types::handle::{PageSetHandle, EndpointHandle, NotificationHandle, ReplyHandle};

/// Any handle that can be exported via sys_export_handle or closed
/// via sys_close_handle.
pub trait Exportable {
    fn raw(&self) -> u64;
}

impl Exportable for PageSetHandle { fn raw(&self) -> u64 { self.0 } }
impl Exportable for EndpointHandle { fn raw(&self) -> u64 { self.0 } }
impl Exportable for NotificationHandle { fn raw(&self) -> u64 { self.0 } }
impl Exportable for ReplyHandle { fn raw(&self) -> u64 { self.0 } }

/// The bootstrap endpoint handle. Every child process receives this at
/// handle index 0 via sys_create_process's handle_to_copy parameter.
/// The child calls this endpoint to bootstrap (receive service handles
/// from init).
pub fn bootstrap_endpoint() -> EndpointHandle {
    EndpointHandle(0)
}

/// RAII guard for a PageSetHandle. Closes the handle on drop unless
/// explicitly taken via `take()`. Prevents handle/page leaks on error
/// paths — same principle as the kernel's PageGuard.
pub struct PageSetGuard {
    handle: Option<PageSetHandle>,
}

impl PageSetGuard {
    /// Wrap a freshly allocated PageSetHandle.
    pub fn new(ps: PageSetHandle) -> Self {
        Self { handle: Some(ps) }
    }

    /// Access the handle without consuming the guard.
    pub fn handle(&self) -> PageSetHandle {
        self.handle.expect("PageSetGuard already taken")
    }

    /// Take ownership of the handle, disarming the drop guard.
    /// Use this once the handle is successfully tracked elsewhere
    /// (e.g., inserted into a buffer table, exported to a client).
    pub fn take(mut self) -> PageSetHandle {
        self.handle.take().expect("PageSetGuard already taken")
    }
}

impl Drop for PageSetGuard {
    fn drop(&mut self) {
        if let Some(ps) = self.handle {
            crate::syscall::sys_close_handle(ps);
        }
    }
}

/// RAII guard for a NotificationHandle. Mirrors `PageSetGuard` for
/// the create-then-bind sequence: `bind_irq` allocates a pageset,
/// converts it to a notification, then binds the IRQ. If the bind
/// step fails, the unbound notification handle must be closed.
pub struct NotificationGuard {
    handle: Option<NotificationHandle>,
}

impl NotificationGuard {
    /// Wrap a freshly created NotificationHandle.
    pub fn new(n: NotificationHandle) -> Self {
        Self { handle: Some(n) }
    }

    /// Access the handle without consuming the guard.
    pub fn handle(&self) -> NotificationHandle {
        self.handle.expect("NotificationGuard already taken")
    }

    /// Take ownership of the handle, disarming the drop guard.
    pub fn take(mut self) -> NotificationHandle {
        self.handle.take().expect("NotificationGuard already taken")
    }
}

impl Drop for NotificationGuard {
    fn drop(&mut self) {
        if let Some(n) = self.handle {
            crate::syscall::sys_close_handle(n);
        }
    }
}
