/// DMA pool allocator — pure model.
///
/// The DMA pool is a physically-contiguous region of RAM reserved at
/// kernel boot. Post-C1 of the cacheable-DMA migration (see
/// `docs/history/cacheable-dma-migration-plan.md`) it participates in the
/// kernel TTBR1 direct map as Cacheable Inner+Outer WB and user
/// processes map it Cacheable as well (single-attribute invariant
/// preserved: Cacheable everywhere). Coherence with devices is
/// maintained via `sys_dma_sync_for_cpu` (`dc civac` —
/// clean-and-invalidate before CPU reads device-written data — see
/// docs/history/post-c1-fix-plan.md §B2.1 for why civac, not ivac) and
/// `sys_dma_sync_for_device` (`dc cvac` — clean before device reads
/// CPU-written data) at handoff points, mirroring Linux's
/// `dma_sync_for_cpu` / `dma_sync_for_device` API.
///
/// Pre-C1 history: the pool was originally `NormalNonCacheable`
/// EVERYWHERE (kernel direct map excluded the pool's L2 block; user
/// mappings forced to NC). The rejection matrix that enforced this
/// invariant still exists, just with the chosen attribute flipped
/// to Cacheable. See M6 sub-commit 2a step 2 (commit `10a01e8`) for
/// the original alias-safety design rationale, and the migration
/// plan for the Cacheable trade-off (Linux/U-Boot pattern alignment).
/// **Device DMA-write drain is the DEVICE's responsibility**,
/// signalled by its completion interrupt/status — NOT a CPU-cache
/// primitive side effect. A pre-B2.1 paragraph here cited "AXI-drain
/// via `dc ivac` bus-protocol side effect" as the migration's drain
/// mechanism; that was wrong (see `src/arch/aarch64/cache.rs`
/// module doc for the corrected explanation).
///
/// **All numerical constants live here**; the kernel module
/// (`src/cap/dma_pool.rs`) wraps this with a locked singleton and
/// MMIO-level page allocation.
///
/// **Allocator design**: bitmap, one bit per page, contiguous
/// first-fit. Pool fixed at boot; no resize. Allocations rounded to
/// power-of-two are NOT required (ADMA2 just needs *contiguous*
/// pages for the descriptor table → buffer base linkage).
///
/// Pool sized at 1024 pages (4 MiB) — large enough for several
/// concurrent block-IO transfers' worth of descriptor tables +
/// buffers, small enough to fit even a tight Pi 4B boot reservation
/// without ceremony.
///
/// Pure; no MMIO, no static state, no `unsafe`. Host-tested.

/// Number of 4 KiB pages reserved for the DMA pool. Pinned constant —
/// changes here must update the kernel reservation in
/// `src/cap/dma_pool.rs` and any failing host tests.
///
/// **Exactly 512 pages = 2 MiB = one AArch64 L2 block**. The kernel's
/// TTBR1 direct-map exclusion (M6) carves out the pool's L2 block
/// descriptor and leaves it invalid; sizing the pool to one L2 block
/// means the exclusion is a single PTE clear, no L3-split work. If
/// this constant grows, the MMU init code must split additional L2
/// blocks accordingly.
pub const DMA_POOL_PAGES: usize = 512;

/// Bitmap word count (1 bit per page).
const BITMAP_WORDS: usize = DMA_POOL_PAGES / 64;

/// Allocator state: bitmap with one bit per pool page (1 = allocated,
/// 0 = free). Wrapped by the kernel-side singleton with a lock.
#[derive(Clone)]
pub struct DmaPool {
    bitmap: [u64; BITMAP_WORDS],
    /// Cached count of allocated pages (sum of set bits in `bitmap`).
    /// Maintained on alloc/free so callers can query without an O(N)
    /// popcount walk.
    allocated_pages: usize,
}

/// Failure modes from `DmaPool::alloc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaPoolError {
    /// Requested count of 0 or > DMA_POOL_PAGES.
    InvalidCount,
    /// No contiguous run of `count` free pages exists in the pool.
    Exhausted,
}

impl DmaPool {
    /// Construct an empty pool — all pages free.
    pub const fn empty() -> Self {
        Self {
            bitmap: [0; BITMAP_WORDS],
            allocated_pages: 0,
        }
    }

    /// Total number of pages in the pool.
    pub const fn total_pages(&self) -> usize {
        DMA_POOL_PAGES
    }

    /// Currently allocated page count.
    pub const fn allocated_pages(&self) -> usize {
        self.allocated_pages
    }

    /// Currently free page count.
    pub const fn free_pages(&self) -> usize {
        DMA_POOL_PAGES - self.allocated_pages
    }

    /// Allocate `count` contiguous pages. Returns the starting page
    /// index within the pool (0..DMA_POOL_PAGES) on success. The
    /// caller maps this index to a physical address by multiplying by
    /// PAGE_SIZE and adding the pool's physical base.
    ///
    /// First-fit search. O(DMA_POOL_PAGES) worst case; trivially fast
    /// at the 1024-page pool size.
    pub fn alloc(&mut self, count: usize) -> Result<usize, DmaPoolError> {
        if count == 0 || count > DMA_POOL_PAGES {
            return Err(DmaPoolError::InvalidCount);
        }
        // Linear scan for `count` consecutive zero bits.
        let mut run_start: Option<usize> = None;
        let mut run_len: usize = 0;
        for i in 0..DMA_POOL_PAGES {
            if self.bit(i) {
                run_start = None;
                run_len = 0;
                continue;
            }
            if run_start.is_none() {
                run_start = Some(i);
            }
            run_len += 1;
            if run_len == count {
                let start = run_start.unwrap();
                for j in 0..count {
                    self.set_bit(start + j, true);
                }
                self.allocated_pages += count;
                return Ok(start);
            }
        }
        Err(DmaPoolError::Exhausted)
    }

    /// Free `count` pages starting at `start_idx`.
    ///
    /// Panics if any of the freed pages were not allocated — a
    /// double-free indicates a bookkeeping bug that must surface, not
    /// be silently ignored.
    pub fn free(&mut self, start_idx: usize, count: usize) {
        assert!(start_idx + count <= DMA_POOL_PAGES,
            "dma_pool::free: range out of bounds");
        for i in start_idx..(start_idx + count) {
            assert!(self.bit(i), "dma_pool::free: page {} not allocated", i);
            self.set_bit(i, false);
        }
        self.allocated_pages -= count;
    }

    /// Test if `page_idx` is in the pool's address range. Used by the
    /// kernel-side wrapper to assert that a free path's `paddr` is a
    /// pool page (catches accidental cross-pool frees).
    pub const fn contains(&self, page_idx: usize) -> bool {
        page_idx < DMA_POOL_PAGES
    }

    // --- bitmap helpers ---

    fn bit(&self, idx: usize) -> bool {
        (self.bitmap[idx / 64] >> (idx % 64)) & 1 != 0
    }

    fn set_bit(&mut self, idx: usize, value: bool) {
        let mask = 1u64 << (idx % 64);
        if value {
            self.bitmap[idx / 64] |= mask;
        } else {
            self.bitmap[idx / 64] &= !mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pool_is_all_free() {
        let p = DmaPool::empty();
        assert_eq!(p.total_pages(), DMA_POOL_PAGES);
        assert_eq!(p.allocated_pages(), 0);
        assert_eq!(p.free_pages(), DMA_POOL_PAGES);
    }

    #[test]
    fn alloc_single_page_returns_index_0() {
        let mut p = DmaPool::empty();
        let idx = p.alloc(1).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(p.allocated_pages(), 1);
        assert_eq!(p.free_pages(), DMA_POOL_PAGES - 1);
    }

    #[test]
    fn alloc_contiguous_finds_run() {
        let mut p = DmaPool::empty();
        let a = p.alloc(4).unwrap();
        let b = p.alloc(8).unwrap();
        assert_eq!(a, 0);
        assert_eq!(b, 4);
        assert_eq!(p.allocated_pages(), 12);
    }

    #[test]
    fn alloc_zero_returns_invalid_count() {
        let mut p = DmaPool::empty();
        assert_eq!(p.alloc(0), Err(DmaPoolError::InvalidCount));
    }

    #[test]
    fn alloc_too_large_returns_invalid_count() {
        let mut p = DmaPool::empty();
        assert_eq!(
            p.alloc(DMA_POOL_PAGES + 1),
            Err(DmaPoolError::InvalidCount),
        );
    }

    #[test]
    fn alloc_full_pool_then_exhausted() {
        let mut p = DmaPool::empty();
        let _ = p.alloc(DMA_POOL_PAGES).unwrap();
        assert_eq!(p.allocated_pages(), DMA_POOL_PAGES);
        assert_eq!(p.alloc(1), Err(DmaPoolError::Exhausted));
    }

    #[test]
    fn free_makes_pages_reusable() {
        let mut p = DmaPool::empty();
        let idx = p.alloc(4).unwrap();
        p.free(idx, 4);
        assert_eq!(p.allocated_pages(), 0);
        assert_eq!(p.alloc(4).unwrap(), 0);
    }

    #[test]
    #[should_panic(expected = "not allocated")]
    fn double_free_panics() {
        let mut p = DmaPool::empty();
        let idx = p.alloc(2).unwrap();
        p.free(idx, 2);
        p.free(idx, 2); // panic
    }

    #[test]
    fn fragmentation_first_fit() {
        // Alloc 4, alloc 4, free first 4 → next 4-alloc reuses the
        // first-fit hole.
        let mut p = DmaPool::empty();
        let a = p.alloc(4).unwrap();
        let _b = p.alloc(4).unwrap();
        p.free(a, 4);
        let c = p.alloc(4).unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn fragmentation_skips_inadequate_run() {
        // Alloc 2 at start, free it, alloc 1 at start (creates a
        // single-page hole). Next alloc of 4 must skip past the
        // smaller hole and land at the next contiguous run.
        let mut p = DmaPool::empty();
        let _a = p.alloc(2).unwrap(); // pages 0-1
        let _b = p.alloc(8).unwrap(); // pages 2-9
        p.free(0, 2);                  // free pages 0-1
        let _c = p.alloc(1).unwrap(); // page 0
        // Now: 0=used, 1=free, 2-9=used, 10+=free.
        // A 4-page request must land at page 10.
        let d = p.alloc(4).unwrap();
        assert_eq!(d, 10);
    }

    #[test]
    fn bitmap_word_count_matches_page_count() {
        // Pool size and bitmap sizing must stay aligned. If
        // DMA_POOL_PAGES ever stops being a multiple of 64, the
        // BITMAP_WORDS computation needs revisiting.
        assert_eq!(BITMAP_WORDS * 64, DMA_POOL_PAGES);
    }
}
