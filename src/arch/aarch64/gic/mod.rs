/// GIC interrupt controller driver.
///
/// Supports both GICv2 (MMIO CPU interface) and GICv3 (system register
/// CPU interface). Version detected from DTB at boot via platform::info().
///
/// The Gic enum holds version-specific state. Each method dispatches
/// to the correct implementation via match. Shared GICD register
/// operations (priority, trigger, enable) are common methods.

mod v2;
mod v3;

use crate::mm::addr::KERNEL_VA_OFFSET;
use core::ptr;

// ---------------------------------------------------------------------------
// Shared GICD register offsets (same on v2 and v3)
// ---------------------------------------------------------------------------

const GICD_TYPER: u64 = 0x0004;
const GICD_IGROUPR: u64 = 0x0080;
const GICD_ISENABLER: u64 = 0x0100;
/// GICD_ICENABLER (Interrupt Clear-Enable Registers, GICv2 §8.13.7 /
/// GICv3 §12.13.10). Mirror layout to GICD_ISENABLER but write-1-to-
/// CLEAR-ENABLE: writing 1 disables the corresponding IRQ at the
/// distributor. Used for level-triggered IRQ masking — the kernel
/// disables the intid in `irq_dispatch` after signaling userspace so
/// the line-still-asserted source can't refire across EOIR.
const GICD_ICENABLER: u64 = 0x0180;
const GICD_IPRIORITYR: u64 = 0x0400;
/// GICD_ITARGETSRn (Interrupt Processor Targets Registers, GICv2
/// §8.13.10). One byte per intid; each byte is a CPU mask (bit 0 =
/// CPU 0, bit 1 = CPU 1, …). For SPIs (>=32) software writable; for
/// PPIs/SGIs (<32) read-only and per-CPU-banked. Reset value is
/// implementation-defined — on some implementations it's 0 (no CPU
/// target, IRQ pending forever). Linux always writes this explicitly
/// before unmasking. GICv3-only platforms use IROUTER instead, so
/// this is touched only inside `enable_spi`; the v2 write is harmless
/// on v3 (the v3 distributor treats the offset as reserved RAZ/WI).
const GICD_ITARGETSR: u64 = 0x0800;
/// GICD_ICFGRn (Interrupt Configuration Registers). Two bits per
/// intid; the HIGH bit of each pair is the trigger mode (1 = edge,
/// 0 = level). For SPIs the field is software writable; reset value
/// is implementation-defined — on GIC-400 (BCM2711) we cannot rely
/// on any particular reset value, so `enable_spi` writes this
/// explicitly for BOTH trigger modes rather than only setting the
/// edge bit and trusting the default for level.
const GICD_ICFGR: u64 = 0x0C00;

/// Virtual timer PPI interrupt ID (generic ARMv8).
const TIMER_PPI_INTID: u32 = super::platform::VIRTUAL_TIMER_INTID;

// ---------------------------------------------------------------------------
// GIC driver enum
// ---------------------------------------------------------------------------

/// GIC driver — version selected at boot from DTB.
enum Gic {
    V2,
    V3,
}

impl Gic {
    /// Initialize the distributor.
    unsafe fn init_distributor(&self) {
        match self {
            Gic::V2 => v2::init_distributor(),
            Gic::V3 => v3::init_distributor(),
        }
    }

    /// Initialize the per-CPU interrupt interface.
    unsafe fn init_cpu(&self, cpu_id: u32) {
        match self {
            Gic::V2 => v2::init_cpu(cpu_id),
            Gic::V3 => v3::init_cpu(cpu_id),
        }
    }

    /// Acknowledge and handle a pending IRQ. Returns the INTID.
    unsafe fn handle_irq(&self) -> u32 {
        match self {
            Gic::V2 => v2::handle_irq(),
            Gic::V3 => v3::handle_irq(),
        }
    }

    /// Version-specific interrupt group assignment for an SPI.
    unsafe fn configure_spi_group(&self, intid: u32) {
        match self {
            Gic::V2 => v2::configure_spi_group(intid),
            Gic::V3 => v3::configure_spi_group(intid),
        }
    }

    fn version_name(&self) -> &'static str {
        match self {
            Gic::V2 => "v2",
            Gic::V3 => "v3",
        }
    }
}

/// Global GIC driver instance. Set once during init().
static mut GIC: Gic = Gic::V3;

// ---------------------------------------------------------------------------
// Shared MMIO helpers (used by v2.rs and v3.rs)
// ---------------------------------------------------------------------------

pub(super) fn gicd_addr() -> u64 {
    super::platform::info().gicd_base + KERNEL_VA_OFFSET
}

pub(super) fn gic_secondary_addr() -> u64 {
    super::platform::info().gic_secondary_base + KERNEL_VA_OFFSET
}

pub(super) unsafe fn mmio_read32(addr: u64) -> u32 {
    // SAFETY: MMIO address
    ptr::read_volatile(addr as *const u32)
}

pub(super) unsafe fn mmio_write32(addr: u64, val: u32) {
    // SAFETY: MMIO address
    ptr::write_volatile(addr as *mut u32, val);
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialize the GIC. Detects version from DTB, initializes
/// distributor + CPU 0 interface.
///
/// # Safety
/// Must be called after MMU + higher-half mapping is active.
pub unsafe fn init() {
    let gic = if super::platform::info().gic_v2 { Gic::V2 } else { Gic::V3 };

    let typer = mmio_read32(gicd_addr() + GICD_TYPER);
    let irq_lines = ((typer & 0x1F) + 1) * 32;
    crate::kprintln!("  GIC", gic.version_name(), " distributor: ", irq_lines, " IRQ lines");

    gic.init_distributor();
    gic.init_cpu(0);

    // Store for future dispatch (init_cpu on secondary CPUs, handle_irq, enable_spi).
    *core::ptr::addr_of_mut!(GIC) = gic;

    crate::kprintln!("  GIC initialized, timer PPI ", TIMER_PPI_INTID, " enabled");
}

/// Initialize the per-CPU interrupt interface for a secondary CPU.
///
/// # Safety
/// Must be called after MMU + higher-half mapping is active.
/// `cpu_id` must match the physical CPU executing this code.
pub unsafe fn init_cpu(cpu_id: u32) {
    (*core::ptr::addr_of!(GIC)).init_cpu(cpu_id);
}

/// Acknowledge and handle a pending IRQ. Returns the INTID.
/// INTID 1023 means spurious (no interrupt pending).
///
/// # Safety
/// Must be called from the IRQ exception handler with interrupts masked.
pub unsafe fn handle_irq() -> u32 {
    (*core::ptr::addr_of!(GIC)).handle_irq()
}

/// Enable a Shared Peripheral Interrupt (SPI) in the GIC distributor.
/// Sets group, priority, trigger mode, CPU target, and enables it.
///
/// # Safety
/// Must be called after `init()`. `intid` must be an SPI (>= 32).
pub unsafe fn enable_spi(intid: u32, edge_triggered: bool) {
    let reg = (intid / 32) as u64;
    let bit = intid % 32;

    // Version-specific group assignment
    (*core::ptr::addr_of!(GIC)).configure_spi_group(intid);

    // Set priority (0xA0 = middle priority)
    let pri_addr = gicd_addr() + GICD_IPRIORITYR + intid as u64;
    mmio_write32(pri_addr & !3, {
        let byte_offset = (intid % 4) * 8;
        let cur = mmio_read32(pri_addr & !3);
        (cur & !(0xFF << byte_offset)) | (0xA0 << byte_offset)
    });

    // Configure trigger type in GICD_ICFGR. Two bits per intid; the
    // high bit selects edge (1) vs level (0). Write explicitly for
    // BOTH modes — reset value is implementation-defined and on
    // GIC-400 (BCM2711) cannot be trusted to be "level". Linux's
    // `gic_configure_irq` does the same: edge → set bit, level →
    // clear bit. Pre-fix, the level branch was silent, which left
    // INTID 158 (emmc2 SDHCI, level-high per BCM2711 DTS) configured
    // as whatever the boot loader left — masking IRQ delivery.
    let icfg_reg = (intid / 16) as u64;
    let icfg_bit = ((intid % 16) * 2 + 1) as u32;
    let icfg_addr = gicd_addr() + GICD_ICFGR + icfg_reg * 4;
    let icfg_before = mmio_read32(icfg_addr);
    let icfg_after = if edge_triggered {
        icfg_before | (1 << icfg_bit)
    } else {
        icfg_before & !(1 << icfg_bit)
    };
    mmio_write32(icfg_addr, icfg_after);

    // Route the SPI to CPU 0 via GICD_ITARGETSR. One byte per intid.
    // Reset value is implementation-defined; on platforms that reset
    // to 0x00 the IRQ would be pending forever with no CPU target.
    // Linux writes this in `gic_set_affinity` before unmasking. We
    // single-target CPU 0 (the bind syscall always runs on CPU 0 in
    // the current scheduler shape; cross-CPU IRQ routing is out of
    // scope today). GICv3 platforms route via IROUTER instead — the
    // write below is harmless on v3 (the v3 distributor treats the
    // offset as a reserved RAZ/WI region) but future v3-only work
    // should split the path explicitly.
    let tgt_addr = gicd_addr() + GICD_ITARGETSR + (intid as u64 & !3);
    let tgt_byte_offset = (intid % 4) * 8;
    let tgt_before = mmio_read32(tgt_addr);
    let tgt_after = (tgt_before & !(0xFF << tgt_byte_offset))
        | (0x01u32 << tgt_byte_offset); // CPU 0 bit
    mmio_write32(tgt_addr, tgt_after);

    // Enable in GICD_ISENABLER
    let en_addr = gicd_addr() + GICD_ISENABLER + reg * 4;
    mmio_write32(en_addr, 1 << bit);
}

/// Disable a Shared Peripheral Interrupt in the GIC distributor.
/// Mirror of `enable_spi` but writes to GICD_ICENABLER (write-1-to-
/// clear-enable). Used by `irq_dispatch` after signaling userspace
/// for a level-triggered IRQ: without disabling, the
/// still-asserted source line refires the IRQ immediately after
/// EOIR. Userspace re-enables via `re_enable_spi` (called from the
/// `sys_unmask_irq` syscall) once it has cleared the source-side
/// status bit.
///
/// # Safety
/// Must be called after `init()`. `intid` must be an SPI (>= 32).
pub unsafe fn disable_spi(intid: u32) {
    let reg = (intid / 32) as u64;
    let bit = intid % 32;
    let dis_addr = gicd_addr() + GICD_ICENABLER + reg * 4;
    mmio_write32(dis_addr, 1 << bit);
}

/// Re-enable an SPI that has been disabled by `disable_spi`. Skips
/// the priority / group / trigger configuration that `enable_spi`
/// would re-run — those were set when the intid was first enabled
/// via `enable_spi` (called from `sys_bind_irq`). Re-running them
/// could clobber the trigger mode for a still-active driver.
///
/// # Safety
/// Must be called after `init()`. `intid` must be an SPI (>= 32)
/// that has previously been enabled via `enable_spi`.
pub unsafe fn re_enable_spi(intid: u32) {
    let reg = (intid / 32) as u64;
    let bit = intid % 32;
    let en_addr = gicd_addr() + GICD_ISENABLER + reg * 4;
    mmio_write32(en_addr, 1 << bit);
}
