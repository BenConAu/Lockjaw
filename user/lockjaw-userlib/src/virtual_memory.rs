/// Process-local virtual address space allocator.
///
/// Tracks reserved VA regions using a sorted free-list. Thread-safe
/// via a userspace spinlock (AArch64 atomics, no syscall needed).
///
/// Initialization is lazy: the first call to `alloc()` or `reserve()`
/// scans the process's page tables via `sys_query_mapping` to discover
/// existing mappings (ELF image, stack) and mark them reserved.
///
/// IMAGE_BASE and USER_STACK_BASE are ABI anchors — fixed starting
/// points for the discovery scan. The extents are learned dynamically.

use core::sync::atomic::{AtomicBool, Ordering};
use lockjaw_types::addr::PAGE_SIZE;
use lockjaw_types::constants::{USER_VA_END, USER_STACK_BASE};
use lockjaw_types::syscall::SyscallError;
use crate::handle::PageSetHandle;
use crate::syscall::sys_unmap_pages;

/// ELF image base address (ABI anchor, matches linker scripts).
const IMAGE_BASE: u64 = 0x0040_0000;

/// Low VA region reserved to catch null pointer dereferences.
const NULL_GUARD_SIZE: u64 = 0x1_0000; // 64KB

/// Maximum number of free regions tracked.
const MAX_REGIONS: usize = 64;

/// A contiguous free VA region.
#[derive(Clone, Copy)]
struct FreeRegion {
    base: u64,
    pages: usize,
}

/// The allocator state, protected by a spinlock.
struct VmState {
    /// Sorted array of free regions (by base VA).
    regions: [FreeRegion; MAX_REGIONS],
    /// Number of valid entries in `regions`.
    count: usize,
    /// Next-fit cursor: index of region to search from next.
    cursor: usize,
    /// Whether initialization has been done.
    initialized: bool,
}

pub struct VirtualMemory {
    lock: AtomicBool,
    state: core::cell::UnsafeCell<VmState>,
}

// SAFETY: the spinlock serializes all access. Only threads in the
// same process share this allocator, and they all go through the lock.
unsafe impl Sync for VirtualMemory {}

impl VirtualMemory {
    pub const fn new() -> Self {
        Self {
            lock: AtomicBool::new(false),
            state: core::cell::UnsafeCell::new(VmState {
                regions: [FreeRegion { base: 0, pages: 0 }; MAX_REGIONS],
                count: 0,
                cursor: 0,
                initialized: false,
            }),
        }
    }

    /// Acquire the spinlock.
    fn lock(&self) {
        while self.lock.compare_exchange_weak(
            false, true, Ordering::Acquire, Ordering::Relaxed,
        ).is_err() {
            core::hint::spin_loop();
        }
    }

    /// Release the spinlock.
    fn unlock(&self) {
        self.lock.store(false, Ordering::Release);
    }

    /// Ensure the allocator is initialized. Called under the lock.
    /// Uses sys_query_mapping to discover existing mappings.
    fn ensure_init(&self) {
        let state = unsafe { &mut *self.state.get() };
        if state.initialized {
            return;
        }

        // Start with the entire user VA range as one free region.
        state.regions[0] = FreeRegion {
            base: 0,
            pages: (USER_VA_END / PAGE_SIZE) as usize,
        };
        state.count = 1;
        state.cursor = 0;

        // Reserve the null guard region [0, NULL_GUARD_SIZE).
        self.reserve_internal(state, 0, (NULL_GUARD_SIZE / PAGE_SIZE) as usize);

        // Discover and reserve ELF image extent.
        if let Ok((mapped, run)) = crate::syscall::sys_query_mapping(IMAGE_BASE) {
            if mapped && run > 0 {
                self.reserve_internal(state, IMAGE_BASE, run as usize);
            }
        }

        // Discover and reserve stack extent.
        if let Ok((mapped, run)) = crate::syscall::sys_query_mapping(USER_STACK_BASE) {
            if mapped && run > 0 {
                self.reserve_internal(state, USER_STACK_BASE, run as usize);
            }
        }

        state.initialized = true;
    }

    /// Allocate `page_count` pages of VA space. Returns the base VA.
    pub fn alloc(&self, page_count: usize) -> Option<u64> {
        if page_count == 0 {
            return None;
        }
        self.lock();
        self.ensure_init();
        let state = unsafe { &mut *self.state.get() };

        // Next-fit: search from cursor, wrap around
        let start = state.cursor;
        let n = state.count;
        for i in 0..n {
            let idx = (start + i) % n;
            if state.regions[idx].pages >= page_count {
                let base = state.regions[idx].base;
                // Shrink or remove the region
                if state.regions[idx].pages == page_count {
                    // Remove by shifting
                    for j in idx..state.count - 1 {
                        state.regions[j] = state.regions[j + 1];
                    }
                    state.count -= 1;
                    state.cursor = if state.count > 0 { idx % state.count } else { 0 };
                } else {
                    // Shrink from the front
                    state.regions[idx].base += (page_count as u64) * PAGE_SIZE;
                    state.regions[idx].pages -= page_count;
                    state.cursor = idx;
                }
                self.unlock();
                return Some(base);
            }
        }

        self.unlock();
        None
    }

    /// Return a previously-mapped + now-unmapped VA range to the
    /// allocator. The proof token can only come from a successful
    /// `unmap_pages_tracked` — caller cannot construct one without
    /// actually unmapping. Type-level guarantee against the
    /// "page tables still map X, allocator hands X out again"
    /// aliasing bug class.
    pub fn free_unmapped(&self, proof: VaUnmapped) {
        self.free_internal(proof.va, proof.pages);
    }

    /// Return a VA range that was allocated but NEVER mapped (e.g.
    /// `sys_map_pages` failed, or `sys_register_device_page` failed
    /// before the map step, or the caller reserved-then-bailed
    /// before mapping). Distinct path because there is no "did the
    /// unmap succeed" question — no mapping was ever established,
    /// so returning the VA cannot create an aliasing bug.
    pub fn free_unused_allocation(&self, va: u64, page_count: usize) {
        self.free_internal(va, page_count);
    }

    /// Raw free — crate-private. The two public free methods above
    /// are the only sanctioned entry points; both prove (by type or
    /// by name) that no mapping currently uses the VA. Tests inside
    /// this module call `free_internal` directly because they
    /// exercise the allocator without involving the kernel.
    pub(crate) fn free_internal(&self, va: u64, page_count: usize) {
        if page_count == 0 {
            return;
        }
        self.lock();
        self.ensure_init();
        let state = unsafe { &mut *self.state.get() };

        let end = va + (page_count as u64) * PAGE_SIZE;

        // Find insertion point (sorted by base) and reject overlaps.
        // The freed range [va, end) must not overlap any existing free
        // region — if it does, the caller double-freed or freed wrong.
        let mut insert_at = state.count;
        for i in 0..state.count {
            let r_base = state.regions[i].base;
            let r_end = r_base + (state.regions[i].pages as u64) * PAGE_SIZE;
            // Overlap: ranges [va, end) and [r_base, r_end) intersect
            if va < r_end && end > r_base {
                // Overlap with existing free region — double free or bad range.
                // Silently ignore to avoid corrupting the free-list.
                self.unlock();
                return;
            }
            if r_base >= end {
                insert_at = i;
                break;
            }
        }

        // Try to merge with predecessor
        let mut merged_prev = false;
        if insert_at > 0 {
            let prev = &mut state.regions[insert_at - 1];
            let prev_end = prev.base + (prev.pages as u64) * PAGE_SIZE;
            if prev_end == va {
                prev.pages += page_count;
                merged_prev = true;
            }
        }

        // Try to merge with successor
        if merged_prev {
            let prev_idx = insert_at - 1;
            if prev_idx + 1 < state.count {
                let prev_end = state.regions[prev_idx].base
                    + (state.regions[prev_idx].pages as u64) * PAGE_SIZE;
                if prev_end == state.regions[prev_idx + 1].base {
                    state.regions[prev_idx].pages += state.regions[prev_idx + 1].pages;
                    // Remove successor
                    for j in (prev_idx + 1)..state.count - 1 {
                        state.regions[j] = state.regions[j + 1];
                    }
                    state.count -= 1;
                }
            }
        } else if insert_at < state.count {
            // Try to merge with successor only
            let succ = &state.regions[insert_at];
            if end == succ.base {
                state.regions[insert_at].base = va;
                state.regions[insert_at].pages += page_count;
            } else {
                // No merge — insert new region
                if state.count >= MAX_REGIONS {
                    // Table full — leak the VA range (not great but safe)
                    self.unlock();
                    return;
                }
                for j in (insert_at..state.count).rev() {
                    state.regions[j + 1] = state.regions[j];
                }
                state.regions[insert_at] = FreeRegion { base: va, pages: page_count };
                state.count += 1;
            }
        } else {
            // Append at end
            if state.count >= MAX_REGIONS {
                self.unlock();
                return;
            }
            state.regions[state.count] = FreeRegion { base: va, pages: page_count };
            state.count += 1;
        }

        self.unlock();
    }

    /// Reserve a specific VA range. Returns false if it overlaps
    /// an existing reservation (i.e., the range is not fully free).
    pub fn reserve(&self, va: u64, page_count: usize) -> bool {
        if page_count == 0 {
            return true;
        }
        self.lock();
        self.ensure_init();
        let state = unsafe { &mut *self.state.get() };
        let result = self.reserve_internal(state, va, page_count);
        self.unlock();
        result
    }

    /// Internal reserve — called under the lock. Splits a free region
    /// to carve out [va, va + page_count * PAGE_SIZE).
    fn reserve_internal(&self, state: &mut VmState, va: u64, page_count: usize) -> bool {
        let end = va + (page_count as u64) * PAGE_SIZE;

        // Find the free region that contains [va, end)
        for i in 0..state.count {
            let r = &state.regions[i];
            let r_end = r.base + (r.pages as u64) * PAGE_SIZE;
            if r.base <= va && r_end >= end {
                // Found containing region — split it
                let before_pages = ((va - r.base) / PAGE_SIZE) as usize;
                let after_pages = ((r_end - end) / PAGE_SIZE) as usize;

                if before_pages == 0 && after_pages == 0 {
                    // Exact match — remove region
                    for j in i..state.count - 1 {
                        state.regions[j] = state.regions[j + 1];
                    }
                    state.count -= 1;
                } else if before_pages == 0 {
                    // Trim from front
                    state.regions[i].base = end;
                    state.regions[i].pages = after_pages;
                } else if after_pages == 0 {
                    // Trim from back
                    state.regions[i].pages = before_pages;
                } else {
                    // Split into two: [r.base, va) and [end, r_end)
                    if state.count >= MAX_REGIONS {
                        return false; // no room for split
                    }
                    state.regions[i].pages = before_pages;
                    // Insert after region
                    for j in (i + 1..state.count).rev() {
                        state.regions[j + 1] = state.regions[j];
                    }
                    state.regions[i + 1] = FreeRegion { base: end, pages: after_pages };
                    state.count += 1;
                }
                return true;
            }
        }

        false // range not fully contained in any free region
    }
}

/// Global VM allocator instance. All programs link against this.
pub static VMEM: VirtualMemory = VirtualMemory::new();

// ---------------------------------------------------------------------------
// Construction-safe VA reclaim — the unmap-then-free invariant as a type.
//
// Calling `sys_unmap_pages` and then `VMEM.free` independently was the
// recurring bug shape: if unmap failed, the page tables could still map
// the VA while the allocator thought it was free, and a future caller
// could be handed an already-mapped range (an aliasing bug strictly
// worse than a leak). The discipline "only free after successful unmap"
// failed twice in 100 LOC during Phase 5 review.
//
// The fix: encode the discipline as a type. `unmap_pages_tracked`
// returns a `VaUnmapped` proof token ONLY on success; `VMEM.free_unmapped`
// requires the token. Code that ignores the unmap result cannot
// construct the token; the aliasing-bug pattern is unrepresentable.
// `VMEM.free_unused_allocation` is the audited path for the orthogonal
// alloc-but-never-mapped case (e.g. `sys_map_pages` failed).
// ---------------------------------------------------------------------------

/// Proof that `sys_unmap_pages` succeeded for a specific (va, pages)
/// range. The only way to construct one is via `unmap_pages_tracked`;
/// the fields are private. Required by `VirtualMemory::free_unmapped`
/// so it is statically impossible to return a still-mapped VA range
/// to the allocator.
///
/// # Compile-fail soundness guarantee
///
/// Construction outside this module is rejected by the privacy of
/// the fields. The doctest below MUST fail to compile; the
/// `compile_fail` annotation runs it as a `cargo test --doc` check.
///
/// ```compile_fail
/// use lockjaw_userlib::virtual_memory::VaUnmapped;
/// // Field-level construction is rejected: `va` and `pages` are
/// // private to `lockjaw_userlib::virtual_memory`.
/// let p = VaUnmapped { va: 0, pages: 1 };
/// ```
pub struct VaUnmapped {
    va: u64,
    pages: usize,
}

impl VaUnmapped {
    /// First-page VA of the unmapped range.
    pub fn va(&self) -> u64 { self.va }
    /// Page count of the unmapped range.
    pub fn pages(&self) -> usize { self.pages }
}

/// Unmap a previously-mapped VA range. On success returns a
/// `VaUnmapped` proof token that the VA is safe to return to VMEM.
/// On failure, returns the error AND the VA stays leaked (page
/// tables may still map it; reusing the VA would create an aliasing
/// bug — leaking is the safer failure mode).
///
/// Pageset close is the caller's responsibility — different release
/// paths have different pageset-ownership semantics (owned vs
/// borrowed mapping; claimed device vs DMA backing).
pub fn unmap_pages_tracked(
    pageset: PageSetHandle,
    va: u64,
    pages: usize,
) -> Result<VaUnmapped, SyscallError> {
    let err = sys_unmap_pages(pageset, va);
    if !err.is_ok() {
        return Err(err);
    }
    Ok(VaUnmapped { va, pages })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// Create a VirtualMemory with a manually initialized free-list
    /// (no sys_query_mapping). The entire range [0, USER_VA_END) is free.
    fn make_allocator() -> VirtualMemory {
        let a = VirtualMemory::new();
        let state = unsafe { &mut *a.state.get() };
        state.regions[0] = FreeRegion {
            base: 0,
            pages: (USER_VA_END / PAGE_SIZE) as usize,
        };
        state.count = 1;
        state.cursor = 0;
        state.initialized = true;
        a
    }

    /// Create a VirtualMemory with a specific free region.
    fn make_with_region(base: u64, pages: usize) -> VirtualMemory {
        let a = VirtualMemory::new();
        let state = unsafe { &mut *a.state.get() };
        state.regions[0] = FreeRegion { base, pages };
        state.count = 1;
        state.cursor = 0;
        state.initialized = true;
        a
    }

    /// Read back the free-list for assertions.
    fn free_regions(a: &VirtualMemory) -> Vec<(u64, usize)> {
        let state = unsafe { &*a.state.get() };
        (0..state.count)
            .map(|i| (state.regions[i].base, state.regions[i].pages))
            .collect()
    }

    #[test]
    fn alloc_single_page() {
        let a = make_with_region(0x10_0000, 100);
        let va = a.alloc(1);
        assert_eq!(va, Some(0x10_0000));
        let regions = free_regions(&a);
        assert_eq!(regions, vec![(0x10_1000, 99)]);
    }

    #[test]
    fn alloc_exact_fit() {
        let a = make_with_region(0x10_0000, 4);
        let va = a.alloc(4);
        assert_eq!(va, Some(0x10_0000));
        let regions = free_regions(&a);
        assert_eq!(regions, vec![]); // region fully consumed
    }

    #[test]
    fn alloc_too_large() {
        let a = make_with_region(0x10_0000, 4);
        let va = a.alloc(5);
        assert_eq!(va, None);
    }

    #[test]
    fn alloc_zero_pages() {
        let a = make_with_region(0x10_0000, 100);
        assert_eq!(a.alloc(0), None);
    }

    #[test]
    fn alloc_multiple_sequential() {
        let a = make_with_region(0x10_0000, 100);
        let v1 = a.alloc(10).unwrap();
        let v2 = a.alloc(20).unwrap();
        let v3 = a.alloc(5).unwrap();
        assert_eq!(v1, 0x10_0000);
        assert_eq!(v2, 0x10_0000 + 10 * 0x1000);
        assert_eq!(v3, 0x10_0000 + 30 * 0x1000);
        let regions = free_regions(&a);
        assert_eq!(regions, vec![(0x10_0000 + 35 * 0x1000, 65)]);
    }

    #[test]
    fn reserve_from_front() {
        let a = make_with_region(0x10_0000, 100);
        assert!(a.reserve(0x10_0000, 10));
        let regions = free_regions(&a);
        assert_eq!(regions, vec![(0x10_0000 + 10 * 0x1000, 90)]);
    }

    #[test]
    fn reserve_from_back() {
        let a = make_with_region(0x10_0000, 100);
        let back_va = 0x10_0000 + 90 * 0x1000;
        assert!(a.reserve(back_va, 10));
        let regions = free_regions(&a);
        assert_eq!(regions, vec![(0x10_0000, 90)]);
    }

    #[test]
    fn reserve_splits_region() {
        let a = make_with_region(0x10_0000, 100);
        let mid_va = 0x10_0000 + 40 * 0x1000;
        assert!(a.reserve(mid_va, 10));
        let regions = free_regions(&a);
        assert_eq!(regions, vec![
            (0x10_0000, 40),
            (0x10_0000 + 50 * 0x1000, 50),
        ]);
    }

    #[test]
    fn reserve_exact_match() {
        let a = make_with_region(0x10_0000, 10);
        assert!(a.reserve(0x10_0000, 10));
        let regions = free_regions(&a);
        assert_eq!(regions, vec![]);
    }

    #[test]
    fn reserve_overlapping_fails() {
        let a = make_with_region(0x10_0000, 10);
        a.reserve(0x10_0000, 5); // take first 5
        // Try to reserve pages 3..8 which overlap the reserved range
        assert!(!a.reserve(0x10_0000 + 3 * 0x1000, 5));
    }

    #[test]
    fn free_merges_with_predecessor() {
        let a = make_with_region(0x10_0000, 100);
        let v1 = a.alloc(10).unwrap(); // takes [0x100000, 0x10A000)
        let v2 = a.alloc(10).unwrap(); // takes [0x10A000, 0x114000)
        // Free v1 — should create new region before the remaining free
        a.free_internal(v1, 10);
        // Free v2 — should merge with v1's freed region
        a.free_internal(v2, 10);
        let regions = free_regions(&a);
        // Should be one contiguous region again
        assert_eq!(regions, vec![(0x10_0000, 100)]);
    }

    #[test]
    fn free_merges_with_successor() {
        let a = make_with_region(0x10_0000, 100);
        let v1 = a.alloc(10).unwrap();
        let _v2 = a.alloc(10).unwrap();
        // Free v1 — creates [0x100000, 10 pages], existing [0x114000, 80 pages]
        a.free_internal(v1, 10);
        let regions = free_regions(&a);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0], (0x10_0000, 10));
    }

    #[test]
    fn alloc_after_free_reuses_space() {
        let a = make_with_region(0x10_0000, 20);
        let v1 = a.alloc(10).unwrap();
        let _v2 = a.alloc(10).unwrap();
        assert_eq!(a.alloc(1), None); // full
        a.free_internal(v1, 10);
        let v3 = a.alloc(5).unwrap();
        assert_eq!(v3, v1); // reuses freed space
    }

    #[test]
    fn reserve_then_alloc_avoids_reserved() {
        let a = make_allocator();
        // Reserve the image region
        a.reserve(0x40_0000, 4);
        // Alloc should not return anything in [0x400000, 0x404000)
        let va = a.alloc(1).unwrap();
        assert!(va < 0x40_0000 || va >= 0x40_4000);
    }

    #[test]
    fn double_free_is_rejected() {
        let a = make_with_region(0x10_0000, 20);
        let v1 = a.alloc(10).unwrap();
        a.free_internal(v1, 10);
        // Double free — should be silently ignored, not corrupt state
        a.free_internal(v1, 10);
        let regions = free_regions(&a);
        // Should still have the original freed region + remainder, merged
        assert_eq!(regions, vec![(0x10_0000, 20)]);
    }

    #[test]
    fn overlapping_free_is_rejected() {
        let a = make_with_region(0x10_0000, 20);
        let v1 = a.alloc(10).unwrap(); // [0x100000, 0x10A000)
        let _v2 = a.alloc(10).unwrap(); // [0x10A000, 0x114000)
        a.free_internal(v1, 10); // free first block
        // Try to free a range that overlaps the now-free first block
        // [0x105000, 0x10F000) overlaps [0x100000, 0x10A000)
        a.free_internal(0x10_5000, 10);
        // Should be rejected — free-list should be unchanged
        let regions = free_regions(&a);
        assert_eq!(regions, vec![(0x10_0000, 10)]);
    }

    #[test]
    fn next_fit_advances_cursor() {
        let a = make_with_region(0x10_0000, 100);
        let v1 = a.alloc(10).unwrap();
        let v2 = a.alloc(10).unwrap();
        assert_eq!(v1, 0x10_0000);
        assert_eq!(v2, 0x10_0000 + 10 * 0x1000);
        // Next alloc continues from where we left off, not from start
        let v3 = a.alloc(10).unwrap();
        assert_eq!(v3, 0x10_0000 + 20 * 0x1000);
    }
}

