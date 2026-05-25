use crate::mm::addr::{PhysAddr, PhysPage, PAGE_SIZE};
use crate::mm::kernel_ptr::KernelMut;
use core::cell::UnsafeCell;
use lockjaw_types::buddy::BuddyAllocator;

// ---------------------------------------------------------------------------
// FrameAllocator singleton — backed by a buddy allocator
// ---------------------------------------------------------------------------

/// The kernel's physical page allocator. A buddy allocator tracks which
/// pages in RAM are free (up to 1GB), supporting both single-page and
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
///   - [crate::mm::addr::ram_start(), kernel_start)   — firmware, DTB
///   - [kernel_start, kernel_end)  — kernel image
///   - [stacks_start, stacks_end)  — per-CPU guard pages + stacks
///
/// Must be called exactly once during boot.
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
    buddy.init(crate::mm::addr::total_pages());

    // Round up to next page boundary
    let kernel_end_page = round_up_page(kernel_end);
    let stacks_start_page = page_index(stacks_start);
    let stacks_end_page = round_up_page(stacks_end);

    // Region 1: gap between kernel end and stacks start
    if stacks_start_page > kernel_end_page {
        let gap_count = stacks_start_page - kernel_end_page;
        buddy.add_range(kernel_end_page, gap_count);
    }

    // Region 2: everything after the stacks, MINUS the M6 DMA pool
    // (carved off the tail so it is never registered with buddy).
    // Post C1 of the cacheable-DMA migration the pool participates
    // in the kernel TTBR1 direct map as Normal Cacheable; the
    // pre-C1 L2-block exclusion is gone. The pool is still kept
    // out of buddy because it has its own allocator
    // (`dma_pool::alloc_pages`) — feeding pool pages into buddy
    // would double-issue them.
    //
    // The pool is exactly DMA_POOL_PAGES pages (one AArch64 L2
    // block = 2 MiB) and is anchored to the highest 2-MiB-aligned
    // PA boundary that fits below ram_end. The 2 MiB alignment is
    // historical: it was the precondition for the pre-C1 L2-only
    // exclusion. Post-C1 nothing requires the alignment, but it
    // costs nothing and keeps the pool's PA range easy to reason
    // about. Pool init is separate (`cap::dma_pool::init`).
    if crate::mm::addr::total_pages() > stacks_end_page {
        let dma_pool_pages = lockjaw_types::dma_pool::DMA_POOL_PAGES;
        let l2_block_pages = lockjaw_types::dma_pool::DMA_POOL_PAGES; // = pages-per-2MiB
        let ram_start_phys = crate::mm::addr::ram_start().as_u64();
        let ram_end_phys = crate::mm::addr::ram_end().as_u64();

        // Round ram_end down to a 2 MiB boundary; subtract the pool
        // size to get the pool's base PA. Both must lie within the
        // post-stacks region for the carve-out to be valid.
        let l2_size_bytes = (l2_block_pages as u64) * PAGE_SIZE;
        let pool_end_aligned = ram_end_phys & !(l2_size_bytes - 1);
        let pool_base_phys = pool_end_aligned.saturating_sub(l2_size_bytes);
        let pool_first_page = ((pool_base_phys - ram_start_phys) / PAGE_SIZE) as usize;
        let pool_last_page = pool_first_page + dma_pool_pages;

        if pool_first_page >= stacks_end_page
            && pool_last_page <= crate::mm::addr::total_pages()
        {
            // Buddy region 2a: stacks_end → pool_base (everything
            // before the pool, aligned to whatever fits).
            let pre_pool_count = pool_first_page - stacks_end_page;
            if pre_pool_count > 0 {
                buddy.add_range(stacks_end_page, pre_pool_count);
            }
            // Buddy region 2b: pool_end → ram_end (whatever tail is
            // beyond the aligned pool end, if any).
            let post_pool_count = crate::mm::addr::total_pages() - pool_last_page;
            if post_pool_count > 0 {
                buddy.add_range(pool_last_page, post_pool_count);
            }
            crate::cap::dma_pool::init(pool_base_phys);
        } else {
            // Tight RAM: pool can't fit aligned without starving
            // buddy entirely. Skip pool init — dma_pool::alloc_pages
            // returns Exhausted; sys_alloc_dma_pages → OUT_OF_MEMORY.
            // Should never happen on Pi 4B (≥2 GiB RAM) or QEMU virt
            // (≥1 GiB).
            let total_post = crate::mm::addr::total_pages() - stacks_end_page;
            buddy.add_range(stacks_end_page, total_post);
            crate::kprintln!("  WARNING: insufficient RAM for DMA pool, skipping");
        }
    }

    let reserved = crate::mm::addr::total_pages() - buddy.free_count();
    crate::kprintln!("  Page allocator: ", reserved, " reserved, ", buddy.free_count(), " free");
}

/// Number of free physical pages currently in the allocator.
/// Used by diagnostic / self-test code to detect leaks.
pub fn free_count() -> usize {
    // SAFETY: single-core, IRQs masked — exclusive access to allocator state.
    unsafe {
        let buddy = &*ALLOCATOR.buddy.get();
        buddy.free_count()
    }
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

/// Convert a physical address to a page index relative to crate::mm::addr::ram_start().
fn page_index(addr: PhysAddr) -> usize {
    ((addr.as_u64() - crate::mm::addr::ram_start().as_u64()) / PAGE_SIZE) as usize
}

/// Page index rounded up (for end-of-region addresses).
fn round_up_page(addr: PhysAddr) -> usize {
    let idx = page_index(addr);
    if addr.as_u64() & (PAGE_SIZE - 1) != 0 { idx + 1 } else { idx }
}

/// Convert a page index back to a PhysPage.
fn index_to_page(idx: usize) -> PhysPage {
    PhysPage::containing(PhysAddr::new(crate::mm::addr::ram_start().as_u64() + (idx as u64) * PAGE_SIZE))
}
