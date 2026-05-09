use crate::mm::addr::{PhysAddr, PhysPage, PAGE_SIZE};
use crate::mm::kernel_ptr::KernelMut;
use crate::mm::page_alloc;
use core::cell::UnsafeCell;
use lockjaw_types::pageset_table::{
    header_pages_for, PageSetEntry, PageSetHeader, PageSetTable,
    MAX_PRACTICAL_PAGES_PER_SET,
};

// ---------------------------------------------------------------------------
// HeaderPageGuard — RAII cleanup for allocated header pages
// ---------------------------------------------------------------------------

/// Owns one or more freshly allocated, physically-contiguous header
/// pages. Frees them on drop unless `take()` is called to claim
/// ownership. Prevents leaks when PageSet table insertion or later
/// steps fail.
struct HeaderPageGuard {
    /// First (lowest) physical address of the contiguous header range.
    paddr: Option<PhysAddr>,
    /// Number of contiguous pages.
    pages: usize,
}

impl HeaderPageGuard {
    fn new(paddr: PhysAddr, pages: usize) -> Self {
        Self { paddr: Some(paddr), pages }
    }

    /// Claim the header pages, preventing them from being freed on drop.
    /// Returns the physical address of the first page.
    fn take(&mut self) -> PhysAddr {
        self.paddr.take().unwrap_or_else(|| panic!("HeaderPageGuard already taken"))
    }
}

impl Drop for HeaderPageGuard {
    fn drop(&mut self) {
        if let Some(paddr) = self.paddr {
            // alloc_pages_contiguous gives back contiguous pages; mirror
            // by freeing N consecutive pages starting at paddr.
            page_alloc::dealloc_pages_contiguous(PhysPage::containing(paddr), self.pages);
        }
    }
}

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

/// Insert an already-initialized header page into the global table.
/// Does NOT own the header page — the caller is responsible for cleanup
/// on failure (typically via HeaderPageGuard).
fn insert_into_table(count: usize, header_paddr: PhysAddr) -> Option<u64> {
    let entry = PageSetEntry { count, header_paddr: header_paddr.as_u64() };
    // SAFETY: single-core, IRQs masked — exclusive table access.
    unsafe { (*TABLE.ptr()).insert(entry).ok().map(|id| id as u64) }
}

/// Allocate enough physically-contiguous header pages to hold `count`
/// page-address entries, initialize the header in place, and insert
/// into the global table. On failure, the header pages are freed via
/// the drop guard. Returns the PageSet table slot ID.
fn alloc_and_insert_header(page_addrs: &[u64], count: usize) -> Option<u64> {
    let header_pages = header_pages_for(count);
    let first_header = page_alloc::alloc_pages_contiguous(header_pages)?;
    let header_paddr = first_header.start_addr();
    let mut guard = HeaderPageGuard::new(header_paddr, header_pages);

    // Zero all header pages so a partially-initialized header (e.g. on
    // alloc-and-then-fail) reads as count=0 to any stale viewer.
    for i in 0..header_pages {
        let p = PhysAddr::new(header_paddr.as_u64() + (i as u64) * PAGE_SIZE);
        page_alloc::zero_page(p);
    }

    // SAFETY: header_paddr is a freshly allocated kernel page (the first
    // of header_pages_for(count) contiguous pages). backed_mut(count)
    // derives backing_pages = header_pages_for(count) internally.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_paddr(header_paddr) };
    let mut backed = unsafe { header_ref.get_mut().backed_mut(count) };
    backed.init(page_addrs);

    let entry = PageSetEntry { count, header_paddr: header_paddr.as_u64() };
    // SAFETY: single-core, IRQs masked — exclusive table access.
    let id = unsafe { (*TABLE.ptr()).insert(entry).ok()? };

    // Success — header pages now belong to the pageset table.
    guard.take();
    Some(id as u64)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Allocate `count` physical pages and register them as a PageSet.
/// Allocates header_pages_for(count) contiguous header pages plus
/// `count` data pages. The header stores all data page addresses
/// (in a variable-size array starting at byte offset 16 from the
/// header), avoiding large stack arrays.
/// Returns the PageSet ID, or `None` if out of memory or table full.
pub fn alloc_pages(count: usize) -> Option<u64> {
    if count == 0 || count > MAX_PRACTICAL_PAGES_PER_SET {
        return None;
    }

    let header_pages = header_pages_for(count);
    let first_header = page_alloc::alloc_pages_contiguous(header_pages)?;
    let header_paddr = first_header.start_addr();
    let mut guard = HeaderPageGuard::new(header_paddr, header_pages);

    // Zero every header page so set_page writes happen into known state
    // and any partial-init failure leaves the header reading as inert.
    for i in 0..header_pages {
        let p = PhysAddr::new(header_paddr.as_u64() + (i as u64) * PAGE_SIZE);
        page_alloc::zero_page(p);
    }

    // SAFETY: header_paddr is a freshly allocated, zeroed kernel page
    // backed by header_pages_for(count) contiguous pages. The wrapper
    // tracks count internally; set_count below writes header.count +
    // header.header_pages to the on-disk header so downstream readers
    // see the same value.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_paddr(header_paddr) };
    let mut backed = unsafe { header_ref.get_mut().backed_mut(count) };
    backed.set_count(count);

    // Allocate data pages one at a time, writing each address directly into the header
    for i in 0..count {
        match page_alloc::alloc_page() {
            Some(page) => {
                backed.set_page(i, page.start_addr().as_u64());
            }
            None => {
                // Roll back: free data pages allocated so far.
                // Header pages freed by guard on return.
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

    let id = insert_into_table(count, header_paddr)?;
    guard.take(); // success — table owns the header pages now
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
    let first_header = page_alloc::alloc_pages_contiguous(header_pages)?;
    let header_paddr = first_header.start_addr();
    let mut guard = HeaderPageGuard::new(header_paddr, header_pages);
    for i in 0..header_pages {
        let p = PhysAddr::new(header_paddr.as_u64() + (i as u64) * PAGE_SIZE);
        page_alloc::zero_page(p);
    }

    // Allocate contiguous data pages (actual_count, not count)
    let first_data = match page_alloc::alloc_pages_contiguous(count) {
        Some(page) => page,
        None => return None, // guard frees header pages
    };

    // SAFETY: header_paddr is a freshly allocated, zeroed kernel page
    // backed by header_pages_for(actual_count) contiguous pages. The
    // wrapper tracks actual_count internally; set_count writes the
    // header.count + header.header_pages fields.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_paddr(header_paddr) };
    let mut backed = unsafe { header_ref.get_mut().backed_mut(actual_count) };
    backed.set_count(actual_count);
    let base = first_data.start_addr().as_u64();
    for i in 0..actual_count {
        backed.set_page(i, base + (i as u64) * PAGE_SIZE);
    }

    match insert_into_table(actual_count, header_paddr) {
        Some(id) => {
            guard.take(); // success — table owns the header pages now
            Some(id)
        }
        None => {
            // Table full — free data pages. Header freed by guard.
            page_alloc::dealloc_pages_contiguous(first_data, count);
            None
        }
    }
}

/// Register a PageSet for existing physical pages (not from the allocator).
/// Used at boot to wrap the DTB pages placed by QEMU firmware.
/// Allocates header_pages_for(count) extra contiguous pages for the header.
pub fn register_existing(count: usize, pages: &[PhysAddr]) -> Option<u64> {
    if count == 0 || count > MAX_PRACTICAL_PAGES_PER_SET {
        return None;
    }

    // SAFETY: PhysAddr is repr(transparent) over u64, same layout.
    let addrs: &[u64] = unsafe {
        core::slice::from_raw_parts(
            // SAFETY: PhysAddr → u64 transmute via repr(transparent)
            pages.as_ptr() as *const u64,
            count,
        )
    };
    // Header pages freed by guard on insert failure.
    // Data pages are firmware-placed — not ours to free.
    alloc_and_insert_header(addrs, count)
}

/// Wrap a physical MMIO address as a 1-page PageSet (no allocation from pool, just tracking).
/// Allocates one header page to store the MMIO address.
/// Header page freed by guard on insert failure. The MMIO data page is
/// device memory and is never freed.
pub fn register_device_page(phys_addr: u64) -> Option<u64> {
    alloc_and_insert_header(&[phys_addr], 1)
}

/// Free only the header pages for a PageSet (not its data pages).
/// Used to roll back device page registration when the handle table is
/// full — MMIO data pages are device memory and must not be freed.
pub fn free_header_page(header_paddr: u64) {
    // Look up header_pages from the PageSetTable BEFORE consuming, so
    // a corrupt header.header_pages cannot redirect dealloc_pages_contiguous
    // to free the wrong block size. SAFETY: GKL held.
    let (_count, header_pages) = unsafe {
        trusted_layout_or_panic(header_paddr, "free_header_page")
    };
    consume_by_header_paddr(header_paddr);
    page_alloc::dealloc_pages_contiguous(
        crate::mm::addr::PhysPage::containing(PhysAddr::new(header_paddr)),
        header_pages,
    );
}

/// Decrement refcount for a PageSet. If both refcount and map_count
/// reach zero, free the PageSet (data pages + header). Used by
/// CloseHandleResult::RemoveAndDecRef.
pub fn dec_refcount_and_maybe_free(header_paddr: u64) {
    let should_free = unsafe {
        read_header_mut(header_paddr).dec_refcount()
    };
    if should_free {
        free_by_header_paddr(header_paddr);
    }
}

/// Decrement both map_count and refcount for a PageSet. If both
/// reach zero, free the PageSet. Used by
/// CloseHandleResult::UnmapThenRemove (after PTEs are already cleared).
pub fn dec_both_and_maybe_free(header_paddr: u64) {
    unsafe { read_header_mut(header_paddr).dec_map_count(); }
    dec_refcount_and_maybe_free(header_paddr);
}

/// Phase 1 of two-phase consume: validate that revoking every
/// cross-process handle to `header_paddr` would succeed. Pure
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
    header_paddr: u64,
) -> Result<(), super::revoke::RevokeError> {
    super::revoke::revoke_validate(header_paddr)
}

/// Phase 2 of two-phase consume: clear every cross-process handle
/// to `header_paddr`, unlink from the global PageSet table, and
/// free the header page. Cannot fail under the precondition that
/// a matching `consume_pageset_validate` returned Ok within the
/// same critical section.
///
/// After return:
/// - Every PageSet handle to `header_paddr` in any process's table
///   is cleared (revoke_apply walks all live processes).
/// - Every active mapping's PTEs are cleared and TLB-invalidated.
/// - The header is unlinked from the PageSet table.
/// - The header page is freed back to the allocator (no tombstone).
///
/// Data pages are NOT freed — the caller takes ownership of them
/// (consume is the ownership-transfer path).
pub fn consume_pageset_apply(header_paddr: u64) {
    // Look up header_pages from the PageSetTable BEFORE revoke_apply /
    // consume_by_header_paddr. Two reasons: (1) consume_by_header_paddr
    // unlinks the table entry, after which trusted_layout would fail;
    // (2) using the table's tracked count rather than the on-disk
    // header.header_pages prevents a corrupt header from steering
    // dealloc_pages_contiguous to free the wrong block size.
    // SAFETY: GKL held.
    let (_count, header_pages) = unsafe {
        trusted_layout_or_panic(header_paddr, "consume_pageset_apply")
    };

    // Phase 2: clear cross-process handles, dec refcount/map_count,
    // clear PTEs. After this returns, header.refcount == 0 and
    // header.map_count == 0; no handle or PTE anywhere references
    // the header.
    let stats = super::revoke::revoke_apply(header_paddr);

    // "revoke OK" diagnostic: proves the walker ran (and how much
    // it cleared) on every consume. The plan calls for this so a
    // make run boot can verify the happy path is exercised; the
    // integration suite asserts the line appears at least once.
    crate::kprintln!(
        "revoke OK: header=", header_paddr,
        " procs=", stats.processes,
        " slots=", stats.slots,
        " maps=", stats.mappings,
    );

    // Unlink from the global PageSet table.
    consume_by_header_paddr(header_paddr);

    // Free the contiguous header pages — safe because revoke cleared
    // every reference. Replaces the previous tombstone-leak pattern:
    // exported handles in other processes are no longer a hazard
    // because revoke walked their tables too. Variable-size headers
    // may span multiple contiguous pages; dealloc_pages_contiguous
    // mirrors the alloc_pages_contiguous in alloc_and_insert_header.
    page_alloc::dealloc_pages_contiguous(
        crate::mm::addr::PhysPage::containing(PhysAddr::new(header_paddr)),
        header_pages,
    );
}

/// Remove a PageSet from the table by its header physical address.
/// Used when consuming via a handle (which stores header_paddr, not the
/// global table slot ID). Does NOT free physical pages — use
/// free_by_header_paddr for full cleanup.
pub fn consume_by_header_paddr(header_paddr: u64) -> bool {
    // SAFETY: single-core, IRQs masked — exclusive table access.
    unsafe {
        let table = &mut *TABLE.ptr();
        match table.find_by_header_paddr(header_paddr) {
            Some(id) => table.remove(id).is_ok(),
            None => false,
        }
    }
}

/// Remove a PageSet from the table AND free all its physical pages
/// (data pages + header page). Used to roll back a failed sys_alloc_pages
/// when the handle table is full.
pub fn free_by_header_paddr(header_paddr: u64) {
    // Trusted bounds for the data-free loop AND the header dealloc.
    // SAFETY: GKL held; trusted_layout reads only PageSetTable state.
    let (count, header_pages) = unsafe {
        trusted_layout_or_panic(header_paddr, "free_by_header_paddr")
    };

    // SAFETY: registered PageSet per the lookup above; wrapper makes
    // get_page safe.
    let backed = unsafe { read_header_backed(header_paddr) };

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

    // Remove from global table
    consume_by_header_paddr(header_paddr);

    // Free header pages (contiguous run starting at header_paddr) — size
    // from PageSetTable, not from the on-disk header.
    page_alloc::dealloc_pages_contiguous(
        crate::mm::addr::PhysPage::containing(PhysAddr::new(header_paddr)),
        header_pages,
    );
}

/// Look up a PageSet by ID. Returns the data page count and header physical address.
/// Used internally by syscall handlers after allocation to get the header
/// paddr for handle table insertion.
pub fn get_pageset(id: u64) -> Option<(usize, u64)> {
    // SAFETY: single-core, IRQs masked — read-only table access.
    unsafe {
        (*TABLE.ptr()).get(id as usize).ok().map(|entry| (entry.count, entry.header_paddr))
    }
}

// ---------------------------------------------------------------------------
// PageSetRef — safe typed wrapper over a registered PageSet
// ---------------------------------------------------------------------------

/// A validated reference to a registered PageSet. Constructed from
/// `PageSetRef::from_header_paddr()` using a handle table entry's
/// object_paddr. All methods are safe — the header_paddr validity is
/// established at construction time.
pub struct PageSetRef {
    count: usize,
    header_paddr: u64,
}

impl PageSetRef {
    /// Construct from a header page physical address (e.g., from a
    /// handle table entry). Caches the trusted count from the
    /// PageSetTable, NOT the on-disk header.count.
    ///
    /// # Safety
    /// `header_paddr` must be a registered PageSet (i.e. it appears
    /// in the global PageSetTable). Panics otherwise — caller used
    /// a stale or unregistered handle.
    pub unsafe fn from_header_paddr(header_paddr: u64) -> Self {
        let (count, _backing) =
            trusted_layout_or_panic(header_paddr, "PageSetRef::from_header_paddr");
        PageSetRef { count, header_paddr }
    }

    /// Number of data pages in this PageSet.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Get the physical address of a data page by index.
    pub fn page(&self, index: usize) -> Option<PhysAddr> {
        // SAFETY: self.header_paddr came from a registered PageSet —
        // read_header_backed enforces that contract and returns a
        // wrapper, after which get_page is safe.
        let backed = unsafe { read_header_backed(self.header_paddr) };
        backed.get_page(index).map(PhysAddr::new)
    }
}

/// Read the PageSetHeader from a header page.
///
/// # Safety
/// `header_paddr` must be a valid header page physical address.
pub(crate) unsafe fn read_header(header_paddr: u64) -> &'static PageSetHeader {
    let header = KernelMut::<PageSetHeader>::from_paddr(PhysAddr::new(header_paddr));
    // Extend lifetime to 'static — the header lives in a kernel-owned page
    // that persists for the life of the PageSet. Callers must not hold the
    // reference past consume_pageset().
    &*header.as_ptr()
}

/// Get a mutable reference to a PageSetHeader for updating refcount/map_count.
///
/// # Safety
/// `header_paddr` must be a valid header page physical address.
/// Caller must ensure exclusive access (GKL held, single-core).
pub(crate) unsafe fn read_header_mut(header_paddr: u64) -> &'static mut PageSetHeader {
    let mut header = KernelMut::<PageSetHeader>::from_paddr(PhysAddr::new(header_paddr));
    &mut *header.as_mut_ptr()
}

/// Look up `(count, header_pages)` from the global PageSetTable for
/// `header_paddr`. Returns `(entry.count, header_pages_for(count))`
/// — both derived from PageSetTable's registered state, independent
/// from the on-disk header. Use these as the trusted bounds for
/// iteration (data page count) and deallocation (header page count).
///
/// Returns None if `header_paddr` is not registered in the table.
///
/// SAFETY: caller must hold the GKL (single-core, IRQs masked).
unsafe fn trusted_layout(header_paddr: u64) -> Option<(usize, usize)> {
    let table = &*TABLE.ptr();
    let id = table.find_by_header_paddr(header_paddr)?;
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
unsafe fn trusted_layout_or_panic(header_paddr: u64, who: &str) -> (usize, usize) {
    trusted_layout(header_paddr).unwrap_or_else(|| {
        panic!("{}: header not registered in PageSetTable", who)
    })
}

/// Read the PageSetHeader as a backed wrapper. Derives the
/// backing-pages witness from the global PageSetTable's registered
/// count for `header_paddr` — independent storage from the header
/// itself, so a tampered or partially-corrupt header cannot inflate
/// the safety bound.
///
/// Panics if `header_paddr` is not registered in the table — calling
/// read_header_backed on an unregistered (or already-consumed)
/// header is a kernel bug.
///
/// # Safety
/// `header_paddr` must be a registered header — i.e. it appears in
/// `TABLE` with a tracked count. The function looks up the trusted
/// count, derives `backing_pages = header_pages_for(count)`, and
/// constructs the wrapper. All subsequent page-addr accesses
/// (get_page, pages_slice) bounds-check against this trusted backing.
pub(crate) unsafe fn read_header_backed(
    header_paddr: u64,
) -> lockjaw_types::pageset_table::BackedHeader<'static> {
    let (trusted_count, _trusted_backing) =
        trusted_layout_or_panic(header_paddr, "read_header_backed");
    let h = read_header(header_paddr);
    // SAFETY: trusted_count comes from the PageSetTable (updated only
    // by the allocator at registration time). The wrapper carries it
    // as the logical count witness AND derives backing_pages =
    // header_pages_for(count) for the safety bound. Both come from
    // trusted state, so neither pages_slice() length nor get_page()
    // bounds can be inflated/truncated by a corrupted on-disk header.
    h.backed(trusted_count)
}


