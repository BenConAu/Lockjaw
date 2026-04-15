use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use crate::mm::page_alloc;
use core::cell::UnsafeCell;
use lockjaw_types::pageset_table::{PageSetTable, PageSetEntry, PageSetHeader, MAX_PAGES_PER_SET};

/// The kernel's global PageSet tracking table.
/// Wraps the pure PageSetTable model from lockjaw-types with the actual
/// page allocator for allocation and deallocation.
///
/// UnsafeCell instead of static mut to avoid Rust 2024 reference UB warnings.
/// Safety: single-core kernel, no concurrent access during a syscall.
struct SyncTable(UnsafeCell<PageSetTable>);
unsafe impl Sync for SyncTable {}

static TABLE: SyncTable = SyncTable(UnsafeCell::new(PageSetTable::new()));

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

    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let header_va = (header_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut PageSetHeader;

    // Allocate data pages one at a time, writing each address directly into the header
    unsafe {
        for i in 0..count {
            match page_alloc::alloc_page() {
                Some(page) => {
                    (*header_va).pages[i] = page.start_addr().as_u64();
                }
                None => {
                    // Roll back: free data pages allocated so far + header
                    for j in 0..i {
                        page_alloc::dealloc_page(
                            crate::mm::addr::PhysPage::containing(PhysAddr::new((*header_va).pages[j]))
                        );
                    }
                    page_alloc::dealloc_page(crate::mm::addr::PhysPage::containing(header_paddr));
                    return None;
                }
            }
        }
        (*header_va).count = count as u64;
    }

    // Insert thin entry into the table
    let entry = PageSetEntry { count, header_paddr: header_paddr.as_u64() };
    let result = unsafe { (*TABLE.0.get()).insert(entry) };
    match result {
        Ok(id) => Some(id as u64),
        Err(_) => {
            // Table full — free all pages (data + header)
            unsafe {
                for j in 0..count {
                    page_alloc::dealloc_page(
                        crate::mm::addr::PhysPage::containing(PhysAddr::new((*header_va).pages[j]))
                    );
                }
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

    // Write the header
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let header_va = (header_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut PageSetHeader;
    unsafe {
        core::ptr::write_bytes(header_va, 0, 1);
        let header = &mut *header_va;
        // SAFETY: PhysAddr is repr(transparent) over u64, same layout
        let addrs: &[u64] = core::slice::from_raw_parts(
            pages.as_ptr() as *const u64,
            count,
        );
        header.init(addrs);
    }

    let entry = PageSetEntry { count, header_paddr: header_paddr.as_u64() };
    unsafe {
        (*TABLE.0.get()).insert(entry).ok().map(|id| id as u64)
    }
}

/// Wrap a physical MMIO address as a 1-page PageSet (no allocation from pool, just tracking).
/// Allocates one header page to store the MMIO address.
pub fn register_device_page(phys_addr: u64) -> Option<u64> {
    let header_page = page_alloc::alloc_page()?;
    let header_paddr = header_page.start_addr();

    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let header_va = (header_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut PageSetHeader;
    unsafe {
        core::ptr::write_bytes(header_va, 0, 1);
        let header = &mut *header_va;
        header.init(&[phys_addr]);
    }

    let entry = PageSetEntry { count: 1, header_paddr: header_paddr.as_u64() };
    unsafe {
        (*TABLE.0.get()).insert(entry).ok().map(|id| id as u64)
    }
}

/// Remove a PageSet from the table, preventing reuse.
/// Called after a PageSet's pages are donated to a kernel object.
pub fn consume_pageset(id: u64) -> bool {
    unsafe {
        (*TABLE.0.get()).remove(id as usize).is_ok()
    }
}

/// Look up a PageSet by ID. Returns the data page count and header physical address.
/// Callers read individual page addresses from the header via read_header().
pub fn get_pageset(id: u64) -> Option<(usize, u64)> {
    unsafe {
        (*TABLE.0.get()).get(id as usize).ok().map(|entry| (entry.count, entry.header_paddr))
    }
}

/// Read the PageSetHeader from a header page. Returns a reference valid for
/// the lifetime of the call (header is in a kernel-mapped page).
///
/// # Safety
/// `header_paddr` must be a valid header page physical address.
pub unsafe fn read_header(header_paddr: u64) -> &'static PageSetHeader {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let va = (header_paddr + KERNEL_VA_OFFSET) as *const PageSetHeader;
    &*va
}
