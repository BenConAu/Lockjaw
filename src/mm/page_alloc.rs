use crate::mm::addr::{PhysAddr, PhysPage, RAM_START, TOTAL_PAGES, PAGE_SIZE};
use crate::mm::kernel_ptr::KernelMut;
use core::cell::UnsafeCell;
use lockjaw_types::buddy::BuddyAllocator;

// ---------------------------------------------------------------------------
// FrameAllocator singleton — backed by a buddy allocator
// ---------------------------------------------------------------------------

/// The kernel's physical page allocator. A buddy allocator tracks which
/// of the 32768 pages in RAM are free, supporting both single-page and
/// contiguous multi-page allocation. All state lives in `UnsafeCell`;
/// the single `unsafe impl Sync` documents the "single-core, IRQs
/// masked" invariant in one place.
struct FrameAllocator {
    buddy: UnsafeCell<BuddyAllocator>,
}

/// SAFETY: single-core kernel. Kernel entry masks IRQs; no concurrent
/// access to allocator state is possible. When SMP lands, replace with
/// a proper SpinMutex.
unsafe impl Sync for FrameAllocator {}

impl FrameAllocator {
    const fn new() -> Self {
        FrameAllocator {
            buddy: UnsafeCell::new(BuddyAllocator::new()),
        }
    }
}

static ALLOCATOR: FrameAllocator = FrameAllocator::new();

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialize the page allocator. Marks firmware, kernel image, and stack
/// pages as reserved by only freeing pages above the kernel. Must be
/// called exactly once during boot.
///
/// # Safety
/// `kernel_start` and `kernel_end` must be valid physical addresses bounding
/// the kernel image (including stack). Typically derived from linker symbols.
pub unsafe fn init(_kernel_start: PhysAddr, kernel_end: PhysAddr) {
    let buddy = &mut *ALLOCATOR.buddy.get();
    buddy.init(TOTAL_PAGES);

    // Round kernel_end up to the next page boundary.
    let kernel_end_page = {
        let idx = page_index(kernel_end);
        if kernel_end.as_u64() & (PAGE_SIZE - 1) != 0 { idx + 1 } else { idx }
    };

    // Free all pages above the kernel. Pages below (firmware, DTB,
    // kernel image, stack) stay allocated (never added to the buddy).
    let free_start = kernel_end_page;
    let free_count = TOTAL_PAGES - free_start;
    buddy.add_range(free_start, free_count);

    let reserved = TOTAL_PAGES - buddy.free_count();
    crate::kprintln!("  Page allocator: {} reserved, {} free",
        reserved, buddy.free_count());
}

/// Allocate a single physical page. Returns `None` if out of memory.
pub fn alloc_page() -> Option<PhysPage> {
    // SAFETY: single-core, IRQs masked — exclusive access to allocator state.
    unsafe {
        let buddy = &mut *ALLOCATOR.buddy.get();
        buddy.alloc(0).map(index_to_page)
    }
}

/// Allocate `count` physically contiguous pages. Returns the first page
/// of the contiguous block, or `None` if no sufficiently large block is
/// available. The block size is rounded up to the next power of two.
pub fn alloc_pages_contiguous(count: usize) -> Option<PhysPage> {
    if count == 0 {
        return None;
    }
    let order = BuddyAllocator::order_for_count(count);
    // SAFETY: single-core, IRQs masked — exclusive access to allocator state.
    unsafe {
        let buddy = &mut *ALLOCATOR.buddy.get();
        buddy.alloc(order).map(index_to_page)
    }
}

/// Free a previously allocated single page. Panics on double-free.
pub fn dealloc_page(page: PhysPage) {
    // SAFETY: single-core, IRQs masked — exclusive access to allocator state.
    unsafe {
        let buddy = &mut *ALLOCATOR.buddy.get();
        buddy.free(page_index(page.start_addr()), 0);
    }
}

/// Free `count` contiguous pages starting at `first_page`. The count
/// is rounded up to the same power-of-two order used by
/// `alloc_pages_contiguous`.
pub fn dealloc_pages_contiguous(first_page: PhysPage, count: usize) {
    if count == 0 { return; }
    let order = BuddyAllocator::order_for_count(count);
    // SAFETY: single-core, IRQs masked — exclusive access to allocator state.
    unsafe {
        let buddy = &mut *ALLOCATOR.buddy.get();
        buddy.free(page_index(first_page.start_addr()), order);
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
