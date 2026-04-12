use crate::mm::addr::PhysAddr;
use crate::mm::page_alloc;
use lockjaw_types::pageset_table::{PageSetTable, PageSetEntry, MAX_PAGES_PER_SET};

/// The kernel's global PageSet tracking table.
/// Wraps the pure PageSetTable model from lockjaw-types with the actual
/// page allocator for allocation and deallocation.
static mut TABLE: PageSetTable = PageSetTable::new();

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
    unsafe {
        TABLE.insert(entry).ok().map(|id| id as u64)
    }
}

/// Look up a PageSet by ID. Returns the page count and physical addresses.
pub fn get_pageset(id: u64) -> Option<(usize, [PhysAddr; MAX_PAGES_PER_SET])> {
    unsafe {
        TABLE.get(id as usize).ok().map(|entry| (entry.count, entry.pages))
    }
}
