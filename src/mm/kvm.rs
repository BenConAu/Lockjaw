//! Kernel virtual-address (KVM) allocator — kernel side.
//!
//! Hands out N-page virtually-contiguous ranges from the higher-half
//! KVM pool (`KVM_POOL_BASE..KVM_POOL_END`), backed by N
//! independently-allocated physical frames stitched into the TTBR1
//! tree at `KERNEL_L0[KVM_L0_INDEX]`.
//!
//! All policy lives in `lockjaw_types::kvm` (free-list, walk
//! state machines, PTE construction). This module only does:
//! - the `unsafe` PTE reads/writes
//! - page allocation for backing frames (L2/L3 page-table pages
//!   are pre-allocated at bootstrap by `kvm_init`; runtime
//!   walks read pre-populated parent PTEs, never grow the tree)
//! - TLB invalidation around mappings
//! - boot-time L0 install + full L1/L2/L3 forest pre-allocation
//!
//! GKL serializes all kernel state, so the allocator's state lives
//! in a single `UnsafeCell`-wrapped singleton without internal
//! locking — same idiom as `FrameAllocator` in `page_alloc.rs`.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use lockjaw_types::addr::{KernelVa, PAGE_SIZE};
use lockjaw_types::kvm::{
    KvmFreeList, KvmFreeStep, KvmFreeWalk, KvmMapStep, KvmMapWalk,
    BOOTSTRAP_CONTIGUOUS_RESERVE, KVM_BOOTSTRAP_TABLE_PAGES,
    KVM_L0_INDEX, KVM_L2_TABLES_COUNT, KVM_L3_TABLES_COUNT,
};
use lockjaw_types::page_table::{PageTable, PageTableEntry};

use crate::mm::addr::{PhysAddr, PhysPage};
use crate::mm::kernel_ptr::KernelMut;
use crate::mm::page_alloc;

// ---------------------------------------------------------------------------
// KvmAllocator singleton
// ---------------------------------------------------------------------------

/// The kernel's KVM allocator. Owns the pure free-list and the
/// paddr of the L1 table installed at `KERNEL_L0[KVM_L0_INDEX]`.
struct KvmAllocator {
    list: UnsafeCell<KvmFreeList>,
    /// Paddr of the L1 table for the KVM pool. Set once by
    /// `kvm_init`. The pure walkers receive this as a u64 input;
    /// the kernel pulls it from here on every alloc/free call.
    l1_paddr: UnsafeCell<u64>,
    /// True once `kvm_init` has installed the L1 table. Allocations
    /// before init panic — there is no sensible fallback.
    initialized: AtomicBool,
}

/// SAFETY: single-core kernel + GKL serializes access. Replace with
/// proper synchronization when SMP lands.
unsafe impl Sync for KvmAllocator {}

impl KvmAllocator {
    const fn new() -> Self {
        Self {
            list: UnsafeCell::new(KvmFreeList::new()),
            l1_paddr: UnsafeCell::new(0),
            initialized: AtomicBool::new(false),
        }
    }
}

static ALLOCATOR: KvmAllocator = KvmAllocator::new();

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A KVM allocation whose backing physical frames are owned by the
/// allocator. Returned by `alloc_kernel_pages` and consumed by
/// `free_kernel_pages`. Free returns the backing frames to
/// `page_alloc` and the VA range to the KVM free list.
#[derive(Clone, Copy, Debug)]
pub struct OwnedKvmRange {
    pub kva: KernelVa,
    pub pages: usize,
}

/// A KVM mapping over caller-supplied physical frames. Returned by
/// `map_existing` and consumed by `unmap_existing`. Unmap returns
/// the VA range to the KVM free list but does NOT free the backing
/// frames — the caller (typically `consume_pageset_apply`) owns those.
///
/// Distinct from `OwnedKvmRange` so the two free paths can never be
/// crossed: passing an `OwnedKvmRange` to `unmap_existing` (or vice
/// versa) is a compile error.
#[derive(Clone, Copy, Debug)]
pub struct MappedKvmRange {
    pub kva: KernelVa,
    pub pages: usize,
}

/// RAII guard that frees an `OwnedKvmRange` on drop unless explicitly
/// taken. Mirrors the `HeaderPageGuard` pattern: callers wrap
/// allocations in a guard, do downstream work that may fail, then
/// call `take()` on success to claim ownership without freeing.
pub struct OwnedKvmRangeGuard {
    range: Option<OwnedKvmRange>,
}

impl OwnedKvmRangeGuard {
    pub fn new(range: OwnedKvmRange) -> Self {
        Self { range: Some(range) }
    }

    /// Claim the range, preventing it from being freed on drop.
    pub fn take(&mut self) -> OwnedKvmRange {
        self.range.take().unwrap_or_else(|| panic!("OwnedKvmRangeGuard already taken"))
    }

    /// The underlying KVA without releasing ownership.
    pub fn kva(&self) -> KernelVa {
        self.range.expect("OwnedKvmRangeGuard already taken").kva
    }
}

impl Drop for OwnedKvmRangeGuard {
    fn drop(&mut self) {
        if let Some(range) = self.range.take() {
            // SAFETY: range came from alloc_kernel_pages and was not
            // taken — no live references into it remain (the caller
            // dropped before claiming ownership).
            unsafe { free_kernel_pages(range); }
        }
    }
}

/// RAII guard for a `MappedKvmRange`. Drop calls `unmap_existing`
/// (clears PTEs + TLBI; does not touch backing frames). `take()`
/// transfers ownership to the caller.
pub struct MappedKvmRangeGuard {
    range: Option<MappedKvmRange>,
}

impl MappedKvmRangeGuard {
    pub fn new(range: MappedKvmRange) -> Self {
        Self { range: Some(range) }
    }

    pub fn take(&mut self) -> MappedKvmRange {
        self.range.take().unwrap_or_else(|| panic!("MappedKvmRangeGuard already taken"))
    }

    pub fn kva(&self) -> KernelVa {
        self.range.expect("MappedKvmRangeGuard already taken").kva
    }
}

impl Drop for MappedKvmRangeGuard {
    fn drop(&mut self) {
        if let Some(range) = self.range.take() {
            // SAFETY: range came from map_existing and was not taken.
            unsafe { unmap_existing(range); }
        }
    }
}

/// Errors from the KVM allocator.
#[derive(Debug)]
pub enum KvmError {
    /// Free list could not satisfy the VA request.
    OutOfVirtualMemory,
    /// `page_alloc::alloc_page` failed for a backing frame or for a
    /// page-table page (L2/L3) the walker wanted to allocate.
    OutOfPhysicalFrames,
}

// ---------------------------------------------------------------------------
// PTE access helpers
// ---------------------------------------------------------------------------

#[inline]
unsafe fn read_pte(pte_paddr: PhysAddr) -> u64 {
    // SAFETY: caller passes a valid page-table-page paddr (from
    // KERNEL_L0 walks); the linear higher-half map is set up by
    // enable_higher_half().
    let p = unsafe { KernelMut::<u64>::from_paddr(pte_paddr) };
    core::ptr::read_volatile(p.as_ptr())
}

#[inline]
unsafe fn write_pte(pte_paddr: PhysAddr, raw: u64) {
    // SAFETY: same as read_pte.
    let mut p = unsafe { KernelMut::<u64>::from_paddr(pte_paddr) };
    core::ptr::write_volatile(p.as_mut_ptr(), raw);
}

#[inline]
unsafe fn tlbi_vae1(kva: KernelVa) {
    // tlbi vae1is operand format: bits[55:12] = VA[55:12], bits[11:0]
    // are ASID (we use 0). Surgical per-page invalidation in the
    // inner-shareable domain.
    let operand = kva.as_u64() >> 12;
    asm!("tlbi vae1is, {}", in(reg) operand, options(nostack, preserves_flags));
}

// ---------------------------------------------------------------------------
// kvm_init
// ---------------------------------------------------------------------------

/// One-shot install of the KVM L1 table at `KERNEL_L0[KVM_L0_INDEX]`,
/// AND pre-allocation of the full L2/L3 page-table forest covering
/// `KVM_POOL_USABLE_SIZE`. After this call, the page-table tree is
/// fully populated for the working pool; `alloc_kernel_pages` /
/// `map_existing` walks pre-existing parent PTEs and only ever
/// allocates *backing* frames — no metadata growth at runtime.
///
/// Bootstrap budget: `KVM_BOOTSTRAP_TABLE_PAGES` (514 today) page-
/// table pages. The buddy rounds up to `BOOTSTRAP_CONTIGUOUS_RESERVE`
/// (1024 today) as one contiguous block; the extra padding stays
/// reserved (inert) as part of the block.
///
/// Must be called after `enable_higher_half` (TTBR1 must be active)
/// and after `page_alloc::init_with_gap` (we allocate the table
/// pages), and before any caller of `alloc_kernel_pages`.
///
/// # Safety
/// Must be called exactly once. Race-free under GKL.
pub unsafe fn kvm_init() {
    if ALLOCATOR.initialized.load(Ordering::Acquire) {
        panic!("kvm_init called twice");
    }

    // Acquire `KVM_BOOTSTRAP_TABLE_PAGES` of contiguous physical
    // memory for the L1 + L2 + L3 forest. The buddy rounds up to
    // the next power of two (`BOOTSTRAP_CONTIGUOUS_RESERVE`), and
    // the extra padding stays reserved as part of the block.
    //
    // Fallback: if `alloc_pages_contiguous` returns `None` (tiny
    // QEMU configs / exhausted buddy at bootstrap), panic with a
    // descriptive message. The contiguous path is the only one
    // exercised by current targets; the per-page fallback is
    // tracked as tech-debt to land before NK2 enlarges the budget.
    let first_page = page_alloc::alloc_pages_contiguous(
        KVM_BOOTSTRAP_TABLE_PAGES,
    )
    .unwrap_or_else(|| {
        panic!(
            "kvm_init: alloc_pages_contiguous failed for bootstrap KVM \
             page-table forest (bootstrap memory pressure)"
        )
    });
    let base_paddr = first_page.start_addr();

    // Slice the contiguous run: page 0 → L1, pages 1..1+L2_COUNT → L2s,
    // remaining → L3s. For 1 GiB usable: 1 + 1 + 512 = 514.
    let l1_paddr = base_paddr;
    let l2_base = PhysAddr::new(base_paddr.as_u64() + PAGE_SIZE);
    let l3_base = PhysAddr::new(
        base_paddr.as_u64() + ((1 + KVM_L2_TABLES_COUNT) as u64) * PAGE_SIZE,
    );

    // Zero every table page so residual bytes can't masquerade as
    // valid PTEs.
    for i in 0..KVM_BOOTSTRAP_TABLE_PAGES {
        let p = PhysAddr::new(base_paddr.as_u64() + (i as u64) * PAGE_SIZE);
        page_alloc::zero_page(p);
    }

    // Stitch parent PTEs. For each L1 entry, point at its L2 table;
    // for each L2 entry, point at its L3 table. L3 PTEs themselves
    // stay zero until alloc_kernel_pages / map_existing writes
    // backing-frame entries at runtime.
    for l2_idx in 0..KVM_L2_TABLES_COUNT {
        let l2_paddr = PhysAddr::new(
            l2_base.as_u64() + (l2_idx as u64) * PAGE_SIZE,
        );
        let l1_pte_paddr = PhysAddr::new(
            l1_paddr.as_u64() + (l2_idx as u64) * 8,
        );
        let l1_entry = PageTableEntry::new_table(
            lockjaw_types::addr::PhysAddr::new(l2_paddr.as_u64()),
        );
        write_pte(l1_pte_paddr, l1_entry.raw());

        // Each L2 covers up to 512 L3 PTEs.
        for sub in 0..512usize {
            let global_l3_idx = l2_idx * 512 + sub;
            if global_l3_idx >= KVM_L3_TABLES_COUNT {
                break;
            }
            let l3_paddr = PhysAddr::new(
                l3_base.as_u64() + (global_l3_idx as u64) * PAGE_SIZE,
            );
            let l2_pte_paddr = PhysAddr::new(
                l2_paddr.as_u64() + (sub as u64) * 8,
            );
            let l2_entry = PageTableEntry::new_table(
                lockjaw_types::addr::PhysAddr::new(l3_paddr.as_u64()),
            );
            write_pte(l2_pte_paddr, l2_entry.raw());
        }
    }

    // Install KERNEL_L0[KVM_L0_INDEX] -> L1 table.
    let l0_paddr = crate::arch::aarch64::mmu::kernel_l0_paddr();
    {
        let mut l0 = unsafe { KernelMut::<PageTable>::from_paddr(l0_paddr) };
        l0.get_mut().entries[KVM_L0_INDEX] = PageTableEntry::new_table(
            lockjaw_types::addr::PhysAddr::new(l1_paddr.as_u64()),
        );
    }

    *ALLOCATOR.l1_paddr.get() = l1_paddr.as_u64();
    ALLOCATOR.initialized.store(true, Ordering::Release);

    // Sweep the inner-shareable TLBs — clear any stale entries that
    // might have been speculatively walked through the previously-
    // empty L0 slot.
    asm!("dsb ish", options(nostack, preserves_flags));
    asm!("tlbi vmalle1is", options(nostack, preserves_flags));
    asm!("dsb ish", options(nostack, preserves_flags));
    asm!("isb", options(nostack, preserves_flags));

    crate::kprintln!(
        "  KVM allocator initialized: ",
        KVM_BOOTSTRAP_TABLE_PAGES,
        " page-table pages pre-allocated (",
        BOOTSTRAP_CONTIGUOUS_RESERVE,
        " pages reserved as a contiguous block) at L0[",
        KVM_L0_INDEX,
        "] -> L1 paddr ",
        crate::print::Hex(l1_paddr.as_u64()),
    );
}

// ---------------------------------------------------------------------------
// alloc_kernel_pages
// ---------------------------------------------------------------------------

/// Allocate `pages` virtually-contiguous pages from the KVM pool,
/// backed by `pages` independently-allocated physical frames.
///
/// On success the returned `OwnedKvmRange` owns:
/// - the VA reservation in the free list
/// - the per-page backing frames (PTEs in the KVM tree point at them)
/// - any L2/L3 page-table pages newly allocated to extend the tree
///
/// On failure (OOM either for VA or for any backing/table frame):
/// - the partial PTE writes are torn down via `free_kernel_pages` on
///   the partial range — backing frames already allocated are
///   returned to the page allocator
/// - the VA reservation is returned to the free list
/// - L2/L3 page-table pages allocated mid-walk are NOT reclaimed
///   (deferred per the plan; bounded waste, doesn't violate
///   correctness — the tree is consistent, just keeps an empty
///   sub-tree branch around)
pub fn alloc_kernel_pages(pages: usize) -> Result<OwnedKvmRange, KvmError> {
    if !ALLOCATOR.initialized.load(Ordering::Acquire) {
        panic!("alloc_kernel_pages before kvm_init");
    }
    if pages == 0 {
        return Err(KvmError::OutOfVirtualMemory);
    }

    let l1_paddr = unsafe { *ALLOCATOR.l1_paddr.get() };
    let list = unsafe { &mut *ALLOCATOR.list.get() };

    let kva = list.try_alloc(pages)
        .map_err(|_| KvmError::OutOfVirtualMemory)?;

    // Drive the map walker. The supplier allocates a fresh frame from
    // page_alloc on every WantBacking step. On OOM during the walk,
    // roll back via free_kernel_pages on the partial range.
    let mut walk = KvmMapWalk::start(kva, pages, l1_paddr);
    let mut pages_done: usize = 0;
    let result = drive_map_walk(&mut walk, &mut pages_done, |_| {
        let frame = page_alloc::alloc_page().ok_or(KvmError::OutOfPhysicalFrames)?;
        Ok(lockjaw_types::addr::PhysAddr::new(frame.start_addr().as_u64()))
    });

    if let Err(e) = result {
        // Roll back: free what we already mapped (releases backing
        // frames + clears PTEs + TLBI), then return the VA range.
        if pages_done > 0 {
            // SAFETY: rollback path. Free the partial range. Backing
            // paddrs are recovered by KvmFreeWalk reading the L3 PTEs
            // we just wrote.
            unsafe { tear_down_range(kva, pages_done, /*dealloc_backing=*/ true); }
        } else {
            // No PTEs written — only the VA reservation needs to be
            // returned.
            let _ = list.free(kva, pages);
        }
        return Err(e);
    }

    tlbi_range(kva, pages);

    Ok(OwnedKvmRange { kva, pages })
}

/// Map a single caller-supplied physical page into the KVM pool.
/// The frame's lifecycle stays with the caller — `unmap_existing`
/// clears the PTE but does NOT return the frame to `page_alloc`.
///
/// Takes a `PhysPage` (page-typed) rather than raw `PhysAddr` so the
/// 4 KB alignment precondition is enforced by the type system.
/// `PageTableEntry::new_page` masks the low 12 bits of the supplied
/// paddr; passing an unaligned address would silently mis-map to the
/// containing page. Requiring `PhysPage` makes the misuse
/// unrepresentable.
///
/// Used by the donated-PageSet → kernel-object path: the user's
/// donated data page becomes the backing for the new kernel object,
/// addressed through KVA.
///
/// Currently 1-page only. Multi-page `map_existing` would take a
/// `&[PhysPage]` slice; not yet needed.
pub fn map_existing(page: PhysPage) -> Result<MappedKvmRange, KvmError> {
    if !ALLOCATOR.initialized.load(Ordering::Acquire) {
        panic!("map_existing before kvm_init");
    }

    let l1_paddr = unsafe { *ALLOCATOR.l1_paddr.get() };
    let list = unsafe { &mut *ALLOCATOR.list.get() };

    let kva = list.try_alloc(1).map_err(|_| KvmError::OutOfVirtualMemory)?;

    // Drive the walker; supplier returns the caller's page-aligned
    // paddr at the single WantBacking step.
    let backing = lockjaw_types::addr::PhysAddr::new(page.start_addr().as_u64());
    let mut walk = KvmMapWalk::start(kva, 1, l1_paddr);
    let mut pages_done: usize = 0;
    let result = drive_map_walk(&mut walk, &mut pages_done, |_| Ok(backing));

    if let Err(e) = result {
        // L2/L3 may have failed to allocate. The caller-supplied
        // backing was never claimed by us, so rollback only needs to
        // clear any PTE we wrote (none, since pages_done==0 unless
        // WritePagePte ran) and return the VA range.
        if pages_done > 0 {
            // SAFETY: rollback path, but with dealloc_backing=false —
            // the backing came from the caller and isn't ours to free.
            unsafe { tear_down_range(kva, pages_done, /*dealloc_backing=*/ false); }
        } else {
            let _ = list.free(kva, 1);
        }
        return Err(e);
    }

    tlbi_range(kva, 1);

    Ok(MappedKvmRange { kva, pages: 1 })
}

/// Drive the `KvmMapWalk` to completion. The `next_backing` closure
/// is invoked on every `WantBacking` step with the page index (0..N)
/// and must return the paddr to install at the next L3 PTE.
///
/// `pages_done` tracks how many pages have completed `WritePagePte`
/// so the caller can roll back exactly that many pages.
fn drive_map_walk<F>(
    walk: &mut KvmMapWalk,
    pages_done: &mut usize,
    mut next_backing: F,
) -> Result<(), KvmError>
where
    F: FnMut(usize) -> Result<lockjaw_types::addr::PhysAddr, KvmError>,
{
    loop {
        match walk.current_step() {
            KvmMapStep::ReadL1Pte { l1_pte_paddr } => {
                let raw = unsafe { read_pte(l1_pte_paddr) };
                walk.step_l1(raw);
            }
            KvmMapStep::AllocL2 { parent_pte_paddr } => {
                let l2 = match page_alloc::alloc_page() {
                    Some(p) => p,
                    None => return Err(KvmError::OutOfPhysicalFrames),
                };
                let l2_paddr = l2.start_addr();
                page_alloc::zero_page(l2_paddr);
                let entry = PageTableEntry::new_table(
                    lockjaw_types::addr::PhysAddr::new(l2_paddr.as_u64()),
                );
                unsafe {
                    write_pte(parent_pte_paddr, entry.raw());
                    asm!("dsb ish", options(nostack, preserves_flags));
                }
                walk.step_l2_allocated(l2_paddr.as_u64());
            }
            KvmMapStep::ReadL2Pte { l2_pte_paddr } => {
                let raw = unsafe { read_pte(l2_pte_paddr) };
                walk.step_l2(raw);
            }
            KvmMapStep::AllocL3 { parent_pte_paddr } => {
                let l3 = match page_alloc::alloc_page() {
                    Some(p) => p,
                    None => return Err(KvmError::OutOfPhysicalFrames),
                };
                let l3_paddr = l3.start_addr();
                page_alloc::zero_page(l3_paddr);
                let entry = PageTableEntry::new_table(
                    lockjaw_types::addr::PhysAddr::new(l3_paddr.as_u64()),
                );
                unsafe {
                    write_pte(parent_pte_paddr, entry.raw());
                    asm!("dsb ish", options(nostack, preserves_flags));
                }
                walk.step_l3_allocated(l3_paddr.as_u64());
            }
            KvmMapStep::ReadL3Pte { l3_pte_paddr } => {
                let raw = unsafe { read_pte(l3_pte_paddr) };
                walk.step_l3(raw);
            }
            KvmMapStep::WantBacking => {
                let paddr = next_backing(*pages_done)?;
                walk.supply_backing(paddr);
            }
            KvmMapStep::WritePagePte { pte_paddr, entry } => {
                unsafe { write_pte(pte_paddr, entry.raw()); }
                *pages_done += 1;
                walk.step_pte_written();
            }
            KvmMapStep::Done => return Ok(()),
            KvmMapStep::Fault => {
                // KvmMapWalk faults are kernel bugs (block descriptor
                // where a table is expected, or a live L3 entry for a
                // VA the freelist said was free). Panic loudly rather
                // than silently corrupt the page tree.
                panic!("KvmMapWalk fault during map");
            }
        }
    }
}

/// Per-page TLB invalidation across `pages` starting at `kva`.
/// Standard sequence: `dsb ish` to publish PTE writes, surgical
/// `tlbi vae1is` per VA, `dsb ish` + `isb` to ensure invalidation
/// completes before any subsequent access.
#[inline]
fn tlbi_range(kva: KernelVa, pages: usize) {
    // SAFETY: pure TLB-management asm; no memory writes.
    unsafe {
        asm!("dsb ish", options(nostack, preserves_flags));
        for i in 0..pages {
            tlbi_vae1(kva.add_pages(i));
        }
        asm!("dsb ish", options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

// ---------------------------------------------------------------------------
// free_kernel_pages
// ---------------------------------------------------------------------------

/// Free a previously-allocated KVM range. Tears down the per-page
/// PTEs (with TLBI), returns each backing frame to the page
/// allocator, and returns the VA range to the free list. L2/L3
/// page-table pages are not reclaimed in v1 (see module docs).
///
/// # Safety
/// `range` must come from a prior `alloc_kernel_pages` and must not
/// have been freed already. The caller must hold no live references
/// (`KernelRef`/`KernelMut`) into the range.
pub unsafe fn free_kernel_pages(range: OwnedKvmRange) {
    if !ALLOCATOR.initialized.load(Ordering::Acquire) {
        panic!("free_kernel_pages before kvm_init");
    }
    tear_down_range(range.kva, range.pages, /*dealloc_backing=*/ true);
}

/// Unmap a `MappedKvmRange`: clears the PTEs (with TLBI) and returns
/// the VA range to the free list, but does NOT return the backing
/// frames to `page_alloc` — the caller-supplied frames belong to
/// whatever donated them.
///
/// # Safety
/// `range` must come from a prior `map_existing` and must not have
/// been unmapped already. No live references into the range.
pub unsafe fn unmap_existing(range: MappedKvmRange) {
    if !ALLOCATOR.initialized.load(Ordering::Acquire) {
        panic!("unmap_existing before kvm_init");
    }
    tear_down_range(range.kva, range.pages, /*dealloc_backing=*/ false);
}

/// Shared tear-down: walk the KVM tree clearing PTEs across `pages`
/// starting at `kva`, run the canonical TLBI sequence, optionally
/// dealloc the backing frames, and return the VA range to the free
/// list.
///
/// `dealloc_backing` controls whether the backing physical frames
/// (recovered from the L3 PTEs as they're cleared) get returned to
/// `page_alloc`:
/// - `true` for `free_kernel_pages` (alloc_kernel_pages owned them)
/// - `false` for `unmap_existing` (caller-supplied frames stay with
///   their original owner)
///
/// Used by both the public free/unmap entries and the alloc/map
/// rollback paths.
unsafe fn tear_down_range(kva: KernelVa, pages: usize, dealloc_backing: bool) {
    if pages == 0 {
        return;
    }
    let l1_paddr = *ALLOCATOR.l1_paddr.get();
    let list = &mut *ALLOCATOR.list.get();

    let mut walk = KvmFreeWalk::start(kva, pages, l1_paddr);
    // Capture backing paddrs as the walker yields them; defer the
    // dealloc until after the TLBI sequence so we don't free a frame
    // whose translation might still be cached.
    let mut to_free_head: usize = 0;
    let mut to_free: [PhysPage; 64] = [PhysPage::containing(PhysAddr::new(0)); 64];

    loop {
        match walk.current_step() {
            KvmFreeStep::ReadL1Pte { l1_pte_paddr } => {
                let raw = read_pte(l1_pte_paddr);
                walk.step_l1(raw);
            }
            KvmFreeStep::ReadL2Pte { l2_pte_paddr } => {
                let raw = read_pte(l2_pte_paddr);
                walk.step_l2(raw);
            }
            KvmFreeStep::ReadL3Pte { l3_pte_paddr } => {
                let raw = read_pte(l3_pte_paddr);
                walk.step_l3(raw);
            }
            KvmFreeStep::ClearPte { pte_paddr, backing } => {
                write_pte(pte_paddr, 0);
                if dealloc_backing {
                    // Record the backing frame for post-TLBI dealloc.
                    let phys = PhysAddr::new(backing.as_u64());
                    if to_free_head < to_free.len() {
                        to_free[to_free_head] = PhysPage::containing(phys);
                        to_free_head += 1;
                    } else {
                        // Should not happen for current
                        // MAX_PRACTICAL_PAGES_PER_SET (33 backing
                        // frames per header alloc), but if a future
                        // caller exceeds 64 we fall back to in-loop
                        // dealloc. The TLBI sequence below still
                        // runs over every freed VA so correctness is
                        // preserved; only the "drain after TLBI"
                        // optimization is lost for overflow pages.
                        page_alloc::dealloc_page(PhysPage::containing(phys));
                    }
                }
                walk.step_pte_cleared();
            }
            KvmFreeStep::Done => break,
            KvmFreeStep::Fault => {
                // Walk fault during free indicates accounting drift
                // between the free list and the actual page tables.
                // Panic loudly rather than continue (we'd be freeing
                // wrong frames).
                panic!("KvmFreeWalk fault during tear_down_range");
            }
        }
    }

    // Per-page TLBI to invalidate the now-cleared range. Standard
    // sequence: dsb ish before to publish PTE writes, the TLBI per
    // VA, dsb ish + isb after to ensure invalidation completes
    // before any subsequent PTE write or instruction fetch.
    asm!("dsb ish", options(nostack, preserves_flags));
    for i in 0..pages {
        tlbi_vae1(kva.add_pages(i));
    }
    asm!("dsb ish", options(nostack, preserves_flags));
    asm!("isb", options(nostack, preserves_flags));

    // Now safe to drain the captured backing frames.
    for i in 0..to_free_head {
        page_alloc::dealloc_page(to_free[i]);
    }

    // Return the VA range to the free list.
    let _ = list.free(kva, pages);
}

// ---------------------------------------------------------------------------
// Boot self-test
// ---------------------------------------------------------------------------

/// One-shot diagnostic: allocates a 33-page KVM range, writes a
/// distinct sentinel into each page (via the KVA), reads it back
/// to prove virtual contiguity holds across non-contiguous physical
/// frames, then frees and asserts the page allocator's free count
/// is restored.
///
/// Called once at boot from `main.rs` immediately after `kvm_init`.
/// Panics on any unexpected condition.
///
/// # Safety
/// Must be called once after `kvm_init` and before any other KVM
/// allocation, so the assertion that "all pages came from
/// freshly-allocated frames" is meaningful.
pub unsafe fn boot_self_test() {
    const N: usize = 33;
    // Pre-fragment the page allocator: alloc 2*N single pages, then
    // free every other one. The remaining holes are guaranteed to be
    // non-contiguous in physical memory, so subsequent alloc_page
    // calls (driven by alloc_kernel_pages) will return scattered
    // frames. This forces the self-test to actually exercise the
    // stitched backing case rather than getting a contiguous run by
    // accident from a fresh allocator.
    let mut pre = [PhysPage::containing(PhysAddr::new(0)); 2 * N];
    for i in 0..2 * N {
        pre[i] = page_alloc::alloc_page().expect("kvm self-test: pre-frag alloc failed");
    }
    // Free even-indexed pages — leaves the odd-indexed ones held,
    // so the free list now contains N single-page regions
    // alternating with held pages. Subsequent allocs come from
    // those scattered single-page regions.
    for i in (0..2 * N).step_by(2) {
        page_alloc::dealloc_page(pre[i]);
    }

    let pre_free = page_alloc::free_count();

    let range = alloc_kernel_pages(N).expect("kvm self-test: alloc failed");
    assert_eq!(range.pages, N);

    // Capture the backing paddr for each page (via the L3 PTE) so
    // we can assert they are NOT a contiguous run — proves the
    // mapping stitched scattered frames into one VA.
    let l1_paddr = *ALLOCATOR.l1_paddr.get();
    let mut backing = [0u64; N];
    for i in 0..N {
        let kva = range.kva.add_pages(i);
        let (_l0, l1, l2, l3) = lockjaw_types::kvm::kvm_pool_indices(kva);
        let l1_pte = read_pte(PhysAddr::new(l1_paddr + (l1 as u64) * 8));
        let l2_paddr = PageTableEntry::from_raw(l1_pte).output_addr().as_u64();
        let l2_pte = read_pte(PhysAddr::new(l2_paddr + (l2 as u64) * 8));
        let l3_paddr = PageTableEntry::from_raw(l2_pte).output_addr().as_u64();
        let l3_pte = read_pte(PhysAddr::new(l3_paddr + (l3 as u64) * 8));
        backing[i] = PageTableEntry::from_raw(l3_pte).output_addr().as_u64();
    }
    // Hard assertion (was a warning): pre-fragmentation guarantees
    // at least one gap. If the backing is contiguous, the test
    // accidentally hit a contiguous run and isn't exercising the
    // stitched path it claims to verify.
    let mut seen_gap = false;
    for i in 1..N {
        if backing[i] != backing[i - 1] + crate::mm::addr::PAGE_SIZE {
            seen_gap = true;
            break;
        }
    }
    assert!(
        seen_gap,
        "kvm self-test: backing frames are contiguous despite \
         pre-fragmentation — the stitched-backing path was not exercised"
    );

    // Write a sentinel at offset 16 + i*PAGE_SIZE for each page,
    // then read it back. Crosses page boundaries inside the KVA —
    // proves virtual contiguity holds.
    for i in 0..N {
        let kva = range.kva.add_pages(i);
        let offset_kva = KernelVa::new(kva.as_u64() + 16);
        let mut slot = KernelMut::<u64>::from_kva(offset_kva);
        core::ptr::write_volatile(slot.as_mut_ptr(), 0xDEAD_BEEF_0000_0000 | (i as u64));
    }
    for i in 0..N {
        let kva = range.kva.add_pages(i);
        let offset_kva = KernelVa::new(kva.as_u64() + 16);
        let slot = crate::mm::kernel_ptr::KernelRef::<u64>::from_kva(offset_kva);
        let got = core::ptr::read_volatile(slot.as_ptr());
        let want = 0xDEAD_BEEF_0000_0000 | (i as u64);
        assert_eq!(got, want, "kvm self-test: sentinel mismatch at page {}", i);
    }

    free_kernel_pages(range);

    let post_free = page_alloc::free_count();
    // After NK1, the KVM tree is fully pre-allocated at boot — no
    // L2/L3 page-table growth happens during alloc_kernel_pages. The
    // only allocations the test causes are backing frames, all of
    // which it frees. Use direct equality (NOT saturating_sub) so an
    // over-release bug (post_free > pre_free) surfaces as a test
    // failure instead of getting clamped to 0.
    assert_eq!(
        pre_free,
        post_free,
        "kvm self-test: page-allocator free count drifted \
         (pre={}, post={})",
        pre_free,
        post_free,
    );

    // Cleanup: return the odd-indexed pre-fragmentation pages to
    // the allocator so the self-test leaves the heap in the same
    // shape it found it.
    for i in (1..2 * N).step_by(2) {
        page_alloc::dealloc_page(pre[i]);
    }

    crate::kprintln!("  KVM self-test OK");
}
