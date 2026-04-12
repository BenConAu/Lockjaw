use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET, PAGE_SIZE};
use crate::mm::page_alloc;
use crate::mm::page_table::*;
use core::ptr;

/// Maximum number of page mappings per address space.
/// Limits stack usage in create_address_space. If you need more mappings,
/// the caller must provide the mapping list from user-allocated pages
/// instead of building it on the kernel stack.
pub const MAX_MAPPINGS: usize = 32;

/// A single VA → PA mapping with permissions.
#[derive(Clone, Copy)]
pub struct Mapping {
    pub virt_addr: u64,
    pub phys_addr: PhysAddr,
    pub user_accessible: bool,
    pub executable: bool,
}

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
    if mappings.len() > MAX_MAPPINGS {
        return Err(VmemError::TooManyMappings);
    }
    // Allocate L0
    let l0_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
    let l0_va = (l0_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
    ptr::write_bytes(l0_va, 0, 1);

    // We need L1 for entry [0] (covers VA 0x00000000-0x3FFFFFFF, where user pages live)
    // and L1 entry [1] for the kernel identity map (0x40000000-0x7FFFFFFF)
    let l1_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
    let l1_va = (l1_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
    ptr::write_bytes(l1_va, 0, 1);

    // L0[0] → L1
    (*l0_va).entries[0] = PageTableEntry::new_table(l1_page.start_addr());

    // Kernel identity map in L1 (same workaround as Phase 6):
    // L1[1] = 1GB block at 0x40000000 (RAM, kernel-only)
    (*l1_va).entries[1] = PageTableEntry::new_block(
        PhysAddr::new(0x4000_0000),
        MAIR_NORMAL,
        AP_RW_EL1,
        SH_INNER,
    );

    // We need L2 for user pages in the first 1GB range
    let l2_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
    let l2_va = (l2_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
    ptr::write_bytes(l2_va, 0, 1);

    // L1[0] → L2
    (*l1_va).entries[0] = PageTableEntry::new_table(l2_page.start_addr());

    // Device MMIO (UART at 0x0900_0000, GIC at 0x0800_0000) — kernel-only
    // L2[4] covers 0x00800000-0x009FFFFF
    (*l2_va).entries[4] = PageTableEntry::new_block(
        PhysAddr::new(0x0080_0000),
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
        let l2_idx = ((m.virt_addr >> 21) & 0x1FF) as usize;
        let l3_idx = ((m.virt_addr >> 12) & 0x1FF) as usize;

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
