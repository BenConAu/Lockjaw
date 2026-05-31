use crate::mm::addr::{PhysAddr, PhysPage, KERNEL_VA_OFFSET};
use crate::mm::page_alloc;
use crate::mm::page_table::*;
use core::ptr;

// Re-export from lockjaw-types — single source of truth.
pub use lockjaw_types::vmem::{Mapping, MAPPINGS_PER_PAGE};
use lockjaw_types::vmem::{L3RegionTracker, L3Lookup, MAX_L3_TABLES, build_process_page};

/// Errors returned by virtual memory operations.
pub enum VmemError {
    TooManyMappings,
    TooManyL3Regions,
    OutOfPages,
    InvalidParameter,
}

/// Incremental page table builder for user address spaces.
///
/// Allocates L0/L1/L2 on construction, then accepts batches of mappings
/// via `map_batch()`. Drop-safe: if construction or mapping fails at any
/// point, the destructor walks and frees the partially-built page table tree.
///
/// Limits:
///   - At most MAX_L3_TABLES (8) distinct 2 MB regions containing user pages
///   - All user VAs must be in the first 1 GB (L1[0] range)
///
/// LIMIT: Each L2 entry covers 2MB, so MAX_L3_TABLES caps the number
/// of distinct 2MB regions a process can span. With user VA 0x400000-
/// 0x800000 (4MB), typical binaries need 2-3 regions; init with ~10MB
/// of embedded binaries needs ~6. If a binary's VA footprint exceeds
/// 16MB (8 × 2MB), this will fail with TooManyL3Regions. Fix by
/// switching to a dynamic Vec-like allocator or walking the L2 table
/// directly instead of caching L3 pointers in a fixed array.
pub struct AddressSpaceBuilder {
    l0_paddr: PhysAddr,
    l2_va: *mut PageTable,
    tracker: L3RegionTracker,
    l3_ptrs: [*mut PageTable; MAX_L3_TABLES],
}

impl AddressSpaceBuilder {
    /// Allocate L0/L1/L2 page tables and set up kernel identity map + device MMIO.
    ///
    /// Automatically includes the kernel identity map (RAM + device MMIO)
    /// so that exception vectors and kernel code remain reachable.
    ///
    /// # Safety
    /// Requires the page allocator to be initialized.
    pub unsafe fn new() -> Result<Self, VmemError> {
        // Allocate L0
        let l0_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let l0_va = (l0_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
        ptr::write_bytes(l0_va, 0, 1);

        // Construct builder early — Drop handles cleanup if later steps fail.
        // free_address_space handles partial trees by checking is_table()
        // at each level before recursing.
        let mut builder = Self {
            l0_paddr: l0_page.start_addr(),
            l2_va: core::ptr::null_mut(),
            tracker: L3RegionTracker::new(),
            l3_ptrs: [core::ptr::null_mut(); MAX_L3_TABLES],
        };

        // We need L1 for entry [0] (covers VA 0x00000000-0x3FFFFFFF, where user pages live)
        // and L1 entry [1] for the kernel identity map (0x40000000-0x7FFFFFFF)
        let l1_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let l1_va = (l1_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
        ptr::write_bytes(l1_va, 0, 1);

        // L0[0] → L1
        (*l0_va).entries[0] = PageTableEntry::new_table(l1_page.start_addr());

        // L1[0] → L2 (user pages live in the first 1 GB of TTBR0).
        let l2_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let l2_va = (l2_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
        ptr::write_bytes(l2_va, 0, 1);
        (*l1_va).entries[0] = PageTableEntry::new_table(l2_page.start_addr());

        // No kernel identity in user TTBR0 anymore. Pre-relink, every
        // user TTBR0 carried L1[1] (1 GB block of kernel RAM) + L2[4]
        // (device MMIO) because some kernel-mode code path on EL0
        // entry depended on it (see docs/history/relink-notes.md "Phase 0:
        // what depended on the user-TTBR0 kernel identity"). After
        // the relink (commit 1b), the kernel image lives in L0[1]
        // (TTBR1) and no longer references lower-half PAs, so the
        // identity is dead weight. Userspace device MMIO mappings
        // still flow through the normal sys_map_pages path with
        // MapMemoryAttribute::Device — drivers that need a device
        // get a per-process mapping, not the kernel's.

        builder.l2_va = l2_va;
        Ok(builder)
    }

    /// Map a batch of page mappings into the address space.
    /// Can be called multiple times (for multi-page scratch buffers).
    ///
    /// Uses L3RegionTracker for L3 table dedup and build_process_page for
    /// permission policy — both from lockjaw-types (host-tested).
    ///
    /// # Safety
    /// All physical addresses in mappings must be valid allocated pages.
    pub unsafe fn map_batch(&mut self, mappings: &[Mapping]) -> Result<(), VmemError> {
        for m in mappings {
            let (_, _, l2_idx, l3_idx) = lockjaw_types::vmem::page_table_indices(m.virt_addr);

            // Allocate L3 table for this 2MB region if not already done
            let l3_va = match self.tracker.lookup(l2_idx) {
                L3Lookup::Existing { slot } => self.l3_ptrs[slot],
                L3Lookup::NeedAlloc { slot } => {
                    let l3_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
                    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
                    let va = (l3_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
                    ptr::write_bytes(va, 0, 1);

                    (*self.l2_va).entries[l2_idx] = PageTableEntry::new_table(l3_page.start_addr());
                    self.l3_ptrs[slot] = va;
                    self.tracker.register(slot, l2_idx);
                    va
                }
                L3Lookup::Full => return Err(VmemError::TooManyL3Regions),
            };

            // Build page entry with appropriate permissions (policy in lockjaw-types)
            (*l3_va).entries[l3_idx] = build_process_page(m.phys_addr, m.user_accessible, m.executable);
        }
        Ok(())
    }

    /// Finalize the address space and return the L0 physical address (for TTBR0).
    /// Consumes the builder, defusing the Drop-based cleanup.
    pub fn finish(self) -> PhysAddr {
        let paddr = self.l0_paddr;
        core::mem::forget(self);
        paddr
    }
}

impl Drop for AddressSpaceBuilder {
    fn drop(&mut self) {
        // SAFETY: l0_paddr was allocated by new() and points to a valid
        // (possibly partially constructed) page table tree.
        // free_address_space walks L0→L1→L2→L3 checking is_table() at
        // each level, so partial trees are handled correctly.
        unsafe { free_address_space(self.l0_paddr); }
    }
}

/// Allocate a fresh set of page tables and map the given pages.
/// Returns the physical address of the L0 table (for TTBR0).
///
/// Convenience wrapper around AddressSpaceBuilder for callers that have
/// all mappings available in a single contiguous slice.
///
/// # Safety
/// All physical addresses in mappings must be valid allocated pages.
pub unsafe fn create_address_space(mappings: &[Mapping]) -> Result<PhysAddr, VmemError> {
    let mut builder = AddressSpaceBuilder::new()?;
    builder.map_batch(mappings)?;
    Ok(builder.finish())
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
/// `attr` selects the MAIR regime: `Device` → MAIR_DEVICE + SH_NON
/// (strongly ordered, non-cacheable, MMIO); `Normal` → MAIR_NORMAL +
/// SH_INNER (write-back cacheable, RAM).
///
/// Validation is done by the pure model in lockjaw_types::vmem (tested on host).
///
/// # Safety
/// `ttbr0_paddr` must be a valid L0 page table.
/// `header` must be a valid BackedHeader with page_count data pages —
/// the wrapper carries the backing-pages witness so get_page is safe.
pub unsafe fn map_pages_in_existing(
    ttbr0_paddr: PhysAddr,
    virt_addr: u64,
    header: &lockjaw_types::pageset_table::BackedHeader<'_>,
    attr: lockjaw_types::vmem::MapMemoryAttribute,
) -> Result<(), VmemError> {
    use lockjaw_types::page_table::{MapWalk, MapWalkResult};
    use lockjaw_types::vmem::{
        classify_l2_entry, map_action_for_l2, L2RegionIter, MapAction,
        select_attrs, build_user_page,
    };

    let page_count = header.data_page_count();

    // Walk L0 → L1 → L2 once via the pure state machine to find the
    // first L2 region. After this we have the L2 table paddr and the
    // starting (l2_idx, l3_start); L2RegionIter slices the rest into
    // per-L2-region chunks (multi-L2 mappings introduced in Phase 2.M).
    let (mut walk, mut result) = MapWalk::start(ttbr0_paddr.as_u64(), virt_addr, page_count);
    let (l2_table_paddr, l2_start, l3_start) = loop {
        match result {
            MapWalkResult::ReadPte(pte_paddr) => {
                // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
                let pte_va = pte_paddr + KERNEL_VA_OFFSET;
                let pte_raw = core::ptr::read_volatile(pte_va as *const u64);
                result = walk.step(pte_raw);
            }
            MapWalkResult::ReachedL2 { l2_table_paddr, l2_idx, l3_start, .. } => {
                // We re-classify each L2 entry inside the loop below
                // rather than caching `state`, since the iterator
                // iterates across L2 entries and only the first one
                // is reflected in MapWalk's returned `state`.
                break (l2_table_paddr, l2_idx, l3_start);
            }
            MapWalkResult::Fault => return Err(VmemError::OutOfPages),
            MapWalkResult::InvalidMapping => return Err(VmemError::TooManyMappings),
        }
    };

    let (mair_attr, sh) = select_attrs(attr);
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let l2_va = (l2_table_paddr + KERNEL_VA_OFFSET) as *mut PageTable;

    // Three-pass transactional layout:
    //   Pass 1 (classify): walk every L2 region read-only. If any
    //     slot is a block conflict, return Err immediately — no
    //     L2/L3 state has been touched.
    //   Pass 2 (pre-allocate): grab the needed number of L3 pages
    //     up front. On partial failure, free what we got and
    //     return Err — still no L2/L3 state touched.
    //   Pass 3 (apply): write L2 + L3 entries. Cannot fail because
    //     all decisions and allocations from the first two passes
    //     are now committed in local state.
    // Without this, a multi-L2 mapping that hits a block conflict
    // or OOM in a later region would leave an earlier region's L3
    // table partially populated and never rolled back — Phase 2.M
    // regression flagged by review.

    // PASS 1: classify each L2 entry, reject on conflict, count
    // L3 allocations needed.
    let mut needed_l3_allocs: usize = 0;
    for region in L2RegionIter::new(l2_start, l3_start, page_count) {
        let l2_state = classify_l2_entry((*l2_va).entries[region.l2_idx]);
        match map_action_for_l2(l2_state) {
            MapAction::UseExistingL3 => {}
            MapAction::AllocateL3 => needed_l3_allocs += 1,
            MapAction::ErrorBlockConflict => {
                return Err(VmemError::TooManyL3Regions);
            }
        }
    }

    // PASS 2: pre-allocate L3 pages into a stack array. Bounded by
    // MAX_L2_REGIONS_PER_MAP (33) — the worst case for a maximum-
    // size practical mapping starting at a non-aligned l3_start.
    // On partial allocation failure, free what we got and return.
    let mut prealloc_l3: [Option<PhysPage>; MAX_L2_REGIONS_PER_MAP] =
        [None; MAX_L2_REGIONS_PER_MAP];
    for i in 0..needed_l3_allocs {
        match page_alloc::alloc_page() {
            Some(p) => prealloc_l3[i] = Some(p),
            None => {
                // Roll back partial allocations. No L2/L3 state
                // touched yet, so this fully restores entry state.
                for slot in &mut prealloc_l3[..i] {
                    if let Some(p) = slot.take() {
                        page_alloc::dealloc_page(p);
                    }
                }
                return Err(VmemError::OutOfPages);
            }
        }
    }

    // PASS 3: apply. Re-classify each L2 entry (state hasn't changed
    // since pass 1 — GKL held throughout), use the pre-allocated L3
    // page when needed. Cannot fail.
    let mut alloc_cursor = 0;
    for region in L2RegionIter::new(l2_start, l3_start, page_count) {
        let l2_entry = (*l2_va).entries[region.l2_idx];
        let l2_state = classify_l2_entry(l2_entry);
        let l3_va = match map_action_for_l2(l2_state) {
            MapAction::UseExistingL3 => {
                let l3_paddr = l2_entry.output_addr();
                // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
                (l3_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut PageTable
            }
            MapAction::AllocateL3 => {
                // SAFETY: pass 2 reserved exactly needed_l3_allocs
                // pages; the iterator visits the same regions in
                // the same order, so alloc_cursor is in bounds and
                // the slot is Some.
                let l3_page = prealloc_l3[alloc_cursor]
                    .take()
                    .expect("L3 page reserved in pass 2");
                alloc_cursor += 1;
                // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
                let va = (l3_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
                ptr::write_bytes(va, 0, 1);
                (*l2_va).entries[region.l2_idx] =
                    PageTableEntry::new_table(l3_page.start_addr());
                va
            }
            MapAction::ErrorBlockConflict => {
                // Pass 1 verified no block conflict. Reaching this
                // arm means the L2 table mutated under us, which
                // can only happen with a kernel bug since GKL is
                // held throughout map_pages_in_existing.
                panic!(
                    "map_pages_in_existing: L2 entry mutated between pass 1 and pass 3 \
                     (l2_idx={})",
                    region.l2_idx
                );
            }
        };

        // Write entries for this L2 region. data_offset / l3_start /
        // pages_in_region all come from the iterator.
        for i in 0..region.pages_in_region {
            let phys = PhysAddr::new(header.get_page(region.data_offset + i).unwrap());
            (*l3_va).entries[region.l3_start + i] = build_user_page(phys, mair_attr, sh);
        }
    }

    // Single TLB invalidate at the end — covers all L2 regions written
    // above. tlbi vmalle1is broadcasts to inner-shareable cores.
    core::arch::asm!(
        "dsb ish",
        "tlbi vmalle1is",
        "dsb ish",
        "isb",
    );

    Ok(())
}

/// Maximum number of L2 regions a single mapping can touch.
/// `MAX_PRACTICAL_PAGES_PER_SET (16384) / PAGES_PER_L2_REGION (512) = 32`,
/// plus one extra to cover a non-aligned starting offset that splits
/// the first region. Used to size the stack array of pre-allocated
/// L3 pages in `map_pages_in_existing`.
const MAX_L2_REGIONS_PER_MAP: usize = 33;

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

/// Validate that L3 PTEs at [va, va + count*PAGE_SIZE) map to the
/// expected physical pages. Read-only walk; never writes and never
/// invalidates the TLB.
///
/// Returns Ok(()) on full match, Err(InvalidParameter) on the first
/// mismatch (the index is folded into a single error here; callers
/// that need the index can use `lockjaw_types::page_table::validate_pte_match`
/// directly).
///
/// Pairs with `clear_validated_pte`: after a successful return,
/// `clear_validated_pte` against the same `(ttbr0, va, count)` is
/// guaranteed to clear every PTE this function inspected, provided the
/// kernel holds the GKL across the pair.
///
/// # Safety
/// `ttbr0_paddr` must be a valid L0 page table base.
/// `expected_pages` must contain valid physical addresses.
pub unsafe fn validate_pte_match(
    ttbr0_paddr: PhysAddr,
    va: u64,
    expected_pages: &[u64],
) -> Result<(), VmemError> {
    lockjaw_types::page_table::validate_pte_match(
        ttbr0_paddr.as_u64(),
        va,
        expected_pages,
        |pte_paddr| {
            // SAFETY: kernel VA via KERNEL_VA_OFFSET
            core::ptr::read_volatile((pte_paddr + KERNEL_VA_OFFSET) as *const u64)
        },
    ).map_err(|_| VmemError::InvalidParameter)
}

/// Clear L3 PTEs at [va, va + count*PAGE_SIZE) and flush the TLB.
/// Write-only — does NOT validate the PTEs match anything.
///
/// MUST be called only after a successful matching `validate_pte_match`
/// against the same `(ttbr0, va, count)` within the same critical
/// section (GKL held). Under that precondition every L3 walk completes
/// successfully; any other walk outcome means the page table changed
/// between phases (kernel bug).
///
/// TLB invalidation lives here, not in the validate wrapper, because
/// only the write phase changes mappings.
///
/// # Safety
/// `ttbr0_paddr` must be a valid L0 page table base.
pub unsafe fn clear_validated_pte(
    ttbr0_paddr: PhysAddr,
    va: u64,
    count: usize,
) {
    lockjaw_types::page_table::clear_validated_pte(
        ttbr0_paddr.as_u64(),
        va,
        count,
        |pte_paddr| {
            // SAFETY: kernel VA via KERNEL_VA_OFFSET
            core::ptr::read_volatile((pte_paddr + KERNEL_VA_OFFSET) as *const u64)
        },
        |pte_paddr, val| {
            // SAFETY: kernel VA via KERNEL_VA_OFFSET
            core::ptr::write_volatile((pte_paddr + KERNEL_VA_OFFSET) as *mut u64, val);
        },
    );

    // dsb ish: drain prior PTE writes before TLBI;
    // tlbi vmalle1is: invalidate TLB across inner-shareable cores;
    // dsb ish: wait for TLBI completion;
    // isb: refetch instructions in case any depend on freshly-flushed
    //      stage-1 mappings.
    core::arch::asm!(
        "dsb ish",
        "tlbi vmalle1is",
        "dsb ish",
        "isb",
    );
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

