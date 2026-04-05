mod boot;
pub mod exceptions;
pub mod mmu;
pub mod uart;

/// IRQ dispatch — called from the exception vector IRQ stub.
/// Will be filled in with GIC + timer handling in Milestones 3.2/3.3.
pub fn irq_dispatch() {
    // Placeholder — no IRQ sources configured yet
}
