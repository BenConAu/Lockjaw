use crate::mm::addr::PhysAddr;

/// Maximum number of IRQ-to-Notification bindings.
/// Known limitation: static table, see docs/tech-debt.md.
const MAX_BINDINGS: usize = 32;

/// Static IRQ binding table. Maps hardware INTID → Notification paddr.
static mut BINDINGS: [Option<PhysAddr>; MAX_BINDINGS] = [None; MAX_BINDINGS];

/// Bind a hardware INTID to a Notification object.
/// When the IRQ fires, the kernel will signal this notification.
/// Returns false if the INTID is out of range or already bound.
pub unsafe fn bind(intid: u32, notification_paddr: PhysAddr) -> bool {
    let idx = intid as usize;
    if idx >= MAX_BINDINGS {
        return false;
    }
    if BINDINGS[idx].is_some() {
        return false; // already bound
    }
    BINDINGS[idx] = Some(notification_paddr);
    true
}

/// Look up the Notification bound to a given INTID.
/// Returns None if the INTID is unbound.
pub unsafe fn lookup(intid: u32) -> Option<PhysAddr> {
    let idx = intid as usize;
    if idx >= MAX_BINDINGS {
        return None;
    }
    BINDINGS[idx]
}
