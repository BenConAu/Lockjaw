use crate::mm::addr::PhysAddr;
use crate::mm::page_alloc;

/// Maximum number of PageSets tracked system-wide.
/// Known limitation: no per-process tracking, no cleanup on process death.
/// The eventual design is for PageSets to be proper kernel objects in
/// per-process handle tables. See docs/object-model.md and the Phase 8
/// plan for the upgrade path.
const MAX_PAGESETS: usize = 32;

/// Maximum pages per PageSet.
const MAX_PAGES_PER_SET: usize = 16;

/// A tracked PageSet: a set of physical pages allocated together.
#[derive(Clone, Copy)]
struct PageSetEntry {
    count: usize,
    pages: [PhysAddr; MAX_PAGES_PER_SET],
}

/// Static PageSet tracking table.
static mut TABLE: [Option<PageSetEntry>; MAX_PAGESETS] = [None; MAX_PAGESETS];

/// Allocate `count` physical pages and return a PageSet ID.
/// Returns None if out of memory, too many pages requested, or table full.
pub fn alloc_pages(count: usize) -> Option<u64> {
    if count == 0 || count > MAX_PAGES_PER_SET {
        return None;
    }

    unsafe {
        // Find an empty slot
        let slot = TABLE.iter().position(|s| s.is_none())?;

        // Allocate the pages
        let mut entry = PageSetEntry {
            count,
            pages: [PhysAddr::new(0); MAX_PAGES_PER_SET],
        };

        for i in 0..count {
            match page_alloc::alloc_page() {
                Some(page) => entry.pages[i] = page.start_addr(),
                None => {
                    // Roll back: free any pages we already allocated
                    for j in 0..i {
                        page_alloc::dealloc_page(
                            crate::mm::addr::PhysPage::containing(entry.pages[j])
                        );
                    }
                    return None;
                }
            }
        }

        TABLE[slot] = Some(entry);
        Some(slot as u64)
    }
}

/// Look up a PageSet by ID. Returns the page count and physical addresses.
pub fn get_pageset(id: u64) -> Option<(usize, [PhysAddr; MAX_PAGES_PER_SET])> {
    unsafe {
        let idx = id as usize;
        if idx >= MAX_PAGESETS {
            return None;
        }
        TABLE[idx].map(|entry| (entry.count, entry.pages))
    }
}
