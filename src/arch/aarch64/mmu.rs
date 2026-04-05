use crate::mm::addr::PhysAddr;
use crate::mm::page_table::*;
use core::arch::asm;

// ---------------------------------------------------------------------------
// Boot page tables (static, in BSS)
// ---------------------------------------------------------------------------

/// L0 table for TTBR0 (identity map, lower VA half).
/// Each entry covers 512 GB. We only use entry [0].
static mut BOOT_L0: PageTable = PageTable::empty();

/// L1 table for TTBR0 identity map.
/// Each entry covers 1 GB. We use:
///   [0] = 0x0000_0000 device memory (UART, GIC, flash)
///   [1] = 0x4000_0000 normal memory (128 MB RAM)
static mut BOOT_L1: PageTable = PageTable::empty();

// ---------------------------------------------------------------------------
// Identity map setup
// ---------------------------------------------------------------------------

/// Build the identity-map page tables using 1 GB block descriptors.
///
/// After this, the tables are ready for MMU enable:
///   VA 0x0000_0000..0x3FFF_FFFF → PA 0x0000_0000 (device)
///   VA 0x4000_0000..0x7FFF_FFFF → PA 0x4000_0000 (normal)
///
/// # Safety
/// Must be called exactly once during boot, before `enable_mmu()`.
pub unsafe fn init_boot_page_tables() {
    // L1[0]: First 1 GB as device memory (covers UART at 0x0900_0000, GIC, flash)
    BOOT_L1.entries[0] = PageTableEntry::new_block(
        PhysAddr::new(0x0000_0000),
        MAIR_DEVICE,
        AP_RW_EL1,
        SH_NON,
    );

    // L1[1]: Second 1 GB as normal memory (covers 128 MB RAM at 0x4000_0000)
    BOOT_L1.entries[1] = PageTableEntry::new_block(
        PhysAddr::new(0x4000_0000),
        MAIR_NORMAL,
        AP_RW_EL1,
        SH_INNER,
    );

    // L0[0]: Table descriptor pointing to BOOT_L1
    BOOT_L0.entries[0] = PageTableEntry::new_table(
        PhysAddr::new(&raw const BOOT_L1 as u64),
    );
}

/// Enable the MMU with the identity-map page tables.
///
/// Sequence: MAIR → TCR → TTBR0 → TLB invalidate → barriers → SCTLR.M
///
/// # Safety
/// `init_boot_page_tables()` must have been called first. Caller must be
/// executing at an identity-mapped address. After this returns, the MMU is on.
pub unsafe fn enable_mmu() {
    // 1. MAIR_EL1 — memory attribute definitions
    asm!(
        "msr MAIR_EL1, {val}",              // Write Memory Attribute Indirection Register
        val = in(reg) MAIR_EL1_VALUE,
    );

    // 2. TCR_EL1 — translation control
    //
    //    T0SZ  = 16  (48-bit VA for TTBR0)       bits [5:0]
    //    IRGN0 = 01  (WB RA WA cacheable)         bits [9:8]
    //    ORGN0 = 01  (WB RA WA cacheable)         bits [11:10]
    //    SH0   = 11  (inner shareable)             bits [13:12]
    //    TG0   = 00  (4 KB granule for TTBR0)     bits [15:14]
    //    T1SZ  = 16  (48-bit VA for TTBR1)        bits [21:16]
    //    IRGN1 = 01  (WB RA WA cacheable)         bits [25:24]
    //    ORGN1 = 01  (WB RA WA cacheable)         bits [27:26]
    //    SH1   = 11  (inner shareable)             bits [29:28]
    //    TG1   = 10  (4 KB granule for TTBR1)     bits [31:30]
    //    IPS   = 001 (36-bit physical addresses)   bits [34:32]
    let tcr: u64 = (16 << 0)           // T0SZ
        | (0b01 << 8)                  // IRGN0
        | (0b01 << 10)                 // ORGN0
        | (0b11 << 12)                 // SH0
        | (0b00 << 14)                 // TG0
        | (16 << 16)                   // T1SZ
        | (0b01 << 24)                 // IRGN1
        | (0b01 << 26)                 // ORGN1
        | (0b11 << 28)                 // SH1
        | (0b10u64 << 30)             // TG1
        | (0b001u64 << 32);           // IPS = 36-bit PA
    asm!(
        "msr TCR_EL1, {val}",               // Write Translation Control Register
        val = in(reg) tcr,
    );

    // 3. TTBR0_EL1 — point to identity-map L0 table
    let ttbr0 = &raw const BOOT_L0 as u64;
    asm!(
        "msr TTBR0_EL1, {val}",             // Write Translation Table Base Register 0
        val = in(reg) ttbr0,
    );

    // 4. TTBR1_EL1 — not used yet, will be set up for higher-half in 2.5
    asm!(
        "msr TTBR1_EL1, {val}",             // Write Translation Table Base Register 1
        val = in(reg) 0u64,
    );

    // 5. Invalidate all TLB entries
    asm!(
        "tlbi vmalle1is",                    // Invalidate all EL1 TLB entries (inner shareable)
    );

    // 6. Data Synchronization Barrier — ensure all writes are visible
    asm!(
        "dsb ish",                           // DSB inner shareable domain
    );

    // 7. Instruction Synchronization Barrier — flush pipeline
    asm!(
        "isb",                               // ISB flushes prefetched instructions
    );

    // 8. Enable MMU + caches via SCTLR_EL1
    let mut sctlr: u64;
    asm!(
        "mrs {val}, SCTLR_EL1",             // Read System Control Register
        val = out(reg) sctlr,
    );
    sctlr |= 1 << 0;                        // M: MMU enable
    sctlr |= 1 << 2;                        // C: data cache enable
    sctlr |= 1 << 12;                       // I: instruction cache enable
    asm!(
        "msr SCTLR_EL1, {val}",             // Write System Control Register
        val = in(reg) sctlr,
    );

    // 9. Final ISB — ensure MMU is active for all subsequent instructions
    asm!(
        "isb",                               // Pipeline flush after MMU enable
    );
}
