//! Buddy allocator for contiguous physical page allocation.
//!
//! Maintains a bitmap per order level (0..MAX_ORDER). A set bit at order k
//! means a 2^k-page aligned block is free. Allocation splits from higher
//! orders when needed; deallocation merges buddies upward.
//!
//! Pure logic — no pointers, no VA access. The kernel wrapper maps page
//! indices to physical addresses.

/// Maximum pages the allocator can manage (128MB / 4KB).
pub const MAX_PAGES: usize = 32768;

/// Maximum order: log2(MAX_PAGES). A single order-15 block covers all RAM.
pub const MAX_ORDER: usize = 15;

/// Bytes needed for order k's bitmap: ceil(MAX_PAGES / 2^k / 8).
const fn bitmap_bytes_for_order(order: usize) -> usize {
    let blocks = MAX_PAGES >> order;
    (blocks + 7) / 8
}

/// Byte offset in the flat bitmap array where order k's bitmap starts.
const fn bitmap_offset(order: usize) -> usize {
    let mut offset = 0;
    let mut k = 0;
    while k < order {
        offset += bitmap_bytes_for_order(k);
        k += 1;
    }
    offset
}

/// Total bytes for all order bitmaps (0..MAX_ORDER inclusive).
const BITMAP_TOTAL: usize = bitmap_offset(MAX_ORDER + 1);

/// Per-page allocated bitmap size in bytes.
const ALLOC_BITMAP_SIZE: usize = (MAX_PAGES + 7) / 8;

/// Bitmap-per-order buddy allocator. All state is flat byte arrays
/// suitable for static allocation in kernel BSS (~12KB).
pub struct BuddyAllocator {
    /// Buddy free-block bitmaps. Order k starts at offset bitmap_offset(k).
    bitmap: [u8; BITMAP_TOTAL],
    /// Per-page allocated tracking. Set bit = page is currently handed out
    /// by alloc and not yet returned by free. Detects double-free even
    /// after buddy merges.
    allocated: [u8; ALLOC_BITMAP_SIZE],
    /// Actual number of pages managed (<= MAX_PAGES).
    total_pages: usize,
    /// Total free pages (an order-k block counts as 2^k pages).
    free_count: usize,
}

impl BuddyAllocator {
    /// Create an empty allocator (all memory considered allocated).
    /// Call `add_range` to mark available pages as free.
    pub const fn new() -> Self {
        BuddyAllocator {
            bitmap: [0u8; BITMAP_TOTAL],
            allocated: [0u8; ALLOC_BITMAP_SIZE],
            total_pages: 0,
            free_count: 0,
        }
    }

    /// Set the total number of managed pages. Must be called exactly
    /// once on a freshly constructed allocator before `add_range`.
    /// Resets all state so the allocator is empty.
    pub fn init(&mut self, total_pages: usize) {
        assert!(total_pages <= MAX_PAGES,
            "init: total_pages {} exceeds MAX_PAGES {}", total_pages, MAX_PAGES);
        self.bitmap = [0u8; BITMAP_TOTAL];
        self.allocated = [0u8; ALLOC_BITMAP_SIZE];
        self.total_pages = total_pages;
        self.free_count = 0;
    }

    /// Free a contiguous range of pages into the buddy system.
    /// Used during boot to add available RAM. Pages are freed
    /// individually with buddy merging, building the tree bottom-up.
    ///
    /// Panics if any page in the range is outside `total_pages`.
    pub fn add_range(&mut self, start_page: usize, count: usize) {
        let end = start_page.checked_add(count)
            .expect("add_range: start + count overflows usize");
        assert!(end <= self.total_pages,
            "add_range: {}..{} exceeds total_pages {}", start_page, end, self.total_pages);
        // Mark as allocated first so free() can clear them normally.
        // One invariant, one code path — no special-case bypass.
        for i in 0..count {
            self.set_allocated(start_page + i);
        }
        for i in 0..count {
            self.free(start_page + i, 0);
        }
    }

    /// Allocate a 2^order contiguous block. Returns the page index
    /// of the first page, or None if no block is available.
    pub fn alloc(&mut self, order: usize) -> Option<usize> {
        if order > MAX_ORDER {
            return None;
        }

        // Search upward for the smallest order with a free block.
        let mut found_order = order;
        let page = loop {
            let max_blocks = self.total_pages >> found_order;
            if let Some(block_idx) = self.find_free(found_order, max_blocks) {
                self.clear_bit(found_order, block_idx);
                self.free_count -= 1 << found_order;
                break block_idx << found_order;
            }
            found_order += 1;
            if found_order > MAX_ORDER {
                return None;
            }
        };

        // Split downward from found_order to the requested order,
        // putting the right half on the free bitmap at each level.
        while found_order > order {
            found_order -= 1;
            let right_half = page + (1 << found_order);
            self.set_bit(found_order, right_half >> found_order);
            self.free_count += 1 << found_order;
        }

        // Mark all pages in the block as allocated.
        let block_size = 1 << order;
        for i in 0..block_size {
            self.set_allocated(page + i);
        }

        Some(page)
    }

    /// Free a 2^order block starting at `page`. Merges with the buddy
    /// if the buddy is also free, iteratively up the order tree.
    ///
    /// Panics on double-free (any page in the block not marked allocated)
    /// or if the block extends beyond `total_pages`.
    pub fn free(&mut self, page: usize, order: usize) {
        assert!(order <= MAX_ORDER,
            "free: order {} exceeds MAX_ORDER {}", order, MAX_ORDER);
        assert!(page + (1 << order) <= self.total_pages,
            "free: page {} order {} exceeds total_pages {}", page, order, self.total_pages);
        assert!(page % (1 << order) == 0,
            "free: page {} not aligned to order {}", page, order);

        // Double-free check via per-page allocated bitmap. Catches
        // double-free even after buddy merges have moved the block
        // to a higher order.
        let block_size = 1 << order;
        for i in 0..block_size {
            assert!(self.is_allocated(page + i),
                "free: page {} not allocated (double-free)", page + i);
            self.clear_allocated(page + i);
        }

        let mut current_page = page;
        let mut current_order = order;

        // Merge buddies upward iteratively.
        loop {
            if current_order == MAX_ORDER {
                // Top order — no buddy to merge with.
                self.set_bit(current_order, current_page >> current_order);
                self.free_count += 1 << current_order;
                return;
            }

            let block_idx = current_page >> current_order;
            let buddy_idx = block_idx ^ 1;
            let buddy_page = buddy_idx << current_order;

            if buddy_page < self.total_pages && self.is_free(current_order, buddy_idx) {
                // Buddy is free — merge: remove buddy, try next order.
                self.clear_bit(current_order, buddy_idx);
                self.free_count -= 1 << current_order;
                current_page = current_page & !(1 << current_order);
                current_order += 1;
            } else {
                // Buddy is allocated or out of bounds — mark this free.
                self.set_bit(current_order, block_idx);
                self.free_count += 1 << current_order;
                return;
            }
        }
    }

    /// Number of free pages.
    pub fn free_count(&self) -> usize {
        self.free_count
    }

    /// Total managed pages.
    pub fn total_pages(&self) -> usize {
        self.total_pages
    }

    /// Smallest order where 2^order >= count.
    pub fn order_for_count(count: usize) -> usize {
        if count <= 1 {
            return 0;
        }
        // ceil(log2(count))
        let mut order = 0;
        let mut size = 1;
        while size < count {
            order += 1;
            size <<= 1;
        }
        order
    }

    // --- Allocated bitmap helpers (per-page, for double-free detection) ---

    fn is_allocated(&self, page: usize) -> bool {
        self.allocated[page / 8] & (1 << (page % 8)) != 0
    }

    fn set_allocated(&mut self, page: usize) {
        self.allocated[page / 8] |= 1 << (page % 8);
    }

    fn clear_allocated(&mut self, page: usize) {
        self.allocated[page / 8] &= !(1 << (page % 8));
    }

    // --- Buddy free-block bitmap helpers ---

    fn is_free(&self, order: usize, block_idx: usize) -> bool {
        let offset = bitmap_offset(order) + block_idx / 8;
        let bit = block_idx % 8;
        self.bitmap[offset] & (1 << bit) != 0
    }

    fn set_bit(&mut self, order: usize, block_idx: usize) {
        let offset = bitmap_offset(order) + block_idx / 8;
        let bit = block_idx % 8;
        self.bitmap[offset] |= 1 << bit;
    }

    fn clear_bit(&mut self, order: usize, block_idx: usize) {
        let offset = bitmap_offset(order) + block_idx / 8;
        let bit = block_idx % 8;
        self.bitmap[offset] &= !(1 << bit);
    }

    /// Find the first free block at the given order, scanning up to
    /// `max_blocks` entries. Returns the block index or None.
    fn find_free(&self, order: usize, max_blocks: usize) -> Option<usize> {
        let base = bitmap_offset(order);
        let full_bytes = max_blocks / 8;
        let remainder_bits = max_blocks % 8;

        // Scan full bytes — skip 0x00 bytes (no free blocks).
        for byte_idx in 0..full_bytes {
            let b = self.bitmap[base + byte_idx];
            if b != 0 {
                let bit = b.trailing_zeros() as usize;
                return Some(byte_idx * 8 + bit);
            }
        }

        // Check remainder bits in the last partial byte.
        if remainder_bits > 0 {
            let b = self.bitmap[base + full_bytes];
            let mask = (1u8 << remainder_bits) - 1;
            let masked = b & mask;
            if masked != 0 {
                let bit = masked.trailing_zeros() as usize;
                return Some(full_bytes * 8 + bit);
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_small() -> BuddyAllocator {
        let mut b = BuddyAllocator::new();
        b.init(16); // 16 pages, max useful order = 4
        b.add_range(0, 16); // all 16 pages free
        b
    }

    #[test]
    fn order_for_count_cases() {
        assert_eq!(BuddyAllocator::order_for_count(1), 0);
        assert_eq!(BuddyAllocator::order_for_count(2), 1);
        assert_eq!(BuddyAllocator::order_for_count(3), 2);
        assert_eq!(BuddyAllocator::order_for_count(4), 2);
        assert_eq!(BuddyAllocator::order_for_count(5), 3);
        assert_eq!(BuddyAllocator::order_for_count(75), 7); // framebuffer
        assert_eq!(BuddyAllocator::order_for_count(128), 7);
        assert_eq!(BuddyAllocator::order_for_count(129), 8);
    }

    #[test]
    fn alloc_single_page() {
        let mut b = make_small();
        assert_eq!(b.free_count(), 16);
        let p = b.alloc(0).unwrap();
        assert_eq!(b.free_count(), 15);
        assert!(p < 16);
    }

    #[test]
    fn alloc_all_singles() {
        let mut b = make_small();
        for _ in 0..16 {
            b.alloc(0).unwrap();
        }
        assert_eq!(b.free_count(), 0);
        assert!(b.alloc(0).is_none());
    }

    #[test]
    fn free_and_realloc() {
        let mut b = make_small();
        let p = b.alloc(0).unwrap();
        b.free(p, 0);
        assert_eq!(b.free_count(), 16);
        let p2 = b.alloc(0).unwrap();
        assert_eq!(p, p2); // should get the same page back
    }

    #[test]
    fn contiguous_alloc() {
        let mut b = make_small();
        // Order 2 = 4 contiguous pages
        let p = b.alloc(2).unwrap();
        assert_eq!(p % 4, 0); // aligned to 4 pages
        assert_eq!(b.free_count(), 12);
    }

    #[test]
    fn contiguous_alloc_entire() {
        let mut b = make_small();
        // Order 4 = 16 pages = entire allocator
        let p = b.alloc(4).unwrap();
        assert_eq!(p, 0);
        assert_eq!(b.free_count(), 0);
        assert!(b.alloc(0).is_none());
    }

    #[test]
    fn buddy_merging() {
        let mut b = make_small();
        // Allocate two order-0 pages (should be buddies)
        let p0 = b.alloc(0).unwrap();
        let p1 = b.alloc(0).unwrap();
        assert_eq!(b.free_count(), 14);

        // Free both — they should merge back up
        b.free(p0, 0);
        b.free(p1, 0);
        assert_eq!(b.free_count(), 16);

        // Should be able to alloc the full block again
        let p = b.alloc(4).unwrap();
        assert_eq!(p, 0);
    }

    #[test]
    fn split_and_merge() {
        let mut b = make_small();
        // Alloc order 3 (8 pages) — splits the 16-page block
        let p = b.alloc(3).unwrap();
        assert_eq!(p, 0);
        assert_eq!(b.free_count(), 8);

        // Alloc another order 3 — takes the other half
        let p2 = b.alloc(3).unwrap();
        assert_eq!(p2, 8);
        assert_eq!(b.free_count(), 0);

        // Free both — should merge back to order 4
        b.free(p, 3);
        b.free(p2, 3);
        assert_eq!(b.free_count(), 16);

        // Full block available again
        assert!(b.alloc(4).is_some());
    }

    #[test]
    fn partial_range() {
        let mut b = BuddyAllocator::new();
        b.init(32);
        // Only free pages 8..24 (16 pages, not aligned to 32)
        b.add_range(8, 16);
        assert_eq!(b.free_count(), 16);

        // Should be able to alloc order 3 (8 pages)
        let p = b.alloc(3).unwrap();
        assert!(p >= 8 && p < 24);
        assert_eq!(p % 8, 0);
    }

    #[test]
    fn oom_returns_none() {
        let mut b = make_small();
        // Exhaust all memory
        while b.alloc(0).is_some() {}
        assert!(b.alloc(0).is_none());
        assert!(b.alloc(1).is_none());
        assert!(b.alloc(4).is_none());
    }

    #[test]
    fn framebuffer_scenario() {
        // Simulate Lockjaw boot: 32768 pages, 640 reserved at bottom
        let mut b = BuddyAllocator::new();
        b.init(MAX_PAGES);
        b.add_range(640, MAX_PAGES - 640);
        assert_eq!(b.free_count(), MAX_PAGES - 640);

        // Allocate a few single pages (kernel structures)
        for _ in 0..20 {
            b.alloc(0).unwrap();
        }

        // Allocate framebuffer: 75 pages → order 7 (128 pages)
        let fb = b.alloc(7).unwrap();
        assert_eq!(fb % 128, 0); // 128-page aligned
        // Verify contiguity: pages fb..fb+128 are the allocated block
        assert!(fb + 128 <= MAX_PAGES);
    }

    #[test]
    #[should_panic(expected = "double-free")]
    fn double_free_simple() {
        let mut b = make_small();
        let p = b.alloc(0).unwrap();
        b.free(p, 0);
        b.free(p, 0); // should panic
    }

    #[test]
    #[should_panic(expected = "double-free")]
    fn double_free_after_merge() {
        let mut b = make_small();
        let p0 = b.alloc(0).unwrap();
        let p1 = b.alloc(0).unwrap();
        // Free both — they merge into an order-1 block
        b.free(p0, 0);
        b.free(p1, 0);
        // Double-free p0 — should panic even though the order-0
        // buddy bitmap bit is clear (merged to higher order)
        b.free(p0, 0);
    }

    #[test]
    fn bitmap_total_size() {
        // Verify the computed bitmap size matches our constant
        let mut total = 0;
        for k in 0..=MAX_ORDER {
            total += bitmap_bytes_for_order(k);
        }
        assert_eq!(total, BITMAP_TOTAL);
    }
}
