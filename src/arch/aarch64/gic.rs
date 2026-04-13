use crate::mm::addr::KERNEL_VA_OFFSET;
use core::arch::asm;
use core::ptr;

// ---------------------------------------------------------------------------
// GICv3 MMIO base addresses (QEMU virt machine)
// ---------------------------------------------------------------------------

use super::platform;

const GICD_BASE_PHYS: u64 = platform::GICD_BASE_PHYS;
const GICR_BASE_PHYS: u64 = platform::GICR_BASE_PHYS;

/// Distributor registers (offsets from GICD_BASE)
const GICD_CTLR: u64 = 0x0000;
const GICD_TYPER: u64 = 0x0004;
const GICD_IGROUPR: u64 = 0x0080;      // Interrupt Group Registers (one bit per INTID)
const GICD_ISENABLER: u64 = 0x0100;    // Interrupt Set-Enable Registers
const GICD_IPRIORITYR: u64 = 0x0400;   // Interrupt Priority Registers (one byte per INTID)

/// Redistributor registers (offsets from GICR_BASE)
/// SGI/PPI region is at GICR_BASE + 0x10000 (64KB offset for RD_base, then SGI_base)
const GICR_WAKER: u64 = 0x0014;
const GICR_SGI_BASE: u64 = 0x10000;
const GICR_IGROUPR0: u64 = GICR_SGI_BASE + 0x0080;
const GICR_ISENABLER0: u64 = GICR_SGI_BASE + 0x0100;

const TIMER_PPI_INTID: u32 = platform::VIRTUAL_TIMER_INTID;

// ---------------------------------------------------------------------------
// GICv3 system register helpers (ICC_* accessed via MSR/MRS)
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
// MMIO helpers
// ---------------------------------------------------------------------------

fn gicd_addr() -> u64 {
    GICD_BASE_PHYS + KERNEL_VA_OFFSET
}

fn gicr_addr() -> u64 {
    GICR_BASE_PHYS + KERNEL_VA_OFFSET
}

unsafe fn mmio_read32(addr: u64) -> u32 {
    ptr::read_volatile(addr as *const u32)
}

unsafe fn mmio_write32(addr: u64, val: u32) {
    ptr::write_volatile(addr as *mut u32, val);
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialize the GICv3 distributor, redistributor, and CPU interface.
/// Enables the virtual timer PPI (INTID 30).
///
/// # Safety
/// Must be called after MMU + higher-half mapping is active.
/// Initialize the GICv3 distributor, redistributor, and CPU interface.
///
/// Enables Group 1 interrupts, wakes the redistributor, sets the timer
/// PPI (INTID from platform::VIRTUAL_TIMER_INTID) to Group 1, enables
/// system register access, and sets priority mask to allow all priorities.
///
/// # Safety
/// Must be called after MMU + higher-half mapping is active (GIC MMIO
/// addresses are accessed via KERNEL_VA_OFFSET).
pub unsafe fn init() {
    // --- Distributor init ---

    let typer = mmio_read32(gicd_addr() + GICD_TYPER);
    let irq_lines = ((typer & 0x1F) + 1) * 32;
    crate::kprintln!("  GIC distributor: {} IRQ lines", irq_lines);

    // Enable distributor: Group 1 non-secure interrupts
    mmio_write32(gicd_addr() + GICD_CTLR, 1 << 1); // EnableGrp1NS

    // --- Redistributor init ---

    // Wake up the redistributor
    let mut waker = mmio_read32(gicr_addr() + GICR_WAKER);
    waker &= !(1 << 1); // Clear ProcessorSleep bit
    mmio_write32(gicr_addr() + GICR_WAKER, waker);

    // Wait until ChildrenAsleep clears
    while mmio_read32(gicr_addr() + GICR_WAKER) & (1 << 2) != 0 {
        core::hint::spin_loop();
    }

    // Set timer PPI (INTID 30) to Group 1
    let grp = mmio_read32(gicr_addr() + GICR_IGROUPR0);
    mmio_write32(gicr_addr() + GICR_IGROUPR0, grp | (1 << TIMER_PPI_INTID));

    // Enable timer PPI (INTID 30)
    mmio_write32(gicr_addr() + GICR_ISENABLER0, 1 << TIMER_PPI_INTID);

    // --- CPU interface init (system registers) ---

    // Enable system register access
    let sre = icc_sre_el1_read();
    icc_sre_el1_write(sre | 1); // SRE bit
    asm!("isb"); // Sync before using other ICC regs

    // Set priority mask to allow all priorities
    icc_pmr_el1_write(0xFF);

    // Enable Group 1 interrupts
    icc_igrpen1_el1_write(1);

    crate::kprintln!("  GIC initialized, timer PPI {} enabled", TIMER_PPI_INTID);
}

/// Acknowledge and handle a pending IRQ. Returns the INTID, or 1023 if spurious.
///
/// # Safety
/// Must be called from the IRQ exception handler.
/// Acknowledge a pending IRQ and signal end-of-interrupt.
///
/// Reads the interrupt ID from ICC_IAR1_EL1 (which also acknowledges it),
/// then writes ICC_EOIR1_EL1 to signal completion. Returns the INTID.
/// INTID 1023 means spurious (no interrupt pending).
///
/// # Safety
/// Must be called from the IRQ exception handler with interrupts masked.
pub unsafe fn handle_irq() -> u32 {
    // Read INTID from IAR — this acknowledges the interrupt
    let intid = icc_iar1_el1_read();

    if intid < 1020 {
        // Signal End of Interrupt
        icc_eoir1_el1_write(intid);
    }
    // INTID 1023 = spurious, no EOI needed

    intid
}

/// Enable a Shared Peripheral Interrupt (SPI) in the GIC distributor.
/// Configures the INTID as Group 1, sets priority, and enables it.
///
/// # Safety
/// Must be called after `init()`. `intid` must be an SPI (>= 32).
pub unsafe fn enable_spi(intid: u32) {
    let reg = (intid / 32) as u64;     // Which 32-bit register
    let bit = intid % 32;              // Which bit within that register

    // Set to Group 1 (NS) in GICD_IGROUPR
    let grp_addr = gicd_addr() + GICD_IGROUPR + reg * 4;
    let grp = mmio_read32(grp_addr);
    mmio_write32(grp_addr, grp | (1 << bit));

    // Set priority (0xA0 = middle priority, lower than default 0x00)
    let pri_addr = gicd_addr() + GICD_IPRIORITYR + intid as u64;
    mmio_write32(pri_addr & !3, {
        // IPRIORITYR is byte-accessible but we use 32-bit MMIO reads.
        // Read the 4-byte register, modify the target byte, write back.
        let byte_offset = (intid % 4) * 8;
        let cur = mmio_read32(pri_addr & !3);
        (cur & !(0xFF << byte_offset)) | (0xA0 << byte_offset)
    });

    // Enable in GICD_ISENABLER (write-1-to-set)
    let en_addr = gicd_addr() + GICD_ISENABLER + reg * 4;
    mmio_write32(en_addr, 1 << bit);
}
