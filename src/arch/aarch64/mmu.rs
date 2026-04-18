use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
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

/// L0 table for TTBR1 (higher-half kernel, upper VA half).
static mut KERNEL_L0: PageTable = PageTable::empty();

/// L1 table for TTBR1 higher-half mapping.
/// Same physical mapping as BOOT_L1 but accessed via upper-half VAs.
static mut KERNEL_L1: PageTable = PageTable::empty();

/// L2 table for the 1 GB RAM region (0x4000_0000). Replaces the L1 block
/// with 512 × 2 MB block entries, except the one containing the guard page
/// which gets broken down to L3.
static mut KERNEL_L2_RAM: PageTable = PageTable::empty();

/// L3 table for the 2 MB region containing the guard page.
/// 512 × 4 KB page entries, with the guard page entry left invalid.
static mut KERNEL_L3_GUARD: PageTable = PageTable::empty();

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

    // 4. TTBR1_EL1 — zeroed here; enable_higher_half() installs the real table
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

// ---------------------------------------------------------------------------
// Higher-half kernel mapping (Milestone 2.5)
// ---------------------------------------------------------------------------

/// Build TTBR1 page tables and install them, enabling higher-half addresses.
///
/// After this call, the kernel is reachable at both identity-mapped (TTBR0)
/// and higher-half (TTBR1) addresses simultaneously. Both resolve to the
/// same physical memory.
///
/// Higher-half VA scheme:
///   PA 0x0000_0000 → VA 0xFFFF_0000_0000_0000 (device)
///   PA 0x4000_0000 → VA 0xFFFF_0000_4000_0000 (RAM)
///
/// # Safety
/// MMU must already be enabled with identity mapping.
pub unsafe fn enable_higher_half() {
    // Build KERNEL_L1 with the same physical mappings as BOOT_L1
    KERNEL_L1.entries[0] = PageTableEntry::new_block(
        PhysAddr::new(0x0000_0000),
        MAIR_DEVICE,
        AP_RW_EL1,
        SH_NON,
    );

    KERNEL_L1.entries[1] = PageTableEntry::new_block(
        PhysAddr::new(0x4000_0000),
        MAIR_NORMAL,
        AP_RW_EL1,
        SH_INNER,
    );

    KERNEL_L0.entries[0] = PageTableEntry::new_table(
        PhysAddr::new(&raw const KERNEL_L1 as u64),
    );

    // Install TTBR1
    let ttbr1 = &raw const KERNEL_L0 as u64;
    asm!(
        "msr TTBR1_EL1, {val}",             // Write kernel page table base
        val = in(reg) ttbr1,
    );

    // Synchronize: ISB + TLB invalidate + barriers
    asm!(
        "isb",                               // Sync TTBR1 write
    );
    asm!(
        "tlbi vmalle1is",                    // Invalidate all TLB entries
    );
    asm!(
        "dsb ish",                           // Ensure TLB invalidation completes
    );
    asm!(
        "isb",                               // Sync before using new mappings
    );

    // Move stack pointer to higher-half address.
    // Both addresses map to the same physical page, so stack contents are unchanged.
    asm!(
        "mov {tmp}, sp",                     // Read current SP (identity-mapped address)
        "add {tmp}, {tmp}, {offset}",        // Add higher-half offset
        "mov sp, {tmp}",                     // Write new SP (higher-half address)
        tmp = out(reg) _,
        offset = in(reg) KERNEL_VA_OFFSET,
    );
}

// ---------------------------------------------------------------------------
// Guard page setup (Milestone 2.6)
// ---------------------------------------------------------------------------

/// Refine the TTBR1 RAM mapping from a single 1 GB block into 2 MB blocks
/// and 4 KB pages around the stack, leaving the guard page unmapped.
///
/// Before: KERNEL_L1[1] = 1 GB block covering all RAM
/// After:  KERNEL_L1[1] → L2 table of 2 MB blocks
///         One L2 entry  → L3 table of 4 KB pages
///         Guard page L3 entry = invalid (unmapped)
///
/// # Safety
/// Higher-half mapping must be active. `guard_page_phys` must be the
/// physical address of the guard page (from linker symbol `__guard_page`).
pub unsafe fn setup_guard_page(guard_page_phys: PhysAddr) {
    let ram_base: u64 = 0x4000_0000;

    // Step 1: Fill L2 with 2 MB block descriptors covering the 1 GB RAM region
    for i in 0..512 {
        let block_phys = ram_base + (i as u64) * (2 * 1024 * 1024); // 2 MB per entry
        KERNEL_L2_RAM.entries[i] = PageTableEntry::new_block(
            PhysAddr::new(block_phys),
            MAIR_NORMAL,
            AP_RW_EL1,
            SH_INNER,
        );
    }

    // Step 2: Find the 2 MB block containing the guard page
    let guard_offset = guard_page_phys.as_u64() - ram_base;
    let l2_index = (guard_offset >> 21) as usize; // 2 MB = 1 << 21
    let l3_base_phys = ram_base + (l2_index as u64) * (2 * 1024 * 1024);

    // Step 3: Fill L3 with 4 KB page descriptors for that 2 MB region
    for i in 0..512 {
        let page_phys = l3_base_phys + (i as u64) * 4096;
        KERNEL_L3_GUARD.entries[i] = PageTableEntry::new_page(
            PhysAddr::new(page_phys),
            MAIR_NORMAL,
            AP_RW_EL1,
            SH_INNER,
        );
    }

    // Step 4: Unmap the guard page — set its L3 entry to invalid
    let l3_index = ((guard_offset & 0x001F_FFFF) >> 12) as usize;
    KERNEL_L3_GUARD.entries[l3_index] = PageTableEntry::empty();

    // Step 5: Replace the L2 block entry with a table descriptor to L3
    KERNEL_L2_RAM.entries[l2_index] = PageTableEntry::new_table(
        PhysAddr::new(&raw const KERNEL_L3_GUARD as u64),
    );

    // Step 6: Replace the L1 block entry with a table descriptor to L2
    KERNEL_L1.entries[1] = PageTableEntry::new_table(
        PhysAddr::new(&raw const KERNEL_L2_RAM as u64),
    );

    // Step 7: TLB invalidate + barriers
    asm!(
        "tlbi vmalle1is",                    // Invalidate all TLB entries
    );
    asm!(
        "dsb ish",                           // Ensure invalidation completes
    );
    asm!(
        "isb",                               // Sync pipeline
    );
}

/// Drop from EL1 to EL0 with a specific TTBR0 page table.
///
/// Installs the given page table in TTBR0, sets user SP and entry point,
/// then `eret` drops to EL0. This function never returns.
///
/// # Safety
/// `ttbr0_paddr` must point to a valid L0 page table with user mappings.
pub unsafe fn drop_to_el0_with_ttbr0(ttbr0_paddr: PhysAddr, entry_va: u64, stack_top: u64) -> ! {
    let ttbr0 = ttbr0_paddr.as_u64();

    asm!(
        "msr TTBR0_EL1, {ttbr0}",           // Install user page table
        "dsb ish",                            // Ensure TTBR0 write completes
        "tlbi vmalle1is",                     // Flush all TLB entries
        "dsb ish",                            // Ensure TLB flush completes
        "isb",                                // Sync pipeline
        "msr SP_EL0, {sp}",                  // Set user stack pointer
        "msr ELR_EL1, {pc}",                 // Set user entry point
        "mov x0, #0",                         // SPSR = 0: EL0t mode, all DAIF clear (IRQs on)
        "msr SPSR_EL1, x0",                  // Write Saved Program Status Register
        "eret",                               // Drop to EL0
        ttbr0 = in(reg) ttbr0,
        sp = in(reg) stack_top,
        pc = in(reg) entry_va,
        options(noreturn),
    );
}
