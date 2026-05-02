use crate::mm::addr::{PhysAddr, PhysPage};
use crate::mm::kernel_ptr::KernelMut;
use crate::mm::page_alloc;
use core::cell::UnsafeCell;
use lockjaw_types::pageset_table::{PageSetTable, PageSetEntry, PageSetHeader, MAX_PAGES_PER_SET};

// ---------------------------------------------------------------------------
// HeaderPageGuard — RAII cleanup for allocated header pages
// ---------------------------------------------------------------------------

/// Owns a freshly allocated header page. Frees it on drop unless
/// `take()` is called to claim ownership. Prevents header page leaks
/// when PageSet table insertion or later steps fail.
struct HeaderPageGuard {
    paddr: Option<PhysAddr>,
}

impl HeaderPageGuard {
    fn new(paddr: PhysAddr) -> Self {
        Self { paddr: Some(paddr) }
    }

    /// Claim the header page, preventing it from being freed on drop.
    /// Returns the physical address.
    fn take(&mut self) -> PhysAddr {
        self.paddr.take().unwrap_or_else(|| panic!("HeaderPageGuard already taken"))
    }
}

impl Drop for HeaderPageGuard {
    fn drop(&mut self) {
        if let Some(paddr) = self.paddr {
            page_alloc::dealloc_page(PhysPage::containing(paddr));
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

/// Allocate a header page, initialize it with the given page addresses,
/// and insert into the global table. On failure, the header page is freed
/// via the drop guard. Returns the PageSet table slot ID.
fn alloc_and_insert_header(page_addrs: &[u64], count: usize) -> Option<u64> {
    let header_page = page_alloc::alloc_page()?;
    let mut guard = HeaderPageGuard::new(header_page.start_addr());

    // SAFETY: header_paddr is a freshly allocated kernel page.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_paddr(header_page.start_addr()) };
    unsafe { core::ptr::write_bytes(header_ref.as_mut_ptr(), 0, 1); }
    header_ref.get_mut().init(page_addrs);

    let entry = PageSetEntry { count, header_paddr: header_page.start_addr().as_u64() };
    // SAFETY: single-core, IRQs masked — exclusive table access.
    let id = unsafe { (*TABLE.ptr()).insert(entry).ok()? };

    // Success — header page now belongs to the pageset table.
    guard.take();
    Some(id as u64)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Allocate `count` physical pages and register them as a PageSet.
/// Allocates count+1 pages: page 0 is the header, pages 1..count are data.
/// The header page stores all data page addresses, avoiding large stack arrays.
/// Returns the PageSet ID, or `None` if out of memory or table full.
pub fn alloc_pages(count: usize) -> Option<u64> {
    if count == 0 || count > MAX_PAGES_PER_SET {
        return None;
    }

    let header_page = page_alloc::alloc_page()?;
    let mut guard = HeaderPageGuard::new(header_page.start_addr());
    page_alloc::zero_page(header_page.start_addr());

    // SAFETY: header_paddr is a freshly allocated, zeroed kernel page.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_paddr(header_page.start_addr()) };

    // Allocate data pages one at a time, writing each address directly into the header
    for i in 0..count {
        match page_alloc::alloc_page() {
            Some(page) => {
                header_ref.get_mut().pages[i] = page.start_addr().as_u64();
            }
            None => {
                // Roll back: free data pages allocated so far.
                // Header page freed by guard on return.
                for j in 0..i {
                    page_alloc::dealloc_page(
                        PhysPage::containing(PhysAddr::new(header_ref.get().pages[j]))
                    );
                }
                return None;
            }
        }
    }
    header_ref.get_mut().count = count as u64;

    let id = insert_into_table(count, header_page.start_addr())?;
    guard.take(); // success — table owns the header page now
    Some(id)
}

/// Allocate `count` physically contiguous pages and register as a PageSet.
/// The buddy allocator rounds up to the next power of two, so the PageSet
/// tracks the full rounded allocation (no leaked tail pages). The caller
/// gets at least `count` contiguous pages; extra pages are uninitialized.
/// The header page is allocated separately (not contiguous with data).
/// Returns the PageSet ID, or `None` if out of memory or table full.
pub fn alloc_pages_contiguous(count: usize) -> Option<u64> {
    if count == 0 || count > MAX_PAGES_PER_SET {
        return None;
    }

    // The buddy allocator rounds up to 2^order. Track the full allocation
    // so no pages are leaked between the buddy and the PageSet.
    let order = lockjaw_types::buddy::BuddyAllocator::order_for_count(count);
    let actual_count = 1 << order;
    if actual_count > MAX_PAGES_PER_SET {
        return None;
    }

    let header_page = page_alloc::alloc_page()?;
    let mut guard = HeaderPageGuard::new(header_page.start_addr());
    page_alloc::zero_page(header_page.start_addr());

    // Allocate contiguous data pages (actual_count, not count)
    let first_data = match page_alloc::alloc_pages_contiguous(count) {
        Some(page) => page,
        None => return None, // guard frees header page
    };

    // Write all actual_count sequential addresses into the header
    // SAFETY: header_paddr is a freshly allocated, zeroed kernel page.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_paddr(header_page.start_addr()) };
    let base = first_data.start_addr().as_u64();
    for i in 0..actual_count {
        header_ref.get_mut().pages[i] = base + (i as u64) * crate::mm::addr::PAGE_SIZE;
    }
    header_ref.get_mut().count = actual_count as u64;

    match insert_into_table(actual_count, header_page.start_addr()) {
        Some(id) => {
            guard.take(); // success — table owns the header page now
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
/// Allocates one extra page for the header.
pub fn register_existing(count: usize, pages: &[PhysAddr]) -> Option<u64> {
    if count == 0 || count > MAX_PAGES_PER_SET {
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
    // Header page freed by guard on insert failure.
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

/// Free only the header page for a PageSet (not its data pages).
/// Used to roll back device page registration when the handle table is
/// full — MMIO data pages are device memory and must not be freed.
pub fn free_header_page(header_paddr: u64) {
    consume_by_header_paddr(header_paddr);
    page_alloc::dealloc_page(
        crate::mm::addr::PhysPage::containing(PhysAddr::new(header_paddr))
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

/// Consume a PageSet for ownership transfer. Data pages are NOT freed —
/// the caller takes ownership of them.
///
/// Steps:
/// 1. Zero the header page — makes any stale handles (local duplicates
///    or cross-process exports) read count=0 from the zeroed header,
///    so they become inert without needing revocation.
/// 2. Remove from global pageset table (unlinks the slot).
/// 3. Remove ALL handles in the given table that point at this header.
///
/// The header page is intentionally NOT freed. It stays allocated as a
/// zeroed tombstone so that stale exported handles in other processes
/// safely read count=0. Freeing would allow the page to be reused,
/// making stale handles point at a live object — a use-after-repurpose
/// bug. The proper fix is handle revocation (future work).
///
/// Used by both create_kernel_object (single-page donation for endpoints,
/// notifications, etc.) and create_process (multi-page image transfer).
pub fn consume_pageset(
    header_paddr: u64,
    handle_table: &super::handle_table::HandleTableRef,
) {
    // Zero header BEFORE removing handles — stale handles read count=0
    page_alloc::zero_page(PhysAddr::new(header_paddr));
    // Unlink from global table
    consume_by_header_paddr(header_paddr);
    // Remove all handles pointing at the consumed PageSet
    handle_table.remove_all_by_object(header_paddr);
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
    // Read the header to find data pages before removing from table.
    // SAFETY: header_paddr is a valid header page (caller just allocated it).
    let header = unsafe { read_header(header_paddr) };
    let count = header.data_page_count();

    // Free data pages (skip device MMIO pages — they are below RAM_START
    // and were never allocated from the buddy allocator).
    let ram_start = crate::mm::addr::ram_start().as_u64();
    for i in 0..count {
        if let Some(paddr) = header.get_page(i) {
            if paddr >= ram_start {
                page_alloc::dealloc_page(
                    crate::mm::addr::PhysPage::containing(PhysAddr::new(paddr))
                );
            }
        }
    }

    // Remove from global table
    consume_by_header_paddr(header_paddr);

    // Free header page
    page_alloc::dealloc_page(
        crate::mm::addr::PhysPage::containing(PhysAddr::new(header_paddr))
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
    /// handle table entry). Reads the count from the header itself.
    ///
    /// # Safety
    /// `header_paddr` must be a valid PageSetHeader page.
    pub unsafe fn from_header_paddr(header_paddr: u64) -> Self {
        let header = read_header(header_paddr);
        PageSetRef { count: header.data_page_count(), header_paddr }
    }

    /// Number of data pages in this PageSet.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Get the physical address of a data page by index.
    pub fn page(&self, index: usize) -> Option<PhysAddr> {
        // SAFETY: header_paddr came from a registered PageSet — valid kernel page.
        let header = unsafe { read_header(self.header_paddr) };
        header.get_page(index).map(PhysAddr::new)
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

