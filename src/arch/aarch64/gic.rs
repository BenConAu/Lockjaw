use crate::mm::addr::KERNEL_VA_OFFSET;
use core::arch::asm;
use core::ptr;

// ---------------------------------------------------------------------------
// GIC MMIO base addresses (from DTB via platform discovery)
// ---------------------------------------------------------------------------

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

const TIMER_PPI_INTID: u32 = super::platform::VIRTUAL_TIMER_INTID;

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
    super::platform::info().gicd_base + KERNEL_VA_OFFSET
}

fn gicr_addr() -> u64 {
    super::platform::info().gic_secondary_base + KERNEL_VA_OFFSET
}

unsafe fn mmio_read32(addr: u64) -> u32 {
    // SAFETY: MMIO address
    ptr::read_volatile(addr as *const u32)
}

unsafe fn mmio_write32(addr: u64, val: u32) {
    // SAFETY: MMIO address
    ptr::write_volatile(addr as *mut u32, val);
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialize the GICv3 distributor. Called once by CPU 0 at boot.
///
/// # Safety
/// Must be called after MMU + higher-half mapping is active.
pub unsafe fn init_distributor() {
    // Guard: GICv2 requires a different driver (Phase C).
    // If the DTB reported GICv2, we cannot proceed with the GICv3 init.
    if super::platform::info().gic_v2 {
        panic!("GICv2 detected but only GICv3 driver is implemented");
    }
    let typer = mmio_read32(gicd_addr() + GICD_TYPER);
    let irq_lines = ((typer & 0x1F) + 1) * 32;
    crate::kprintln!("  GIC distributor: {} IRQ lines", irq_lines);

    // Enable distributor: Group 1 non-secure interrupts
    mmio_write32(gicd_addr() + GICD_CTLR, 1 << 1); // EnableGrp1NS
}

/// GICv3 redistributor stride: each CPU's redistributor occupies
/// 128 KB (64 KB RD_base + 64 KB SGI_base).
const GICR_STRIDE: u64 = 0x20000;

/// Initialize the GICv3 redistributor and CPU interface for this CPU.
/// Called by every CPU (including CPU 0) during boot.
///
/// # Safety
/// Must be called after MMU + higher-half mapping is active.
/// `cpu_id` must match the physical CPU that is executing this code.
pub unsafe fn init_redistributor(cpu_id: u32) {
    let gicr_base = gicr_addr() + (cpu_id as u64) * GICR_STRIDE;

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

/// Convenience: init distributor + redistributor for CPU 0.
/// Backwards-compatible with the old single init() call.
///
/// # Safety
/// Must be called after MMU + higher-half mapping is active.
pub unsafe fn init() {
    init_distributor();
    init_redistributor(0);
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
pub unsafe fn enable_spi(intid: u32, edge_triggered: bool) {
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

    // Configure trigger type in GICD_ICFGR.
    // Each INTID gets 2 bits: bit[1] = 1 for edge, 0 for level.
    if edge_triggered {
        let icfg_reg = (intid / 16) as u64;
        let icfg_bit = ((intid % 16) * 2 + 1) as u32;
        let icfg_addr = gicd_addr() + 0xC00 + icfg_reg * 4;
        let icfg = mmio_read32(icfg_addr);
        mmio_write32(icfg_addr, icfg | (1 << icfg_bit));
    }

    // Enable in GICD_ISENABLER (write-1-to-set)
    let en_addr = gicd_addr() + GICD_ISENABLER + reg * 4;
    mmio_write32(en_addr, 1 << bit);
}

