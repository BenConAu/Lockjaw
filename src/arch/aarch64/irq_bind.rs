use crate::mm::addr::PhysAddr;
use core::cell::UnsafeCell;

/// Maximum number of IRQ-to-Notification bindings.
/// Must be > 33 to cover UART0 (SPI 1 = INTID 33).
/// Known limitation: static table, see docs/tech-debt.md.
const MAX_BINDINGS: usize = 64;

// ---------------------------------------------------------------------------
// IrqBindings singleton
// ---------------------------------------------------------------------------

/// Static IRQ binding table. Maps hardware INTID → Notification paddr.
/// Wraps the mutable array in `UnsafeCell` so the `unsafe impl Sync`
/// safety argument lives in one place rather than at every call site.
struct IrqBindings(UnsafeCell<[Option<PhysAddr>; MAX_BINDINGS]>);

/// SAFETY: single-core kernel. IRQ handler reads this table; syscall
/// handler writes it. Both run with IRQs masked at EL1, so no concurrent
/// access is possible.
unsafe impl Sync for IrqBindings {}

static BINDINGS: IrqBindings = IrqBindings(UnsafeCell::new([None; MAX_BINDINGS]));

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Bind a hardware INTID to a Notification object.
/// When the IRQ fires, the kernel will signal this notification.
/// Returns false if the INTID is out of range or already bound.
/// Reserved INTIDs that cannot be bound by userspace.
/// INTID 0 = kernel reschedule SGI (cross-core wakeup).
const RESERVED_INTID_SGI_RESCHED: u32 = 0;

pub fn bind(intid: u32, notification_paddr: PhysAddr) -> bool {
    let idx = intid as usize;
    if idx >= MAX_BINDINGS || intid == RESERVED_INTID_SGI_RESCHED {
        return false;
    }
    // SAFETY: single-core, IRQs masked — exclusive access.
    unsafe {
        let table = &mut *BINDINGS.0.get();
        if table[idx].is_some() {
            return false; // already bound
        }
        table[idx] = Some(notification_paddr);
    }
    true
}

/// Look up the Notification bound to a given INTID.
/// Returns None if the INTID is unbound or out of range.
pub fn lookup(intid: u32) -> Option<PhysAddr> {
    let idx = intid as usize;
    if idx >= MAX_BINDINGS {
        return None;
    }
    // SAFETY: single-core, IRQs masked — read-only access.
    unsafe { (*BINDINGS.0.get())[idx] }
}
