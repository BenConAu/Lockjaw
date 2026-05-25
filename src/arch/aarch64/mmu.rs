use crate::mm::addr::PhysAddr;
use crate::mm::page_table::*;
use core::arch::asm;
use lockjaw_types::addr::KernelImageVa;

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
// L0[1] kernel-image mapping
// ---------------------------------------------------------------------------
// The kernel image is linked at KERNEL_IMAGE_LINKER_BASE (a fixed VA in
// L0[1]) but loaded by firmware at an arbitrary physical address. The
// boot trampoline:
//   1. Discovers the runtime PA via PC-relative (`&raw const __sym`).
//   2. Computes KERNEL_PHYS_OFFSET = load_PA - LINKER_BASE.
//   3. Walks every kernel image page from __kernel_start through
//      __per_cpu_stacks_end and writes 4 KB L3 PTEs mapping
//      load_PA + offset → LINKER_BASE + offset.
//   4. Installs the L0[1] table descriptor.
// After the pivot, all kernel-image accesses go through this mapping.

/// Linker.ld ORIGIN. Must equal the value in linker.ld.
pub const KERNEL_IMAGE_LINKER_BASE: u64 = 0xFFFF_0080_0000_0000;

/// L1 table for L0[1] kernel-image region. Only entry [0] is used —
/// the kernel image fits well within one 1 GB L1[0] entry.
static mut KERNEL_IMAGE_L1: PageTable = PageTable::empty();

/// L2 table feeding KERNEL_IMAGE_L1[0]. Each entry covers 2 MB and
/// points to one of `KERNEL_IMAGE_L3`. Populated by
/// `init_kernel_image_map` based on the image's actual span.
static mut KERNEL_IMAGE_L2: PageTable = PageTable::empty();

/// Number of L3 tables reserved for the kernel image. Each L3 covers
/// 2 MB, so `MAX_KERNEL_IMAGE_L3` × 2 MB = supported image span.
/// Current kernel image (~256 KB code/data + 2 MB-aligned per-CPU
/// stacks) fits in ≤ 2 L3 tables; 4 gives headroom up to 8 MB.
const MAX_KERNEL_IMAGE_L3: usize = 4;

/// L3 tables for KERNEL_IMAGE_L2 entries. Each table contains 512
/// 4 KB page entries; one table per 2 MB span of kernel image.
static mut KERNEL_IMAGE_L3: [PageTable; MAX_KERNEL_IMAGE_L3] =
    [const { PageTable::empty() }; MAX_KERNEL_IMAGE_L3];

/// Runtime PA-VA shift: load_PA - LINKER_BASE. Set once by
/// `init_kernel_image_map`. Read by `kernel_image_kva_to_pa` and by
/// callers that need to know the relink delta (e.g., the pivot).
///
/// On QEMU virt this is `0x4020_0000 - 0xFFFF_0080_0000_0000`, which
/// wraps; the wrapping arithmetic in the converter accepts that.
static mut KERNEL_PHYS_OFFSET: u64 = 0;

/// Recover the physical address of a kernel-image VA. Uses the
/// boot-discovered offset; valid only for VAs in the kernel image
/// span ([__kernel_start, __per_cpu_stacks_end)) and only after
/// `init_kernel_image_map` has run.
///
/// All current PA-recovery sites in the boot path run **pre-pivot**,
/// where `&__sym as u64` returns the runtime PA directly via
/// PC-relative addressing — they do not need this helper. The
/// helper exists for post-pivot callers that hold a `KernelImageVa`
/// and need its PA. None today; kept and exported so future code
/// can use it without re-deriving the math.
///
/// # Safety
/// Caller must ensure `va` falls inside the kernel image span. Out
/// of range yields a meaningless paddr.
#[allow(dead_code)]
pub unsafe fn kernel_image_kva_to_pa(va: KernelImageVa) -> PhysAddr {
    PhysAddr::new(va.as_u64().wrapping_add(KERNEL_PHYS_OFFSET))
}

/// Boot-discovered shift: LINKER_BASE - load_PA. The value the pivot
/// should add to SP/FP/LR to land them in the L0[1] mapping.
///
/// Pre-pivot, PC-relative `&raw const __sym` returns load_PA + offset
/// (PA). Adding this shift yields LINKER_BASE + offset (the L0[1] VA).
///
/// # Safety
/// Must be called after `init_kernel_image_map`.
pub unsafe fn kernel_image_pivot_shift() -> u64 {
    KERNEL_IMAGE_LINKER_BASE.wrapping_sub(load_pa_of_kernel_image())
}

/// The runtime physical address where the kernel image was loaded.
/// Recovered from KERNEL_PHYS_OFFSET (the inverse of the per-image
/// `LINKER_BASE - load_PA` shift).
unsafe fn load_pa_of_kernel_image() -> u64 {
    KERNEL_IMAGE_LINKER_BASE.wrapping_add(KERNEL_PHYS_OFFSET)
}

// ---------------------------------------------------------------------------
// Identity map setup
// ---------------------------------------------------------------------------

/// Build the identity-map page tables using 1 GB block descriptors.
///
/// Maps the first 4 GB of physical address space with attributes
/// appropriate for both QEMU virt and Pi 4B:
///   - The 1 GB block containing the kernel → normal memory
///   - QEMU device range (0x00-0x3F) → device if kernel isn't there
///   - Pi 4B device range (0xC0-0xFF) → device
///
/// # Safety
/// Must be called exactly once during boot, before `enable_mmu()`.
pub unsafe fn init_boot_page_tables() {
    // Determine which 1 GB block the kernel is loaded in.
    // AArch64 Rust emits PC-relative (adr/adrp) for &raw const, so
    // this gives the actual runtime physical address regardless of
    // the linker-assigned address. No phys_offset adjustment needed.
    let kernel_phys = &raw const BOOT_L0 as u64;
    let kernel_gb = (kernel_phys >> 30) as usize;

    // Map the first 4 L1 entries (4 GB total).
    // The kernel's 1 GB block is normal memory; everything else is
    // device. Covers QEMU (kernel in block 1, devices in block 0)
    // and Pi 4B (kernel in block 0, devices in block 3).
    for i in 0..4 {
        let block_base = (i as u64) << 30;
        if i == kernel_gb {
            BOOT_L1.entries[i] = PageTableEntry::new_block(
                PhysAddr::new(block_base),
                MAIR_NORMAL,
                AP_RW_EL1,
                SH_INNER,
            );
        } else {
            BOOT_L1.entries[i] = PageTableEntry::new_block(
                PhysAddr::new(block_base),
                MAIR_DEVICE,
                AP_RW_EL1,
                SH_NON,
            );
        }
    }

    // L0[0]: Table descriptor pointing to BOOT_L1.
    // &raw const gives the runtime physical address (adr on AArch64).
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
/// Physical address of the kernel L0 page table (TTBR1 root). The
/// kernel binary is linked at physical addresses today, so
/// `&raw const KERNEL_L0` returns the static's physical location.
/// Used by the KVM allocator to install its L1 table at
/// `KERNEL_L0[KVM_L0_INDEX]`.
pub fn kernel_l0_paddr() -> PhysAddr {
    PhysAddr::new(&raw const KERNEL_L0 as u64)
}

pub unsafe fn enable_higher_half() {
    // Build KERNEL_L1 with the same layout as BOOT_L1.
    let kernel_phys = &raw const BOOT_L0 as u64;
    let kernel_gb = (kernel_phys >> 30) as usize;

    for i in 0..4 {
        let block_base = (i as u64) << 30;
        if i == kernel_gb {
            KERNEL_L1.entries[i] = PageTableEntry::new_block(
                PhysAddr::new(block_base),
                MAIR_NORMAL,
                AP_RW_EL1,
                SH_INNER,
            );
        } else {
            KERNEL_L1.entries[i] = PageTableEntry::new_block(
                PhysAddr::new(block_base),
                MAIR_DEVICE,
                AP_RW_EL1,
                SH_NON,
            );
        }
    }

    KERNEL_L0.entries[0] = PageTableEntry::new_table(
        PhysAddr::new(&raw const KERNEL_L1 as u64),
    );

    // Build the L0[1] kernel-image mapping. Computes KERNEL_PHYS_OFFSET,
    // walks the kernel image one 4 KB page at a time, and installs L3
    // PTEs mapping load_PA + offset → LINKER_BASE + offset. Skips guard
    // pages (left invalid). Must run before the pivot so the new VAs
    // are reachable when PC moves into them.
    init_kernel_image_map();

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

    // SP and PC transition to higher-half is done by _pivot_to_higher_half
    // (boot.rs assembly), called separately after this function returns.
    // That stub adds KERNEL_VA_OFFSET to SP, FP, and LR atomically, so
    // the caller resumes at a higher-half address.
}

/// Build the L0[1] kernel-image mapping. Pre-MMU/pre-pivot, PC-relative
/// `&raw const __sym` returns the runtime PA of `__sym`; that's how we
/// discover where firmware loaded the kernel.
///
/// Steps:
/// 1. Compute KERNEL_PHYS_OFFSET from `__kernel_start`'s runtime PA
///    minus its linker VA (the constant LINKER_BASE).
/// 2. Walk every kernel image page from `__kernel_start` through
///    `__per_cpu_stacks_end`, in 4 KB strides.
/// 3. For each page that is NOT a per-CPU guard page, write a 4 KB L3
///    page entry mapping `load_PA + offset` → `LINKER_BASE + offset`
///    in the appropriate `KERNEL_IMAGE_L3` table.
/// 4. Install `KERNEL_IMAGE_L2` at `KERNEL_IMAGE_L1[0]` and
///    `KERNEL_IMAGE_L1` at `KERNEL_L0[1]`.
///
/// Guard pages are intentionally left as zero L3 entries (invalid),
/// matching `setup_guard_pages`'s treatment in the linear map.
///
/// # Safety
/// Must be called before the pivot. Mutates statics under the
/// pre-pivot single-CPU invariant.
unsafe fn init_kernel_image_map() {
    extern "C" {
        static __kernel_start: u8;
        static __per_cpu_stacks_end: u8;
        static __guard_page_0: u8;
        static __guard_page_1: u8;
        static __guard_page_2: u8;
        static __guard_page_3: u8;
    }

    // Step 1: discover the load PA via PC-relative on a kernel symbol,
    // then derive the boot-wide phys offset.
    let load_pa = &raw const __kernel_start as u64;
    let phys_offset = load_pa.wrapping_sub(KERNEL_IMAGE_LINKER_BASE);
    KERNEL_PHYS_OFFSET = phys_offset;

    // Step 2: walk the image. Per-CPU guard PAs (also via PC-relative).
    let image_end_pa = &raw const __per_cpu_stacks_end as u64;
    let image_size = image_end_pa.wrapping_sub(load_pa);
    let guard_pas: [u64; 4] = [
        &raw const __guard_page_0 as u64,
        &raw const __guard_page_1 as u64,
        &raw const __guard_page_2 as u64,
        &raw const __guard_page_3 as u64,
    ];
    let page_size: u64 = lockjaw_types::addr::PAGE_SIZE;
    let pages = (image_size + page_size - 1) / page_size;
    let l3_span: u64 = 512 * page_size; // 2 MB per L3 table
    let max_pages = (MAX_KERNEL_IMAGE_L3 as u64) * 512;
    assert!(
        pages <= max_pages,
        "kernel image span exceeds MAX_KERNEL_IMAGE_L3 — bump the constant",
    );

    // Step 3: write L3 PTEs for every non-guard page.
    for i in 0..pages {
        let page_pa = load_pa + i * page_size;
        if guard_pas.iter().any(|&g| g == page_pa) {
            continue; // leave guard pages invalid
        }
        let page_va_offset = i * page_size;
        let l2_idx = (page_va_offset / l3_span) as usize;
        let l3_idx = ((page_va_offset / page_size) as usize) & 0x1FF;
        // SAFETY: l2_idx < MAX_KERNEL_IMAGE_L3 guaranteed by pages cap.
        KERNEL_IMAGE_L3[l2_idx].entries[l3_idx] = PageTableEntry::new_page(
            PhysAddr::new(page_pa),
            MAIR_NORMAL,
            AP_RW_EL1,
            SH_INNER,
        );
    }

    // Step 4: install table descriptors. KERNEL_IMAGE_L2 entry [i] →
    // KERNEL_IMAGE_L3[i] for each L3 used (we install all, even unused
    // tables; their L3 entries are all zero so they're harmless).
    for i in 0..MAX_KERNEL_IMAGE_L3 {
        KERNEL_IMAGE_L2.entries[i] = PageTableEntry::new_table(
            PhysAddr::new(&raw const KERNEL_IMAGE_L3[i] as u64),
        );
    }
    KERNEL_IMAGE_L1.entries[0] = PageTableEntry::new_table(
        PhysAddr::new(&raw const KERNEL_IMAGE_L2 as u64),
    );
    KERNEL_L0.entries[1] = PageTableEntry::new_table(
        PhysAddr::new(&raw const KERNEL_IMAGE_L1 as u64),
    );
}

// ---------------------------------------------------------------------------
// Secondary CPU MMU init
// ---------------------------------------------------------------------------

/// Enable MMU on a secondary CPU using the page tables already built by
/// CPU 0. Installs TTBR0 (identity map), TTBR1 (higher-half with guard
/// pages), programs MAIR/TCR/SCTLR, and transitions SP to higher-half.
///
/// # Safety
/// CPU 0 must have completed init_boot_page_tables(), enable_mmu(),
/// enable_higher_half(), and setup_guard_pages() before any secondary
/// calls this. The page tables are shared read-only across all cores.
pub unsafe fn enable_mmu_secondary() {
    // MAIR — same memory attributes as primary
    asm!("msr MAIR_EL1, {val}", val = in(reg) MAIR_EL1_VALUE);

    // TCR — same translation control as primary
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
    asm!("msr TCR_EL1, {val}", val = in(reg) tcr);

    // TTBR0 — identity map (same L0 table as primary)
    let ttbr0 = &raw const BOOT_L0 as u64;
    asm!("msr TTBR0_EL1, {val}", val = in(reg) ttbr0);

    // TTBR1 — higher-half (same L0 table, already has guard page refinements)
    let ttbr1 = &raw const KERNEL_L0 as u64;
    asm!("msr TTBR1_EL1, {val}", val = in(reg) ttbr1);

    // TLB invalidate + barriers
    asm!("tlbi vmalle1is", "dsb ish", "isb");

    // Enable MMU + caches
    let mut sctlr: u64;
    asm!("mrs {val}, SCTLR_EL1", val = out(reg) sctlr);
    sctlr |= (1 << 0) | (1 << 2) | (1 << 12); // M + C + I
    asm!("msr SCTLR_EL1, {val}", val = in(reg) sctlr);
    asm!("isb");

    // SP and PC transition to higher-half is done by _pivot_to_higher_half
    // (boot.rs assembly), called separately after this function returns.
}

// ---------------------------------------------------------------------------
// Guard page setup (Milestone 2.6)
// ---------------------------------------------------------------------------

/// Refine the TTBR1 RAM mapping from a single 1 GB block into 2 MB blocks
/// and 4 KB pages around the stacks, leaving guard pages unmapped.
///
/// Before: KERNEL_L1[1] = 1 GB block covering all RAM
/// After:  KERNEL_L1[1] → L2 table of 2 MB blocks
///         One L2 entry  → L3 table of 4 KB pages
///         Guard page L3 entries = invalid (unmapped)
///
/// All guard pages must be within the same 2 MB L2 region (true when
/// they are contiguous in the linker script, adjacent to the kernel image).
///
/// # Safety
/// Higher-half mapping must be active. Each element in `guard_pages`
/// must be a valid guard page physical address from a linker symbol.
pub unsafe fn setup_guard_pages(guard_pages: &[PhysAddr]) {
    assert!(!guard_pages.is_empty(), "need at least one guard page");
    let ram_base: u64 = super::platform::info().ram_base;

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

    // Step 2: Find the 2 MB block containing the first guard page.
    // All guard pages must be in the same 2 MB region (asserted below).
    let first_offset = guard_pages[0].as_u64() - ram_base;
    let l2_index = (first_offset >> 21) as usize; // 2 MB = 1 << 21
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

    // Step 4: Unmap each guard page — set its L3 entry to invalid
    for &gp in guard_pages {
        let guard_offset = gp.as_u64() - ram_base;
        let gp_l2_index = (guard_offset >> 21) as usize;
        assert!(gp_l2_index == l2_index, "guard pages must be in same 2 MB region");
        let l3_index = ((guard_offset & 0x001F_FFFF) >> 12) as usize;
        KERNEL_L3_GUARD.entries[l3_index] = PageTableEntry::empty();
    }

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

// M6 sub-commit 2a step 2's `exclude_dma_pool_from_direct_map`
// was deleted in C1 of the cacheable-DMA migration (see
// docs/cacheable-dma-migration-plan.md). The pool now
// participates in the kernel TTBR1 direct map as Cacheable
// Inner+Outer WB; sync syscalls maintain coherency at the
// device-handoff points instead of preventing the alias by
// excluding the mapping. The single-attribute invariant the M6
// substrate enforces is preserved: pool pages are now Cacheable
// EVERYWHERE they are mapped (kernel + every user process), no
// NC opt-in.

/// Drop from EL1 to EL0 with a specific TTBR0 page table.
///
/// Installs the given page table in TTBR0, sets user SP and entry point,
/// then `eret` drops to EL0. This function never returns.
///
/// # Safety
/// `ttbr0_paddr` must point to a valid L0 page table with user mappings.
pub unsafe fn drop_to_el0_with_ttbr0(
    ttbr0_paddr: PhysAddr, entry_va: u64, stack_top: u64, user_arg: u64,
) -> ! {
    let ttbr0 = ttbr0_paddr.as_u64();

    asm!(
        "msr TTBR0_EL1, {ttbr0}",           // Install user page table
        "dsb ish",                            // Ensure TTBR0 write completes
        "tlbi vmalle1is",                     // Flush all TLB entries
        "dsb ish",                            // Ensure TLB flush completes
        "isb",                                // Sync pipeline
        "msr SP_EL0, {sp}",                  // Set user stack pointer
        "msr ELR_EL1, {pc}",                 // Set user entry point
        "msr SPSR_EL1, xzr",                 // SPSR = 0: EL0t, IRQs on
        // Set x0 = user_arg FIRST (before zeroing, since {arg} may
        // be in any register that the zeroing would clobber).
        "mov x0, {arg}",
        // Zero x1-x30 to prevent kernel register leakage to EL0.
        "mov x1, xzr",
        "mov x2, xzr",
        "mov x3, xzr",
        "mov x4, xzr",
        "mov x5, xzr",
        "mov x6, xzr",
        "mov x7, xzr",
        "mov x8, xzr",
        "mov x9, xzr",
        "mov x10, xzr",
        "mov x11, xzr",
        "mov x12, xzr",
        "mov x13, xzr",
        "mov x14, xzr",
        "mov x15, xzr",
        "mov x16, xzr",
        "mov x17, xzr",
        "mov x18, xzr",
        "mov x19, xzr",
        "mov x20, xzr",
        "mov x21, xzr",
        "mov x22, xzr",
        "mov x23, xzr",
        "mov x24, xzr",
        "mov x25, xzr",
        "mov x26, xzr",
        "mov x27, xzr",
        "mov x28, xzr",
        "mov x29, xzr",
        "mov x30, xzr",
        "msr TPIDR_EL0, xzr",                // Zero TLS pointer — prevent kernel leak to EL0
        "eret",                               // Drop to EL0
        ttbr0 = in(reg) ttbr0,
        sp = in(reg) stack_top,
        pc = in(reg) entry_va,
        arg = in(reg) user_arg,
        options(noreturn),
    );
}
