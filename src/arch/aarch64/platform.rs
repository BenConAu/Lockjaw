/// Platform discovery from DTB.
///
/// The DTB is the single source of truth for all hardware addresses.
/// No defaults, no prints before discovery. If DTB is missing or
/// malformed, the kernel halts — without DTB we don't know where
/// any hardware is.

use core::sync::atomic::{AtomicBool, Ordering};
use lockjaw_types::fdt::{CpuInfo, SmpMethod};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// QEMU `-kernel` bare-metal boot places the DTB at the start of RAM.
/// If the firmware DTB pointer (x0) is zero, search here as a fallback.
/// This is NOT a default for any MMIO — it's purely a DTB search address.
pub const QEMU_DTB_SEARCH_ADDR: u64 = 0x4000_0000;

/// Virtual timer PPI interrupt ID (generic ARMv8, platform-independent).
pub const VIRTUAL_TIMER_INTID: u32 = 27;

/// Maximum number of CPUs supported. Re-exported from
/// `lockjaw_types::scheduler::MAX_CPUS` (single source of truth,
/// shared with the pure scheduler model in lockjaw-types). The
/// re-export keeps existing `platform::MAX_CPUS` call sites
/// working without churn.
pub use lockjaw_types::scheduler::MAX_CPUS;

// ---------------------------------------------------------------------------
// Runtime platform info (populated from DTB)
// ---------------------------------------------------------------------------

/// Discovered platform information. All fields are zero until discover()
/// populates them from the DTB. Zero means "not discovered".
pub struct PlatformInfo {
    pub pl011_base: u64,
    pub gicd_base: u64,
    /// GICv3: redistributor base. GICv2: CPU interface base.
    pub gic_secondary_base: u64,
    pub ram_base: u64,
    pub ram_size: u64,
    pub device_mmio_base: u64,
    pub gic_v2: bool,
    /// How to boot secondary CPUs (PSCI, spin-table, or none).
    pub smp_method: SmpMethod,
    /// Per-CPU identity and release addresses from DTB.
    pub cpus: [CpuInfo; MAX_CPUS],
    /// Number of CPUs described in DTB.
    pub cpu_count: u8,
}

/// Errors from platform discovery.
#[derive(Debug)]
pub enum PlatformError {
    /// DTB pointer invalid or magic mismatch.
    NoDtb,
    /// DTB structure unparseable.
    ParseFailed,
    /// No PL011 UART found in DTB.
    MissingPl011,
    /// No GIC distributor found in DTB.
    MissingGic,
    /// No memory node found in DTB.
    MissingRam,
}

/// Global platform info. All zeros until discover() populates from DTB.
static mut PLATFORM: PlatformInfo = PlatformInfo {
    pl011_base: 0,
    gicd_base: 0,
    gic_secondary_base: 0,
    ram_base: 0,
    ram_size: 0,
    device_mmio_base: 0,
    gic_v2: false,
    smp_method: SmpMethod::None,
    cpus: [CpuInfo::EMPTY; MAX_CPUS],
    cpu_count: 0,
};

static DISCOVERED: AtomicBool = AtomicBool::new(false);

/// Read platform info. Only valid after discover() succeeds.
/// In debug builds, asserts that discovery has run.
pub fn info() -> &'static PlatformInfo {
    debug_assert!(DISCOVERED.load(Ordering::Acquire), "platform::info() called before discover()");
    // SAFETY: PLATFORM is written once by discover() during single-core
    // boot, then read-only. No concurrent mutation after DISCOVERED is set.
    unsafe { &*core::ptr::addr_of!(PLATFORM) }
}

/// Discover platform hardware from a DTB at the given physical address.
/// Must be called pre-MMU (raw physical address) or with identity mapping.
///
/// Owns all DTB validation: magic check, size check, parsing, and
/// required-field validation. Returns `Err` if any essential hardware
/// is missing — the caller should halt immediately.
pub fn discover(dtb_paddr: u64) -> Result<(), PlatformError> {
    // Validate DTB magic before reading the full blob.
    let header = unsafe {
        // SAFETY: pre-MMU physical address; caller guarantees dtb_paddr
        // points to at least 40 bytes of readable memory.
        core::slice::from_raw_parts(dtb_paddr as *const u8, 40)
    };
    let magic = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    if magic != 0xd00dfeed {
        return Err(PlatformError::NoDtb);
    }

    let dtb_size = lockjaw_types::fdt::dtb_content_size(header)
        .map_err(|_| PlatformError::NoDtb)?;

    let dtb = unsafe {
        // SAFETY: dtb_paddr + dtb_size within DTB blob; pre-MMU physical address
        core::slice::from_raw_parts(dtb_paddr as *const u8, dtb_size)
    };

    // Use the lightweight scanner — fixed-size output, no large array
    // on the kernel boot stack.
    let hw = lockjaw_types::fdt::scan_platform(dtb)
        .map_err(|_| PlatformError::ParseFailed)?;

    // Validate required fields — all essential hardware must be present.
    if hw.pl011_base == 0 {
        return Err(PlatformError::MissingPl011);
    }
    if hw.gicd_base == 0 {
        return Err(PlatformError::MissingGic);
    }
    if hw.ram_size == 0 {
        return Err(PlatformError::MissingRam);
    }

    // SAFETY: single-core boot, no concurrent access yet.
    let p = unsafe { &mut *core::ptr::addr_of_mut!(PLATFORM) };

    p.pl011_base = hw.pl011_base;
    p.gicd_base = hw.gicd_base;
    p.gic_secondary_base = hw.gic_secondary_base;
    p.gic_v2 = hw.gic_v2;
    p.ram_base = hw.ram_base;
    p.ram_size = hw.ram_size;

    // Heuristic: infer device MMIO region from PL011 address range.
    // Works for QEMU virt (0x08000000) and Pi 4B (0xFE000000).
    // Future platforms may need explicit MMIO range discovery from DTB.
    if hw.pl011_base >= 0xFE00_0000 {
        p.device_mmio_base = 0xFE00_0000;
    } else {
        p.device_mmio_base = 0x0800_0000;
    }

    p.smp_method = hw.smp_method;
    p.cpu_count = hw.cpu_count;
    let count = hw.cpu_count as usize;
    let copy_count = if count <= MAX_CPUS { count } else { MAX_CPUS };
    p.cpus[..copy_count].copy_from_slice(&hw.cpus[..copy_count]);

    DISCOVERED.store(true, Ordering::Release);
    Ok(())
}
