//! Typed IRQ-binding helpers — userspace driver framework.
//!
//! Drivers should not open-code the `sys_alloc_pages` +
//! `sys_create_notification` + `sys_bind_irq_flags` sequence, nor
//! the `sys_wait_any_until` + threshold-bump + `sys_unmask_irq`
//! dance. Both are framework concerns: the kernel owns the IRQ
//! binding table and the GIC mask state; the driver only owns its
//! device's W1C status-clear path.
//!
//! Surface:
//!   - `bind_irq_level(intid)` does the bootstrap sequence and
//!     returns a `BoundIrq` whose internal `threshold` matches the
//!     kernel's view (counter starts at 0; the first IRQ delivery
//!     advances to 1).
//!   - `BoundIrq::wait_until(deadline)` blocks until the next IRQ
//!     or the deadline, advancing the threshold so each subsequent
//!     wait expects a fresh delivery.
//!   - `BoundIrq::unmask()` re-enables the GIC line after the
//!     driver has cleared its device-side status (W1C).
//!
//! Composition: driver calls `wait_until` → reads its device's
//! status register → W1C-clears the latched bits → `unmask()`. The
//! W1C step stays in driver source because the status-register
//! layout is device-specific; everything else is in here.

use lockjaw_types::time::MonoTicks;

use crate::handle::{NotificationHandle, PageSetHandle};
use crate::syscall::{
    sys_alloc_pages, sys_bind_irq_flags, sys_create_notification, sys_unmask_irq,
    sys_wait_any_until, IRQ_FLAG_EDGE,
};
use crate::WaitEntry;
use lockjaw_types::syscall::SyscallError;

/// Bootstrap-time failure modes for `bind_irq_level` /
/// `bind_irq_edge`. Each variant carries the underlying
/// `SyscallError` from the kernel so callers can distinguish (e.g.)
/// out-of-memory from "intid already bound" without re-querying.
#[derive(Copy, Clone, Debug)]
pub enum BindIrqError {
    /// `sys_alloc_pages(1)` failed — usually out of pages.
    AllocNotificationPage(SyscallError),
    /// `sys_create_notification` failed — usually a bad page handle.
    CreateNotification(SyscallError),
    /// `sys_bind_irq_flags` failed — intid out of range, already
    /// bound, reserved, or some kernel-side rejection.
    BindIrq(SyscallError),
}

/// Failure modes for `BoundIrq::wait_until`. Same shape as the old
/// raw `sys_wait_any_until` error surface, but typed so callers can
/// distinguish "deadline expired" (the experiment-vs-hang gate)
/// from "kernel rejected the wait".
#[derive(Copy, Clone, Debug)]
pub enum IrqWaitError {
    /// The deadline elapsed before any IRQ delivery.
    Timeout,
    /// `sys_wait_any_until` returned an error (handle vanished,
    /// invalid wait list, etc.).
    Syscall(SyscallError),
}

/// A bound IRQ + its notification handle + the next threshold the
/// driver should wait on. `threshold` follows the kernel's counter:
/// after `bind_irq_level` it is 1 (the first IRQ delivery brings
/// the counter from 0 → 1), and each successful `wait_until` bumps
/// it by 1.
pub struct BoundIrq {
    /// Notification handle the kernel signals on IRQ delivery.
    pub notif: NotificationHandle,
    /// Hardware INTID. Held so `unmask()` doesn't require the
    /// driver to thread it separately.
    pub intid: u64,
    /// Counter value the next `wait_until` should wait for. Equal
    /// to the kernel's notification-counter value + 1.
    pub threshold: u64,
}

impl BoundIrq {
    /// Block until the kernel signals our notification past
    /// `self.threshold`, or until `deadline` expires. On success
    /// advances `self.threshold` by 1 so the next call expects a
    /// fresh delivery.
    pub fn wait_until(&mut self, deadline: MonoTicks) -> Result<(), IrqWaitError> {
        let entries = [WaitEntry {
            handle: self.notif.0 as u64,
            threshold: self.threshold,
        }];
        let mask = sys_wait_any_until(&entries, deadline).map_err(IrqWaitError::Syscall)?;
        if mask == 0 {
            return Err(IrqWaitError::Timeout);
        }
        self.threshold += 1;
        Ok(())
    }

    /// Re-enable the GIC line for this intid. The driver MUST have
    /// cleared its device-side status (W1C) BEFORE calling this,
    /// otherwise the still-asserted level source will re-deliver
    /// immediately. No-op for edge-triggered intids (kernel does
    /// not mask edge), but the syscall still validates the caller
    /// owns the binding so it can't unmask another driver's IRQ.
    pub fn unmask(&self) -> SyscallError {
        sys_unmask_irq(self.intid)
    }
}

/// Bind a level-triggered IRQ. The kernel will mask the intid in
/// the GIC between signalling our notification and our subsequent
/// `unmask()` call — required for level sources whose line stays
/// asserted until the device's status latch is cleared. Bootstrap
/// is allocate-page + create-notification + bind-IRQ; on success
/// returns a `BoundIrq` with `threshold = 1`.
pub fn bind_irq_level(intid: u64) -> Result<BoundIrq, BindIrqError> {
    bind_irq_with_flags(intid, 0)
}

/// Bind an edge-triggered IRQ. Same bootstrap as
/// `bind_irq_level`, but flags = `IRQ_FLAG_EDGE` so the kernel
/// does not mask between deliveries (the GIC's edge semantics
/// re-arm naturally on the next rising edge).
pub fn bind_irq_edge(intid: u64) -> Result<BoundIrq, BindIrqError> {
    bind_irq_with_flags(intid, IRQ_FLAG_EDGE)
}

fn bind_irq_with_flags(intid: u64, flags: u64) -> Result<BoundIrq, BindIrqError> {
    let notif_ps: PageSetHandle =
        sys_alloc_pages(1).map_err(BindIrqError::AllocNotificationPage)?;
    let notif: NotificationHandle =
        sys_create_notification(notif_ps).map_err(BindIrqError::CreateNotification)?;
    let bind_err = sys_bind_irq_flags(intid, notif, flags);
    if !bind_err.is_ok() {
        return Err(BindIrqError::BindIrq(bind_err));
    }
    Ok(BoundIrq {
        notif,
        intid,
        threshold: 1,
    })
}
