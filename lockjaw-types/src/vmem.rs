/// Pure page table walk logic for address space management.
///
/// Computes indices and determines what action the kernel should take
/// when mapping pages — no pointers, no MMIO, no unsafe.

use crate::addr::{PhysAddr, PAGE_SIZE};

/// A single virtual-to-physical page mapping with access permissions.
/// Used by the kernel's address space builder to create page tables.
#[derive(Clone, Copy)]
pub struct Mapping {
    pub virt_addr: u64,
    pub phys_addr: PhysAddr,
    pub user_accessible: bool,
    pub executable: bool,
}

/// How many Mapping structs fit in a single 4KB page.
/// Callers allocate page(s) for the mapping buffer rather than using the stack.
pub const MAPPINGS_PER_PAGE: usize = PAGE_SIZE as usize / core::mem::size_of::<Mapping>();

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
    if page_count == 0 || page_count > 512 {
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
// L3 region tracker — pure dedup cache for create_address_space
// ---------------------------------------------------------------------------

/// Hard limit on distinct 2MB L2 regions a single address space can span.
/// Each region requires an L3 page table (4KB). With user VA 0x400000-
/// 0x800000 (4MB), typical binaries need 2-3 regions. 8 covers ~16MB of
/// non-contiguous VA. If a binary exceeds this, the tracker returns Full
/// and the kernel must allocate dynamically.
pub const MAX_L3_TABLES: usize = 8;

/// Result of looking up an L2 index in the L3 region tracker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum L3Lookup {
    /// Already tracked at this slot. Kernel should reuse its cached
    /// L3 table pointer for this slot.
    Existing { slot: usize },
    /// Not tracked yet. Kernel should allocate a new L3 table, store
    /// the pointer at this slot, then call `register()`.
    NeedAlloc { slot: usize },
    /// Capacity exceeded — too many distinct 2MB regions.
    Full,
}

/// Pure fixed-size associative cache tracking which L2 indices have
/// L3 tables allocated. The kernel keeps a parallel pointer array
/// indexed by the same slot numbers.
pub struct L3RegionTracker {
    indices: [usize; MAX_L3_TABLES],
    count: usize,
}

impl L3RegionTracker {
    pub const fn new() -> Self {
        Self {
            indices: [usize::MAX; MAX_L3_TABLES],
            count: 0,
        }
    }

    /// Look up an L2 index. Returns what the kernel should do.
    pub fn lookup(&self, l2_idx: usize) -> L3Lookup {
        for i in 0..self.count {
            if self.indices[i] == l2_idx {
                return L3Lookup::Existing { slot: i };
            }
        }
        if self.count >= MAX_L3_TABLES {
            L3Lookup::Full
        } else {
            L3Lookup::NeedAlloc { slot: self.count }
        }
    }

    /// Register a newly allocated L3 table for this L2 index.
    /// Only valid after `lookup` returned `NeedAlloc { slot }`.
    pub fn register(&mut self, slot: usize, l2_idx: usize) {
        debug_assert!(slot == self.count, "register must use the slot from NeedAlloc");
        debug_assert!(slot < MAX_L3_TABLES, "register beyond capacity");
        self.indices[slot] = l2_idx;
        self.count += 1;
    }

    /// Number of distinct L3 regions tracked.
    pub fn count(&self) -> usize {
        self.count
    }
}

// ---------------------------------------------------------------------------
// build_process_page — current process-page permission policy
// ---------------------------------------------------------------------------

/// Build a page table entry for process address space construction.
///
/// Current permission policy (may grow exceptions later):
/// - kernel-only (`user_accessible=false`): AP_RW_EL1, no UXN/PXN
/// - user executable: AP_RW_ALL + PXN (user can execute, kernel cannot)
/// - user non-executable: AP_RW_ALL + UXN + PXN (no one executes)
///
/// All pages use MAIR_NORMAL + inner-shareable attributes.
pub fn build_process_page(
    phys: crate::addr::PhysAddr,
    user_accessible: bool,
    executable: bool,
) -> crate::page_table::PageTableEntry {
    use crate::page_table::*;
    if !user_accessible {
        // Kernel-only page — no execute restrictions
        PageTableEntry::new_page(phys, MAIR_NORMAL, AP_RW_EL1, SH_INNER)
    } else if executable {
        // User executable — kernel cannot execute (PXN)
        PageTableEntry::new_page(phys, MAIR_NORMAL, AP_RW_ALL, SH_INNER)
            .with_pxn()
    } else {
        // User non-executable — no one executes (UXN + PXN)
        PageTableEntry::new_page(phys, MAIR_NORMAL, AP_RW_ALL, SH_INNER)
            .with_uxn()
            .with_pxn()
    }
}

// ---------------------------------------------------------------------------
// ScratchCursor — pagination state machine for multi-page scratch buffers
// ---------------------------------------------------------------------------

/// What the kernel should do after writing a mapping into the scratch buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScratchAction {
    /// Continue writing into the current page.
    Continue,
    /// Current page is full. Kernel should flush
    /// `mappings[..MAPPINGS_PER_PAGE]` to the builder, then set up the
    /// scratch page at `next_page_idx`.
    FlushAndAdvance { next_page_idx: usize },
}

/// Pure pagination state machine for multi-page scratch buffers.
/// Drives the kernel's flush/advance cycle without the kernel managing
/// any offset or page index arithmetic.
pub struct ScratchCursor {
    page_idx: usize,
    offset: usize,
    total_pages: usize,
    total_written: usize,
}

impl ScratchCursor {
    pub const fn new(total_scratch_pages: usize) -> Self {
        Self {
            page_idx: 0,
            offset: 0,
            total_pages: total_scratch_pages,
            total_written: 0,
        }
    }

    /// Current write offset within the active scratch page.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Total mappings written across all pages so far.
    pub fn total_written(&self) -> usize {
        self.total_written
    }

    /// Record that one mapping was written at `offset()`. Returns what
    /// the kernel should do next.
    pub fn advance(&mut self) -> ScratchAction {
        self.offset += 1;
        self.total_written += 1;
        if self.offset >= MAPPINGS_PER_PAGE && self.page_idx + 1 < self.total_pages {
            ScratchAction::FlushAndAdvance { next_page_idx: self.page_idx + 1 }
        } else {
            ScratchAction::Continue
        }
    }

    /// Confirm that the kernel flushed and advanced to the next page.
    /// Must be called after `FlushAndAdvance` before the next write.
    pub fn did_advance(&mut self) {
        self.page_idx += 1;
        self.offset = 0;
    }

    /// True if there are un-flushed mappings in the current page.
    pub fn has_pending(&self) -> bool {
        self.offset > 0
    }

    /// Number of pending (un-flushed) mappings in the current page.
    pub fn pending_count(&self) -> usize {
        self.offset
    }
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
        assert_eq!(validate_mapping(0x0040_0000, 513), MapValidation::ErrorTooManyPages);
    }

    #[test]
    fn validate_framebuffer_75_pages() {
        // 320x240x4 = 307,200 bytes = 75 pages, fits in one L2 region at 0x0020_0000
        let result = validate_mapping(0x0020_0000, 75);
        assert_eq!(result, MapValidation::Ok { l2_idx: 1, l3_start: 0 });
    }

    #[test]
    fn validate_framebuffer_300_pages() {
        // 640x480x4 = 1,228,800 bytes = 300 pages, fits in one L2 region at 0x0020_0000
        let result = validate_mapping(0x0020_0000, 300);
        assert_eq!(result, MapValidation::Ok { l2_idx: 1, l3_start: 0 });
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

    // --- L3RegionTracker tests ---

    #[test]
    fn l3_tracker_first_lookup_returns_need_alloc() {
        let tracker = L3RegionTracker::new();
        assert_eq!(tracker.lookup(5), L3Lookup::NeedAlloc { slot: 0 });
    }

    #[test]
    fn l3_tracker_same_index_returns_existing() {
        let mut tracker = L3RegionTracker::new();
        assert_eq!(tracker.lookup(5), L3Lookup::NeedAlloc { slot: 0 });
        tracker.register(0, 5);
        assert_eq!(tracker.lookup(5), L3Lookup::Existing { slot: 0 });
    }

    #[test]
    fn l3_tracker_different_indices_get_consecutive_slots() {
        let mut tracker = L3RegionTracker::new();
        for i in 0..MAX_L3_TABLES {
            assert_eq!(tracker.lookup(i * 10), L3Lookup::NeedAlloc { slot: i });
            tracker.register(i, i * 10);
        }
        assert_eq!(tracker.count(), MAX_L3_TABLES);
    }

    #[test]
    fn l3_tracker_ninth_distinct_returns_full() {
        let mut tracker = L3RegionTracker::new();
        for i in 0..MAX_L3_TABLES {
            tracker.register(i, i);
        }
        assert_eq!(tracker.lookup(99), L3Lookup::Full);
    }

    #[test]
    fn l3_tracker_dedup_across_batches() {
        let mut tracker = L3RegionTracker::new();
        // Batch 1: register index 3
        tracker.register(0, 3);
        // Batch 2: same index 3 should return Existing
        assert_eq!(tracker.lookup(3), L3Lookup::Existing { slot: 0 });
        // Different index should get next slot
        assert_eq!(tracker.lookup(7), L3Lookup::NeedAlloc { slot: 1 });
    }

    // --- build_process_page tests ---

    #[test]
    fn build_process_page_kernel_only() {
        let pte = build_process_page(PhysAddr::new(0x1000), false, false);
        assert_eq!(pte.ap(), AP_RW_EL1);
        assert_eq!(pte.attr_index(), MAIR_NORMAL);
        assert_eq!(pte.sh(), SH_INNER);
        // No UXN or PXN for kernel-only
        assert!(!pte.is_uxn());
        assert!(!pte.is_pxn());
    }

    #[test]
    fn build_process_page_user_executable() {
        let pte = build_process_page(PhysAddr::new(0x2000), true, true);
        assert_eq!(pte.ap(), AP_RW_ALL);
        // PXN set (kernel cannot execute), UXN clear (user can execute)
        assert!(pte.is_pxn());
        assert!(!pte.is_uxn());
    }

    #[test]
    fn build_process_page_user_non_executable() {
        let pte = build_process_page(PhysAddr::new(0x3000), true, false);
        assert_eq!(pte.ap(), AP_RW_ALL);
        // Both UXN and PXN set
        assert!(pte.is_uxn());
        assert!(pte.is_pxn());
    }

    #[test]
    fn build_process_page_matches_build_user_page() {
        // User non-executable should produce the same PTE as build_user_page
        let phys = PhysAddr::new(0x5000);
        let process = build_process_page(phys, true, false);
        let user = build_user_page(phys, MAIR_NORMAL, SH_INNER);
        assert_eq!(process.raw(), user.raw());
    }

    #[test]
    fn build_process_page_kernel_only_executable_ignored() {
        // Even with executable=true, kernel-only pages have no UXN/PXN
        let pte = build_process_page(PhysAddr::new(0x4000), false, true);
        assert_eq!(pte.ap(), AP_RW_EL1);
        assert!(!pte.is_uxn());
        assert!(!pte.is_pxn());
    }

    // --- ScratchCursor tests ---

    #[test]
    fn scratch_cursor_single_page_no_flush() {
        let mut cursor = ScratchCursor::new(1);
        // Write MAPPINGS_PER_PAGE entries — no flush triggered (only one page)
        for i in 0..MAPPINGS_PER_PAGE {
            assert_eq!(cursor.offset(), i);
            assert_eq!(cursor.advance(), ScratchAction::Continue);
        }
        assert_eq!(cursor.total_written(), MAPPINGS_PER_PAGE);
        // All entries are pending since single page never triggers FlushAndAdvance
        assert!(cursor.has_pending());
        assert_eq!(cursor.pending_count(), MAPPINGS_PER_PAGE);
    }

    #[test]
    fn scratch_cursor_two_pages_flush_at_boundary() {
        let mut cursor = ScratchCursor::new(2);
        // Fill first page
        for _ in 0..MAPPINGS_PER_PAGE - 1 {
            assert_eq!(cursor.advance(), ScratchAction::Continue);
        }
        // Last entry on first page triggers flush
        assert_eq!(
            cursor.advance(),
            ScratchAction::FlushAndAdvance { next_page_idx: 1 }
        );
        cursor.did_advance();
        // Now on second page, offset reset
        assert_eq!(cursor.offset(), 0);
        assert!(!cursor.has_pending());
    }

    #[test]
    fn scratch_cursor_total_written_spans_pages() {
        let mut cursor = ScratchCursor::new(3);
        // Fill first page + trigger flush
        for _ in 0..MAPPINGS_PER_PAGE {
            cursor.advance();
        }
        cursor.did_advance();
        // Write 5 more on second page
        for _ in 0..5 {
            cursor.advance();
        }
        assert_eq!(cursor.total_written(), MAPPINGS_PER_PAGE + 5);
        assert_eq!(cursor.pending_count(), 5);
    }

    #[test]
    fn scratch_cursor_exact_capacity() {
        let mut cursor = ScratchCursor::new(2);
        // Fill both pages exactly
        for _ in 0..MAPPINGS_PER_PAGE {
            cursor.advance();
        }
        cursor.did_advance();
        for _ in 0..MAPPINGS_PER_PAGE {
            // Second page, no more pages to advance to
            assert_eq!(cursor.advance(), ScratchAction::Continue);
        }
        assert_eq!(cursor.total_written(), 2 * MAPPINGS_PER_PAGE);
        assert_eq!(cursor.pending_count(), MAPPINGS_PER_PAGE);
    }

    #[test]
    fn scratch_cursor_has_pending_after_partial_fill() {
        let mut cursor = ScratchCursor::new(1);
        assert!(!cursor.has_pending());
        cursor.advance();
        assert!(cursor.has_pending());
        assert_eq!(cursor.pending_count(), 1);
    }
}
