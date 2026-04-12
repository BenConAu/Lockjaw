/// Pure page table walk logic for address space management.
///
/// Computes indices and determines what action the kernel should take
/// when mapping pages — no pointers, no MMIO, no unsafe.

use crate::addr::PAGE_SIZE;

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
}
