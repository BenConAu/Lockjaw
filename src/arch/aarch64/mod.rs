mod boot;
pub mod exceptions;
pub mod gic;
pub mod mmu;
pub mod uart;

/// IRQ dispatch — called from the exception vector IRQ stub.
/// Acknowledges the interrupt via GIC and logs unexpected IRQs.
pub fn irq_dispatch() {
    let intid = unsafe { gic::handle_irq() };

    if intid != 1023 {
        crate::kprintln!("WARNING: unexpected IRQ INTID={}", intid);
    }
    // INTID 1023 = spurious, silently ignore
}
