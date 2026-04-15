use crate::mm::addr::PhysAddr;
use crate::mm::page_alloc;
use core::cell::UnsafeCell;
use lockjaw_types::pageset_table::{PageSetTable, PageSetEntry, MAX_PAGES_PER_SET};

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
/// Returns the PageSet ID, or `None` if out of memory, too many pages, or table full.
pub fn alloc_pages(count: usize) -> Option<u64> {
    // Allocate the physical pages
    let mut pages = [PhysAddr::new(0); MAX_PAGES_PER_SET];
    for i in 0..count {
        match page_alloc::alloc_page() {
            Some(page) => pages[i] = page.start_addr(),
            None => {
                // Roll back: free any pages we already allocated
                for j in 0..i {
                    page_alloc::dealloc_page(
                        crate::mm::addr::PhysPage::containing(pages[j])
                    );
                }
                return None;
            }
        }
    }

    // Insert into the table (pure logic, validated by unit tests)
    let entry = PageSetEntry { count, pages };
    let result = unsafe { (*TABLE.0.get()).insert(entry) };
    match result {
        Ok(id) => Some(id as u64),
        Err(_) => {
            // Table full — free the pages we allocated so they aren't leaked
            for j in 0..count {
                page_alloc::dealloc_page(
                    crate::mm::addr::PhysPage::containing(pages[j])
                );
            }
            None
        }
    }
}

/// Register a PageSet for existing physical pages (not from the allocator).
/// Used at boot to wrap the DTB pages placed by QEMU firmware.
pub fn register_existing(count: usize, pages: [PhysAddr; MAX_PAGES_PER_SET]) -> Option<u64> {
    let entry = PageSetEntry { count, pages };
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

/// Look up a PageSet by ID. Returns the page count and physical addresses.
pub fn get_pageset(id: u64) -> Option<(usize, [PhysAddr; MAX_PAGES_PER_SET])> {
    unsafe {
        (*TABLE.0.get()).get(id as usize).ok().map(|entry| (entry.count, entry.pages))
    }
}
