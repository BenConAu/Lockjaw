use crate::mm::addr::{PhysAddr, PhysPage, RAM_START, TOTAL_PAGES, PAGE_SIZE};

/// Bitmap size in bytes: 32768 pages / 8 bits per byte = 4096 bytes.
const BITMAP_SIZE: usize = (TOTAL_PAGES + 7) / 8;

/// Static bitmap — lives in BSS, zeroed at boot. A set bit means allocated/reserved.
///
/// Safety: single-threaded access only. Must be wrapped in a lock once
/// interrupts or scheduling are introduced.
static mut BITMAP: [u8; BITMAP_SIZE] = [0u8; BITMAP_SIZE];

/// Hint for next-fit allocation — index of the next page to check.
static mut NEXT_FREE_HINT: usize = 0;

/// Number of pages currently marked as reserved or allocated.
static mut ALLOCATED_COUNT: usize = 0;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialize the page allocator. Marks firmware, kernel image, and stack
/// pages as reserved. Must be called exactly once during boot.
///
/// # Safety
/// `kernel_start` and `kernel_end` must be valid physical addresses bounding
/// the kernel image (including stack). Typically derived from linker symbols.
pub unsafe fn init(kernel_start: PhysAddr, kernel_end: PhysAddr) {
    // Reserve pages below the kernel load address (firmware, DTB, etc.)
    let firmware_end_page = page_index(kernel_start);
    mark_range_reserved(0, firmware_end_page);

    // Reserve kernel image + stack pages
    let kernel_start_page = page_index(kernel_start);
    let kernel_end_page = page_index(kernel_end);
    // Round up in case kernel_end is not page-aligned
    let kernel_end_page = if kernel_end.as_u64() & (PAGE_SIZE - 1) != 0 {
        kernel_end_page + 1
    } else {
        kernel_end_page
    };
    mark_range_reserved(kernel_start_page, kernel_end_page);

    // Set hint past all reserved pages
    NEXT_FREE_HINT = kernel_end_page;

    let reserved = ALLOCATED_COUNT;
    crate::kprintln!("  Page allocator: {} reserved, {} free",
        reserved, TOTAL_PAGES - reserved);
}

/// Allocate a single physical page. Returns `None` if out of memory.
pub fn alloc_page() -> Option<PhysPage> {
    unsafe {
        let start = NEXT_FREE_HINT;

        // Scan from hint to end
        for i in start..TOTAL_PAGES {
            if !is_set(i) {
                set_bit(i);
                ALLOCATED_COUNT += 1;
                NEXT_FREE_HINT = i + 1;
                return Some(index_to_page(i));
            }
        }

        // Wrap around: scan from 0 to hint
        for i in 0..start {
            if !is_set(i) {
                set_bit(i);
                ALLOCATED_COUNT += 1;
                NEXT_FREE_HINT = i + 1;
                return Some(index_to_page(i));
            }
        }

        None
    }
}

/// Free a previously allocated page. Returns false on double-free
/// (page was not allocated). Callers should treat double-free as a
/// kernel bug and panic — but the decision is theirs.
pub fn dealloc_page(page: PhysPage) -> bool {
    unsafe {
        let idx = page_index(page.start_addr());
        if !is_set(idx) {
            return false;
        }
        clear_bit(idx);
        ALLOCATED_COUNT -= 1;

        // Update hint if this page is lower
        if idx < NEXT_FREE_HINT {
            NEXT_FREE_HINT = idx;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a physical address to a page index relative to RAM_START.
fn page_index(addr: PhysAddr) -> usize {
    ((addr.as_u64() - RAM_START.as_u64()) / PAGE_SIZE) as usize
}

/// Convert a page index back to a PhysPage.
fn index_to_page(idx: usize) -> PhysPage {
    PhysPage::containing(PhysAddr::new(RAM_START.as_u64() + (idx as u64) * PAGE_SIZE))
}

fn set_bit(idx: usize) {
    unsafe { BITMAP[idx / 8] |= 1 << (idx % 8); }
}

fn clear_bit(idx: usize) {
    unsafe { BITMAP[idx / 8] &= !(1 << (idx % 8)); }
}

fn is_set(idx: usize) -> bool {
    unsafe { BITMAP[idx / 8] & (1 << (idx % 8)) != 0 }
}

fn mark_range_reserved(start: usize, end_exclusive: usize) {
    unsafe {
        for i in start..end_exclusive {
            if !is_set(i) {
                set_bit(i);
                ALLOCATED_COUNT += 1;
            }
        }
    }
}
