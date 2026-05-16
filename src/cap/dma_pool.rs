/// Kernel-side singleton wrapping the pure `DmaPool` allocator.
///
/// Pool lives at a fixed physical-address range carved out of buddy's
/// free-list at boot (see `dma_pool::init`). Pages are handed out
/// contiguously via `alloc_pages` and returned via `free_pages`. The
/// pool tracks "page index within pool" internally; the public API
/// returns / accepts `PhysAddr`.
///
/// Threading: single-core, IRQs masked — same `UnsafeCell` pattern as
/// `page_alloc.rs`. Replace with SpinMutex when SMP lands.
///
/// **Direct-map note**: sub-commit 2a does NOT yet exclude pool PAs
/// from the TTBR1 direct map. The structural alias prevention comes
/// from (1) pool pages never being registered with buddy and (2) the
/// kernel rejecting DmaPool-origin PageSets in every cacheable-
/// mapping path. Speculative CPU caching via the direct-map block
/// descriptor remains theoretically possible; the direct-map
/// exclusion lands as a follow-up commit before sub-commit 2b
/// exercises the pool from a real driver.

use core::cell::UnsafeCell;
use lockjaw_types::addr::{PhysAddr, PAGE_SIZE};
use lockjaw_types::dma_pool::{DmaPool, DmaPoolError, DMA_POOL_PAGES};

/// Wraps DmaPool + the pool's physical base address (set at boot).
/// `base_phys = 0` is the "not yet initialised" sentinel — alloc
/// before init panics.
struct DmaPoolSingleton {
    pool: UnsafeCell<DmaPool>,
    base_phys: UnsafeCell<u64>,
}

/// SAFETY: single-core kernel, IRQs masked at all caller sites.
unsafe impl Sync for DmaPoolSingleton {}

impl DmaPoolSingleton {
    const fn new() -> Self {
        Self {
            pool: UnsafeCell::new(DmaPool::empty()),
            base_phys: UnsafeCell::new(0),
        }
    }
}

static POOL: DmaPoolSingleton = DmaPoolSingleton::new();

/// Initialize the pool with its physical base address. Called once at
/// boot, after `page_alloc::init_with_gap` has finished registering
/// buddy's free ranges (so we know what's available). The caller is
/// responsible for ensuring `base_phys..base_phys + DMA_POOL_PAGES *
/// PAGE_SIZE` is NOT added to buddy.
///
/// # Safety
/// Must be called exactly once during boot. `base_phys` must be
/// page-aligned and the full pool range must be valid RAM that the
/// buddy allocator does not own.
pub unsafe fn init(base_phys: u64) {
    assert_eq!(base_phys & (PAGE_SIZE - 1), 0, "dma_pool::init: base not page-aligned");
    *POOL.base_phys.get() = base_phys;
}

/// Pool base physical address, or 0 if `init` hasn't run yet. Used by
/// the direct-map-exclusion code (to be added in a follow-up commit)
/// to skip the pool's range when building the kernel L2 table.
pub fn base_phys() -> u64 {
    // SAFETY: single-core read.
    unsafe { *POOL.base_phys.get() }
}

/// Total pool size in bytes.
pub const fn size_bytes() -> u64 {
    (DMA_POOL_PAGES as u64) * PAGE_SIZE
}

/// Allocate `count` physically-contiguous pages from the pool.
/// Returns the physical address of the first page on success.
///
/// Fails with `DmaPoolError::Exhausted` if no contiguous run is
/// available, or `InvalidCount` if `count == 0` or
/// `count > DMA_POOL_PAGES`.
pub fn alloc_pages(count: usize) -> Result<PhysAddr, DmaPoolError> {
    // SAFETY: single-core, IRQs masked.
    unsafe {
        let base = *POOL.base_phys.get();
        if base == 0 {
            // Pool was not initialised at boot — tight-RAM platforms
            // skip the carve-out (page_alloc::init_with_gap). Surface
            // as Exhausted so the syscall layer maps to OUT_OF_MEMORY.
            return Err(DmaPoolError::Exhausted);
        }
        let pool = &mut *POOL.pool.get();
        let idx = pool.alloc(count)?;
        Ok(PhysAddr::new(base + (idx as u64) * PAGE_SIZE))
    }
}

/// Free `count` contiguous pages starting at `first_phys` back to the
/// pool. Panics if `first_phys` is outside the pool or if any of the
/// pages weren't allocated (caught by the pure layer's double-free
/// assertion).
pub fn free_pages(first_phys: PhysAddr, count: usize) {
    // SAFETY: single-core, IRQs masked.
    unsafe {
        let base = *POOL.base_phys.get();
        assert!(base != 0, "dma_pool::free_pages before init");
        let offset = first_phys.as_u64().checked_sub(base)
            .expect("dma_pool::free_pages: paddr below pool base");
        assert_eq!(offset & (PAGE_SIZE - 1), 0,
            "dma_pool::free_pages: paddr not page-aligned");
        let idx = (offset / PAGE_SIZE) as usize;
        let pool = &mut *POOL.pool.get();
        pool.free(idx, count);
    }
}

/// Diagnostic: number of pages currently allocated from the pool.
pub fn allocated_pages() -> usize {
    // SAFETY: single-core read.
    unsafe { (*POOL.pool.get()).allocated_pages() }
}
