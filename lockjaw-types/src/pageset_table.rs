/// Pure PageSet table model for tracking allocated page sets.
///
/// This is the decision logic — no static mut, no page allocator, no unsafe.
/// The kernel owns the mutable state and calls these functions to determine
/// what to do. Tests can create instances and verify behavior directly.

use crate::addr::PhysAddr;

/// Maximum number of PageSets tracked in a table.
pub const MAX_PAGESETS: usize = 32;

/// Maximum pages per PageSet.
pub const MAX_PAGES_PER_SET: usize = 16;

/// A tracked PageSet: a set of physical page addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageSetEntry {
    pub count: usize,
    pub pages: [PhysAddr; MAX_PAGES_PER_SET],
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
/// No global state — the caller owns the instance.
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(count: usize) -> PageSetEntry {
        let mut pages = [PhysAddr::new(0); MAX_PAGES_PER_SET];
        for i in 0..count {
            pages[i] = PhysAddr::new(0x1000 * (i as u64 + 1));
        }
        PageSetEntry { count, pages }
    }

    #[test]
    fn insert_and_get() {
        let mut table = PageSetTable::new();
        let entry = make_entry(2);
        let id = table.insert(entry).unwrap();
        assert_eq!(id, 0);

        let got = table.get(id).unwrap();
        assert_eq!(got.count, 2);
        assert_eq!(got.pages[0].as_u64(), 0x1000);
        assert_eq!(got.pages[1].as_u64(), 0x2000);
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

        // Remove slot 0
        table.remove(id0).unwrap();
        assert_eq!(table.count(), 1);

        // Next insert should reuse slot 0
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
        assert_eq!(table.insert(make_entry(0)), Err(PageSetError::InvalidCount));
    }

    #[test]
    fn invalid_count_too_large() {
        let mut table = PageSetTable::new();
        let mut entry = make_entry(1);
        entry.count = MAX_PAGES_PER_SET + 1;
        assert_eq!(table.insert(entry), Err(PageSetError::InvalidCount));
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
    fn remove_returns_entry() {
        let mut table = PageSetTable::new();
        let id = table.insert(make_entry(3)).unwrap();

        let removed = table.remove(id).unwrap();
        assert_eq!(removed.count, 3);

        // Slot is now empty
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
}
