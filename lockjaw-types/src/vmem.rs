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

/// Memory-attribute selector for `sys_map_pages`. Carried in syscall x2;
/// `#[repr(u64)]` with explicit discriminants pins the ABI so the kernel-
/// side `from_raw` decoder and the userlib wrapper agree.
///
/// Variants name the MAIR regime (Normal / Device) — not the use case
/// (DmaCoherent / MmioRegister) — so a future reader sees exactly which
/// MAIR slot the kernel will install. Use-case typing happens at the
/// userspace wrapper layer (e.g. ClockClient holds a `Device` mapping).
///
/// `Normal = 0` so callers that previously passed the literal `0` for
/// "default normal memory" keep the same wire encoding through the
/// migration. M6 sub-commit 2 adds the `NormalNonCacheable` variant for
/// the ADMA2 buffer + descriptor-table mappings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum MapMemoryAttribute {
    /// MAIR_NORMAL — write-back, write-allocate, inner shareable.
    /// Default for all RAM mappings (DTB pages, IPC buffers, stacks).
    Normal = 0,
    /// MAIR_DEVICE — strongly ordered, non-cacheable, non-shareable.
    /// Required for MMIO regions (UART, GIC, SDHCI controller registers).
    Device = 1,
}

impl MapMemoryAttribute {
    /// Decode from the raw u64 carried in syscall x2. Preserves the
    /// existing `select_attrs(flags)` semantics: bit 0 set → Device,
    /// otherwise Normal. Stray high bits ignored. M6 sub-commit 2 will
    /// tighten this to reject unknown discriminants once the new variant
    /// lands and we can characterise the validation surface as a whole.
    pub const fn from_raw(raw: u64) -> Self {
        if raw & 1 != 0 { Self::Device } else { Self::Normal }
    }

    /// Convert to the `(MAIR index, shareability)` pair consumed by
    /// page-table entry construction. Pure; no MMU side effects.
    pub const fn to_pte_attrs(self) -> (u8, u8) {
        use crate::page_table::*;
        match self {
            Self::Normal => (MAIR_NORMAL, SH_INNER),
            Self::Device => (MAIR_DEVICE, SH_NON),
        }
    }
}

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

/// Validate a mapping request. Returns the starting L2 / L3 indices,
/// or an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapValidation {
    /// Valid: start at this L2 index and L3 offset within that L2's
    /// L3 table. The kernel iterates L2 indices itself if the
    /// mapping spans multiple L2 regions (Phase 2.M).
    Ok { l2_idx: usize, l3_start: usize },
    /// Error: VA is not in the first 1GB (L1[0] range), or the
    /// mapping would extend past USER_VA_END.
    ErrorOutOfRange,
    /// Error: too many pages (zero, or above the practical cap).
    ErrorTooManyPages,
}

/// Validate a contiguous mapping of `page_count` pages starting at
/// `virt_addr`. Phase 2.M lifted the previous one-L2-region cap (512
/// pages); the mapping may now span multiple L2 regions, bounded by
/// `MAX_PRACTICAL_PAGES_PER_SET` and the end of user VA range.
pub fn validate_mapping(virt_addr: u64, page_count: usize) -> MapValidation {
    if page_count == 0
        || page_count > crate::pageset_table::MAX_PRACTICAL_PAGES_PER_SET
    {
        return MapValidation::ErrorTooManyPages;
    }

    let (l0, l1, l2_start, l3_start) = page_table_indices(virt_addr);

    // Must be in L0[0], L1[0] (first 1GB, user range).
    if l0 != 0 || l1 != 0 {
        return MapValidation::ErrorOutOfRange;
    }

    // End VA must also fit within the user range. checked_add catches
    // both overflow and overrun past USER_VA_END.
    let bytes = (page_count as u64) * PAGE_SIZE;
    let end_va_excl = match virt_addr.checked_add(bytes) {
        Some(v) => v,
        None => return MapValidation::ErrorOutOfRange,
    };
    if end_va_excl > crate::constants::USER_VA_END {
        return MapValidation::ErrorOutOfRange;
    }

    MapValidation::Ok {
        l2_idx: l2_start,
        l3_start,
    }
}

// ---------------------------------------------------------------------------
// Multi-L2 region iterator (Phase 2.M)
// ---------------------------------------------------------------------------

/// Number of entries per page table at any level (4 KiB / 8 bytes).
pub const PAGES_PER_L2_REGION: usize = 512;

/// One contiguous slice of a mapping that fits in a single L2 region's
/// L3 table. The kernel iterates over `L2RegionIter` and writes
/// `pages_in_region` entries starting at `L3[l3_start]` for the L3
/// table indexed by `L2[l2_idx]`. `data_offset` points into the source
/// PageSet's pages array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L2Region {
    /// Index into the L2 table (0..512).
    pub l2_idx: usize,
    /// Starting offset within this L2's L3 table (0..512). Only the
    /// first region returned by an iterator can be non-zero;
    /// subsequent regions always start at 0.
    pub l3_start: usize,
    /// How many page entries to write at L3[l3_start..l3_start+pages_in_region].
    /// At most `PAGES_PER_L2_REGION - l3_start`.
    pub pages_in_region: usize,
    /// Index into the source PageSet's pages array. The kernel writes
    /// `pages[data_offset..data_offset + pages_in_region]` into the L3
    /// table. Accumulates across regions.
    pub data_offset: usize,
}

/// Iterator that slices a mapping request `(l2_start, l3_start,
/// page_count)` into one `L2Region` per L2 table the mapping touches.
/// Pure: no I/O, no syscalls. The kernel applies the side effects
/// (allocate L3, write PTEs) per region.
///
/// Precondition: callers should pre-validate via [`validate_mapping`]
/// to ensure `l2_start + region_count <= PAGES_PER_L2_REGION` (i.e.
/// the mapping stays within one L1 region). The iterator does not
/// re-check this — it just produces regions until `page_count` is
/// exhausted.
pub struct L2RegionIter {
    next_l2_idx: usize,
    next_l3_offset: usize,
    pages_remaining: usize,
    data_offset: usize,
}

impl L2RegionIter {
    /// Begin slicing. `l2_start` and `l3_start` come from
    /// `validate_mapping`; `page_count` is the total page count
    /// across all regions.
    pub fn new(l2_start: usize, l3_start: usize, page_count: usize) -> Self {
        Self {
            next_l2_idx: l2_start,
            next_l3_offset: l3_start,
            pages_remaining: page_count,
            data_offset: 0,
        }
    }
}

impl Iterator for L2RegionIter {
    type Item = L2Region;
    fn next(&mut self) -> Option<L2Region> {
        if self.pages_remaining == 0 {
            return None;
        }
        let pages_in_region =
            (PAGES_PER_L2_REGION - self.next_l3_offset).min(self.pages_remaining);
        let region = L2Region {
            l2_idx: self.next_l2_idx,
            l3_start: self.next_l3_offset,
            pages_in_region,
            data_offset: self.data_offset,
        };
        self.next_l2_idx += 1;
        self.next_l3_offset = 0;
        self.pages_remaining -= pages_in_region;
        self.data_offset += pages_in_region;
        Some(region)
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

/// Select memory attributes for the given attribute selector.
/// Thin wrapper over `MapMemoryAttribute::to_pte_attrs` so existing
/// kernel call sites in `map_pages_in_existing` keep their function-call
/// shape; new code can call the method directly.
pub fn select_attrs(attr: MapMemoryAttribute) -> (u8, u8) {
    attr.to_pte_attrs()
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
    extern crate std;
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
    fn validate_pages_spanning_l2_boundary_now_ok() {
        // Phase 2.M: a mapping crossing an L2 region boundary is
        // valid. validate_mapping returns the starting (l2_idx,
        // l3_start); the kernel iterates L2 indices itself.
        // Start at page 511 of L2[2]: VA = 0x005F_F000, two pages —
        // one in L2[2], one in L2[3].
        let result = validate_mapping(0x005F_F000, 2);
        assert_eq!(result, MapValidation::Ok { l2_idx: 2, l3_start: 511 });
    }

    #[test]
    fn validate_pages_spanning_many_l2_regions() {
        // 8 MiB = 2048 pages spans L2[0..4]. Returns the starting
        // (l2_idx=0, l3_start=0); kernel iterates the rest.
        let result = validate_mapping(0x0000_0000, 2048);
        assert_eq!(result, MapValidation::Ok { l2_idx: 0, l3_start: 0 });
    }

    #[test]
    fn validate_out_of_range_l1_above_user() {
        // VA in second 1GB (L1[1]) — kernel territory
        let result = validate_mapping(0x4000_0000, 1);
        assert_eq!(result, MapValidation::ErrorOutOfRange);
    }

    #[test]
    fn validate_extends_past_user_va_end() {
        // Start in user range but length pushes past USER_VA_END.
        // USER_VA_END = 0x4000_0000; start at 0x3FFE_0000 with 64
        // pages = 256 KiB → ends at 0x3FFE_0000 + 0x4_0000 =
        // 0x4002_0000 (past end).
        let result = validate_mapping(0x3FFE_0000, 64);
        assert_eq!(result, MapValidation::ErrorOutOfRange);
    }

    #[test]
    fn validate_zero_pages() {
        assert_eq!(validate_mapping(0x0040_0000, 0), MapValidation::ErrorTooManyPages);
    }

    #[test]
    fn validate_too_many_pages() {
        // Above the variable-header cap (16384 pages = 64 MiB).
        let too_many = crate::pageset_table::MAX_PRACTICAL_PAGES_PER_SET + 1;
        assert_eq!(
            validate_mapping(0x0040_0000, too_many),
            MapValidation::ErrorTooManyPages,
        );
    }

    // --- L2RegionIter tests (Phase 2.M) ---

    fn collect_regions(l2_start: usize, l3_start: usize, page_count: usize)
        -> std::vec::Vec<L2Region>
    {
        L2RegionIter::new(l2_start, l3_start, page_count).collect()
    }

    #[test]
    fn iter_single_page_one_region() {
        let r = collect_regions(2, 0, 1);
        assert_eq!(r, std::vec![
            L2Region { l2_idx: 2, l3_start: 0, pages_in_region: 1, data_offset: 0 },
        ]);
    }

    #[test]
    fn iter_full_l2_region_fits_in_one() {
        // Exactly 512 pages starting at l3_offset 0 should fit in
        // L2[2]'s L3 table without spilling.
        let r = collect_regions(2, 0, 512);
        assert_eq!(r, std::vec![
            L2Region { l2_idx: 2, l3_start: 0, pages_in_region: 512, data_offset: 0 },
        ]);
    }

    #[test]
    fn iter_513_pages_spills_to_second_l2() {
        // One more than fits in L2[2] forces a second region of 1 page.
        let r = collect_regions(2, 0, 513);
        assert_eq!(r, std::vec![
            L2Region { l2_idx: 2, l3_start: 0, pages_in_region: 512, data_offset: 0 },
            L2Region { l2_idx: 3, l3_start: 0, pages_in_region: 1, data_offset: 512 },
        ]);
    }

    #[test]
    fn iter_starts_mid_region() {
        // Two pages starting at L2[2] / L3[511] crosses the boundary
        // into L2[3] for the second page.
        let r = collect_regions(2, 511, 2);
        assert_eq!(r, std::vec![
            L2Region { l2_idx: 2, l3_start: 511, pages_in_region: 1, data_offset: 0 },
            L2Region { l2_idx: 3, l3_start: 0, pages_in_region: 1, data_offset: 1 },
        ]);
    }

    #[test]
    fn iter_8_mib_aligned_four_full_regions() {
        // 8 MiB / 4 KiB = 2048 pages starting at l3_offset 0 fills
        // exactly 4 L2 regions.
        let r = collect_regions(0, 0, 2048);
        assert_eq!(r.len(), 4);
        for (i, region) in r.iter().enumerate() {
            assert_eq!(region.l2_idx, i);
            assert_eq!(region.l3_start, 0);
            assert_eq!(region.pages_in_region, 512);
            assert_eq!(region.data_offset, i * 512);
        }
    }

    #[test]
    fn iter_8_mib_offset_five_regions() {
        // 2048 pages starting at L2[0] / L3[256] spills across 5 L2
        // regions: 256, 512, 512, 512, 256.
        let r = collect_regions(0, 256, 2048);
        let expected = std::vec![
            L2Region { l2_idx: 0, l3_start: 256, pages_in_region: 256, data_offset: 0 },
            L2Region { l2_idx: 1, l3_start: 0, pages_in_region: 512, data_offset: 256 },
            L2Region { l2_idx: 2, l3_start: 0, pages_in_region: 512, data_offset: 768 },
            L2Region { l2_idx: 3, l3_start: 0, pages_in_region: 512, data_offset: 1280 },
            L2Region { l2_idx: 4, l3_start: 0, pages_in_region: 256, data_offset: 1792 },
        ];
        assert_eq!(r, expected);
    }

    #[test]
    fn iter_zero_pages_yields_nothing() {
        // Defensive: validate_mapping rejects page_count == 0, but
        // the iterator should also handle it gracefully.
        let r = collect_regions(2, 0, 0);
        assert!(r.is_empty());
    }

    #[test]
    fn iter_data_offsets_sum_to_total_pages() {
        // Sanity: across any region count, the sum of pages_in_region
        // equals the requested page_count.
        for &count in &[1usize, 100, 511, 512, 513, 1024, 2048, 16384] {
            let r = collect_regions(0, 0, count);
            let total: usize = r.iter().map(|x| x.pages_in_region).sum();
            assert_eq!(total, count, "count {} mismatched", count);
            // data_offset of region i should equal sum of earlier regions.
            let mut acc = 0;
            for region in r.iter() {
                assert_eq!(region.data_offset, acc);
                acc += region.pages_in_region;
            }
        }
    }

    #[test]
    fn iter_first_region_only_can_have_nonzero_l3_start() {
        // Subsequent regions always start at l3_start = 0.
        let r = collect_regions(2, 100, 1024);
        assert_eq!(r[0].l3_start, 100);
        for region in r.iter().skip(1) {
            assert_eq!(region.l3_start, 0);
        }
    }

    #[test]
    fn iter_l2_idx_strictly_increases() {
        let r = collect_regions(0, 0, 2048);
        for window in r.windows(2) {
            assert!(window[1].l2_idx > window[0].l2_idx);
        }
    }

    #[test]
    fn iter_count_matches_validate_mapping_for_max_practical() {
        // Maximum practical mapping (16384 pages = 64 MiB) yields 32
        // L2 regions starting at l3_start=0.
        let count = crate::pageset_table::MAX_PRACTICAL_PAGES_PER_SET;
        let r = collect_regions(0, 0, count);
        assert_eq!(r.len(), 32);
        assert_eq!(r.iter().map(|x| x.pages_in_region).sum::<usize>(), count);
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
        let (attr, sh) = select_attrs(MapMemoryAttribute::Normal);
        assert_eq!(attr, MAIR_NORMAL);
        assert_eq!(sh, SH_INNER);
    }

    #[test]
    fn attrs_device_memory() {
        let (attr, sh) = select_attrs(MapMemoryAttribute::Device);
        assert_eq!(attr, MAIR_DEVICE);
        assert_eq!(sh, SH_NON);
    }

    #[test]
    fn map_memory_attribute_discriminants_pinned() {
        // Wire encoding must stay stable: x2 == 0 → Normal, x2 == 1 → Device.
        // Both userlib (sender) and kernel (decoder) depend on this match.
        assert_eq!(MapMemoryAttribute::Normal as u64, 0);
        assert_eq!(MapMemoryAttribute::Device as u64, 1);
    }

    #[test]
    fn map_memory_attribute_from_raw_preserves_legacy_bit_test() {
        // Pre-M6 `select_attrs(flags)` was: bit 0 set → Device, else Normal.
        // `from_raw` must preserve that for any legacy caller still passing
        // `0` or `MAP_FLAG_DEVICE`-derived values across the syscall.
        assert_eq!(MapMemoryAttribute::from_raw(0), MapMemoryAttribute::Normal);
        assert_eq!(MapMemoryAttribute::from_raw(1), MapMemoryAttribute::Device);
        // Stray high bits without bit 0 → still Normal (matches current semantics).
        assert_eq!(MapMemoryAttribute::from_raw(0x4), MapMemoryAttribute::Normal);
        // Stray high bits WITH bit 0 → Device (matches `flags & 1 != 0`).
        assert_eq!(MapMemoryAttribute::from_raw(0x5), MapMemoryAttribute::Device);
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
