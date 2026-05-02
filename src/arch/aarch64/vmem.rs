use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET, PAGE_SIZE};
use crate::mm::page_alloc;
use crate::mm::page_table::*;
use core::ptr;

/// How many Mapping structs fit in a single 4KB page.
/// Callers allocate a page for the mapping buffer rather than using the stack.
pub const MAPPINGS_PER_PAGE: usize = PAGE_SIZE as usize / core::mem::size_of::<Mapping>();

/// A single virtual-to-physical page mapping with access permissions.
#[derive(Clone, Copy)]
pub struct Mapping {
    pub virt_addr: u64,
    pub phys_addr: PhysAddr,
    pub user_accessible: bool,
    pub executable: bool,
}

/// Errors returned by virtual memory operations.
pub enum VmemError {
    TooManyMappings,
    TooManyL3Regions,
    OutOfPages,
    InvalidParameter,
}

/// Allocate a fresh set of page tables and map the given pages.
/// Returns the physical address of the L0 table (for TTBR0).
///
/// Limits:
///   - At most MAX_MAPPINGS (32) page mappings
///   - At most MAX_L3_TABLES (8) distinct 2 MB regions containing user pages
///   - All user VAs must be in the first 1 GB (L1[0] range)
///
/// Automatically includes the kernel identity map (RAM + device MMIO)
/// so that exception vectors and kernel code remain reachable.
///
/// # Safety
/// All physical addresses in mappings must be valid allocated pages.
pub unsafe fn create_address_space(mappings: &[Mapping]) -> Result<PhysAddr, VmemError> {
    if mappings.len() > MAPPINGS_PER_PAGE {
        return Err(VmemError::TooManyMappings);
    }
    // Allocate L0
    let l0_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let l0_va = (l0_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
    ptr::write_bytes(l0_va, 0, 1);

    // We need L1 for entry [0] (covers VA 0x00000000-0x3FFFFFFF, where user pages live)
    // and L1 entry [1] for the kernel identity map (0x40000000-0x7FFFFFFF)
    let l1_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let l1_va = (l1_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
    ptr::write_bytes(l1_va, 0, 1);

    // L0[0] → L1
    (*l0_va).entries[0] = PageTableEntry::new_table(l1_page.start_addr());

    // Kernel physical RAM in user TTBR0 (kernel-only, normal memory).
    // Required: some kernel exception handling path accesses TTBR0-range
    // addresses in the kernel physical range. Removing this causes
    // immediate crash on EL0 entry — must investigate which code path.
    //
    // QEMU: kernel at 0x4020_0000 in L1[1], ram_base = 0x4000_0000.
    // Pi 4B: kernel at 0x8_0000 in L1[0], ram_base = 0x0. L1[0] is
    // already used for user pages (L1[0] → L2), so a 1GB block won't
    // work. Pi 4B fix requires identifying the TTBR0 access and either
    // eliminating it or mapping kernel pages via L2/L3 entries.
    (*l1_va).entries[1] = PageTableEntry::new_block(
        PhysAddr::new(super::platform::info().ram_base),
        MAIR_NORMAL,
        AP_RW_EL1,
        SH_INNER,
    );

    // We need L2 for user pages in the first 1GB range
    let l2_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let l2_va = (l2_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
    ptr::write_bytes(l2_va, 0, 1);

    // L1[0] → L2
    (*l1_va).entries[0] = PageTableEntry::new_table(l2_page.start_addr());

    // Device MMIO — kernel-only, device memory attributes.
    // On QEMU virt: GIC at 0x0800_0000, UART at 0x0900_0000 (first 1GB).
    // On Pi 4B: peripherals at 0xFE00_0000 (fourth 1GB).
    let device_mmio_base = super::platform::info().device_mmio_base;
    let device_l1_idx = (device_mmio_base >> 30) as usize; // which 1GB slot
    if device_l1_idx == 0 {
        // Device MMIO in the first 1GB — use the L2 table (shared with
        // user pages). Index is within the 512-entry L2.
        let device_l2_idx = ((device_mmio_base >> 21) & 0x1FF) as usize;
        (*l2_va).entries[device_l2_idx] = PageTableEntry::new_block(
            PhysAddr::new(device_mmio_base),
            MAIR_DEVICE,
            AP_RW_EL1,
            SH_NON,
        );
    } else {
        // Device MMIO in a higher 1GB slot — map as a 1GB L1 block.
        // Aligns down to the 1GB boundary containing device_mmio_base.
        let block_base = device_l1_idx as u64 * (1 << 30);
        (*l1_va).entries[device_l1_idx] = PageTableEntry::new_block(
            PhysAddr::new(block_base),
            MAIR_DEVICE,
            AP_RW_EL1,
            SH_NON,
        );
    }

    // Map each user page. Group by L2 index (2MB region) and allocate
    // L3 tables as needed.
    // Track which L2 entries already have L3 tables.
    // Only need to track the few L2 indices that user pages fall in.
    // With our VA layout (pages around 0x400000-0x800000), at most ~4 L2 regions.
    const MAX_L3_TABLES: usize = 8;
    let mut l3_indices: [usize; MAX_L3_TABLES] = [usize::MAX; MAX_L3_TABLES];
    let mut l3_ptrs: [*mut PageTable; MAX_L3_TABLES] = [core::ptr::null_mut(); MAX_L3_TABLES];
    let mut l3_count: usize = 0;

    for m in mappings {
        let (_, _, l2_idx, l3_idx) = lockjaw_types::vmem::page_table_indices(m.virt_addr);

        // Allocate L3 table for this 2MB region if not already done
        let l3_va = {
            let mut found: *mut PageTable = core::ptr::null_mut();
            for j in 0..l3_count {
                if l3_indices[j] == l2_idx {
                    found = l3_ptrs[j];
                    break;
                }
            }
            if found.is_null() {
                if l3_count >= MAX_L3_TABLES {
                    return Err(VmemError::TooManyL3Regions);
                }
                let l3_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
                // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
                let va = (l3_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
                ptr::write_bytes(va, 0, 1);

                (*l2_va).entries[l2_idx] = PageTableEntry::new_table(l3_page.start_addr());
                l3_indices[l3_count] = l2_idx;
                l3_ptrs[l3_count] = va;
                l3_count += 1;
                va
            } else {
                found
            }
        };

        // Build the page entry with appropriate permissions
        let ap = if m.user_accessible { AP_RW_ALL } else { AP_RW_EL1 };
        let mut entry = PageTableEntry::new_page(m.phys_addr, MAIR_NORMAL, ap, SH_INNER);

        if m.user_accessible {
            // Kernel cannot execute user code
            entry = entry.with_pxn();
        }
        if !m.executable {
            // Non-executable pages get UXN (user) and PXN (kernel)
            entry = entry.with_uxn().with_pxn();
        }

        (*l3_va).entries[l3_idx] = entry;
    }

    Ok(l0_page.start_addr())
}

/// Free all page table pages allocated by create_address_space.
/// Walks L0 → L1 → L2 → L3 and frees each table page. Does NOT free
/// the user data pages mapped by L3 entries — those belong to the
/// process's owned_pages.
///
/// # Safety
/// `ttbr0` must be a valid L0 page table base from create_address_space.
/// No ASID/TTBR0 may reference this table after this call.
pub unsafe fn free_address_space(ttbr0: PhysAddr) {
    use crate::mm::addr::PhysPage;

    // SAFETY: kernel VA via KERNEL_VA_OFFSET — ttbr0 is a valid page table base
    let l0_va = (ttbr0.as_u64() + KERNEL_VA_OFFSET) as *const PageTable;

    // L0[0] → L1 (only entry we allocate)
    let l0_entry = (*l0_va).entries[0];
    if l0_entry.is_table() {
        let l1_paddr = l0_entry.output_addr();
        // SAFETY: kernel VA via KERNEL_VA_OFFSET — L1 paddr from valid L0 table entry
        let l1_va = (l1_paddr.as_u64() + KERNEL_VA_OFFSET) as *const PageTable;

        // L1[0] → L2 (user pages). L1[1] is a kernel 1GB block, not a table.
        let l1_entry = (*l1_va).entries[0];
        if l1_entry.is_table() {
            let l2_paddr = l1_entry.output_addr();
            // SAFETY: kernel VA via KERNEL_VA_OFFSET — L2 paddr from valid L1 table entry
            let l2_va = (l2_paddr.as_u64() + KERNEL_VA_OFFSET) as *const PageTable;

            // Scan L2 for table entries → L3 pages to free
            for i in 0..512 {
                let entry = (*l2_va).entries[i];
                if entry.is_table() {
                    page_alloc::dealloc_page(PhysPage::containing(entry.output_addr()));
                }
            }

            page_alloc::dealloc_page(PhysPage::containing(l2_paddr));
        }

        page_alloc::dealloc_page(PhysPage::containing(l1_paddr));
    }

    page_alloc::dealloc_page(PhysPage::containing(ttbr0));
}

/// Translate a user virtual address to a kernel-accessible VA by walking
/// the given page table. Returns None if any level is unmapped.
///
/// Walk logic is in lockjaw_types::page_table::PageTableWalk (tested on host).
/// This function only does the physical memory reads via TTBR1.
///
/// # Safety
/// `ttbr0_paddr` must be a valid L0 page table base.
pub unsafe fn translate_user_va(ttbr0_paddr: PhysAddr, user_va: u64) -> Option<u64> {
    use lockjaw_types::page_table::{PageTableWalk, WalkResult};

    let (mut walk, mut result) = PageTableWalk::start(ttbr0_paddr.as_u64(), user_va);
    loop {
        match result {
            WalkResult::Continue(pte_paddr) => {
                // SAFETY: kernel VA via KERNEL_VA_OFFSET — reads PTE through TTBR1
                let pte_va = pte_paddr + KERNEL_VA_OFFSET;
                // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
                let pte_raw = core::ptr::read_volatile(pte_va as *const u64);
                result = walk.step(pte_raw);
            }
            WalkResult::Done(phys_addr) => return Some(phys_addr + KERNEL_VA_OFFSET),
            WalkResult::Fault => return None,
        }
    }
}

/// Map pages into an existing address space (identified by its L0 paddr).
/// Walks L0→L1→L2, allocates an L3 table if the target 2MB region doesn't
/// have one, and writes page entries for the given physical pages at the
/// requested virtual address.
///
/// All mapped pages get AP_RW_ALL (user read-write), UXN + PXN (no execute).
/// If MAP_FLAG_DEVICE is set, uses MAIR_DEVICE + SH_NON (strongly ordered,
/// non-cacheable) instead of MAIR_NORMAL + SH_INNER.
///
/// Validation is done by the pure model in lockjaw_types::vmem (tested on host).
///
/// # Safety
/// `ttbr0_paddr` must be a valid L0 page table.
/// `header` must be a valid PageSetHeader with page_count data pages.
pub unsafe fn map_pages_in_existing(
    ttbr0_paddr: PhysAddr,
    virt_addr: u64,
    header: &lockjaw_types::pageset_table::PageSetHeader,
    flags: u64,
) -> Result<(), VmemError> {
    use lockjaw_types::page_table::{MapWalk, MapWalkResult};
    use lockjaw_types::vmem::{map_action_for_l2, MapAction, select_attrs, build_user_page};

    let page_count = header.data_page_count();

    // Walk L0 → L1 → L2 using the pure state machine (tested on host).
    // The kernel only does memory reads; all PTE interpretation is in lockjaw-types.
    let (mut walk, mut result) = MapWalk::start(ttbr0_paddr.as_u64(), virt_addr, page_count);
    loop {
        match result {
            MapWalkResult::ReadPte(pte_paddr) => {
                // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
                let pte_va = pte_paddr + KERNEL_VA_OFFSET;
                let pte_raw = core::ptr::read_volatile(pte_va as *const u64);
                result = walk.step(pte_raw);
            }
            MapWalkResult::ReachedL2 { l2_table_paddr, l2_idx, l3_start, state } => {
                // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
                let l2_va = (l2_table_paddr + KERNEL_VA_OFFSET) as *mut PageTable;

                // Ask the model what to do with this L2 slot
                let l3_va = match map_action_for_l2(state) {
                    MapAction::UseExistingL3 => {
                        let l3_paddr = (*l2_va).entries[l2_idx].output_addr();
                        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
                        (l3_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut PageTable
                    }
                    MapAction::AllocateL3 => {
                        let l3_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
                        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
                        let va = (l3_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
                        ptr::write_bytes(va, 0, 1);
                        (*l2_va).entries[l2_idx] = PageTableEntry::new_table(l3_page.start_addr());
                        va
                    }
                    MapAction::ErrorBlockConflict => {
                        return Err(VmemError::TooManyL3Regions);
                    }
                };

                // Select memory attributes and write page entries.
                // Read each page address from the header (no stack array needed).
                let (attr, sh) = select_attrs(flags);
                for i in 0..page_count {
                    let phys = PhysAddr::new(header.get_page(i).unwrap());
                    (*l3_va).entries[l3_start + i] = build_user_page(phys, attr, sh);
                }

                // TLB invalidate so the new mappings take effect
                core::arch::asm!(
                    "dsb ish",
                    "tlbi vmalle1is",
                    "dsb ish",
                    "isb",
                );

                return Ok(());
            }
            MapWalkResult::Fault => return Err(VmemError::OutOfPages),
            MapWalkResult::InvalidMapping => return Err(VmemError::TooManyMappings),
        }
    }
}

/// Query the mapping state at a user VA and count consecutive pages
/// Validate that L3 PTEs at [va, va + count*PAGE_SIZE) map to the expected
/// physical pages, then clear them and flush the TLB.
/// L3 page entries only — rejects L2 block mappings.
///
/// # Safety
/// `ttbr0_paddr` must be a valid L0 page table base.
/// `expected_pages` must contain valid physical addresses.
pub unsafe fn unmap_validated(
    ttbr0_paddr: PhysAddr,
    va: u64,
    expected_pages: &[u64],
) -> Result<(), VmemError> {
    // Pure validation + PTE clearing logic in lockjaw-types.
    // Kernel provides read/write closures for PTE access via TTBR1.
    lockjaw_types::page_table::unmap_validated(
        ttbr0_paddr.as_u64(),
        va,
        expected_pages,
        |pte_paddr| {
            // SAFETY: kernel VA via KERNEL_VA_OFFSET
            core::ptr::read_volatile((pte_paddr + KERNEL_VA_OFFSET) as *const u64)
        },
        |pte_paddr, val| {
            // SAFETY: kernel VA via KERNEL_VA_OFFSET
            core::ptr::write_volatile((pte_paddr + KERNEL_VA_OFFSET) as *mut u64, val);
        },
    ).map_err(|_| VmemError::InvalidParameter)?;

    // TLB flush after PTE clearing
    core::arch::asm!(
        "dsb ish",
        "tlbi vmalle1is",
        "dsb ish",
        "isb",
    );

    Ok(())
}

/// with the same state. Uses the pure MappingQuery state machine from
/// lockjaw-types (host-testable); the kernel only does memory reads.
///
/// # Safety
/// `ttbr0_paddr` must be a valid L0 page table base.
/// `start_va` must be page-aligned and < USER_VA_END.
pub unsafe fn query_mapping_run(ttbr0_paddr: PhysAddr, start_va: u64) -> (bool, usize) {
    lockjaw_types::page_table::query_mapping_run(
        ttbr0_paddr.as_u64(),
        start_va,
        |pte_paddr| {
            // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
            let pte_va = pte_paddr + KERNEL_VA_OFFSET;
            core::ptr::read_volatile(pte_va as *const u64)
        },
    )
}

