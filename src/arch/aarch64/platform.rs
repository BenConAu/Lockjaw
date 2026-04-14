/// Platform constants for QEMU virt machine (AArch64).
///
/// These are specific to the QEMU virt board. On real hardware, some of
/// these would be read from the device tree at boot.

/// Kernel load address. QEMU's -kernel flag loads the ELF at this physical address.
/// Kernel load address. Offset 2MB from RAM_BASE to leave room for the
/// 1MB DTB that QEMU places at RAM_BASE during bare-metal ELF boot.
pub const KERNEL_LOAD_ADDR: u64 = 0x4020_0000;

/// PL011 UART0 physical base address.
pub const UART0_BASE_PHYS: u64 = 0x0900_0000;

/// GICv3 distributor physical base address.
pub const GICD_BASE_PHYS: u64 = 0x0800_0000;

/// GICv3 redistributor physical base address.
pub const GICR_BASE_PHYS: u64 = 0x080A_0000;

/// Physical base address of RAM.
pub const RAM_BASE: u64 = 0x4000_0000;

/// Device MMIO region base (covers UART and GIC in one 2MB block).
pub const DEVICE_MMIO_BASE: u64 = 0x0080_0000;

/// Virtual timer PPI interrupt ID on GICv3.
/// Note: INTID 27 is the virtual timer, INTID 30 is the physical timer.
pub const VIRTUAL_TIMER_INTID: u32 = 27;
