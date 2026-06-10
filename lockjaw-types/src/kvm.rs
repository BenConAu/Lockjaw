//! Pure scaffolding for the kernel virtual-address (KVM) allocator.
//!
//! The KVM allocator hands out N-page virtually-contiguous ranges from a
//! dedicated higher-half pool, backed by N independently-allocated
//! physical frames. The kernel uses it for objects that need virtual
//! contiguity but not physical contiguity (initially: PageSet headers
//! whose `BackedHeader` accessors do `base.add(byte_offset)` arithmetic
//! across page boundaries).
//!
//! This module owns the *pure* pieces:
//! - the `KvmFreeList` state machine (region tracking, alloc/free,
//!   coalesce);
//! - `build_kernel_page` for the EL1-only / UXN / PXN PTE attributes
//!   used by KVM mappings;
//! - the `KvmMapWalk` and `KvmFreeWalk` state machines that decide,
//!   step-by-step, what the kernel must read or write next when
//!   stitching pages into the TTBR1 tree.
//!
//! The kernel side (`src/mm/kvm.rs`) holds the singleton, performs
//! the raw PTE reads/writes and TLB invalidations, and calls
//! `page_alloc::alloc_page` for backing frames and page-table pages.

use crate::addr::{KernelVa, PhysAddr, PAGE_SIZE};
use crate::page_table::{
    PageTableEntry, AP_RW_EL1, MAIR_NORMAL, SH_INNER,
};

// ---------------------------------------------------------------------------
// Pool layout
// ---------------------------------------------------------------------------

/// Base of the KVM pool: midway up the canonical AArch64 high half,
/// well above the linear map's `0xFFFF_0000_..` window. Aligned on a
/// 256 TiB boundary so the KVM pool occupies its own L0 entry and
/// never shares an L0 slot with the linear map.
pub const KVM_POOL_BASE: u64 = 0xFFFF_8000_0000_0000;

/// VA reservation for the KVM pool: one L1 entry (512 GiB) of address
/// space. This is the carve-out size at the L0/L1 level — only the
/// `KVM_POOL_USABLE_SIZE` prefix has pre-allocated L2/L3 page-table
/// metadata. Addresses above `KVM_POOL_BASE + KVM_POOL_USABLE_SIZE`
/// are inside the L0 carve-out but have no walker-reachable mapping,
/// so `free()`'s bounds check uses `KVM_POOL_END` (the full span)
/// while `KvmFreeList::new()` sizes only against the usable prefix.
pub const KVM_POOL_VA_SPAN: u64 = 1u64 << 39;

pub const KVM_POOL_END: u64 = KVM_POOL_BASE + KVM_POOL_VA_SPAN;

/// Index of the KVM pool's L0 entry. With a 4 KB granule the L0
/// index is bits 47:39 of the VA. For `KVM_POOL_BASE`:
/// `(0xFFFF_8000_0000_0000 >> 39) & 0x1FF == 0x100 == 256`.
pub const KVM_L0_INDEX: usize = 256;

// ---------------------------------------------------------------------------
// Working pool sizing & bootstrap page-table-tree budget
//
// Pre-allocating L2/L3 metadata for the *full* 512 GiB carve-out
// would cost ~1 GiB of RAM (512 L2 + 262144 L3 × 4 KiB). Unworkable
// on Pi 4B (2 GiB). Resolution: split the concept — KVM_POOL_VA_SPAN
// is the L0/L1 reservation (kept at 512 GiB so the L0 entry is
// dedicated to KVM), KVM_POOL_USABLE_SIZE is the working portion
// whose L2/L3 tree gets pre-allocated at bootstrap. Working size is
// anchored to the kernel-object hard caps (MAX_PAGESETS, MAX_THREADS)
// so the bound is computable, not arbitrary.
// ---------------------------------------------------------------------------

/// Maximum header-page count for a single PageSet. Encoded by
/// `header_pages_for` in `lockjaw-types/src/pageset_table.rs` — 33 is
/// the largest the header table reaches today (1 page per 64 entries
/// across the 2048-entry maximum allocation).
pub const MAX_HEADER_PAGES_PER_PAGESET: usize = 33;

/// Per-process worst-case KVM page count: ProcessObject (1) +
/// HandleTable (1) + L0/L1/L2 page tables (3) + generous L3 pool
/// from V14's per-process page-table donation (~16).
pub const PAGES_PER_PROCESS_WORST_CASE: usize = 1 + 1 + 3 + 16;

/// Budget — PageSets. Until NK2 lands the dedicated header pool,
/// every PageSet header carves from KVM.
pub const KVM_BUDGET_PAGESETS: usize =
    crate::pageset_table::MAX_PAGESETS * MAX_HEADER_PAGES_PER_PAGESET;

/// Budget — threads. TCB + kernel stack = 2 KVM pages per thread.
pub const KVM_BUDGET_THREADS: usize = crate::scheduler::MAX_THREADS * 2;

/// Budget — processes. No explicit MAX_PROCESSES exists today;
/// process count is bounded by thread count (every process holds
/// ≥1 thread), so MAX_THREADS is the upper bound for processes too.
/// This overcounts safely: distinct from the thread budget because
/// process-level kernel objects (ProcessObject, HandleTable, page
/// tables) are not the same pages as the thread budget's TCBs and
/// stacks.
pub const KVM_BUDGET_PROCESSES: usize =
    crate::scheduler::MAX_THREADS * PAGES_PER_PROCESS_WORST_CASE;

/// Sum of audited kernel-object KVM consumption at the current caps.
pub const KVM_BUDGET_TOTAL: usize =
    KVM_BUDGET_PAGESETS + KVM_BUDGET_THREADS + KVM_BUDGET_PROCESSES;

/// Working budget = total + 30% headroom for kernel objects not yet
/// enumerated (kernel-side caps that NK6/NK7 will surface).
/// Integer 30%: ×13/10.
pub const KVM_BUDGET_WORKING: usize = KVM_BUDGET_TOTAL * 13 / 10;

/// Working portion of the pool that gets pre-allocated L2/L3 metadata
/// at bootstrap. 1 GiB is the next-power-of-two ceiling above
/// `KVM_BUDGET_WORKING` (~140 MiB today, ~7× headroom), and snaps
/// cleanly to one L1 entry's worth of L2 at the 4 KiB granule
/// (1 L2 covers 1 GiB). If a future workload approaches the budget,
/// raise `MAX_PAGESETS` / `MAX_THREADS` first and the formula tells
/// you the new pool size.
pub const KVM_POOL_USABLE_SIZE: u64 = 1u64 << 30;

const _USABLE_COVERS_BUDGET: () = assert!(
    (KVM_POOL_USABLE_SIZE as usize) >= KVM_BUDGET_WORKING * (PAGE_SIZE as usize),
    "KVM_POOL_USABLE_SIZE must cover the cap-driven working budget",
);

const _USABLE_FITS_VA_SPAN: () = assert!(
    KVM_POOL_USABLE_SIZE <= KVM_POOL_VA_SPAN,
    "KVM_POOL_USABLE_SIZE cannot exceed KVM_POOL_VA_SPAN",
);

const _USABLE_PAGE_ALIGNED: () = assert!(
    KVM_POOL_USABLE_SIZE % PAGE_SIZE == 0,
    "KVM_POOL_USABLE_SIZE must be a multiple of PAGE_SIZE — \
     KvmFreeList::new and the L2/L3 table-count formulas assume it",
);

/// Bytes covered by one L1 PTE at the 4 KiB granule (= 1 GiB).
const L1_ENTRY_BYTES: u64 = 1u64 << 30;
/// Bytes covered by one L2 PTE at the 4 KiB granule (= 2 MiB).
const L2_ENTRY_BYTES: u64 = 1u64 << 21;

/// Number of L2 page tables required to cover `KVM_POOL_USABLE_SIZE`.
/// For 1 GiB usable: 1 L2 table.
pub const KVM_L2_TABLES_COUNT: usize =
    ((KVM_POOL_USABLE_SIZE + L1_ENTRY_BYTES - 1) / L1_ENTRY_BYTES) as usize;

/// Number of L3 page tables required to cover `KVM_POOL_USABLE_SIZE`.
/// Each L3 covers 2 MiB. For 1 GiB usable: 512 L3 tables.
pub const KVM_L3_TABLES_COUNT: usize =
    ((KVM_POOL_USABLE_SIZE + L2_ENTRY_BYTES - 1) / L2_ENTRY_BYTES) as usize;

/// Page-table pages the bootstrap pre-allocates: 1 L1 + L2s + L3s.
/// For 1 GiB usable: 1 + 1 + 512 = 514.
pub const KVM_BOOTSTRAP_TABLE_PAGES: usize =
    1 + KVM_L2_TABLES_COUNT + KVM_L3_TABLES_COUNT;

/// Next-power-of-two contiguous block the buddy allocator returns
/// when asked for `KVM_BOOTSTRAP_TABLE_PAGES`. For 514 → 1024.
/// The extra pages stay reserved as inert padding for the contiguous
/// block.
pub const BOOTSTRAP_CONTIGUOUS_RESERVE: usize =
    KVM_BOOTSTRAP_TABLE_PAGES.next_power_of_two();

/// Decompose a KVA in the KVM pool into (L0, L1, L2, L3) page-table
/// indices. Reuses `vmem::page_table_indices` so all 4-level
/// page-table arithmetic in this kernel goes through one helper.
pub const fn kvm_pool_indices(kva: KernelVa) -> (usize, usize, usize, usize) {
    crate::vmem::page_table_indices(kva.as_u64())
}

/// Build the PTE for a kernel page mapping in the KVM pool.
///
/// Attributes: `MAIR_NORMAL | AP_RW_EL1 | SH_INNER | UXN | PXN`.
/// EL1-only access (no AP_RW_ALL) so a stray user TTBR0 lookup
/// cannot reach a kernel header. UXN+PXN because no one executes
/// from KVM-backed objects.
pub fn build_kernel_page(phys: PhysAddr) -> PageTableEntry {
    PageTableEntry::new_page(phys, MAIR_NORMAL, AP_RW_EL1, SH_INNER)
        .with_uxn()
        .with_pxn()
}

// ---------------------------------------------------------------------------
// KvmFreeList — pure free-region tracker
// ---------------------------------------------------------------------------

/// Maximum number of distinct free regions tracked at once. Empirically
/// this stays small: PageSet header allocation is the only caller
/// today, lifetimes correlate with process lifetimes, and free
/// coalesces neighboring regions immediately.
pub const KVM_MAX_FREE_REGIONS: usize = 64;

/// One free region in the KVM pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KvmFreeRegion {
    pub start: KernelVa,
    pub pages: usize,
}

/// Errors from the free-list operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvmFreeListError {
    /// `try_alloc` could not find a contiguous run of the requested
    /// size in any region.
    OutOfVirtualMemory,
    /// `free` would split or insert a region but the fixed-size
    /// region table is full. Should not happen under normal
    /// workloads; surfaces as a kernel bug if it does.
    RegionTableFull,
    /// `free` was given a range that overlaps the existing free
    /// list (caller bug — double-free or accounting drift).
    DoubleFree,
    /// `free` was given a range outside the pool.
    OutOfPool,
}

/// Sorted free-region list. First-fit on alloc, two-sided coalesce
/// on free. Storage is a fixed-size array; the kernel side wraps
/// this in an `UnsafeCell` singleton serialised by the GKL.
#[derive(Debug)]
pub struct KvmFreeList {
    regions: [KvmFreeRegion; KVM_MAX_FREE_REGIONS],
    count: usize,
}

impl KvmFreeList {
    /// New free list covering the entire pool.
    pub const fn new() -> Self {
        let mut regions = [KvmFreeRegion {
            start: KernelVa::new(KVM_POOL_BASE),
            pages: 0,
        }; KVM_MAX_FREE_REGIONS];
        regions[0] = KvmFreeRegion {
            start: KernelVa::new(KVM_POOL_BASE),
            pages: (KVM_POOL_USABLE_SIZE / PAGE_SIZE) as usize,
        };
        Self { regions, count: 1 }
    }

    /// Number of free regions currently tracked. For tests and diagnostics.
    pub fn region_count(&self) -> usize {
        self.count
    }

    /// Total free pages across all regions. For diagnostics.
    pub fn free_pages(&self) -> usize {
        let mut total = 0usize;
        for i in 0..self.count {
            total += self.regions[i].pages;
        }
        total
    }

    /// Snapshot a region (for tests and diagnostics).
    pub fn region(&self, idx: usize) -> Option<KvmFreeRegion> {
        if idx < self.count { Some(self.regions[idx]) } else { None }
    }

    /// Allocate `pages` virtually-contiguous pages. Returns the base
    /// `KernelVa` of the allocation, or `OutOfVirtualMemory` if no
    /// region is large enough. First-fit: walks regions in address
    /// order, picks the first one that fits.
    pub fn try_alloc(&mut self, pages: usize) -> Result<KernelVa, KvmFreeListError> {
        if pages == 0 {
            // Zero-page allocation is meaningless; reject explicitly.
            return Err(KvmFreeListError::OutOfVirtualMemory);
        }
        for i in 0..self.count {
            if self.regions[i].pages >= pages {
                let base = self.regions[i].start;
                let remaining = self.regions[i].pages - pages;
                if remaining == 0 {
                    // Region consumed entirely — remove it.
                    for j in i..self.count - 1 {
                        self.regions[j] = self.regions[j + 1];
                    }
                    self.count -= 1;
                } else {
                    // Region shrinks at the front.
                    self.regions[i] = KvmFreeRegion {
                        start: base.add_pages(pages),
                        pages: remaining,
                    };
                }
                return Ok(base);
            }
        }
        Err(KvmFreeListError::OutOfVirtualMemory)
    }

    /// Return a range to the pool. Coalesces with neighbouring free
    /// regions (two-sided merge) so the free list stays compact.
    pub fn free(&mut self, start: KernelVa, pages: usize) -> Result<(), KvmFreeListError> {
        if pages == 0 {
            return Ok(());
        }
        let start_va = start.as_u64();
        let end_va = start_va + (pages as u64) * PAGE_SIZE;
        // Bounds-check against the *usable* end, not KVM_POOL_END
        // (which spans the full VA reservation). Addresses in
        // `[KVM_POOL_BASE + KVM_POOL_USABLE_SIZE, KVM_POOL_END)` are
        // inside the L0 carve-out but have no pre-allocated L2/L3
        // metadata; accepting a free there would let try_alloc later
        // return a VA whose walker hits an unallocated parent PTE,
        // re-introducing the V15/V16 runtime metadata growth NK1
        // eliminated.
        let usable_end = KVM_POOL_BASE + KVM_POOL_USABLE_SIZE;
        if start_va < KVM_POOL_BASE || end_va > usable_end {
            return Err(KvmFreeListError::OutOfPool);
        }

        // Find insertion point (first region with start > start_va).
        let mut insert_at = self.count;
        for i in 0..self.count {
            if self.regions[i].start.as_u64() > start_va {
                insert_at = i;
                break;
            }
        }

        // Overlap check against neighbours.
        if insert_at > 0 {
            let prev = self.regions[insert_at - 1];
            let prev_end = prev.start.as_u64() + (prev.pages as u64) * PAGE_SIZE;
            if prev_end > start_va {
                return Err(KvmFreeListError::DoubleFree);
            }
        }
        if insert_at < self.count {
            let next = self.regions[insert_at];
            if end_va > next.start.as_u64() {
                return Err(KvmFreeListError::DoubleFree);
            }
        }

        // Try to coalesce with previous (touching at the front).
        let merged_with_prev = if insert_at > 0 {
            let prev = self.regions[insert_at - 1];
            let prev_end = prev.start.as_u64() + (prev.pages as u64) * PAGE_SIZE;
            if prev_end == start_va {
                self.regions[insert_at - 1] = KvmFreeRegion {
                    start: prev.start,
                    pages: prev.pages + pages,
                };
                true
            } else {
                false
            }
        } else {
            false
        };

        // Try to coalesce with next (touching at the back).
        if merged_with_prev {
            // We expanded prev. Check if it now touches next.
            let prev_idx = insert_at - 1;
            let prev = self.regions[prev_idx];
            let prev_end = prev.start.as_u64() + (prev.pages as u64) * PAGE_SIZE;
            if insert_at < self.count
                && self.regions[insert_at].start.as_u64() == prev_end
            {
                let next = self.regions[insert_at];
                self.regions[prev_idx] = KvmFreeRegion {
                    start: prev.start,
                    pages: prev.pages + next.pages,
                };
                // Shift to remove next.
                for j in insert_at..self.count - 1 {
                    self.regions[j] = self.regions[j + 1];
                }
                self.count -= 1;
            }
            return Ok(());
        }

        // Try to coalesce with next only.
        if insert_at < self.count
            && self.regions[insert_at].start.as_u64() == end_va
        {
            let next = self.regions[insert_at];
            self.regions[insert_at] = KvmFreeRegion {
                start,
                pages: pages + next.pages,
            };
            return Ok(());
        }

        // No coalesce — insert a new region.
        if self.count >= KVM_MAX_FREE_REGIONS {
            return Err(KvmFreeListError::RegionTableFull);
        }
        // Shift to make room at insert_at.
        for j in (insert_at..self.count).rev() {
            self.regions[j + 1] = self.regions[j];
        }
        self.regions[insert_at] = KvmFreeRegion { start, pages };
        self.count += 1;
        Ok(())
    }
}

impl Default for KvmFreeList {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// KvmMapWalk — pure step-by-step walker for installing a kernel mapping
// ---------------------------------------------------------------------------

/// What the kernel must do next while installing one page of a KVM
/// mapping. The kernel performs the side effect (read PTE, allocate
/// page-table page, write PTE) and calls the matching `step_*`
/// method to advance the state machine, then re-queries
/// `current_step()` for the next action.
///
/// `#[must_use]` because dropping a step value silently halts the
/// allocator — a previous version of `drive_map_walk` did exactly
/// that and spun forever. Caught at compile time now.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvmMapStep {
    /// Read the L1 PTE at this physical address, hand the value back
    /// via `step_l1`. The kernel obtains the L1 table paddr from
    /// `KERNEL_L0[KVM_L0_INDEX]` (set up once at `kvm_init`).
    ReadL1Pte { l1_pte_paddr: PhysAddr },
    /// L2 table is missing here — allocate a fresh page-table page,
    /// install it as a table descriptor at `parent_pte_paddr`, and
    /// then call `step_l2_allocated` with the new L2's paddr.
    AllocL2 { parent_pte_paddr: PhysAddr },
    /// Read the L2 PTE at this physical address; feed via `step_l2`.
    ReadL2Pte { l2_pte_paddr: PhysAddr },
    /// L3 table is missing here — allocate a page-table page, install
    /// it at `parent_pte_paddr`, and call `step_l3_allocated`.
    AllocL3 { parent_pte_paddr: PhysAddr },
    /// Read the target L3 PTE so the walker can verify the slot is
    /// invalid before writing — symmetric with `KvmFreeWalk`'s L3
    /// read. Without this check the allocator would trust the
    /// freelist as the sole source of truth for "this VA is
    /// unmapped"; a freelist/page-table drift would silently
    /// overwrite a live mapping. The walker faults instead if the
    /// L3 entry is valid.
    ReadL3Pte { l3_pte_paddr: PhysAddr },
    /// Walker has reached the L3 slot for the current page and needs
    /// a backing frame. The kernel calls `page_alloc::alloc_page`
    /// (or returns OOM) and feeds the result via `supply_backing`,
    /// which advances to `WritePagePte`.
    WantBacking,
    /// Write the L3 PTE: the page entry the walker built (via
    /// `build_kernel_page`) goes at `pte_paddr`. Then call
    /// `step_pte_written` to advance to the next page in the range.
    WritePagePte { pte_paddr: PhysAddr, entry: PageTableEntry },
    /// All pages in the range have been mapped.
    Done,
    /// An invariant was violated. Possible causes: an L1/L2 entry
    /// exists but is a block descriptor (not a table); the target
    /// L3 slot is already valid (alloc would overwrite a live
    /// mapping — freelist/page-table drift). Surfaces as a kernel
    /// bug.
    Fault,
}

/// State machine for installing a KVM mapping page by page. The
/// kernel constructs one walker per `kvm_alloc` call, feeds in the
/// L1 paddr and the per-page backing paddrs, and obeys the steps.
pub struct KvmMapWalk {
    base: KernelVa,
    pages: usize,
    page_idx: usize,
    /// Per-page state.
    state: PageState,
    /// Cached table paddrs for the current page's walk.
    l1_paddr: u64,
    l2_paddr: u64,
    l3_paddr: u64,
    /// Backing paddr the kernel supplied for the current page; the
    /// walker uses it to build the PTE.
    pending_phys: Option<PhysAddr>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PageState {
    /// About to read L1 PTE for the current page.
    NeedL1,
    /// L1 entry was empty — kernel must allocate a fresh L2 table,
    /// then call `step_l2_allocated`.
    NeedAllocL2,
    /// L1 has been resolved (existing or freshly allocated); about
    /// to read L2 PTE.
    NeedL2,
    /// L2 entry was empty — kernel must allocate a fresh L3 table,
    /// then call `step_l3_allocated`.
    NeedAllocL3,
    /// L2 has been resolved; about to read the target L3 PTE to
    /// verify the slot is invalid (must be unmapped before alloc
    /// writes a new entry there).
    NeedL3,
    /// L3 PTE was confirmed invalid; walker is waiting for the
    /// kernel to supply a backing frame.
    NeedBacking,
    /// Backing supplied; about to write the page entry.
    NeedWritePte,
    /// Walker hit an invariant violation. `current_step` returns
    /// `Fault` from this state until the walker is dropped.
    Faulted,
    /// All pages have been mapped.
    Done,
}

impl KvmMapWalk {
    /// Begin walking a KVM mapping for `pages` pages starting at
    /// `base`. The kernel passes in the L1 table paddr it read from
    /// `KERNEL_L0[KVM_L0_INDEX]` once at init.
    pub fn start(base: KernelVa, pages: usize, l1_paddr: u64) -> Self {
        Self {
            base,
            pages,
            page_idx: 0,
            state: if pages == 0 { PageState::Done } else { PageState::NeedL1 },
            l1_paddr,
            l2_paddr: 0,
            l3_paddr: 0,
            pending_phys: None,
        }
    }

    /// What the kernel should do for the current page. The single
    /// source of truth for the next action — `step_*` methods only
    /// mutate state; the kernel always re-queries `current_step()`
    /// after each call.
    pub fn current_step(&self) -> KvmMapStep {
        let kva = self.base.add_pages(self.page_idx);
        let (_l0, l1, l2, l3) = kvm_pool_indices(kva);
        match self.state {
            PageState::NeedL1 => KvmMapStep::ReadL1Pte {
                l1_pte_paddr: PhysAddr::new(self.l1_paddr + (l1 as u64) * 8),
            },
            PageState::NeedAllocL2 => KvmMapStep::AllocL2 {
                parent_pte_paddr: PhysAddr::new(self.l1_paddr + (l1 as u64) * 8),
            },
            PageState::NeedL2 => KvmMapStep::ReadL2Pte {
                l2_pte_paddr: PhysAddr::new(self.l2_paddr + (l2 as u64) * 8),
            },
            PageState::NeedAllocL3 => KvmMapStep::AllocL3 {
                parent_pte_paddr: PhysAddr::new(self.l2_paddr + (l2 as u64) * 8),
            },
            PageState::NeedL3 => KvmMapStep::ReadL3Pte {
                l3_pte_paddr: PhysAddr::new(self.l3_paddr + (l3 as u64) * 8),
            },
            PageState::NeedBacking => KvmMapStep::WantBacking,
            PageState::NeedWritePte => {
                let phys = match self.pending_phys {
                    Some(p) => p,
                    // Logic error: NeedWritePte without backing is
                    // a state-machine bug; surface as Fault.
                    None => return KvmMapStep::Fault,
                };
                KvmMapStep::WritePagePte {
                    pte_paddr: PhysAddr::new(self.l3_paddr + (l3 as u64) * 8),
                    entry: build_kernel_page(phys),
                }
            }
            PageState::Faulted => KvmMapStep::Fault,
            PageState::Done => KvmMapStep::Done,
        }
    }

    /// Feed the L1 PTE the kernel just read. State becomes
    /// `NeedAllocL2` if the entry is empty, `NeedL2` if it's a
    /// table, or `Faulted` if it's a block descriptor.
    pub fn step_l1(&mut self, pte_raw: u64) {
        debug_assert!(self.state == PageState::NeedL1);
        let pte = PageTableEntry::from_raw(pte_raw);
        if !pte.is_valid() {
            self.state = PageState::NeedAllocL2;
            return;
        }
        if !pte.is_table() {
            self.state = PageState::Faulted;
            return;
        }
        self.l2_paddr = pte.output_addr().as_u64();
        self.state = PageState::NeedL2;
    }

    /// The kernel has allocated and installed a fresh L2 table at
    /// the L1 slot. Walker advances to reading the L2 PTE (which
    /// will return 0 for a freshly-zeroed table → AllocL3 next).
    pub fn step_l2_allocated(&mut self, l2_paddr: u64) {
        debug_assert!(self.state == PageState::NeedAllocL2);
        self.l2_paddr = l2_paddr;
        self.state = PageState::NeedL2;
    }

    /// Feed the L2 PTE the kernel just read.
    pub fn step_l2(&mut self, pte_raw: u64) {
        debug_assert!(self.state == PageState::NeedL2);
        let pte = PageTableEntry::from_raw(pte_raw);
        if !pte.is_valid() {
            self.state = PageState::NeedAllocL3;
            return;
        }
        if !pte.is_table() {
            self.state = PageState::Faulted;
            return;
        }
        self.l3_paddr = pte.output_addr().as_u64();
        self.state = PageState::NeedL3;
    }

    /// The kernel has allocated and installed a fresh L3 table at
    /// the L2 slot. Walker still reads the target L3 PTE for
    /// symmetry with the existing-table path (the read will
    /// return 0 for a fresh-and-zeroed table, matching the
    /// invariant).
    pub fn step_l3_allocated(&mut self, l3_paddr: u64) {
        debug_assert!(self.state == PageState::NeedAllocL3);
        self.l3_paddr = l3_paddr;
        self.state = PageState::NeedL3;
    }

    /// Feed the L3 PTE the kernel just read. The slot must be
    /// invalid — alloc walks must not overwrite a live mapping. If
    /// the slot is already valid the walker faults (freelist /
    /// page-table drift; the kernel must not silently clobber the
    /// existing entry).
    pub fn step_l3(&mut self, pte_raw: u64) {
        debug_assert!(self.state == PageState::NeedL3);
        let pte = PageTableEntry::from_raw(pte_raw);
        if pte.is_valid() {
            self.state = PageState::Faulted;
            return;
        }
        self.state = PageState::NeedBacking;
    }

    /// The kernel allocated a backing frame. Walker advances to
    /// `WritePagePte`.
    pub fn supply_backing(&mut self, phys: PhysAddr) {
        debug_assert!(self.state == PageState::NeedBacking);
        self.pending_phys = Some(phys);
        self.state = PageState::NeedWritePte;
    }

    /// The kernel has written the page PTE. Advance to the next
    /// page in the range, or to `Done`.
    pub fn step_pte_written(&mut self) {
        debug_assert!(self.state == PageState::NeedWritePte);
        self.pending_phys = None;
        self.page_idx += 1;
        if self.page_idx >= self.pages {
            self.state = PageState::Done;
            return;
        }
        // Decide whether to re-walk or reuse cached table paddrs:
        // if the next page lives in the same L2 region (same L1, L2
        // indices), the L2/L3 paddrs are still valid. If the next
        // page crosses an L2 boundary (`l2` index changed), we need
        // to read the L2 entry for the new index — but we can reuse
        // the L1 paddr until `l1` changes.
        let kva = self.base.add_pages(self.page_idx);
        let prev_kva = self.base.add_pages(self.page_idx - 1);
        let (_l0_p, l1_p, l2_p, _l3_p) = kvm_pool_indices(prev_kva);
        let (_l0, l1, l2, _l3) = kvm_pool_indices(kva);
        if l1 != l1_p {
            self.state = PageState::NeedL1;
        } else if l2 != l2_p {
            self.state = PageState::NeedL2;
        } else {
            // Same L3 table — must still read the next L3 slot to
            // verify it's invalid before writing. Symmetry with the
            // first-page path: every PTE write is preceded by a
            // "must be invalid" check.
            self.state = PageState::NeedL3;
        }
    }
}

// ---------------------------------------------------------------------------
// KvmFreeWalk — pure step-by-step walker for tearing a kernel mapping down
// ---------------------------------------------------------------------------

/// What the kernel must do next while tearing down a KVM mapping.
/// Symmetric with `KvmMapStep` but for the destructive direction:
/// the walker decides which PTEs to read (to capture backing paddrs)
/// and which to clear.
///
/// `#[must_use]` for the same reason as `KvmMapStep` — silently
/// dropping a step from `current_step()` would silently halt the
/// teardown.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvmFreeStep {
    /// Read the L1 PTE for the current page; feed back via `step_l1`.
    ReadL1Pte { l1_pte_paddr: PhysAddr },
    /// Read the L2 PTE; feed back via `step_l2`.
    ReadL2Pte { l2_pte_paddr: PhysAddr },
    /// Read the L3 PTE; feed back via `step_l3`. The walker extracts
    /// the backing paddr from the value and yields it via the next
    /// step.
    ReadL3Pte { l3_pte_paddr: PhysAddr },
    /// Clear the L3 PTE for the current page; the kernel zeroes it
    /// and calls `step_pte_cleared`. The walker yields the backing
    /// paddr it captured from the previous read so the kernel can
    /// queue the frame for `dealloc_page` after the TLBI sequence.
    ClearPte { pte_paddr: PhysAddr, backing: PhysAddr },
    /// All pages in the range have been processed.
    Done,
    /// Invariant violation: a table entry on the path was missing
    /// or non-table. Indicates accounting drift between the free
    /// list and the actual page tables.
    Fault,
}

pub struct KvmFreeWalk {
    base: KernelVa,
    pages: usize,
    page_idx: usize,
    state: FreeState,
    l1_paddr: u64,
    l2_paddr: u64,
    l3_paddr: u64,
    /// Backing paddr captured from the most-recently-read L3 PTE.
    captured_backing: Option<PhysAddr>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FreeState {
    NeedL1,
    NeedL2,
    NeedL3,
    NeedClear,
    Faulted,
    Done,
}

impl KvmFreeWalk {
    pub fn start(base: KernelVa, pages: usize, l1_paddr: u64) -> Self {
        Self {
            base,
            pages,
            page_idx: 0,
            state: if pages == 0 { FreeState::Done } else { FreeState::NeedL1 },
            l1_paddr,
            l2_paddr: 0,
            l3_paddr: 0,
            captured_backing: None,
        }
    }

    /// Single source of truth for the next action. Kernel always
    /// re-queries this after each `step_*` call.
    pub fn current_step(&self) -> KvmFreeStep {
        let kva = self.base.add_pages(self.page_idx);
        let (_l0, l1, l2, l3) = kvm_pool_indices(kva);
        match self.state {
            FreeState::NeedL1 => KvmFreeStep::ReadL1Pte {
                l1_pte_paddr: PhysAddr::new(self.l1_paddr + (l1 as u64) * 8),
            },
            FreeState::NeedL2 => KvmFreeStep::ReadL2Pte {
                l2_pte_paddr: PhysAddr::new(self.l2_paddr + (l2 as u64) * 8),
            },
            FreeState::NeedL3 => KvmFreeStep::ReadL3Pte {
                l3_pte_paddr: PhysAddr::new(self.l3_paddr + (l3 as u64) * 8),
            },
            FreeState::NeedClear => {
                let backing = match self.captured_backing {
                    Some(p) => p,
                    None => return KvmFreeStep::Fault,
                };
                KvmFreeStep::ClearPte {
                    pte_paddr: PhysAddr::new(self.l3_paddr + (l3 as u64) * 8),
                    backing,
                }
            }
            FreeState::Faulted => KvmFreeStep::Fault,
            FreeState::Done => KvmFreeStep::Done,
        }
    }

    pub fn step_l1(&mut self, pte_raw: u64) {
        debug_assert!(self.state == FreeState::NeedL1);
        let pte = PageTableEntry::from_raw(pte_raw);
        if !pte.is_table() {
            self.state = FreeState::Faulted;
            return;
        }
        self.l2_paddr = pte.output_addr().as_u64();
        self.state = FreeState::NeedL2;
    }

    pub fn step_l2(&mut self, pte_raw: u64) {
        debug_assert!(self.state == FreeState::NeedL2);
        let pte = PageTableEntry::from_raw(pte_raw);
        if !pte.is_table() {
            self.state = FreeState::Faulted;
            return;
        }
        self.l3_paddr = pte.output_addr().as_u64();
        self.state = FreeState::NeedL3;
    }

    pub fn step_l3(&mut self, pte_raw: u64) {
        debug_assert!(self.state == FreeState::NeedL3);
        let pte = PageTableEntry::from_raw(pte_raw);
        if !pte.is_valid() {
            self.state = FreeState::Faulted;
            return;
        }
        self.captured_backing = Some(pte.output_addr());
        self.state = FreeState::NeedClear;
    }

    pub fn step_pte_cleared(&mut self) {
        debug_assert!(self.state == FreeState::NeedClear);
        self.captured_backing = None;
        self.page_idx += 1;
        if self.page_idx >= self.pages {
            self.state = FreeState::Done;
            return;
        }
        let kva = self.base.add_pages(self.page_idx);
        let prev_kva = self.base.add_pages(self.page_idx - 1);
        let (_l0_p, l1_p, l2_p, _l3_p) = kvm_pool_indices(prev_kva);
        let (_l0, l1, l2, _l3) = kvm_pool_indices(kva);
        if l1 != l1_p {
            self.state = FreeState::NeedL1;
        } else if l2 != l2_p {
            self.state = FreeState::NeedL2;
        } else {
            // Same L3 table — go straight to reading the next L3 PTE.
            self.state = FreeState::NeedL3;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // KvmFreeList
    // -----------------------------------------------------------------------

    #[test]
    fn free_list_initial_state_covers_pool() {
        let fl = KvmFreeList::new();
        assert_eq!(fl.region_count(), 1);
        let r = fl.region(0).unwrap();
        assert_eq!(r.start.as_u64(), KVM_POOL_BASE);
        assert_eq!(r.pages, (KVM_POOL_USABLE_SIZE / PAGE_SIZE) as usize);
    }

    #[test]
    fn free_list_alloc_consumes_from_front() {
        let mut fl = KvmFreeList::new();
        let kva = fl.try_alloc(33).unwrap();
        assert_eq!(kva.as_u64(), KVM_POOL_BASE);
        let r = fl.region(0).unwrap();
        assert_eq!(r.start.as_u64(), KVM_POOL_BASE + 33 * PAGE_SIZE);
        assert_eq!(r.pages, (KVM_POOL_USABLE_SIZE / PAGE_SIZE) as usize - 33);
    }

    #[test]
    fn free_list_zero_pages_is_oom() {
        let mut fl = KvmFreeList::new();
        assert_eq!(fl.try_alloc(0), Err(KvmFreeListError::OutOfVirtualMemory));
    }

    #[test]
    fn free_list_alloc_then_free_round_trips() {
        let mut fl = KvmFreeList::new();
        let initial_total = fl.free_pages();
        let kva = fl.try_alloc(10).unwrap();
        assert_eq!(fl.free_pages(), initial_total - 10);
        fl.free(kva, 10).unwrap();
        assert_eq!(fl.free_pages(), initial_total);
        assert_eq!(fl.region_count(), 1);
    }

    #[test]
    fn free_list_three_way_coalesce() {
        let mut fl = KvmFreeList::new();
        let a = fl.try_alloc(1).unwrap();
        let b = fl.try_alloc(1).unwrap();
        let c = fl.try_alloc(1).unwrap();
        // Tail layout after 3 allocs: [used(A) | used(B) | used(C) | free(tail)].
        // free(A): inserts an A-region at the front; not adjacent to tail.
        // → [free(A) | used(B) | used(C) | free(tail)]  ⇒ 2 free regions.
        fl.free(a, 1).unwrap();
        assert_eq!(fl.region_count(), 2);
        // free(C): C is adjacent to the front of the tail-free region, so
        // it coalesces with it. → [free(A) | used(B) | free(C+tail)] ⇒ 2.
        fl.free(c, 1).unwrap();
        assert_eq!(fl.region_count(), 2);
        // free(B): B is adjacent to A on the left and to (C+tail) on the
        // right — three-way coalesce into one region covering the pool.
        fl.free(b, 1).unwrap();
        assert_eq!(fl.region_count(), 1);
        assert_eq!(fl.free_pages(), (KVM_POOL_USABLE_SIZE / PAGE_SIZE) as usize);
    }

    #[test]
    fn free_list_first_fit_picks_first_sufficient() {
        let mut fl = KvmFreeList::new();
        // Carve three holes of distinct sizes by allocating then freeing.
        let a = fl.try_alloc(2).unwrap();
        let _ = fl.try_alloc(1).unwrap(); // separator
        let c = fl.try_alloc(8).unwrap();
        let _ = fl.try_alloc(1).unwrap(); // separator
        let e = fl.try_alloc(4).unwrap();
        fl.free(a, 2).unwrap();
        fl.free(c, 8).unwrap();
        fl.free(e, 4).unwrap();
        // Free regions in address order: 2, 8, 4, then the tail.
        // Asking for 3 must come from the 8-page region (first-fit
        // skips the 2-page region).
        let kva = fl.try_alloc(3).unwrap();
        // The 8-page hole started at base + (2+1) pages = base + 3 pages.
        assert_eq!(kva.as_u64(), KVM_POOL_BASE + 3 * PAGE_SIZE);
    }

    #[test]
    fn free_list_oom_when_no_region_large_enough() {
        let mut fl = KvmFreeList {
            regions: [KvmFreeRegion { start: KernelVa::new(KVM_POOL_BASE), pages: 0 };
                      KVM_MAX_FREE_REGIONS],
            count: 1,
        };
        fl.regions[0] = KvmFreeRegion { start: KernelVa::new(KVM_POOL_BASE), pages: 5 };
        assert_eq!(fl.try_alloc(6), Err(KvmFreeListError::OutOfVirtualMemory));
    }

    #[test]
    fn free_list_split_consumes_only_what_asked() {
        let mut fl = KvmFreeList::new();
        let _ = fl.try_alloc(10).unwrap();
        let r = fl.region(0).unwrap();
        // Remaining region is shifted by exactly 10 pages.
        assert_eq!(r.start.as_u64(), KVM_POOL_BASE + 10 * PAGE_SIZE);
        assert_eq!(r.pages, (KVM_POOL_USABLE_SIZE / PAGE_SIZE) as usize - 10);
    }

    #[test]
    fn free_rejects_out_of_pool() {
        let mut fl = KvmFreeList::new();
        assert_eq!(
            fl.free(KernelVa::new(0xFFFF_0000_0000_0000), 1),
            Err(KvmFreeListError::OutOfPool),
        );
    }

    #[test]
    fn free_rejects_inside_va_span_but_outside_usable() {
        // The L0 carve-out reaches KVM_POOL_BASE + KVM_POOL_VA_SPAN
        // (512 GiB), but only the KVM_POOL_USABLE_SIZE (1 GiB) prefix
        // has pre-allocated L2/L3 metadata. A free starting at the
        // first VA past the usable end is "inside the L0 reservation"
        // by `KVM_POOL_END`'s definition, but the walker would fault
        // on the missing parent PTE if try_alloc later handed it out.
        // The bounds check must use the usable end, not the VA-span
        // end, so a free here returns OutOfPool — locks the contract
        // that the free list never reaches an unallocated parent.
        let mut fl = KvmFreeList::new();
        let past_usable = KernelVa::new(KVM_POOL_BASE + KVM_POOL_USABLE_SIZE);
        assert_eq!(fl.free(past_usable, 1), Err(KvmFreeListError::OutOfPool));
    }

    #[test]
    fn free_rejects_overlap_with_existing_free_region() {
        let mut fl = KvmFreeList::new();
        let _ = fl.try_alloc(1).unwrap(); // pool now [used(1) | free(rest)]
        // Try to free a range that's still inside the (already-free) tail.
        let inside_tail = KernelVa::new(KVM_POOL_BASE + 5 * PAGE_SIZE);
        assert_eq!(fl.free(inside_tail, 1), Err(KvmFreeListError::DoubleFree));
    }

    // -----------------------------------------------------------------------
    // build_kernel_page
    // -----------------------------------------------------------------------

    #[test]
    fn build_kernel_page_sets_el1_only_uxn_pxn() {
        let pte = build_kernel_page(PhysAddr::new(0x4020_0000));
        assert!(pte.is_valid());
        assert!(pte.is_table()); // page descriptor uses the same TABLE bit at L3
        assert!(pte.is_uxn(), "kernel pages must be UXN");
        assert!(pte.is_pxn(), "kernel pages must be PXN");
        assert_eq!(pte.ap(), AP_RW_EL1, "kernel pages must be EL1-only");
        assert_eq!(pte.attr_index(), MAIR_NORMAL);
        assert_eq!(pte.sh(), SH_INNER);
        assert!(pte.af());
        assert_eq!(pte.output_addr().as_u64(), 0x4020_0000);
    }

    // -----------------------------------------------------------------------
    // KvmMapWalk
    // -----------------------------------------------------------------------

    #[test]
    fn map_walk_zero_pages_done_immediately() {
        let walk = KvmMapWalk::start(KernelVa::new(KVM_POOL_BASE), 0, 0xCAFE_0000);
        assert_eq!(walk.current_step(), KvmMapStep::Done);
    }

    #[test]
    fn map_walk_single_page_into_empty_l1_allocs_l2_and_l3() {
        let l1_paddr = 0xAAAA_0000;
        let mut walk = KvmMapWalk::start(KernelVa::new(KVM_POOL_BASE), 1, l1_paddr);
        // First step: read the L1 PTE. Index = (KVM_POOL_BASE >> 30) & 0x1FF.
        let (_l0, l1, _l2, _l3) = kvm_pool_indices(KernelVa::new(KVM_POOL_BASE));
        assert_eq!(
            walk.current_step(),
            KvmMapStep::ReadL1Pte { l1_pte_paddr: PhysAddr::new(l1_paddr + (l1 as u64) * 8) },
        );
        // L1 entry is empty → next step is AllocL2.
        walk.step_l1(0);
        assert!(matches!(walk.current_step(), KvmMapStep::AllocL2 { .. }));
        // Kernel allocates an L2 page at 0xBBBB_0000.
        walk.step_l2_allocated(0xBBBB_0000);
        assert!(matches!(walk.current_step(), KvmMapStep::ReadL2Pte { .. }));
        // L2 entry empty → AllocL3.
        walk.step_l2(0);
        assert!(matches!(walk.current_step(), KvmMapStep::AllocL3 { .. }));
        // Kernel allocates an L3 page → walker reads the target L3 PTE.
        walk.step_l3_allocated(0xCCCC_0000);
        assert!(matches!(walk.current_step(), KvmMapStep::ReadL3Pte { .. }));
        // L3 PTE is zero (fresh table) → walker requests backing.
        walk.step_l3(0);
        assert_eq!(walk.current_step(), KvmMapStep::WantBacking);
        // Kernel supplies a backing paddr; walker emits WritePagePte.
        walk.supply_backing(PhysAddr::new(0x4020_0000));
        let pte = match walk.current_step() {
            KvmMapStep::WritePagePte { pte_paddr, entry } => {
                assert_eq!(pte_paddr.as_u64(), 0xCCCC_0000); // L3 index = 0
                entry
            }
            other => panic!("expected WritePagePte, got {:?}", other),
        };
        assert!(pte.is_uxn() && pte.is_pxn() && pte.ap() == AP_RW_EL1);
        assert_eq!(pte.output_addr().as_u64(), 0x4020_0000);
        walk.step_pte_written();
        assert_eq!(walk.current_step(), KvmMapStep::Done);
    }

    #[test]
    fn map_walk_reuses_existing_l1_l2_l3_when_present() {
        let l1_paddr = 0xAAAA_0000;
        let l2_paddr = 0xBBBB_0000;
        let l3_paddr = 0xCCCC_0000;
        let mut walk = KvmMapWalk::start(KernelVa::new(KVM_POOL_BASE), 1, l1_paddr);
        // Populated L1 entry (table → l2_paddr).
        walk.step_l1(PageTableEntry::new_table(PhysAddr::new(l2_paddr)).raw());
        assert!(matches!(walk.current_step(), KvmMapStep::ReadL2Pte { .. }));
        // Populated L2 entry (table → l3_paddr) → walker reads target L3 PTE.
        walk.step_l2(PageTableEntry::new_table(PhysAddr::new(l3_paddr)).raw());
        assert!(matches!(walk.current_step(), KvmMapStep::ReadL3Pte { .. }));
        // L3 PTE invalid → request backing → write.
        walk.step_l3(0);
        assert_eq!(walk.current_step(), KvmMapStep::WantBacking);
        walk.supply_backing(PhysAddr::new(0x4020_0000));
        assert!(matches!(walk.current_step(), KvmMapStep::WritePagePte { .. }));
        walk.step_pte_written();
        assert_eq!(walk.current_step(), KvmMapStep::Done);
    }

    #[test]
    fn map_walk_fault_when_target_l3_already_valid() {
        // Lockdown for the bug Codex flagged: alloc must read the
        // target L3 PTE and reject if a live mapping exists. Tests
        // the freelist/page-table-drift scenario where the freelist
        // says "this VA is free" but the page table still has an
        // entry for it — alloc must fault loudly, not silently
        // overwrite (which would alias / leak the previous
        // backing frame).
        let l1_paddr = 0xAAAA_0000;
        let l2_paddr = 0xBBBB_0000;
        let l3_paddr = 0xCCCC_0000;
        let mut walk = KvmMapWalk::start(KernelVa::new(KVM_POOL_BASE), 1, l1_paddr);
        walk.step_l1(PageTableEntry::new_table(PhysAddr::new(l2_paddr)).raw());
        walk.step_l2(PageTableEntry::new_table(PhysAddr::new(l3_paddr)).raw());
        // The L3 slot already holds a valid kernel-page entry. This
        // should never happen with a healthy freelist; alloc must
        // fault rather than overwrite.
        let stale_entry = build_kernel_page(PhysAddr::new(0x4020_5000)).raw();
        walk.step_l3(stale_entry);
        assert_eq!(walk.current_step(), KvmMapStep::Fault);
    }

    #[test]
    fn map_walk_fault_on_block_in_l1_or_l2() {
        let mut walk = KvmMapWalk::start(KernelVa::new(KVM_POOL_BASE), 1, 0xAAAA_0000);
        let block = PageTableEntry::new_block(PhysAddr::new(0x4000_0000), MAIR_NORMAL, AP_RW_EL1, SH_INNER).raw();
        walk.step_l1(block);
        assert_eq!(walk.current_step(), KvmMapStep::Fault);

        let mut walk = KvmMapWalk::start(KernelVa::new(KVM_POOL_BASE), 1, 0xAAAA_0000);
        walk.step_l1(PageTableEntry::new_table(PhysAddr::new(0xBBBB_0000)).raw());
        walk.step_l2(block);
        assert_eq!(walk.current_step(), KvmMapStep::Fault);
    }

    #[test]
    fn map_walk_multiple_pages_within_one_l3_skips_table_rewalks_but_still_reads_l3() {
        let l1_paddr = 0xAAAA_0000;
        let l2_paddr = 0xBBBB_0000;
        let l3_paddr = 0xCCCC_0000;
        // Two pages in same L3 (consecutive within the first L3 table).
        let mut walk = KvmMapWalk::start(KernelVa::new(KVM_POOL_BASE), 2, l1_paddr);
        // Page 0: walk L1, L2, read L3, get backing, write L3.
        walk.step_l1(PageTableEntry::new_table(PhysAddr::new(l2_paddr)).raw());
        walk.step_l2(PageTableEntry::new_table(PhysAddr::new(l3_paddr)).raw());
        assert!(matches!(walk.current_step(), KvmMapStep::ReadL3Pte { .. }));
        walk.step_l3(0);
        assert_eq!(walk.current_step(), KvmMapStep::WantBacking);
        walk.supply_backing(PhysAddr::new(0x4000_0000));
        let pte0 = match walk.current_step() {
            KvmMapStep::WritePagePte { pte_paddr, .. } => pte_paddr.as_u64(),
            other => panic!("expected WritePagePte for page 0, got {:?}", other),
        };
        // Advance — same L1/L2 → walker should jump straight to ReadL3Pte
        // for the next slot (must still verify it's invalid before writing).
        walk.step_pte_written();
        assert!(matches!(walk.current_step(), KvmMapStep::ReadL3Pte { .. }));
        walk.step_l3(0);
        assert_eq!(walk.current_step(), KvmMapStep::WantBacking);
        walk.supply_backing(PhysAddr::new(0x4001_0000));
        let pte1 = match walk.current_step() {
            KvmMapStep::WritePagePte { pte_paddr, .. } => pte_paddr.as_u64(),
            other => panic!("expected WritePagePte for page 1, got {:?}", other),
        };
        // PTE addresses are 8 bytes apart (consecutive L3 entries).
        assert_eq!(pte1 - pte0, 8);
        walk.step_pte_written();
        assert_eq!(walk.current_step(), KvmMapStep::Done);
    }

    // -----------------------------------------------------------------------
    // KvmFreeWalk
    // -----------------------------------------------------------------------

    #[test]
    fn free_walk_zero_pages_done_immediately() {
        let walk = KvmFreeWalk::start(KernelVa::new(KVM_POOL_BASE), 0, 0xCAFE_0000);
        assert_eq!(walk.current_step(), KvmFreeStep::Done);
    }

    #[test]
    fn free_walk_extracts_backing_paddr_and_clears_pte() {
        let l1_paddr = 0xAAAA_0000;
        let l2_paddr = 0xBBBB_0000;
        let l3_paddr = 0xCCCC_0000;
        let backing = PhysAddr::new(0x4020_5000);
        let mut walk = KvmFreeWalk::start(KernelVa::new(KVM_POOL_BASE), 1, l1_paddr);
        walk.step_l1(PageTableEntry::new_table(PhysAddr::new(l2_paddr)).raw());
        walk.step_l2(PageTableEntry::new_table(PhysAddr::new(l3_paddr)).raw());
        // L3 → page entry pointing at `backing`.
        walk.step_l3(build_kernel_page(backing).raw());
        match walk.current_step() {
            KvmFreeStep::ClearPte { pte_paddr, backing: b } => {
                assert_eq!(pte_paddr.as_u64(), l3_paddr); // L3 index = 0
                assert_eq!(b.as_u64(), backing.as_u64());
            }
            other => panic!("expected ClearPte, got {:?}", other),
        }
        walk.step_pte_cleared();
        assert_eq!(walk.current_step(), KvmFreeStep::Done);
    }

    #[test]
    fn free_walk_fault_on_invalid_l3_pte() {
        let mut walk = KvmFreeWalk::start(KernelVa::new(KVM_POOL_BASE), 1, 0xAAAA_0000);
        walk.step_l1(PageTableEntry::new_table(PhysAddr::new(0xBBBB_0000)).raw());
        walk.step_l2(PageTableEntry::new_table(PhysAddr::new(0xCCCC_0000)).raw());
        // L3 entry is invalid (zero) — accounting drift.
        walk.step_l3(0);
        assert_eq!(walk.current_step(), KvmFreeStep::Fault);
    }

    #[test]
    fn free_walk_fault_on_missing_l1_l2_table() {
        let mut walk = KvmFreeWalk::start(KernelVa::new(KVM_POOL_BASE), 1, 0xAAAA_0000);
        // L1 entry is invalid — accounting drift.
        walk.step_l1(0);
        assert_eq!(walk.current_step(), KvmFreeStep::Fault);
    }

    #[test]
    fn pool_constants_consistent() {
        assert_eq!(KVM_L0_INDEX, ((KVM_POOL_BASE >> 39) & 0x1FF) as usize);
        assert_eq!(KVM_POOL_END - KVM_POOL_BASE, KVM_POOL_VA_SPAN);
    }
}
