/// Pure page table walk logic for address space management.
///
/// Computes indices and determines what action the kernel should take
/// when mapping pages — no pointers, no MMIO, no unsafe.

use crate::addr::PAGE_SIZE;

/// Mapping flags for map_pages_in_existing. Shared between kernel and userspace.
/// Device memory: use MAIR_DEVICE (strongly ordered, non-cacheable) instead of
/// MAIR_NORMAL. Required for MMIO regions (UART, GIC, etc).
pub const MAP_FLAG_DEVICE: u64 = 1 << 0;

/// Extract the 4-level page table indices from a virtual address (4KB granule).
pub const fn page_table_indices(va: u64) -> (usize, usize, usize, usize) {
    let l0 = ((va >> 39) & 0x1FF) as usize;
    let l1 = ((va >> 30) & 0x1FF) as usize;
    let l2 = ((va >> 21) & 0x1FF) as usize;
    let l3 = ((va >> 12) & 0x1FF) as usize;
    (l0, l1, l2, l3)
}

/// What the kernel found at an L2 slot when trying to map a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum L2SlotState {
    /// Slot is empty (entry invalid) — need to allocate a new L3 table.
    Empty,
    /// Slot points to an existing L3 table — reuse it.
    HasL3Table,
    /// Slot is a 2MB block descriptor — cannot add L3 pages here.
    IsBlock,
}

/// What action the kernel should take for a given L2 slot state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapAction {
    /// Allocate a new L3 table and install it at this L2 index.
    AllocateL3,
    /// Use the existing L3 table.
    UseExistingL3,
    /// Error: the L2 slot is a block and cannot be split.
    ErrorBlockConflict,
}

/// Determine the action for mapping a page given the L2 slot state.
pub fn map_action_for_l2(state: L2SlotState) -> MapAction {
    match state {
        L2SlotState::Empty => MapAction::AllocateL3,
        L2SlotState::HasL3Table => MapAction::UseExistingL3,
        L2SlotState::IsBlock => MapAction::ErrorBlockConflict,
    }
}

/// Validate a mapping request. Returns the L2 and L3 indices, or an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapValidation {
    /// Valid: map at this L2 index, starting at this L3 index.
    Ok { l2_idx: usize, l3_start: usize },
    /// Error: VA is not in the first 1GB (L1[0] range).
    ErrorOutOfRange,
    /// Error: pages would span two different L2 regions.
    ErrorSpansL2Boundary,
    /// Error: too many pages.
    ErrorTooManyPages,
}

/// Validate a contiguous mapping of `page_count` pages starting at `virt_addr`.
pub fn validate_mapping(virt_addr: u64, page_count: usize) -> MapValidation {
    if page_count == 0 || page_count > 16 {
        return MapValidation::ErrorTooManyPages;
    }

    let (l0, l1, l2_start, l3_start) = page_table_indices(virt_addr);

    // Must be in L0[0], L1[0] (first 1GB, user range)
    if l0 != 0 || l1 != 0 {
        return MapValidation::ErrorOutOfRange;
    }

    // Check all pages fall in the same L2 region
    let end_va = virt_addr + ((page_count - 1) as u64) * PAGE_SIZE;
    let (_, _, l2_end, _) = page_table_indices(end_va);
    if l2_start != l2_end {
        return MapValidation::ErrorSpansL2Boundary;
    }

    MapValidation::Ok {
        l2_idx: l2_start,
        l3_start,
    }
}

/// Validate that a copy_from_user read doesn't straddle a page boundary.
/// Returns true if the entire object fits within a single 4KB page.
pub fn validate_intra_page(user_va: u64, size: u64) -> bool {
    let offset = user_va & (PAGE_SIZE - 1);
    offset + size <= PAGE_SIZE
}

/// Validate that user mappings + stack pages fit in the mapping buffer.
/// Returns true if the combined count fits within `capacity` entries
/// and both counts are nonzero.
pub fn validate_process_mappings(mapping_count: usize, stack_count: usize, capacity: usize) -> bool {
    mapping_count > 0 && stack_count > 0 && mapping_count + stack_count <= capacity
}

/// Classify an L2 page table entry for the mapping path.
/// Determines whether the slot is empty (need to allocate L3), already has
/// an L3 table (reuse it), or is a block descriptor (conflict).
pub fn classify_l2_entry(pte: crate::page_table::PageTableEntry) -> L2SlotState {
    if pte.is_table() {
        L2SlotState::HasL3Table
    } else if !pte.is_valid() {
        L2SlotState::Empty
    } else {
        L2SlotState::IsBlock
    }
}

/// Select memory attributes from mapping flags.
/// Returns (MAIR index, shareability) for the page table entry.
pub fn select_attrs(flags: u64) -> (u8, u8) {
    use crate::page_table::*;
    if flags & MAP_FLAG_DEVICE != 0 {
        (MAIR_DEVICE, SH_NON)
    } else {
        (MAIR_NORMAL, SH_INNER)
    }
}

/// Build a user-accessible page entry with no-execute permissions.
/// All user pages are AP_RW_ALL + UXN + PXN (read-write, no execute).
pub fn build_user_page(phys: crate::addr::PhysAddr, attr: u8, sh: u8) -> crate::page_table::PageTableEntry {
    use crate::page_table::*;
    PageTableEntry::new_page(phys, attr, AP_RW_ALL, sh)
        .with_uxn()
        .with_pxn()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::PhysAddr;
    use crate::page_table::*;

    #[test]
    fn indices_for_user_code_va() {
        // VA 0x0040_0000 = 4MB
        let (l0, l1, l2, l3) = page_table_indices(0x0040_0000);
        assert_eq!(l0, 0);
        assert_eq!(l1, 0);
        assert_eq!(l2, 2); // 4MB / 2MB = 2
        assert_eq!(l3, 0);
    }

    #[test]
    fn indices_for_user_stack_va() {
        // VA 0x0080_0000 = 8MB
        let (l0, l1, l2, l3) = page_table_indices(0x0080_0000);
        assert_eq!(l0, 0);
        assert_eq!(l1, 0);
        assert_eq!(l2, 4); // 8MB / 2MB = 4
        assert_eq!(l3, 0);
    }

    #[test]
    fn indices_offset_within_page() {
        // VA 0x0040_1000 = 4MB + 4KB
        let (l0, l1, l2, l3) = page_table_indices(0x0040_1000);
        assert_eq!(l0, 0);
        assert_eq!(l1, 0);
        assert_eq!(l2, 2);
        assert_eq!(l3, 1);
    }

    #[test]
    fn map_action_empty_allocates() {
        assert_eq!(map_action_for_l2(L2SlotState::Empty), MapAction::AllocateL3);
    }

    #[test]
    fn map_action_existing_reuses() {
        assert_eq!(map_action_for_l2(L2SlotState::HasL3Table), MapAction::UseExistingL3);
    }

    #[test]
    fn map_action_block_errors() {
        assert_eq!(map_action_for_l2(L2SlotState::IsBlock), MapAction::ErrorBlockConflict);
    }

    #[test]
    fn validate_single_page() {
        let result = validate_mapping(0x0040_0000, 1);
        assert_eq!(result, MapValidation::Ok { l2_idx: 2, l3_start: 0 });
    }

    #[test]
    fn validate_multiple_pages_same_l2() {
        // 4 pages starting at 0x0040_0000: all in L2[2]
        let result = validate_mapping(0x0040_0000, 4);
        assert_eq!(result, MapValidation::Ok { l2_idx: 2, l3_start: 0 });
    }

    #[test]
    fn validate_pages_spanning_l2_boundary() {
        // Start near end of L2[2] region, span into L2[3]
        // L2[2] covers 0x0040_0000 to 0x005F_FFFF (512 pages)
        // Start at page 511 of L2[2]: VA = 0x005F_F000
        let result = validate_mapping(0x005F_F000, 2);
        assert_eq!(result, MapValidation::ErrorSpansL2Boundary);
    }

    #[test]
    fn validate_out_of_range() {
        // VA in second 1GB (L1[1]) — kernel territory
        let result = validate_mapping(0x4000_0000, 1);
        assert_eq!(result, MapValidation::ErrorOutOfRange);
    }

    #[test]
    fn validate_zero_pages() {
        assert_eq!(validate_mapping(0x0040_0000, 0), MapValidation::ErrorTooManyPages);
    }

    #[test]
    fn validate_too_many_pages() {
        assert_eq!(validate_mapping(0x0040_0000, 17), MapValidation::ErrorTooManyPages);
    }

    // --- classify_l2_entry tests ---

    #[test]
    fn classify_table_entry() {
        let pte = PageTableEntry::new_table(PhysAddr::new(0x1000));
        assert_eq!(classify_l2_entry(pte), L2SlotState::HasL3Table);
    }

    #[test]
    fn classify_empty_entry() {
        assert_eq!(classify_l2_entry(PageTableEntry::empty()), L2SlotState::Empty);
    }

    #[test]
    fn classify_block_entry() {
        let pte = PageTableEntry::new_block(PhysAddr::new(0x20_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER);
        assert_eq!(classify_l2_entry(pte), L2SlotState::IsBlock);
    }

    // --- select_attrs tests ---

    #[test]
    fn attrs_normal_memory() {
        let (attr, sh) = select_attrs(0);
        assert_eq!(attr, MAIR_NORMAL);
        assert_eq!(sh, SH_INNER);
    }

    #[test]
    fn attrs_device_memory() {
        let (attr, sh) = select_attrs(MAP_FLAG_DEVICE);
        assert_eq!(attr, MAIR_DEVICE);
        assert_eq!(sh, SH_NON);
    }

    // --- build_user_page tests ---

    #[test]
    fn user_page_has_correct_flags() {
        let pte = build_user_page(PhysAddr::new(0x5000), MAIR_NORMAL, SH_INNER);
        assert!(pte.is_valid());
        assert_eq!(pte.ap(), AP_RW_ALL);
        assert_eq!(pte.attr_index(), MAIR_NORMAL);
        assert_eq!(pte.sh(), SH_INNER);
        assert!(pte.af());
        assert_eq!(pte.output_addr().as_u64(), 0x5000);
        // UXN (bit 54) and PXN (bit 53) must be set
        assert_ne!(pte.raw() & (1 << 54), 0);
        assert_ne!(pte.raw() & (1 << 53), 0);
    }

    #[test]
    fn user_page_device_attrs() {
        let pte = build_user_page(PhysAddr::new(0x0900_0000), MAIR_DEVICE, SH_NON);
        assert_eq!(pte.attr_index(), MAIR_DEVICE);
        assert_eq!(pte.sh(), SH_NON);
    }

    // --- validate_process_mappings tests ---

    #[test]
    fn process_mappings_fit() {
        assert!(validate_process_mappings(30, 1, 32));
    }

    #[test]
    fn process_mappings_exact_fit() {
        assert!(validate_process_mappings(28, 4, 32));
    }

    #[test]
    fn process_mappings_overflow() {
        assert!(!validate_process_mappings(30, 5, 32));
    }

    #[test]
    fn process_mappings_zero_mappings() {
        assert!(!validate_process_mappings(0, 1, 32));
    }

    #[test]
    fn process_mappings_zero_stack() {
        assert!(!validate_process_mappings(10, 0, 32));
    }

    #[test]
    fn process_mappings_no_room_for_stack() {
        assert!(!validate_process_mappings(32, 1, 32));
    }

    // --- validate_intra_page tests ---

    #[test]
    fn intra_page_aligned_start() {
        assert!(validate_intra_page(0x1000, 16));
    }

    #[test]
    fn intra_page_fits_end() {
        assert!(validate_intra_page(0x1FF0, 16));
    }

    #[test]
    fn intra_page_straddles() {
        assert!(!validate_intra_page(0x1FF8, 16));
    }

    #[test]
    fn intra_page_last_byte() {
        assert!(validate_intra_page(0x1FFF, 1));
    }

    #[test]
    fn intra_page_crosses_by_one() {
        assert!(!validate_intra_page(0x1FFF, 2));
    }

    #[test]
    fn intra_page_zero_size() {
        assert!(validate_intra_page(0x1FFF, 0));
    }
}
