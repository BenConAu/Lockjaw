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

/// Initialize the page allocator with a gap between the kernel image and
/// the per-CPU stacks. The 2 MB alignment of stacks creates a gap that
/// must be freed explicitly to avoid wasting physical memory.
///
/// Freed regions:
///   1. [kernel_end, stacks_start) — the alignment gap
///   2. [stacks_end, RAM_END)      — everything after the stacks
///
/// Reserved regions (never freed):
///   - [RAM_START, kernel_start)   — firmware, DTB
///   - [kernel_start, kernel_end)  — kernel image
///   - [stacks_start, stacks_end)  — per-CPU guard pages + stacks
///
/// # Safety
/// All addresses must be valid physical addresses from linker symbols.
pub unsafe fn init_with_gap(
    _kernel_start: PhysAddr,
    kernel_end: PhysAddr,
    stacks_start: PhysAddr,
    stacks_end: PhysAddr,
) {
    let buddy = &mut *ALLOCATOR.buddy.get();
    buddy.init(TOTAL_PAGES);

    // Round up to next page boundary
    let kernel_end_page = round_up_page(kernel_end);
    let stacks_start_page = page_index(stacks_start);
    let stacks_end_page = round_up_page(stacks_end);

    // Region 1: gap between kernel end and stacks start
    if stacks_start_page > kernel_end_page {
        let gap_count = stacks_start_page - kernel_end_page;
        buddy.add_range(kernel_end_page, gap_count);
    }

    // Region 2: everything after the stacks
    if TOTAL_PAGES > stacks_end_page {
        let post_count = TOTAL_PAGES - stacks_end_page;
        buddy.add_range(stacks_end_page, post_count);
    }

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

/// Page index rounded up (for end-of-region addresses).
fn round_up_page(addr: PhysAddr) -> usize {
    let idx = page_index(addr);
    if addr.as_u64() & (PAGE_SIZE - 1) != 0 { idx + 1 } else { idx }
}

/// Convert a page index back to a PhysPage.
fn index_to_page(idx: usize) -> PhysPage {
    PhysPage::containing(PhysAddr::new(RAM_START.as_u64() + (idx as u64) * PAGE_SIZE))
}
