use crate::mm::addr::{PhysAddr, PhysPage, RAM_START, TOTAL_PAGES, PAGE_SIZE};
use crate::mm::kernel_ptr::KernelMut;
use core::cell::UnsafeCell;

/// Bitmap size in bytes: 32768 pages / 8 bits per byte = 4096 bytes.
const BITMAP_SIZE: usize = (TOTAL_PAGES + 7) / 8;

// ---------------------------------------------------------------------------
// FrameAllocator singleton
// ---------------------------------------------------------------------------

/// The kernel's physical page allocator. A bitmap tracks which of the
/// 32768 pages in RAM are allocated/reserved. A next-fit hint accelerates
/// allocation. All state lives in `UnsafeCell`; the single `unsafe impl
/// Sync` documents the "single-core, IRQs masked" invariant in one place.
struct FrameAllocator {
    /// A set bit means allocated/reserved. Lives in BSS, zeroed at boot.
    bitmap: UnsafeCell<[u8; BITMAP_SIZE]>,
    /// Next-fit allocation hint — index of the next page to check.
    next_free_hint: UnsafeCell<usize>,
    /// Number of pages currently marked as reserved or allocated.
    allocated_count: UnsafeCell<usize>,
}

/// SAFETY: single-core kernel. Kernel entry masks IRQs; no concurrent
/// access to allocator state is possible. When SMP lands, replace with
/// a proper SpinMutex.
unsafe impl Sync for FrameAllocator {}

impl FrameAllocator {
    const fn new() -> Self {
        FrameAllocator {
            bitmap: UnsafeCell::new([0u8; BITMAP_SIZE]),
            next_free_hint: UnsafeCell::new(0),
            allocated_count: UnsafeCell::new(0),
        }
    }
}

static ALLOCATOR: FrameAllocator = FrameAllocator::new();

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
    *ALLOCATOR.next_free_hint.get() = kernel_end_page;

    let reserved = *ALLOCATOR.allocated_count.get();
    crate::kprintln!("  Page allocator: {} reserved, {} free",
        reserved, TOTAL_PAGES - reserved);
}

/// Allocate a single physical page. Returns `None` if out of memory.
pub fn alloc_page() -> Option<PhysPage> {
    // SAFETY: single-core, IRQs masked — exclusive access to allocator state.
    unsafe {
        let hint = ALLOCATOR.next_free_hint.get();
        let count = ALLOCATOR.allocated_count.get();
        let start = *hint;

        // Scan from hint to end
        for i in start..TOTAL_PAGES {
            if !is_set(i) {
                set_bit(i);
                *count += 1;
                *hint = i + 1;
                return Some(index_to_page(i));
            }
        }

        // Wrap around: scan from 0 to hint
        for i in 0..start {
            if !is_set(i) {
                set_bit(i);
                *count += 1;
                *hint = i + 1;
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
    // SAFETY: single-core, IRQs masked — exclusive access to allocator state.
    unsafe {
        let idx = page_index(page.start_addr());
        if !is_set(idx) {
            return false;
        }
        clear_bit(idx);
        *ALLOCATOR.allocated_count.get() -= 1;

        // Update hint if this page is lower
        let hint = ALLOCATOR.next_free_hint.get();
        if idx < *hint {
            *hint = idx;
        }
        true
    }
}

/// Zero a single 4KB page at the given physical address.
/// Safe because the kernel higher-half mapping covers all RAM.
pub fn zero_page(paddr: PhysAddr) {
    // SAFETY: paddr is a kernel-owned page (produced by alloc_page).
    let mut page = unsafe { KernelMut::<u8>::from_paddr(paddr) };
    // SAFETY: writing PAGE_SIZE zeroes into a kernel-mapped page.
    unsafe { core::ptr::write_bytes(page.as_mut_ptr(), 0, PAGE_SIZE as usize); }
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

/// Set a bit in the allocator bitmap. Must be called inside an unsafe block
/// that has established exclusive access to ALLOCATOR.
unsafe fn set_bit(idx: usize) {
    (*ALLOCATOR.bitmap.get())[idx / 8] |= 1 << (idx % 8);
}

/// Clear a bit in the allocator bitmap.
unsafe fn clear_bit(idx: usize) {
    (*ALLOCATOR.bitmap.get())[idx / 8] &= !(1 << (idx % 8));
}

/// Test whether a bit is set in the allocator bitmap.
unsafe fn is_set(idx: usize) -> bool {
    (*ALLOCATOR.bitmap.get())[idx / 8] & (1 << (idx % 8)) != 0
}

/// Mark a range of page indices as reserved.
unsafe fn mark_range_reserved(start: usize, end_exclusive: usize) {
    for i in start..end_exclusive {
        if !is_set(i) {
            set_bit(i);
            *ALLOCATOR.allocated_count.get() += 1;
        }
    }
}
