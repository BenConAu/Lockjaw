/// Pure PageSet table and header model for tracking allocated page sets.
///
/// This is the decision logic -- no static mut, no page allocator, no unsafe.
/// The kernel owns the mutable state and calls these functions to determine
/// what to do. Tests can create instances and verify behavior directly.

/// Maximum number of PageSets tracked in a table.
pub const MAX_PAGESETS: usize = 128;

/// Maximum data pages per PageSet. Each PageSet uses one additional page
/// as a header storing the physical addresses of all data pages. The header
/// page holds: count (u64), reserved (u64), then up to 510 page addresses.
pub const MAX_PAGES_PER_SET: usize = 510;

// ---------------------------------------------------------------------------
// PageSetHeader -- lives in the first allocated page of every PageSet
// ---------------------------------------------------------------------------

/// Page-resident header for a PageSet. Stored in the first allocated page.
/// Contains the physical addresses of all data pages in the set.
/// The kernel reads/writes this in place via KERNEL_VA_OFFSET.
/// Tests create instances on the stack and verify page lookup logic.
#[repr(C)]
pub struct PageSetHeader {
    /// Number of data pages in the set (does not count the header page itself).
    pub count: u64,
    /// Handle reference count. Incremented on handle_insert, decremented
    /// on handle_remove. Initialized to 0 by page zeroing.
    pub refcount: u32,
    /// Active mapping count across all processes. Incremented by
    /// sys_map_pages, decremented by sys_unmap_pages. Pages are freed
    /// when both refcount and map_count reach zero.
    pub map_count: u32,
    /// Physical addresses of the data pages. Only pages[0..count] are valid.
    pub pages: [u64; MAX_PAGES_PER_SET],
}

impl PageSetHeader {
    /// Create an empty header.
    pub const fn empty() -> Self {
        Self {
            count: 0,
            refcount: 0,
            map_count: 0,
            pages: [0; MAX_PAGES_PER_SET],
        }
    }

    /// Initialize the header with the given page addresses.
    pub fn init(&mut self, page_addrs: &[u64]) {
        self.count = page_addrs.len() as u64;
        for (i, addr) in page_addrs.iter().enumerate() {
            self.pages[i] = *addr;
        }
    }

    /// Get the physical address of a data page by index.
    /// Returns None if the index is out of range.
    pub fn get_page(&self, index: usize) -> Option<u64> {
        if index < self.count as usize {
            Some(self.pages[index])
        } else {
            None
        }
    }

    /// Get the number of data pages.
    pub fn data_page_count(&self) -> usize {
        self.count as usize
    }

    /// Increment the mapping count. Called when pages are mapped into
    /// an address space via sys_map_pages.
    pub fn inc_map_count(&mut self) {
        self.map_count = self.map_count.checked_add(1)
            .expect("map_count overflow");
    }

    /// Decrement the mapping count. Called when pages are unmapped.
    /// Returns true if both map_count and refcount are now zero
    /// (caller should free the PageSet).
    pub fn dec_map_count(&mut self) -> bool {
        assert!(self.map_count > 0, "map_count underflow");
        self.map_count -= 1;
        self.map_count == 0 && self.refcount == 0
    }

    /// Increment the handle reference count. Called when a new handle
    /// to this PageSet is inserted into a handle table.
    pub fn inc_refcount(&mut self) {
        self.refcount = self.refcount.checked_add(1)
            .expect("refcount overflow");
    }

    /// Decrement the handle reference count. Called when a handle to
    /// this PageSet is removed (sys_close_handle).
    /// Returns true if both refcount and map_count are now zero
    /// (caller should free the PageSet).
    pub fn dec_refcount(&mut self) -> bool {
        assert!(self.refcount > 0, "refcount underflow");
        self.refcount -= 1;
        self.refcount == 0 && self.map_count == 0
    }
}

// ---------------------------------------------------------------------------
// PageSetEntry -- thin entry stored in the tracking table
// ---------------------------------------------------------------------------

/// A tracked PageSet: just the count and the header page's physical address.
/// The actual page addresses live in the header page, not in the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageSetEntry {
    pub count: usize,
    pub header_paddr: u64,
}

/// Errors from PageSet table operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageSetError {
    TableFull,
    InvalidCount,
    InvalidId,
    SlotEmpty,
}

/// A table that tracks allocated PageSets by slot index.
/// No global state -- the caller owns the instance.
pub struct PageSetTable {
    slots: [Option<PageSetEntry>; MAX_PAGESETS],
}

impl PageSetTable {
    /// Create an empty table.
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_PAGESETS],
        }
    }

    /// Reserve a slot and store a PageSet entry. Returns the slot index (ID).
    pub fn insert(&mut self, entry: PageSetEntry) -> Result<usize, PageSetError> {
        if entry.count == 0 || entry.count > MAX_PAGES_PER_SET {
            return Err(PageSetError::InvalidCount);
        }

        let slot = self.slots.iter()
            .position(|s| s.is_none())
            .ok_or(PageSetError::TableFull)?;

        self.slots[slot] = Some(entry);
        Ok(slot)
    }

    /// Look up a PageSet by ID.
    pub fn get(&self, id: usize) -> Result<&PageSetEntry, PageSetError> {
        if id >= MAX_PAGESETS {
            return Err(PageSetError::InvalidId);
        }
        self.slots[id].as_ref().ok_or(PageSetError::SlotEmpty)
    }

    /// Remove a PageSet by ID, returning it. The caller is responsible for
    /// freeing the physical pages.
    pub fn remove(&mut self, id: usize) -> Result<PageSetEntry, PageSetError> {
        if id >= MAX_PAGESETS {
            return Err(PageSetError::InvalidId);
        }
        self.slots[id].take().ok_or(PageSetError::SlotEmpty)
    }

    /// Number of occupied slots.
    pub fn count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Find a PageSet by its header physical address. Returns the slot
    /// index, or None if no entry has that header_paddr. Used for
    /// reverse lookup when consuming via a handle.
    pub fn find_by_header_paddr(&self, header_paddr: u64) -> Option<usize> {
        for i in 0..MAX_PAGESETS {
            if let Some(entry) = &self.slots[i] {
                if entry.header_paddr == header_paddr {
                    return Some(i);
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(count: usize) -> PageSetEntry {
        PageSetEntry {
            count,
            header_paddr: 0x1000 * (count as u64 + 1),
        }
    }

    // --- PageSetTable tests ---

    #[test]
    fn insert_and_get() {
        let mut table = PageSetTable::new();
        let entry = make_entry(2);
        let id = table.insert(entry).unwrap();
        assert_eq!(id, 0);

        let got = table.get(id).unwrap();
        assert_eq!(got.count, 2);
    }

    #[test]
    fn insert_fills_slots_sequentially() {
        let mut table = PageSetTable::new();
        for i in 0..5 {
            let id = table.insert(make_entry(1)).unwrap();
            assert_eq!(id, i);
        }
        assert_eq!(table.count(), 5);
    }

    #[test]
    fn insert_reuses_removed_slot() {
        let mut table = PageSetTable::new();
        let id0 = table.insert(make_entry(1)).unwrap();
        let id1 = table.insert(make_entry(1)).unwrap();
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);

        table.remove(id0).unwrap();
        assert_eq!(table.count(), 1);

        let id2 = table.insert(make_entry(1)).unwrap();
        assert_eq!(id2, 0);
    }

    #[test]
    fn table_full() {
        let mut table = PageSetTable::new();
        for _ in 0..MAX_PAGESETS {
            table.insert(make_entry(1)).unwrap();
        }
        assert_eq!(table.insert(make_entry(1)), Err(PageSetError::TableFull));
    }

    #[test]
    fn invalid_count_zero() {
        let mut table = PageSetTable::new();
        assert_eq!(
            table.insert(PageSetEntry { count: 0, header_paddr: 0x1000 }),
            Err(PageSetError::InvalidCount)
        );
    }

    #[test]
    fn invalid_count_too_large() {
        let mut table = PageSetTable::new();
        assert_eq!(
            table.insert(PageSetEntry { count: MAX_PAGES_PER_SET + 1, header_paddr: 0x1000 }),
            Err(PageSetError::InvalidCount)
        );
    }

    #[test]
    fn get_invalid_id() {
        let table = PageSetTable::new();
        assert_eq!(table.get(MAX_PAGESETS), Err(PageSetError::InvalidId));
        assert_eq!(table.get(999), Err(PageSetError::InvalidId));
    }

    #[test]
    fn get_empty_slot() {
        let table = PageSetTable::new();
        assert_eq!(table.get(0), Err(PageSetError::SlotEmpty));
    }

    #[test]
    fn consume_after_get_prevents_reuse() {
        let mut table = PageSetTable::new();
        let id = table.insert(make_entry(1)).unwrap();

        let entry = table.get(id).unwrap();
        assert_eq!(entry.count, 1);

        table.remove(id).unwrap();

        assert_eq!(table.get(id), Err(PageSetError::SlotEmpty));
        assert_eq!(table.remove(id), Err(PageSetError::SlotEmpty));
    }

    #[test]
    fn consume_does_not_affect_other_entries() {
        let mut table = PageSetTable::new();
        let id0 = table.insert(make_entry(1)).unwrap();
        let id1 = table.insert(make_entry(2)).unwrap();

        table.remove(id0).unwrap();

        let entry = table.get(id1).unwrap();
        assert_eq!(entry.count, 2);
    }

    #[test]
    fn remove_returns_entry() {
        let mut table = PageSetTable::new();
        let id = table.insert(make_entry(3)).unwrap();

        let removed = table.remove(id).unwrap();
        assert_eq!(removed.count, 3);

        assert_eq!(table.get(id), Err(PageSetError::SlotEmpty));
    }

    #[test]
    fn remove_empty_slot_fails() {
        let mut table = PageSetTable::new();
        assert_eq!(table.remove(0), Err(PageSetError::SlotEmpty));
    }

    #[test]
    fn remove_invalid_id_fails() {
        let mut table = PageSetTable::new();
        assert_eq!(table.remove(MAX_PAGESETS), Err(PageSetError::InvalidId));
    }

    // --- PageSetHeader tests ---

    #[test]
    fn header_empty() {
        let h = PageSetHeader::empty();
        assert_eq!(h.data_page_count(), 0);
        assert_eq!(h.get_page(0), None);
    }

    #[test]
    fn header_init_and_get() {
        let mut h = PageSetHeader::empty();
        h.init(&[0x1000, 0x2000, 0x3000]);
        assert_eq!(h.data_page_count(), 3);
        assert_eq!(h.get_page(0), Some(0x1000));
        assert_eq!(h.get_page(1), Some(0x2000));
        assert_eq!(h.get_page(2), Some(0x3000));
        assert_eq!(h.get_page(3), None);
    }

    #[test]
    fn header_single_page() {
        let mut h = PageSetHeader::empty();
        h.init(&[0xABCD_0000]);
        assert_eq!(h.data_page_count(), 1);
        assert_eq!(h.get_page(0), Some(0xABCD_0000));
    }

    #[test]
    fn header_many_pages() {
        let mut h = PageSetHeader::empty();
        // Test with 75 pages (320x240x4 framebuffer)
        let mut addrs = [0u64; 75];
        for i in 0..75 {
            addrs[i] = (i as u64 + 1) * 0x1000;
        }
        h.init(&addrs);
        assert_eq!(h.data_page_count(), 75);
        assert_eq!(h.get_page(0), Some(0x1000));
        assert_eq!(h.get_page(74), Some(75 * 0x1000));
        assert_eq!(h.get_page(75), None);
    }

    #[test]
    fn header_size_fits_in_page() {
        assert!(core::mem::size_of::<PageSetHeader>() <= 4096);
    }

    // --- find_by_header_paddr ---

    #[test]
    fn find_by_header_paddr_found() {
        let mut table = PageSetTable::new();
        let e0 = PageSetEntry { count: 1, header_paddr: 0xA000 };
        let e1 = PageSetEntry { count: 2, header_paddr: 0xB000 };
        let id0 = table.insert(e0).unwrap();
        let _id1 = table.insert(e1).unwrap();

        assert_eq!(table.find_by_header_paddr(0xA000), Some(id0));
        assert_eq!(table.find_by_header_paddr(0xB000), Some(1));
    }

    #[test]
    fn find_by_header_paddr_not_found() {
        let mut table = PageSetTable::new();
        table.insert(PageSetEntry { count: 1, header_paddr: 0xA000 }).unwrap();
        assert_eq!(table.find_by_header_paddr(0xC000), None);
    }

    #[test]
    fn find_by_header_paddr_after_remove() {
        let mut table = PageSetTable::new();
        let id = table.insert(PageSetEntry { count: 1, header_paddr: 0xA000 }).unwrap();
        table.remove(id).unwrap();
        assert_eq!(table.find_by_header_paddr(0xA000), None);
    }

    // --- map_count lifecycle ---

    #[test]
    fn map_count_inc_dec() {
        let mut h = PageSetHeader::empty();
        h.refcount = 1;
        h.inc_map_count();
        assert_eq!(h.map_count, 1);
        h.inc_map_count();
        assert_eq!(h.map_count, 2);
        assert!(!h.dec_map_count()); // map_count=1, refcount=1 → not free
        assert!(!h.dec_map_count()); // map_count=0, refcount=1 → not free
    }

    #[test]
    fn map_count_zero_with_zero_refcount_signals_free() {
        let mut h = PageSetHeader::empty();
        h.refcount = 0;
        h.inc_map_count();
        assert!(h.dec_map_count()); // map_count=0, refcount=0 → free
    }

    #[test]
    #[should_panic(expected = "map_count underflow")]
    fn map_count_underflow_panics() {
        let mut h = PageSetHeader::empty();
        h.dec_map_count();
    }

    // --- zeroed header makes stale reads inert ---

    #[test]
    fn zeroed_header_has_no_pages() {
        let h = PageSetHeader::empty();
        // A zeroed header (as would result from page zeroing after
        // consumption) must report 0 pages and reject all lookups.
        assert_eq!(h.data_page_count(), 0);
        assert_eq!(h.get_page(0), None);
    }

    // --- refcount ---

    #[test]
    fn inc_refcount() {
        let mut h = PageSetHeader::empty();
        h.inc_refcount();
        assert_eq!(h.refcount, 1);
        h.inc_refcount();
        assert_eq!(h.refcount, 2);
    }

    #[test]
    fn dec_refcount_not_zero() {
        let mut h = PageSetHeader::empty();
        h.inc_refcount();
        h.inc_refcount();
        assert!(!h.dec_refcount()); // refcount 1, not free-on-zero
    }

    #[test]
    fn dec_refcount_free_on_zero() {
        let mut h = PageSetHeader::empty();
        h.inc_refcount();
        assert!(h.dec_refcount()); // refcount 0, map_count 0 → free
    }

    #[test]
    fn dec_refcount_not_free_if_mapped() {
        let mut h = PageSetHeader::empty();
        h.inc_refcount();
        h.inc_map_count();
        assert!(!h.dec_refcount()); // refcount 0, map_count 1 → NOT free
    }

    #[test]
    fn dec_map_count_free_when_refcount_zero() {
        let mut h = PageSetHeader::empty();
        h.inc_map_count();
        // refcount is 0, map_count goes to 0 → free
        assert!(h.dec_map_count());
    }

    #[test]
    fn dec_map_count_not_free_when_refcount_nonzero() {
        let mut h = PageSetHeader::empty();
        h.inc_refcount();
        h.inc_map_count();
        assert!(!h.dec_map_count()); // refcount 1 → NOT free
    }

    #[test]
    #[should_panic(expected = "refcount underflow")]
    fn dec_refcount_underflow_panics() {
        let mut h = PageSetHeader::empty();
        h.dec_refcount();
    }
}
