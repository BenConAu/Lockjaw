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
    // L1[1] = 1GB block at RAM_BASE (kernel-only)
    (*l1_va).entries[1] = PageTableEntry::new_block(
        PhysAddr::new(super::platform::RAM_BASE),
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

// Re-export the flag constant so kernel code can use it without importing lockjaw_types.
pub use lockjaw_types::vmem::MAP_FLAG_DEVICE;

/// Translate a user virtual address to a kernel-accessible VA by walking
/// the given page table. Returns None if any level is unmapped.
///
/// All reads go through TTBR1 (kernel higher-half) — this function is
/// immune to TTBR0 changes from context switches.
///
/// # Safety
/// `ttbr0_paddr` must be a valid L0 page table base.
pub unsafe fn translate_user_va(ttbr0_paddr: PhysAddr, user_va: u64) -> Option<u64> {
    let (l0_idx, l1_idx, l2_idx, l3_idx) = lockjaw_types::vmem::page_table_indices(user_va);

    // Walk L0 → L1
    let l0_va = (ttbr0_paddr.as_u64() + KERNEL_VA_OFFSET) as *const PageTable;
    let l0_entry = (*l0_va).entries[l0_idx];
    if !l0_entry.is_table() { return None; }

    // Walk L1
    let l1_va = (l0_entry.output_addr().as_u64() + KERNEL_VA_OFFSET) as *const PageTable;
    let l1_entry = (*l1_va).entries[l1_idx];
    if l1_entry.is_block() {
        // 1GB block mapping — compute physical address from block base + offset
        let block_phys = l1_entry.output_addr().as_u64();
        let offset = user_va & 0x3FFF_FFFF; // offset within 1GB block
        return Some(block_phys + offset + KERNEL_VA_OFFSET);
    }
    if !l1_entry.is_table() { return None; }

    // Walk L2
    let l2_va = (l1_entry.output_addr().as_u64() + KERNEL_VA_OFFSET) as *const PageTable;
    let l2_entry = (*l2_va).entries[l2_idx];
    if l2_entry.is_block() {
        // 2MB block mapping
        let block_phys = l2_entry.output_addr().as_u64();
        let offset = user_va & 0x1F_FFFF; // offset within 2MB block
        return Some(block_phys + offset + KERNEL_VA_OFFSET);
    }
    if !l2_entry.is_table() { return None; }

    // Walk L3
    let l3_va = (l2_entry.output_addr().as_u64() + KERNEL_VA_OFFSET) as *const PageTable;
    let l3_entry = (*l3_va).entries[l3_idx];
    if !l3_entry.is_valid() { return None; }

    // L3 page entry — compute physical address from page base + page offset
    let page_phys = l3_entry.output_addr().as_u64();
    let offset = user_va & 0xFFF; // offset within 4KB page
    Some(page_phys + offset + KERNEL_VA_OFFSET)
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
/// `ttbr0_paddr` must be a valid L0 page table. All page paddrs must be valid.
pub unsafe fn map_pages_in_existing(
    ttbr0_paddr: PhysAddr,
    virt_addr: u64,
    pages: &[PhysAddr],
    flags: u64,
) -> Result<(), VmemError> {
    use lockjaw_types::vmem::{validate_mapping, map_action_for_l2, MapValidation, MapAction, L2SlotState};

    // Validate using the pure model (tested by unit tests)
    let (l2_idx, l3_start) = match validate_mapping(virt_addr, pages.len()) {
        MapValidation::Ok { l2_idx, l3_start } => (l2_idx, l3_start),
        _ => return Err(VmemError::TooManyMappings),
    };

    // Walk L0 → L1 → L2 (read existing page table pointers)
    let l0_va = (ttbr0_paddr.as_u64() + KERNEL_VA_OFFSET) as *const PageTable;
    let l0_entry = (*l0_va).entries[0];
    if !l0_entry.is_table() {
        return Err(VmemError::OutOfPages);
    }

    let l1_va = (l0_entry.output_addr().as_u64() + KERNEL_VA_OFFSET) as *const PageTable;
    let l1_entry = (*l1_va).entries[0];
    if !l1_entry.is_table() {
        return Err(VmemError::OutOfPages);
    }

    let l2_va = (l1_entry.output_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;

    // Determine L2 slot state and ask the model what to do
    let l2_entry = (*l2_va).entries[l2_idx];
    let l2_state = if l2_entry.is_table() {
        L2SlotState::HasL3Table
    } else if !l2_entry.is_valid() {
        L2SlotState::Empty
    } else {
        L2SlotState::IsBlock
    };

    let l3_va = match map_action_for_l2(l2_state) {
        MapAction::UseExistingL3 => {
            let l3_paddr = l2_entry.output_addr();
            (l3_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut PageTable
        }
        MapAction::AllocateL3 => {
            let l3_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
            let va = (l3_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
            ptr::write_bytes(va, 0, 1);
            (*l2_va).entries[l2_idx] = PageTableEntry::new_table(l3_page.start_addr());
            va
        }
        MapAction::ErrorBlockConflict => {
            return Err(VmemError::TooManyL3Regions);
        }
    };

    // Select memory attributes based on flags
    let (attr, sh) = if flags & MAP_FLAG_DEVICE != 0 {
        (MAIR_DEVICE, SH_NON)   // Strongly ordered, non-cacheable device memory
    } else {
        (MAIR_NORMAL, SH_INNER) // Normal cacheable memory
    };

    // Map each page at the L3 indices computed by the model
    for (i, phys) in pages.iter().enumerate() {
        let l3_idx = l3_start + i;

        // User page: read-write, no execute
        let entry = PageTableEntry::new_page(*phys, attr, AP_RW_ALL, sh)
            .with_uxn()
            .with_pxn();

        (*l3_va).entries[l3_idx] = entry;
    }

    // TLB invalidate so the new mappings take effect
    core::arch::asm!(
        "dsb ish",
        "tlbi vmalle1is",
        "dsb ish",
        "isb",
    );

    Ok(())
}

