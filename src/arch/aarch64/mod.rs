mod boot;
pub mod exceptions;
pub mod gic;
pub mod irq_bind;
pub mod mmu;
pub mod platform;
pub mod timer;
pub mod uart;
pub mod vmem;

/// IRQ dispatch — called from the exception vector IRQ stub.
/// Acknowledges the interrupt via GIC, checks for a userspace notification
/// binding, then falls through to built-in handlers (timer).
pub fn irq_dispatch() {
    let intid = unsafe { gic::handle_irq() };

    // Check if this INTID is bound to a userspace notification
    if let Some(notif_paddr) = unsafe { irq_bind::lookup(intid) } {
        // Signal the notification — increment timeline value by 1
        // The notification_signal function handles waking any waiting thread
        unsafe {
            use crate::ipc::notification;
            let obj = (notif_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET)
                as *const notification::NotificationObject;
            let new_value = (*obj).state.value + 1;
            let _ = notification::notification_signal(notif_paddr, new_value);
        }
        return;
    }

    match intid {
        27 => timer::handle_tick(),
        1023 => {} // Spurious, silently ignore
        _ => crate::kprintln!("WARNING: unexpected IRQ INTID={}", intid),
    }
}
