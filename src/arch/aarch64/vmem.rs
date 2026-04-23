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
#[derive(Debug)]
pub enum VmemError {
    TooManyMappings,
    TooManyL3Regions,
    OutOfPages,
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

    // Kernel identity map in L1 (workaround: kernel linked at phys addrs):
    // L1[1] = 1GB block at RAM_BASE (kernel-only)
    (*l1_va).entries[1] = PageTableEntry::new_block(
        PhysAddr::new(super::platform::RAM_BASE),
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

    // Device MMIO (GIC at 0x0800_0000, UART at 0x0900_0000) — kernel-only
    let device_l2_idx = (super::platform::DEVICE_MMIO_BASE >> 21) as usize;
    (*l2_va).entries[device_l2_idx] = PageTableEntry::new_block(
        PhysAddr::new(super::platform::DEVICE_MMIO_BASE),
        MAIR_DEVICE,
        AP_RW_EL1,
        SH_NON,
    );

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

