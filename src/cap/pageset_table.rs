use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::KernelMut;
use crate::mm::page_alloc;
use core::cell::UnsafeCell;
use lockjaw_types::pageset_table::{PageSetTable, PageSetEntry, PageSetHeader, MAX_PAGES_PER_SET};

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

    // Allocate the header page first
    let header_page = page_alloc::alloc_page()?;
    let header_paddr = header_page.start_addr();
    page_alloc::zero_page(header_paddr);

    // SAFETY: header_paddr is a freshly allocated, zeroed kernel page.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_paddr(header_paddr) };

    // Allocate data pages one at a time, writing each address directly into the header
    for i in 0..count {
        match page_alloc::alloc_page() {
            Some(page) => {
                header_ref.get_mut().pages[i] = page.start_addr().as_u64();
            }
            None => {
                // Roll back: free data pages allocated so far + header
                for j in 0..i {
                    page_alloc::dealloc_page(
                        crate::mm::addr::PhysPage::containing(PhysAddr::new(header_ref.get().pages[j]))
                    );
                }
                page_alloc::dealloc_page(crate::mm::addr::PhysPage::containing(header_paddr));
                return None;
            }
        }
    }
    header_ref.get_mut().count = count as u64;

    // Insert thin entry into the table
    let entry = PageSetEntry { count, header_paddr: header_paddr.as_u64() };
    // SAFETY: single-core, IRQs masked — exclusive table access.
    let result = unsafe { (*TABLE.ptr()).insert(entry) };
    match result {
        Ok(id) => Some(id as u64),
        Err(_) => {
            // Table full — free all pages (data + header)
            for j in 0..count {
                page_alloc::dealloc_page(
                    crate::mm::addr::PhysPage::containing(PhysAddr::new(header_ref.get().pages[j]))
                );
            }
            page_alloc::dealloc_page(crate::mm::addr::PhysPage::containing(header_paddr));
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

    // Allocate a header page
    let header_page = page_alloc::alloc_page()?;
    let header_paddr = header_page.start_addr();

    // SAFETY: header_paddr is a freshly allocated kernel page.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_paddr(header_paddr) };
    // Zero the header before init
    unsafe { core::ptr::write_bytes(header_ref.as_mut_ptr(), 0, 1); }
    // SAFETY: PhysAddr is repr(transparent) over u64, same layout.
    let addrs: &[u64] = unsafe {
        core::slice::from_raw_parts(
            // SAFETY: PhysAddr → u64 transmute via repr(transparent)
            pages.as_ptr() as *const u64,
            count,
        )
    };
    header_ref.get_mut().init(addrs);

    let entry = PageSetEntry { count, header_paddr: header_paddr.as_u64() };
    // SAFETY: single-core, IRQs masked — exclusive table access.
    unsafe {
        (*TABLE.ptr()).insert(entry).ok().map(|id| id as u64)
    }
}

/// Wrap a physical MMIO address as a 1-page PageSet (no allocation from pool, just tracking).
/// Allocates one header page to store the MMIO address.
pub fn register_device_page(phys_addr: u64) -> Option<u64> {
    let header_page = page_alloc::alloc_page()?;
    let header_paddr = header_page.start_addr();

    // SAFETY: header_paddr is a freshly allocated kernel page.
    let mut header_ref = unsafe { KernelMut::<PageSetHeader>::from_paddr(header_paddr) };
    unsafe { core::ptr::write_bytes(header_ref.as_mut_ptr(), 0, 1); }
    header_ref.get_mut().init(&[phys_addr]);

    let entry = PageSetEntry { count: 1, header_paddr: header_paddr.as_u64() };
    // SAFETY: single-core, IRQs masked — exclusive table access.
    unsafe {
        (*TABLE.ptr()).insert(entry).ok().map(|id| id as u64)
    }
}

/// Remove a PageSet from the table, preventing reuse.
/// Called after a PageSet's pages are donated to a kernel object.
pub fn consume_pageset(id: u64) -> bool {
    // SAFETY: single-core, IRQs masked — exclusive table access.
    unsafe {
        (*TABLE.ptr()).remove(id as usize).is_ok()
    }
}

/// Look up a PageSet by ID. Returns the data page count and header physical address.
/// Prefer `donate_single_page` for the common 1-page-consume pattern.
pub fn get_pageset(id: u64) -> Option<(usize, u64)> {
    // SAFETY: single-core, IRQs masked — read-only table access.
    unsafe {
        (*TABLE.ptr()).get(id as usize).ok().map(|entry| (entry.count, entry.header_paddr))
    }
}

/// Read the PageSetHeader from a header page.
///
/// # Safety
/// `header_paddr` must be a valid header page physical address.
pub unsafe fn read_header(header_paddr: u64) -> &'static PageSetHeader {
    let header = KernelMut::<PageSetHeader>::from_paddr(PhysAddr::new(header_paddr));
    // Extend lifetime to 'static — the header lives in a kernel-owned page
    // that persists for the life of the PageSet. Callers must not hold the
    // reference past consume_pageset().
    &*header.as_ptr()
}

/// Validate that a PageSet is exactly 1 page and return its physical
/// address wrapped in a [`ObjectInitPage`]. Folds the
/// get → validate → read_header → get_page sequence into one safe call,
/// eliminating raw `u64` header_paddr and `unsafe read_header` from callers.
///
/// Does NOT consume the PageSet — the caller must call
/// [`consume_pageset`] after successful use. This preserves rollback
/// semantics: if the factory that receives the `ObjectInitPage` fails, the
/// PageSet is still live and can be reclaimed.
///
/// Returns distinct errors for "invalid PageSet ID" vs "wrong page count"
/// so syscall handlers can propagate the right error code to userspace.
pub fn donate_single_page(pageset_id: u64) -> Result<crate::mm::addr::ObjectInitPage, lockjaw_types::syscall::SyscallError> {
    use lockjaw_types::syscall::SyscallError;
    let (count, header_paddr) = get_pageset(pageset_id)
        .ok_or(SyscallError::INVALID_HANDLE)?;
    if count != 1 {
        return Err(SyscallError::INVALID_PARAMETER);
    }
    // SAFETY: header_paddr came from a registered PageSet — valid kernel page.
    let header = unsafe { read_header(header_paddr) };
    let paddr = PhysAddr::new(header.get_page(0)
        .ok_or(SyscallError::INVALID_HANDLE)?);
    // SAFETY: the page is kernel-allocated via the page allocator;
    // ObjectInitPage just wraps the paddr in a typed newtype.
    Ok(unsafe { crate::mm::addr::ObjectInitPage::new(paddr) })
}
