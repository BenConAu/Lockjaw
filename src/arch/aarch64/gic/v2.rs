/// GICv2 CPU interface — GICC MMIO registers.
///
/// Used on Raspberry Pi 4B (GIC-400), QEMU virt with gic-version=2,
/// and other GICv2 platforms.

use super::{gicd_addr, gic_secondary_addr, mmio_read32, mmio_write32, TIMER_PPI_INTID};

// ---------------------------------------------------------------------------
// GICC register offsets (from GICC base, the second DTB reg entry)
// ---------------------------------------------------------------------------

const GICC_CTLR: u64 = 0x0000;  // CPU Interface Control
const GICC_PMR: u64 = 0x0004;   // Interrupt Priority Mask
const GICC_IAR: u64 = 0x000C;   // Interrupt Acknowledge
const GICC_EOIR: u64 = 0x0010;  // End of Interrupt

// ---------------------------------------------------------------------------
// Public API (called by gic/mod.rs dispatch)
// ---------------------------------------------------------------------------

/// Initialize the GICv2 distributor: enable Group 0 interrupts.
pub unsafe fn init_distributor() {
    // Enable Group 0 in distributor (bit 0).
    // All interrupts stay in Group 0 — GICC_IAR reads them directly.
    mmio_write32(super::gicd_addr() + 0x0000, 1 << 0);
}

/// GICv2 Group 0: no-op. Interrupts default to Group 0, which is what
/// GICC_IAR reads. No IGROUPR write needed.
pub unsafe fn configure_spi_group(_intid: u32) {}

/// Initialize the GICv2 CPU interface for this CPU.
///
/// On GICv2, per-CPU PPI/SGI configuration uses banked GICD registers
/// (same offsets as GICD_IGROUPR0/ISENABLER0, but each CPU sees its
/// own copy). The CPU interface uses GICC MMIO registers.
pub unsafe fn init_cpu(_cpu_id: u32) {
    let gicc = gic_secondary_addr();

    // Enable the CPU interface (bit 0 = master enable).
    mmio_write32(gicc + GICC_CTLR, 1);

    // Set priority mask to allow all priorities
    mmio_write32(gicc + GICC_PMR, 0xFF);

    // Enable timer PPI via banked GICD_ISENABLER0.
    // Leave in Group 0 (default) — GICC_IAR reads Group 0 interrupts.
    // Setting Group 1 would require GICC_AIAR for acknowledge.
    mmio_write32(gicd_addr() + 0x0100, 1 << TIMER_PPI_INTID);
}

/// Acknowledge and handle a pending IRQ via GICC MMIO registers.
pub unsafe fn handle_irq() -> u32 {
    let gicc = gic_secondary_addr();
    let intid = mmio_read32(gicc + GICC_IAR);
    let masked = intid & 0x3FF; // INTID is in bits [9:0]
    if masked < 1020 {
        mmio_write32(gicc + GICC_EOIR, intid);
    }
    masked
}
