mod boot;
pub mod exceptions;
pub mod gic;
pub mod mmu;
pub mod timer;
pub mod uart;
pub mod vmem;

/// IRQ dispatch — called from the exception vector IRQ stub.
/// Acknowledges the interrupt via GIC and dispatches by INTID.
pub fn irq_dispatch() {
    let intid = unsafe { gic::handle_irq() };

    match intid {
        27 => timer::handle_tick(),
        1023 => {} // Spurious, silently ignore
        _ => crate::kprintln!("WARNING: unexpected IRQ INTID={}", intid),
    }
}
