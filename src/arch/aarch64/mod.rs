mod boot;
pub mod cache;
pub mod exceptions;
pub mod gic;
pub mod irq_bind;
pub mod mmu;
pub mod platform;
pub mod psci;
pub mod spin_table;
pub mod timer;
pub mod uart;
pub mod vmem;

/// IRQ dispatch — called from the exception vector IRQ stub.
/// Acknowledges the interrupt via GIC, checks for a userspace notification
/// binding, then falls through to built-in handlers (timer).
pub fn irq_dispatch() {
    let intid = unsafe { gic::handle_irq() };

    // Check if this INTID is bound to a userspace notification
    if let Some(notif_kva) = irq_bind::lookup(intid) {
        // Signal the notification — increment timeline value by 1
        // The notification_signal function handles waking any waiting thread
        use crate::ipc::notification;
        use crate::mm::kernel_ptr::KernelMut;
        // SAFETY: notif_kva was stored by irq_bind::bind from a valid
        // NotificationObject handle lookup; Notification objects live
        // in the KVM pool.
        let mut obj = unsafe { KernelMut::<notification::NotificationObject>::from_kva(notif_kva) };
        let new_value = obj.get().state.value + 1;
        let _ = notification::notification_signal(obj.get_mut(), new_value);
        return;
    }

    match intid {
        27 => timer::handle_tick(),
        1023 => {} // Spurious, silently ignore
        _ => crate::kprintln!("WARNING: unexpected IRQ INTID=", intid),
    }
}
