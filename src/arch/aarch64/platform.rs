/// Platform discovery from DTB.
///
/// At boot, the DTB is parsed to discover hardware addresses. Before
/// this runs, QEMU-compatible defaults are used. After discover(),
/// all platform constants come from the DTB.
///
/// The old hardcoded constants are kept as compile-time defaults for
/// the pre-discovery window (early boot prints, MMU setup).

use core::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// Compile-time defaults (QEMU virt, used before DTB discovery)
// ---------------------------------------------------------------------------

/// PL011 UART0 physical base address (QEMU virt default).
pub const DEFAULT_UART0_BASE: u64 = 0x0900_0000;

/// GIC distributor physical base address (QEMU virt default).
pub const DEFAULT_GICD_BASE: u64 = 0x0800_0000;

/// GIC redistributor / CPU interface physical base address (QEMU virt default).
pub const DEFAULT_GICR_BASE: u64 = 0x080A_0000;

/// Physical base address of RAM (QEMU virt default).
pub const DEFAULT_RAM_BASE: u64 = 0x4000_0000;

/// Device MMIO region base (QEMU virt default).
pub const DEFAULT_DEVICE_MMIO_BASE: u64 = 0x0800_0000;

/// Virtual timer PPI interrupt ID (generic ARMv8, platform-independent).
pub const VIRTUAL_TIMER_INTID: u32 = 27;

/// Maximum number of CPUs supported.
pub const MAX_CPUS: usize = 4;

// ---------------------------------------------------------------------------
// Runtime platform info (populated from DTB)
// ---------------------------------------------------------------------------

/// Discovered platform information. Populated by discover() from DTB.
/// Before discover() runs, all fields hold QEMU virt defaults.
pub struct PlatformInfo {
    pub uart0_base: u64,
    pub gicd_base: u64,
    /// GICv3: redistributor base. GICv2: CPU interface base.
    pub gic_secondary_base: u64,
    pub ram_base: u64,
    pub ram_size: u64,
    pub device_mmio_base: u64,
    pub gic_v2: bool,
}

/// Global platform info. Initialized with QEMU virt defaults, then
/// overwritten by discover() before any consumer reads it.
static mut PLATFORM: PlatformInfo = PlatformInfo {
    uart0_base: DEFAULT_UART0_BASE,
    gicd_base: DEFAULT_GICD_BASE,
    gic_secondary_base: DEFAULT_GICR_BASE,
    ram_base: DEFAULT_RAM_BASE,
    ram_size: 0x0800_0000, // 128 MB default
    device_mmio_base: DEFAULT_DEVICE_MMIO_BASE,
    gic_v2: false,
};

static DISCOVERED: AtomicBool = AtomicBool::new(false);

/// Read platform info. Before discover() runs, returns QEMU virt defaults.
/// After discover(), returns DTB-discovered values.
/// All consumers must call this — never read the old constants directly.
pub fn info() -> &'static PlatformInfo {
    // SAFETY: PLATFORM is written once by discover() before any concurrent
    // access. After DISCOVERED is set, it is read-only.
    // SAFETY: PLATFORM is written once by discover() during single-core
    // boot, then read-only. No concurrent mutation after DISCOVERED is set.
    unsafe { &*core::ptr::addr_of!(PLATFORM) }
}

// Legacy constant — still used by DTB fallback path in main.rs.
pub const RAM_BASE: u64 = DEFAULT_RAM_BASE;

/// Discover platform hardware from a DTB at the given physical address.
/// Must be called pre-MMU (raw physical address) or with identity mapping.
///
/// Extracts: UART address, GIC addresses + version, RAM layout.
/// Falls back to QEMU virt defaults for anything not found in the DTB.
pub fn discover(dtb_paddr: u64) {
    // Construct a byte slice from the physical DTB address.
    // SAFETY: dtb_paddr was validated by the caller (DTB magic check).
    // Pre-MMU, physical addresses are directly accessible.
    let header = unsafe {
        // SAFETY: dtb_paddr validated by caller (magic check); pre-MMU physical address
        core::slice::from_raw_parts(dtb_paddr as *const u8, 40)
    };
    let dtb_size = match lockjaw_types::fdt::dtb_content_size(header) {
        Ok(size) => size,
        Err(_) => return, // invalid DTB, keep defaults
    };
    let dtb = unsafe {
        // SAFETY: dtb_paddr + dtb_size within DTB blob; pre-MMU physical address
        core::slice::from_raw_parts(dtb_paddr as *const u8, dtb_size)
    };

    // Use the lightweight scanner — fixed-size output, no large array
    // on the kernel boot stack.
    let hw = match lockjaw_types::fdt::scan_platform(dtb) {
        Ok(hw) => hw,
        Err(_) => return, // parse failed, keep defaults
    };

    // SAFETY: single-core boot, no concurrent access yet.
    // SAFETY: single-core boot, no concurrent access yet.
    let p = unsafe { &mut *core::ptr::addr_of_mut!(PLATFORM) };

    if hw.uart_base != 0 {
        p.uart0_base = hw.uart_base;
    }
    if hw.gicd_base != 0 {
        p.gicd_base = hw.gicd_base;
        p.gic_secondary_base = hw.gic_secondary_base;
        p.gic_v2 = hw.gic_v2;
    }
    if hw.ram_base != 0 || hw.ram_size != 0 {
        p.ram_base = hw.ram_base;
        p.ram_size = hw.ram_size;
        // Infer device MMIO base from platform layout.
        // Pi 4B: peripherals at 0xFE000000. QEMU virt: 0x08000000.
        if hw.uart_base >= 0xFE00_0000 {
            p.device_mmio_base = 0xFE00_0000;
        }
    }

    DISCOVERED.store(true, Ordering::Release);
}
