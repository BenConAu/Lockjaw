/// GICv3 CPU interface — ICC system registers + redistributor.
///
/// Used on QEMU virt with gic-version=3 and other GICv3 platforms.

use core::arch::asm;
use super::{gic_secondary_addr, mmio_read32, mmio_write32, TIMER_PPI_INTID};

// ---------------------------------------------------------------------------
// GICv3 redistributor constants
// ---------------------------------------------------------------------------

/// Each CPU's redistributor occupies 128 KB (64 KB RD_base + 64 KB SGI_base).
const GICR_STRIDE: u64 = 0x20000;
const GICR_WAKER: u64 = 0x0014;
const GICR_SGI_BASE: u64 = 0x10000;

// ---------------------------------------------------------------------------
// ICC system register helpers (GICv3 only)
// ---------------------------------------------------------------------------

unsafe fn icc_sre_el1_read() -> u64 {
    let val: u64;
    asm!(
        "mrs {val}, ICC_SRE_EL1",            // Read ICC System Register Enable
        val = out(reg) val,
    );
    val
}

unsafe fn icc_sre_el1_write(val: u64) {
    asm!(
        "msr ICC_SRE_EL1, {val}",            // Write ICC System Register Enable
        val = in(reg) val,
    );
}

unsafe fn icc_pmr_el1_write(val: u64) {
    asm!(
        "msr ICC_PMR_EL1, {val}",            // Write Priority Mask Register
        val = in(reg) val,
    );
}

unsafe fn icc_igrpen1_el1_write(val: u64) {
    asm!(
        "msr ICC_IGRPEN1_EL1, {val}",        // Write Interrupt Group 1 Enable
        val = in(reg) val,
    );
}

unsafe fn icc_iar1_el1_read() -> u32 {
    let val: u64;
    asm!(
        "mrs {val}, ICC_IAR1_EL1",            // Read Interrupt Acknowledge Register
        val = out(reg) val,
    );
    val as u32
}

unsafe fn icc_eoir1_el1_write(intid: u32) {
    asm!(
        "msr ICC_EOIR1_EL1, {val}",          // Write End of Interrupt Register
        val = in(reg) intid as u64,
    );
}

// ---------------------------------------------------------------------------
// Public API (called by gic/mod.rs dispatch)
// ---------------------------------------------------------------------------

/// Initialize the GICv3 distributor: enable Group 1 NS interrupts.
pub unsafe fn init_distributor() {
    // EnableGrp1NS at bit 1
    super::mmio_write32(super::gicd_addr() + 0x0000, 1 << 1);
}

/// Initialize the GICv3 redistributor and CPU interface for this CPU.
pub unsafe fn init_cpu(cpu_id: u32) {
    let gicr_base = gic_secondary_addr() + (cpu_id as u64) * GICR_STRIDE;

    // Wake up this CPU's redistributor
    let mut waker = mmio_read32(gicr_base + GICR_WAKER);
    waker &= !(1 << 1); // Clear ProcessorSleep bit
    mmio_write32(gicr_base + GICR_WAKER, waker);

    // Wait until ChildrenAsleep clears
    while mmio_read32(gicr_base + GICR_WAKER) & (1 << 2) != 0 {
        core::hint::spin_loop();
    }

    // Set timer PPI to Group 1
    let sgi_base = gicr_base + GICR_SGI_BASE;
    let grp = mmio_read32(sgi_base + 0x0080); // GICR_IGROUPR0
    mmio_write32(sgi_base + 0x0080, grp | (1 << TIMER_PPI_INTID));

    // Enable timer PPI
    mmio_write32(sgi_base + 0x0100, 1 << TIMER_PPI_INTID); // GICR_ISENABLER0

    // --- CPU interface init (system registers, per-CPU) ---
    let sre = icc_sre_el1_read();
    icc_sre_el1_write(sre | 1); // SRE bit
    asm!("isb");

    // Set priority mask to allow all priorities
    icc_pmr_el1_write(0xFF);

    // Enable Group 1 interrupts
    icc_igrpen1_el1_write(1);
}

/// Set SPI to Group 1 NS (GICv3 uses Group 1 for non-secure interrupts).
pub unsafe fn configure_spi_group(intid: u32) {
    let reg = (intid / 32) as u64;
    let bit = intid % 32;
    let grp_addr = super::gicd_addr() + super::GICD_IGROUPR + reg * 4;
    let grp = super::mmio_read32(grp_addr);
    super::mmio_write32(grp_addr, grp | (1 << bit));
}

/// Acknowledge and handle a pending IRQ via ICC system registers.
pub unsafe fn handle_irq() -> u32 {
    let intid = icc_iar1_el1_read();
    if intid < 1020 {
        icc_eoir1_el1_write(intid);
    }
    intid
}
