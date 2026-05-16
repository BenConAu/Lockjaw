use crate::mm::addr::{PhysAddr, PhysPage, PAGE_SIZE};
use crate::mm::kernel_ptr::KernelMut;
use crate::mm::kvm::{self, OwnedKvmRange, OwnedKvmRangeGuard};
use crate::mm::page_alloc;
use core::cell::UnsafeCell;
use lockjaw_types::addr::KernelVa;
use lockjaw_types::pageset_table::{
    header_pages_for, PageSetEntry, PageSetHeader, PageSetOrigin, PageSetTable,
    MAX_PRACTICAL_PAGES_PER_SET,
};

// ---------------------------------------------------------------------------
// PageSetTable singleton
// ---------------------------------------------------------------------------

/// The kernel's global PageSet tracking table.
/// Wraps the pure PageSetTable model from lockjaw-types with the actual
/// page allocator for allocation and deallocation.
///
/// Internal access uses raw pointers from UnsafeCell; the SAFETY
/// justification for `impl Sync` lives here rather than at every call site.
struct PageSetTableWrapper(UnsafeCell<PageSetTable>);

/// SAFETY: single-core kernel. Kernel entry masks IRQs before touching
/// kernel state; no concurrent access to the table is possible.
unsafe impl Sync for PageSetTableWrapper {}

impl PageSetTableWrapper {
    const fn new() -> Self {
        PageSetTableWrapper(UnsafeCell::new(PageSetTable::new()))
    }

    fn ptr(&self) -> *mut PageSetTable {
        self.0.get()
    }
}

static TABLE: PageSetTableWrapper = PageSetTableWrapper::new();

/// Insert an already-initialized header range into the global table.
/// Does NOT own the header range — the caller is responsible for
/// cleanup on failure (typically via OwnedKvmRangeGuard).
fn insert_into_table(count: usize, header_kva: KernelVa) -> Option<u64> {
    let entry = PageSetEntry { count, header_kva, origin: PageSetOrigin::Buddy };
    // SAFETY: single-core, IRQs masked — exclusive table access.
    unsafe { (*TABLE.ptr()).insert(entry).ok().map(|id| id as u64) }
}

/// Allocate a virtually-contiguous header range from the KVM pool
/// large enough to hold `count` page-address entries, initialize
/// the header in place, and insert into the global table. On
/// failure, the range is freed via the drop guard. Returns the
/// PageSet table slot ID.
fn alloc_and_insert_header(page_addrs: &[u64], count: usize) -> Option<u64> {
    let header_pages = header_pages_for(count);
    let range = kvm::alloc_kernel_pages(header_pages).ok()?;
    let mut guard = OwnedKvmRangeGuard::new(range);
    let header_kva = guard.kva();

    // Zero the header range. KVM pages are freshly allocated but
    // page_alloc returns whatever's in the buddy free list — zero
    // explicitly so a partially-initialized header reads as count=0
    // to any stale viewer.
    // SAFETY: header_kva is a freshly-allocated KVM range; we hold
    // exclusive access via the guard.
    unsafe {
        let mut p = KernelMut::<u8>::from_kva(header_kva);
        core::ptr::write_bytes(p.as_mut_ptr(),
            0,
            header_pages * (PAGE_SIZE as usize));
    }

    // SAFETY: header_kva is a freshly allocated, virtually-contiguous
    // KVM range backed by header_pages_for(count) pages.
    // backed_mut(count) derives backing_pages = header_pages_for(count)
    // internally; the wrapper's pointer arithmetic is sound because
    // KVM stitched the backing frames into a contiguous VA.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_kva(header_kva) };
    let mut backed = unsafe { header_ref.get_mut().backed_mut(count) };
    backed.init(page_addrs);

    let entry = PageSetEntry { count, header_kva, origin: PageSetOrigin::Buddy };
    // SAFETY: single-core, IRQs masked — exclusive table access.
    let id = unsafe { (*TABLE.ptr()).insert(entry).ok()? };

    // Success — header range now belongs to the pageset table.
    guard.take();
    Some(id as u64)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Allocate `count` physical pages and register them as a PageSet.
/// Allocates header_pages_for(count) contiguous header pages plus
/// `count` data pages. The header stores all data page addresses
/// (in a variable-size array starting at byte offset 24 from the
/// header — 16 bytes of u32 counters + 8 bytes of origin enum),
/// avoiding large stack arrays.
/// Returns the PageSet ID, or `None` if out of memory or table full.
pub fn alloc_pages(count: usize) -> Option<u64> {
    if count == 0 || count > MAX_PRACTICAL_PAGES_PER_SET {
        return None;
    }

    let header_pages = header_pages_for(count);
    let range = kvm::alloc_kernel_pages(header_pages).ok()?;
    let mut guard = OwnedKvmRangeGuard::new(range);
    let header_kva = guard.kva();

    // Zero the header range so set_page writes happen into known
    // state and any partial-init failure leaves the header reading
    // as inert.
    // SAFETY: header_kva is a freshly-allocated KVM range; we hold
    // exclusive access via the guard.
    unsafe {
        let mut p = KernelMut::<u8>::from_kva(header_kva);
        core::ptr::write_bytes(p.as_mut_ptr(),
            0,
            header_pages * (PAGE_SIZE as usize));
    }

    // SAFETY: header_kva is a freshly allocated, zeroed KVM range
    // virtually backed by header_pages_for(count) pages. The wrapper
    // tracks count internally; set_count below writes header.count +
    // header.header_pages to the on-disk header so downstream readers
    // see the same value.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_kva(header_kva) };
    let mut backed = unsafe { header_ref.get_mut().backed_mut(count) };
    backed.set_count(count);

    // Allocate data pages one at a time, writing each address directly into the header.
    // Zero each page so userspace mmap-backed allocations see the zero-init
    // contract that POSIX MAP_ANONYMOUS guarantees (mallocng's slot
    // header validation reads the bytes preceding the user pointer
    // and crashes on stale non-zero residue from prior kernel use).
    for i in 0..count {
        match page_alloc::alloc_page() {
            Some(page) => {
                page_alloc::zero_page(page.start_addr());
                backed.set_page(i, page.start_addr().as_u64());
            }
            None => {
                // Roll back: free data pages allocated so far.
                // Header range freed by guard on return.
                for j in 0..i {
                    let paddr = backed.get_page(j).unwrap();
                    page_alloc::dealloc_page(
                        PhysPage::containing(PhysAddr::new(paddr))
                    );
                }
                return None;
            }
        }
    }

    let id = insert_into_table(count, header_kva)?;
    guard.take(); // success — table owns the header range now
    Some(id)
}

/// Allocate `count` physically contiguous pages and register as a PageSet.
/// The buddy allocator rounds up to the next power of two, so the PageSet
/// tracks the full rounded allocation (no leaked tail pages). The caller
/// gets at least `count` contiguous pages; extra pages are uninitialized.
/// The header pages are allocated separately (not contiguous with data).
/// Returns the PageSet ID, or `None` if out of memory or table full.
pub fn alloc_pages_contiguous(count: usize) -> Option<u64> {
    if count == 0 || count > MAX_PRACTICAL_PAGES_PER_SET {
        return None;
    }

    // The buddy allocator rounds up to 2^order. Track the full allocation
    // so no pages are leaked between the buddy and the PageSet.
    let order = lockjaw_types::buddy::BuddyAllocator::order_for_count(count);
    let actual_count = 1 << order;
    if actual_count > MAX_PRACTICAL_PAGES_PER_SET {
        return None;
    }

    let header_pages = header_pages_for(actual_count);
    let range = kvm::alloc_kernel_pages(header_pages).ok()?;
    let mut guard = OwnedKvmRangeGuard::new(range);
    let header_kva = guard.kva();
    // SAFETY: header_kva is a freshly-allocated KVM range.
    unsafe {
        let mut p = KernelMut::<u8>::from_kva(header_kva);
        core::ptr::write_bytes(p.as_mut_ptr(),
            0,
            header_pages * (PAGE_SIZE as usize));
    }

    // Allocate contiguous data pages (actual_count, not count)
    let first_data = match page_alloc::alloc_pages_contiguous(count) {
        Some(page) => page,
        None => return None, // guard frees header range
    };

    // SAFETY: header_kva is a freshly allocated, zeroed KVM range
    // virtually backed by header_pages_for(actual_count) pages. The
    // wrapper tracks actual_count internally; set_count writes the
    // header.count + header.header_pages fields.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_kva(header_kva) };
    let mut backed = unsafe { header_ref.get_mut().backed_mut(actual_count) };
    backed.set_count(actual_count);
    let base = first_data.start_addr().as_u64();
    // Zero pages so userspace mmap-backed allocations see the
    // POSIX MAP_ANONYMOUS zero-init contract (see alloc_pages above).
    for i in 0..actual_count {
        let paddr = PhysAddr::new(base + (i as u64) * PAGE_SIZE);
        page_alloc::zero_page(paddr);
        backed.set_page(i, paddr.as_u64());
    }

    match insert_into_table(actual_count, header_kva) {
        Some(id) => {
            guard.take(); // success — table owns the header range now
            Some(id)
        }
        None => {
            // Table full — free data pages. Header range freed by guard.
            page_alloc::dealloc_pages_contiguous(first_data, count);
            None
        }
    }
}

/// Maximum pages for a single `register_existing` call. The only
/// caller today is the DTB boot path, which caps itself at 16
/// (`dtb_page_count > 16` panics in `src/main.rs`). 64 is generous
/// headroom for any future firmware-placed region we might want to
/// wrap. Constrained to keep the per-call stack temporary small.
pub const MAX_REGISTER_EXISTING_PAGES: usize = 64;

/// Register a PageSet for existing physical pages (not from the
/// allocator). Used at boot to wrap firmware-placed regions like the
/// DTB. Allocates `header_pages_for(count)` extra contiguous pages
/// for the header.
///
/// **Type-level alignment guarantee:** the slice element is
/// `PhysPage`, not `PhysAddr`. This is the load-bearing distinction
/// that prevents the Pi DTB class of bug — `PhysPage` literally
/// cannot represent an unaligned address (its inner u64 is the page
/// number; `start_addr()` shifts left by `PAGE_SHIFT` so the low 12
/// bits are always zero by construction). Callers wanting to wrap
/// an arbitrary firmware-supplied address must explicitly choose
/// between `PhysPage::aligned(addr)?` (fail loudly when the address
/// should already be aligned) and `PhysPage::containing(addr)`
/// (round down and own the in-page offset themselves — see
/// `lockjaw_types::dtb_layout` for the canonical pattern).
///
/// Capped at `MAX_REGISTER_EXISTING_PAGES` so the per-call stack
/// temporary stays small. Returns `None` if `count` is zero, exceeds
/// the cap, or is larger than the supplied slice.
pub fn register_existing(count: usize, pages: &[PhysPage]) -> Option<u64> {
    if count == 0 || count > MAX_REGISTER_EXISTING_PAGES || pages.len() < count {
        return None;
    }

    // Materialise page-base PhysAddrs for the header. PhysPage stores
    // the page number; the header stores page-base byte addresses.
    // The two differ by a PAGE_SHIFT shift, so a transmute would
    // silently store wrong values — emit through start_addr().
    let mut addrs = [0u64; MAX_REGISTER_EXISTING_PAGES];
    for i in 0..count {
        addrs[i] = pages[i].start_addr().as_u64();
    }

    // Header pages freed by guard on insert failure.
    // Data pages are firmware-placed — not ours to free.
    alloc_and_insert_header(&addrs[..count], count)
}

/// Wrap a physical MMIO address as a 1-page PageSet (no allocation from pool, just tracking).
/// Allocates one header page to store the MMIO address.
/// Header page freed by guard on insert failure. The MMIO data page is
/// device memory and is never freed.
pub fn register_device_page(phys_addr: u64) -> Option<u64> {
    alloc_and_insert_header(&[phys_addr], 1)
}

/// Free only the header range for a PageSet (not its data pages).
/// Used to roll back device page registration when the handle table is
/// full — MMIO data pages are device memory and must not be freed.
pub fn free_header_page(header_kva: KernelVa) {
    // Look up header_pages from the PageSetTable BEFORE consuming, so
    // a corrupt header.header_pages cannot redirect kvm::free_kernel_pages
    // to free the wrong range size. SAFETY: GKL held.
    let (_count, header_pages) = unsafe {
        trusted_layout_or_panic(header_kva, "free_header_page")
    };
    consume_by_header_kva(header_kva);
    // SAFETY: range came from kvm::alloc_kernel_pages and the
    // PageSetTable just released its claim — no live references remain.
    unsafe {
        kvm::free_kernel_pages(OwnedKvmRange { kva: header_kva, pages: header_pages });
    }
}

/// Decrement refcount for a PageSet. If both refcount and map_count
/// reach zero, free the PageSet (data pages + header). Used by
/// CloseHandleResult::RemoveAndDecRef.
pub fn dec_refcount_and_maybe_free(header_kva: KernelVa) {
    let should_free = unsafe {
        read_header_mut(header_kva).dec_refcount()
    };
    if should_free {
        free_by_header_kva(header_kva);
    }
}

/// Decrement both map_count and refcount for a PageSet. If both
/// reach zero, free the PageSet. Used by
/// CloseHandleResult::UnmapThenRemove (after PTEs are already cleared).
pub fn dec_both_and_maybe_free(header_kva: KernelVa) {
    unsafe { read_header_mut(header_kva).dec_map_count(); }
    dec_refcount_and_maybe_free(header_kva);
}

/// Phase 1 of two-phase consume: validate that revoking every
/// cross-process handle to `header_kva` would succeed. Pure
/// read-only walk over `scheduler::for_each_tcb` — no state
/// mutated on either success or failure.
///
/// On success, the caller MUST call `consume_pageset_apply` for
/// the same header within the same critical section (GKL held
/// continuously). On failure, the caller propagates the error to
/// userspace and the PageSet is unchanged — the user can retry
/// or close it.
///
/// Used by `create_kernel_object` (one PageSet → one new endpoint
/// / notification / reply / TCB) and by `sys_create_process`
/// (per consumed PageSet header). See
/// `docs/handle-revocation-plan.md`.
pub fn consume_pageset_validate(
    header_kva: KernelVa,
) -> Result<(), super::revoke::RevokeError> {
    super::revoke::revoke_validate(header_kva)
}

/// Phase 2 of two-phase consume: clear every cross-process handle
/// to `header_kva`, unlink from the global PageSet table, and
/// free the header range. Cannot fail under the precondition that
/// a matching `consume_pageset_validate` returned Ok within the
/// same critical section.
///
/// After return:
/// - Every PageSet handle to `header_kva` in any process's table
///   is cleared (revoke_apply walks all live processes).
/// - Every active mapping's PTEs are cleared and TLB-invalidated.
/// - The header is unlinked from the PageSet table.
/// - The header page is freed back to the allocator (no tombstone).
///
/// Data pages are NOT freed — the caller takes ownership of them
/// (consume is the ownership-transfer path).
pub fn consume_pageset_apply(header_kva: KernelVa) {
    // Look up header_pages from the PageSetTable BEFORE revoke_apply /
    // consume_by_header_kva. Two reasons: (1) consume_by_header_kva
    // unlinks the table entry, after which trusted_layout would fail;
    // (2) using the table's tracked count rather than the on-disk
    // header.header_pages prevents a corrupt header from steering
    // kvm::free_kernel_pages to free the wrong range size.
    // SAFETY: GKL held.
    let (_count, header_pages) = unsafe {
        trusted_layout_or_panic(header_kva, "consume_pageset_apply")
    };

    // Phase 2: clear cross-process handles, dec refcount/map_count,
    // clear PTEs. After this returns, header.refcount == 0 and
    // header.map_count == 0; no handle or PTE anywhere references
    // the header.
    let stats = super::revoke::revoke_apply(header_kva);

    // "revoke OK" diagnostic: proves the walker ran (and how much
    // it cleared) on every consume. The plan calls for this so a
    // make run boot can verify the happy path is exercised; the
    // integration suite asserts the line appears at least once.
    crate::kprintln!(
        "revoke OK: header=", header_kva.as_u64(),
        " procs=", stats.processes,
        " slots=", stats.slots,
        " maps=", stats.mappings,
    );

    // Unlink from the global PageSet table.
    consume_by_header_kva(header_kva);

    // Free the KVM-pool header range — safe because revoke cleared
    // every reference. Replaces the previous tombstone-leak pattern:
    // exported handles in other processes are no longer a hazard
    // because revoke walked their tables too.
    // SAFETY: range came from kvm::alloc_kernel_pages and revoke
    // cleared every cross-process reference; no live KernelMut/Ref
    // can remain.
    unsafe {
        kvm::free_kernel_pages(OwnedKvmRange { kva: header_kva, pages: header_pages });
    }
}

/// Remove a PageSet from the table by its header KVA.
/// Used when consuming via a handle (which stores header_kva, not the
/// global table slot ID). Does NOT free the header range — use
/// `free_by_header_kva` for full cleanup.
pub fn consume_by_header_kva(header_kva: KernelVa) -> bool {
    // SAFETY: single-core, IRQs masked — exclusive table access.
    unsafe {
        let table = &mut *TABLE.ptr();
        match table.find_by_header_kva(header_kva) {
            Some(id) => table.remove(id).is_ok(),
            None => false,
        }
    }
}

/// Remove a PageSet from the table AND free its data pages + KVM
/// header range. Used to roll back a failed sys_alloc_pages when
/// the handle table is full.
pub fn free_by_header_kva(header_kva: KernelVa) {
    // Trusted bounds for the data-free loop AND the header dealloc.
    // SAFETY: GKL held; trusted_layout reads only PageSetTable state.
    let (count, header_pages) = unsafe {
        trusted_layout_or_panic(header_kva, "free_by_header_kva")
    };

    // SAFETY: registered PageSet per the lookup above; wrapper makes
    // get_page safe.
    let backed = unsafe { read_header_backed(header_kva) };

    // Free data pages (skip device MMIO pages — they are below RAM_START
    // and were never allocated from the buddy allocator). Iterating
    // 0..count uses the trusted PageSetTable count, so a corrupt
    // header.count cannot extend or truncate the loop.
    let ram_start = crate::mm::addr::ram_start().as_u64();
    for i in 0..count {
        if let Some(paddr) = backed.get_page(i) {
            if paddr >= ram_start {
                page_alloc::dealloc_page(
                    crate::mm::addr::PhysPage::containing(PhysAddr::new(paddr))
                );
            }
        }
    }

    // Remove from global table.
    consume_by_header_kva(header_kva);

    // Free the KVM-pool header range — size from PageSetTable, not
    // from the on-disk header.
    // SAFETY: range came from kvm::alloc_kernel_pages; refs above
    // have been dropped before this call.
    unsafe {
        kvm::free_kernel_pages(OwnedKvmRange { kva: header_kva, pages: header_pages });
    }
}

/// Look up a PageSet by ID. Returns the data page count and header KVA.
/// Used internally by syscall handlers after allocation to get the header
/// KVA for handle table insertion.
pub fn get_pageset(id: u64) -> Option<(usize, KernelVa)> {
    // SAFETY: single-core, IRQs masked — read-only table access.
    unsafe {
        (*TABLE.ptr()).get(id as usize).ok().map(|entry| (entry.count, entry.header_kva))
    }
}

// ---------------------------------------------------------------------------
// PageSetRef — safe typed wrapper over a registered PageSet
// ---------------------------------------------------------------------------

/// A validated reference to a registered PageSet. Constructed from
/// `PageSetRef::from_header_kva()` using a handle table entry's
/// object KVA. All methods are safe — the header_kva validity is
/// established at construction time.
pub struct PageSetRef {
    count: usize,
    header_kva: KernelVa,
}

impl PageSetRef {
    /// Construct from a header KVA (e.g., from a handle table entry).
    /// Caches the trusted count from the PageSetTable, NOT the on-disk
    /// header.count.
    ///
    /// # Safety
    /// `header_kva` must be a registered PageSet (i.e. it appears
    /// in the global PageSetTable). Panics otherwise — caller used
    /// a stale or unregistered handle.
    pub unsafe fn from_header_kva(header_kva: KernelVa) -> Self {
        let (count, _backing) =
            trusted_layout_or_panic(header_kva, "PageSetRef::from_header_kva");
        PageSetRef { count, header_kva }
    }

    /// Number of data pages in this PageSet.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Get the physical address of a data page by index.
    pub fn page(&self, index: usize) -> Option<PhysAddr> {
        // SAFETY: self.header_kva came from a registered PageSet —
        // read_header_backed enforces that contract and returns a
        // wrapper, after which get_page is safe.
        let backed = unsafe { read_header_backed(self.header_kva) };
        backed.get_page(index).map(PhysAddr::new)
    }
}

/// Read the PageSetHeader from a header KVA.
///
/// # Safety
/// `header_kva` must be a valid KVM-mapped header KVA.
pub(crate) unsafe fn read_header(header_kva: KernelVa) -> &'static PageSetHeader {
    let header = KernelMut::<PageSetHeader>::from_kva(header_kva);
    // Extend lifetime to 'static — the header lives in a kernel-owned page
    // that persists for the life of the PageSet. Callers must not hold the
    // reference past consume_pageset().
    &*header.as_ptr()
}

/// Get a mutable reference to a PageSetHeader for updating refcount/map_count.
///
/// # Safety
/// `header_kva` must be a valid KVM-mapped header KVA.
/// Caller must ensure exclusive access (GKL held, single-core).
pub(crate) unsafe fn read_header_mut(header_kva: KernelVa) -> &'static mut PageSetHeader {
    let mut header = KernelMut::<PageSetHeader>::from_kva(header_kva);
    &mut *header.as_mut_ptr()
}

/// Look up `(count, header_pages)` from the global PageSetTable for
/// `header_kva`. Returns `(entry.count, header_pages_for(count))`
/// — both derived from PageSetTable's registered state, independent
/// from the on-disk header. Use these as the trusted bounds for
/// iteration (data page count) and deallocation (header page count).
///
/// Returns None if `header_kva` is not registered in the table.
///
/// SAFETY: caller must hold the GKL (single-core, IRQs masked).
unsafe fn trusted_layout(header_kva: KernelVa) -> Option<(usize, usize)> {
    let table = &*TABLE.ptr();
    let id = table.find_by_header_kva(header_kva)?;
    // Avoid Result::expect — it pulls in PageSetError's Debug impl
    // into kernel .rodata, which the data-to-code check rejects.
    match table.get(id) {
        Ok(entry) => {
            let count = entry.count;
            Some((count, lockjaw_types::pageset_table::header_pages_for(count)))
        }
        Err(_) => None,
    }
}

/// Same as `trusted_layout` but panics if not registered. Used at
/// kernel-internal sites where unregistered means a bug, not a
/// recoverable condition.
unsafe fn trusted_layout_or_panic(header_kva: KernelVa, who: &str) -> (usize, usize) {
    trusted_layout(header_kva).unwrap_or_else(|| {
        panic!("{}: header not registered in PageSetTable", who)
    })
}

/// Read the PageSetHeader as a backed wrapper. Derives the
/// backing-pages witness from the global PageSetTable's registered
/// count for `header_kva` — independent storage from the header
/// itself, so a tampered or partially-corrupt header cannot inflate
/// the safety bound.
///
/// Panics if `header_kva` is not registered in the table — calling
/// read_header_backed on an unregistered (or already-consumed)
/// header is a kernel bug.
///
/// # Safety
/// `header_kva` must be a registered header — i.e. it appears in
/// `TABLE` with a tracked count. The function looks up the trusted
/// count, derives `backing_pages = header_pages_for(count)`, and
/// constructs the wrapper. All subsequent page-addr accesses
/// (get_page, pages_slice) bounds-check against this trusted backing.
pub(crate) unsafe fn read_header_backed(
    header_kva: KernelVa,
) -> lockjaw_types::pageset_table::BackedHeader<'static> {
    let (trusted_count, _trusted_backing) =
        trusted_layout_or_panic(header_kva, "read_header_backed");
    let h = read_header(header_kva);
    // SAFETY: trusted_count comes from the PageSetTable (updated only
    // by the allocator at registration time). The wrapper carries it
    // as the logical count witness AND derives backing_pages =
    // header_pages_for(count) for the safety bound. Both come from
    // trusted state, so neither pages_slice() length nor get_page()
    // bounds can be inflated/truncated by a corrupted on-disk header.
    h.backed(trusted_count)
}


