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
/// **Cacheable direct-map participation** (post C1 of the
/// cacheable-DMA migration — see
/// `docs/history/cacheable-dma-migration-plan.md`): the pool's 2 MiB L2
/// block participates in the kernel TTBR1 direct map as Normal
/// Cacheable Inner+Outer WB. Per-process user mappings of pool
/// pages are also Normal Cacheable, enforced by the rejection
/// matrix in `sys_map_pages` — DmaPool origin accepts ONLY
/// `Normal`; `NormalNonCacheable` and `Device` are rejected.
/// Single-attribute invariant preserved: pool pages are
/// Cacheable everywhere they are mapped. Coherence with devices
/// is maintained at the device-handoff points via the
/// `sys_dma_sync_for_cpu` / `sys_dma_sync_for_device` syscalls,
/// mirroring Linux's `dma_sync_for_cpu` /
/// `dma_sync_for_device` API.
///
/// `create_process` and donate-as-kernel-object continue to
/// reject DmaPool origin — not for alias reasons (the alias
/// bug is unreachable by construction), but because (a) the
/// pool is a tight 2 MiB reservation and stacks / scratch /
/// kernel objects would starve the actual DMA path, and
/// (b) those code paths have no sync-syscall handoff sites
/// and would silently slip the discipline. Pre-C1 history:
/// the pool was originally `NormalNonCacheable` everywhere
/// (M6 sub-commit 2a step 2, commit `10a01e8`) with the
/// kernel direct map excluding the pool's L2 block; that
/// exclusion is gone now.

use core::cell::UnsafeCell;
use lockjaw_types::addr::{PhysAddr, PAGE_SIZE};
use lockjaw_types::dma_pool::{DmaPool, DmaPoolError};

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

