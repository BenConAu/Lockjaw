//! Pure state for the bootstrap-allocated PageSet header pool.
//!
//! NK2 eliminates the runtime `kvm::alloc_kernel_pages` calls
//! that the PageSet allocation path currently performs
//! (`src/cap/pageset_table.rs:75/131/206/279`) by carving a
//! fixed pool of header slots at bootstrap and handing them out
//! via claim/release. After NK2, the kernel's PageSet creation
//! path does NOT grow kernel memory — it only walks pre-existing
//! pool slots.
//!
//! This module is pure decision logic — no `static mut`, no page
//! allocator, no `KernelVa`. The kernel adapter at
//! `src/cap/pageset_header_pool.rs` owns the singleton + the base
//! KVA + the GKL-serialized `UnsafeCell`.
//!
//! ## Slot layout
//!
//! Each slot occupies `MAX_HEADER_PAGES_PER_PAGESET = 33` pages
//! of contiguous KVA, with `slot_idx`'s base at
//! `pool_base + slot_idx * 33 * PAGE_SIZE`. A claim of
//! `header_pages ≤ 33` uses only the first `header_pages` pages
//! of the slot; the rest stays unused but unfreed (the slot is
//! the unit of allocation). Worst-case storage cost:
//! `MAX_PAGESETS × MAX_HEADER_PAGES_PER_PAGESET × PAGE_SIZE`
//! = 128 × 33 × 4 KiB ≈ 16.5 MiB.
//!
//! ## Bitmap semantics
//!
//! `free_bits[0..2]` tracks slots `0..MAX_PAGESETS` (128 slots,
//! indices 0..=127) with **1 = used, 0 = free**.
//! `claim()` picks the lowest free slot via `bits.trailing_ones()`
//! (returns the lowest 0-bit position from the LSB). For an
//! all-free word (`bits = 0`), `trailing_ones = 0` → slot 0 is
//! free. For full (`bits = u64::MAX`), `trailing_ones = 64`,
//! signalling fall-through to the next word or exhaustion.
//!
//! Inverted-bit semantics ("1 = free") with the same primitive
//! would misclassify `u64::MAX` (all free) as 64 = exhausted,
//! which is the exact failure mode codex r1 flagged on the plan.

use crate::pageset_table::MAX_PAGESETS;
pub use crate::pageset_table::MAX_HEADER_PAGES_PER_PAGESET;

/// Number of u64 words required to track `MAX_PAGESETS` slots.
/// For 128 slots: 2 words.
pub const POOL_BITMAP_WORDS: usize = (MAX_PAGESETS + 63) / 64;

/// Pure state for the pool: per-slot used/free bit + the
/// header-page count the caller claimed (so `release` doesn't
/// need a size argument and can detect double-release).
#[derive(Debug)]
pub struct PageSetHeaderPoolState {
    /// 1 = used, 0 = free. Slot `i`'s bit is bit `i % 64` of
    /// word `i / 64`.
    pub free_bits: [u64; POOL_BITMAP_WORDS],
    /// Per-slot header-page count claimed at `claim` time.
    /// 0 means the slot is unclaimed; otherwise 1..=33.
    pub claimed_pages: [u8; MAX_PAGESETS],
}

impl PageSetHeaderPoolState {
    /// All slots free, no claims recorded.
    pub const fn new() -> Self {
        Self {
            free_bits: [0; POOL_BITMAP_WORDS],
            claimed_pages: [0; MAX_PAGESETS],
        }
    }

    /// Claim the lowest free slot for `header_pages` pages
    /// (1..=`MAX_HEADER_PAGES_PER_PAGESET`). Returns the slot
    /// index, or `None` if the pool is exhausted or
    /// `header_pages` is out of range.
    pub fn claim(&mut self, header_pages: usize) -> Option<usize> {
        if header_pages == 0 || header_pages > MAX_HEADER_PAGES_PER_PAGESET {
            return None;
        }
        for (word_idx, word) in self.free_bits.iter_mut().enumerate() {
            let pos = word.trailing_ones() as usize;
            if pos < 64 {
                let slot = word_idx * 64 + pos;
                if slot >= MAX_PAGESETS {
                    // Last word may have a tail of bits beyond
                    // MAX_PAGESETS; treat them as permanently
                    // unavailable.
                    continue;
                }
                *word |= 1u64 << pos;
                self.claimed_pages[slot] = header_pages as u8;
                return Some(slot);
            }
        }
        None
    }

    /// Release a previously-claimed slot. Returns the
    /// header-page count the slot was claimed for. Panics on
    /// double-release (slot wasn't marked used) or invalid slot
    /// index — Tier 1 #1 correctness-by-construction.
    pub fn release(&mut self, slot_idx: usize) -> u8 {
        assert!(
            slot_idx < MAX_PAGESETS,
            "PageSetHeaderPool::release: slot_idx {} out of range",
            slot_idx,
        );
        let word_idx = slot_idx / 64;
        let bit = 1u64 << (slot_idx % 64);
        assert!(
            self.free_bits[word_idx] & bit != 0,
            "PageSetHeaderPool::release: double-release of slot {}",
            slot_idx,
        );
        self.free_bits[word_idx] &= !bit;
        let pages = self.claimed_pages[slot_idx];
        self.claimed_pages[slot_idx] = 0;
        pages
    }

    /// True if `slot_idx` is currently claimed.
    pub fn is_used(&self, slot_idx: usize) -> bool {
        if slot_idx >= MAX_PAGESETS {
            return false;
        }
        let word_idx = slot_idx / 64;
        let bit = 1u64 << (slot_idx % 64);
        self.free_bits[word_idx] & bit != 0
    }

    /// Number of slots currently claimed. For diagnostics.
    pub fn used_count(&self) -> usize {
        let mut total = 0;
        for word in self.free_bits.iter() {
            total += word.count_ones() as usize;
        }
        total
    }

    /// Slots remaining free. For diagnostics.
    pub fn free_count(&self) -> usize {
        MAX_PAGESETS - self.used_count()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn new_pool_all_slots_free() {
        let pool = PageSetHeaderPoolState::new();
        assert_eq!(pool.used_count(), 0);
        assert_eq!(pool.free_count(), MAX_PAGESETS);
        assert!(!pool.is_used(0));
        assert!(!pool.is_used(MAX_PAGESETS - 1));
    }

    #[test]
    fn claim_zero_or_oversize_returns_none() {
        let mut pool = PageSetHeaderPoolState::new();
        assert_eq!(pool.claim(0), None);
        assert_eq!(pool.claim(MAX_HEADER_PAGES_PER_PAGESET + 1), None);
        // Pool state unchanged.
        assert_eq!(pool.used_count(), 0);
    }

    #[test]
    fn first_claim_picks_slot_zero() {
        let mut pool = PageSetHeaderPoolState::new();
        assert_eq!(pool.claim(1), Some(0));
        assert!(pool.is_used(0));
        assert!(!pool.is_used(1));
        assert_eq!(pool.used_count(), 1);
    }

    #[test]
    fn claim_picks_lowest_free_slot() {
        let mut pool = PageSetHeaderPoolState::new();
        // Claim slots 0, 1, 2.
        assert_eq!(pool.claim(1), Some(0));
        assert_eq!(pool.claim(1), Some(1));
        assert_eq!(pool.claim(1), Some(2));
        // Release slot 1; next claim should pick it (lowest free).
        let pages = pool.release(1);
        assert_eq!(pages, 1);
        assert_eq!(pool.claim(1), Some(1));
    }

    #[test]
    fn claim_records_header_pages() {
        let mut pool = PageSetHeaderPoolState::new();
        let s = pool.claim(33).unwrap();
        let pages = pool.release(s);
        assert_eq!(pages, 33);
    }

    #[test]
    fn claim_until_exhausted_returns_none() {
        let mut pool = PageSetHeaderPoolState::new();
        for i in 0..MAX_PAGESETS {
            assert_eq!(pool.claim(1), Some(i));
        }
        assert_eq!(pool.used_count(), MAX_PAGESETS);
        assert_eq!(pool.claim(1), None);
    }

    #[test]
    fn release_clears_bit_and_returns_pages() {
        let mut pool = PageSetHeaderPoolState::new();
        let s = pool.claim(7).unwrap();
        assert!(pool.is_used(s));
        let pages = pool.release(s);
        assert_eq!(pages, 7);
        assert!(!pool.is_used(s));
        assert_eq!(pool.used_count(), 0);
    }

    #[test]
    #[should_panic(expected = "double-release")]
    fn double_release_panics() {
        let mut pool = PageSetHeaderPoolState::new();
        let s = pool.claim(1).unwrap();
        pool.release(s);
        pool.release(s);
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn release_out_of_range_panics() {
        let mut pool = PageSetHeaderPoolState::new();
        pool.release(MAX_PAGESETS);
    }

    #[test]
    fn bitmap_word_boundary_handled() {
        // Slot 64 lives in word 1; verify the claim/release
        // boundary works.
        let mut pool = PageSetHeaderPoolState::new();
        for _ in 0..64 {
            pool.claim(1).unwrap();
        }
        assert_eq!(pool.claim(1), Some(64));
        let pages = pool.release(64);
        assert_eq!(pages, 1);
    }

    #[test]
    fn exhausted_first_word_falls_through_to_second() {
        let mut pool = PageSetHeaderPoolState::new();
        // Manually fill word 0 (slots 0-63).
        pool.free_bits[0] = u64::MAX;
        for i in 0..64 {
            pool.claimed_pages[i] = 1;
        }
        // Next claim must come from word 1.
        let s = pool.claim(1).unwrap();
        assert_eq!(s, 64);
    }
}
