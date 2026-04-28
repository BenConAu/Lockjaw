/// Typed handle wrappers for kernel objects.
///
/// Each kernel object type has its own newtype handle. The compiler
/// prevents passing an EndpointHandle where a PageSetHandle is expected.
/// The raw syscall ABI is still u64 — these wrappers convert at the
/// boundary.

/// PageSet handle — from sys_alloc_pages, used with sys_map_pages.
#[derive(Clone, Copy, Debug)]
pub struct PageSetHandle(pub u64);

/// Endpoint handle — from sys_create_endpoint, used with sys_call/receive.
#[derive(Clone, Copy, Debug)]
pub struct EndpointHandle(pub u64);

/// Notification handle — from sys_create_notification.
#[derive(Clone, Copy, Debug)]
pub struct NotificationHandle(pub u64);

/// Reply handle — from sys_create_reply, used with sys_call.
#[derive(Clone, Copy, Debug)]
pub struct ReplyHandle(pub u64);

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
