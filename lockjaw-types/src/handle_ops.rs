/// Pure handle-table slot operations on `&[HandleEntry]` slices.
///
/// These functions contain the array-scanning, bounds-checking, and
/// rights-checking logic extracted from `src/cap/handle_table.rs`.
/// The kernel calls `table_slots()` to obtain the slice from a
/// PhysAddr, then delegates to these functions. No kernel APIs
/// (alloc, PTE, scheduler) are needed here.

use crate::object::{HandleEntry, ObjectType};
use crate::rights::Rights;

// ---------------------------------------------------------------------------
// HandleError — pure error vocabulary for slot operations
// ---------------------------------------------------------------------------

/// Errors from handle slot operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleError {
    TableFull,
    InvalidHandle,
    InsufficientRights,
}

// ---------------------------------------------------------------------------
// Slot operations
// ---------------------------------------------------------------------------

/// Find the first empty slot in a handle table slice.
/// A slot is empty when `object_paddr == 0`.
pub fn find_empty_slot(slots: &[HandleEntry]) -> Option<usize> {
    slots.iter().position(|s| s.object_paddr == 0)
}

/// Look up a handle entry by index with rights checking.
/// Returns a copy of the entry on success.
pub fn slot_lookup(
    slots: &[HandleEntry],
    index: u32,
    required: Rights,
) -> Result<HandleEntry, HandleError> {
    let slot = slots.get(index as usize)
        .ok_or(HandleError::InvalidHandle)?;
    if slot.object_paddr == 0 {
        return Err(HandleError::InvalidHandle);
    }
    if !slot.rights.contains(required) {
        return Err(HandleError::InsufficientRights);
    }
    Ok(*slot)
}

/// Insert a new handle entry into the first empty slot.
/// Returns the slot index on success.
pub fn slot_insert(
    slots: &mut [HandleEntry],
    object_paddr: u64,
    obj_type: ObjectType,
    rights: Rights,
) -> Result<u32, HandleError> {
    let idx = find_empty_slot(slots).ok_or(HandleError::TableFull)?;
    slots[idx] = HandleEntry {
        object_paddr,
        obj_type,
        rights,
        _padding: [0; 2],
        mapped_va_page: 0,
    };
    Ok(idx as u32)
}

/// Remove a handle entry by index. Copies the entry out, then zeros
/// the slot. Returns the removed entry on success.
///
/// Ordering: copy-out BEFORE zeroing — kernel code reads
/// mapped_va_page, rights, and obj_type from the returned entry.
pub fn slot_remove(
    slots: &mut [HandleEntry],
    index: u32,
) -> Result<HandleEntry, HandleError> {
    let slot = slots.get_mut(index as usize)
        .ok_or(HandleError::InvalidHandle)?;
    if slot.object_paddr == 0 {
        return Err(HandleError::InvalidHandle);
    }
    let removed = *slot;
    *slot = HandleEntry::EMPTY;
    Ok(removed)
}

/// Remove all handle entries pointing at a given object address.
/// Returns the number of slots cleared.
///
/// Callers don't need the removed entries — cleanup decisions
/// have already moved to `decide_close_handle` / `decide_teardown_handle`.
pub fn slot_remove_all_by_object(
    slots: &mut [HandleEntry],
    object_paddr: u64,
) -> usize {
    let mut count = 0;
    for slot in slots.iter_mut() {
        if slot.object_paddr == object_paddr {
            *slot = HandleEntry::EMPTY;
            count += 1;
        }
    }
    count
}

/// Get the `mapped_va_page` field for a handle entry by index.
pub fn slot_get_mapped_va(
    slots: &[HandleEntry],
    index: u32,
) -> Result<u32, HandleError> {
    let slot = slots.get(index as usize)
        .ok_or(HandleError::InvalidHandle)?;
    if slot.object_paddr == 0 {
        return Err(HandleError::InvalidHandle);
    }
    Ok(slot.mapped_va_page)
}

/// Set the `mapped_va_page` field on a handle entry by index.
pub fn slot_set_mapped_va(
    slots: &mut [HandleEntry],
    index: u32,
    va_page: u32,
) -> Result<(), HandleError> {
    let slot = slots.get_mut(index as usize)
        .ok_or(HandleError::InvalidHandle)?;
    if slot.object_paddr == 0 {
        return Err(HandleError::InvalidHandle);
    }
    slot.mapped_va_page = va_page;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use crate::rights::{RIGHT_READ, RIGHT_WRITE, RIGHT_GRANT};
    use alloc::vec;

    fn empty_table(n: usize) -> alloc::vec::Vec<HandleEntry> {
        vec![HandleEntry::EMPTY; n]
    }

    // --- insert ---

    #[test]
    fn insert_into_empty_table_returns_index_0() {
        let mut slots = empty_table(4);
        let idx = slot_insert(&mut slots, 0x1000, ObjectType::Endpoint, Rights::from_bits(RIGHT_READ));
        assert_eq!(idx, Ok(0));
        assert_eq!(slots[0].object_paddr, 0x1000);
    }

    #[test]
    fn insert_fills_sequentially() {
        let mut slots = empty_table(4);
        assert_eq!(slot_insert(&mut slots, 0x1000, ObjectType::Endpoint, Rights::none()), Ok(0));
        assert_eq!(slot_insert(&mut slots, 0x2000, ObjectType::Notification, Rights::none()), Ok(1));
        assert_eq!(slot_insert(&mut slots, 0x3000, ObjectType::Reply, Rights::none()), Ok(2));
        assert_eq!(slot_insert(&mut slots, 0x4000, ObjectType::PageSet, Rights::none()), Ok(3));
    }

    #[test]
    fn insert_reuses_removed_slot() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, ObjectType::Endpoint, Rights::none()).unwrap();
        slot_insert(&mut slots, 0x2000, ObjectType::Endpoint, Rights::none()).unwrap();
        // Remove slot 0
        slot_remove(&mut slots, 0).unwrap();
        // Next insert should reuse slot 0
        let idx = slot_insert(&mut slots, 0x3000, ObjectType::Endpoint, Rights::none()).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(slots[0].object_paddr, 0x3000);
    }

    #[test]
    fn insert_full_table_returns_table_full() {
        let mut slots = empty_table(2);
        slot_insert(&mut slots, 0x1000, ObjectType::Endpoint, Rights::none()).unwrap();
        slot_insert(&mut slots, 0x2000, ObjectType::Endpoint, Rights::none()).unwrap();
        assert_eq!(
            slot_insert(&mut slots, 0x3000, ObjectType::Endpoint, Rights::none()),
            Err(HandleError::TableFull)
        );
    }

    // --- lookup ---

    #[test]
    fn lookup_valid_entry() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, ObjectType::Endpoint, Rights::from_bits(RIGHT_READ | RIGHT_WRITE)).unwrap();
        let entry = slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ)).unwrap();
        assert_eq!(entry.object_paddr, 0x1000);
        assert_eq!(entry.obj_type, ObjectType::Endpoint);
    }

    #[test]
    fn lookup_out_of_bounds_returns_invalid() {
        let slots = empty_table(4);
        assert_eq!(slot_lookup(&slots, 99, Rights::none()), Err(HandleError::InvalidHandle));
    }

    #[test]
    fn lookup_empty_slot_returns_invalid() {
        let slots = empty_table(4);
        assert_eq!(slot_lookup(&slots, 0, Rights::none()), Err(HandleError::InvalidHandle));
    }

    #[test]
    fn lookup_rights_subset_passes() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, ObjectType::Endpoint, Rights::from_bits(RIGHT_READ | RIGHT_WRITE)).unwrap();
        // Requesting only READ against READ|WRITE — should pass
        assert!(slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ)).is_ok());
        // Requesting READ|WRITE against READ|WRITE — should pass
        assert!(slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ | RIGHT_WRITE)).is_ok());
    }

    #[test]
    fn lookup_missing_rights_returns_insufficient() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, ObjectType::Endpoint, Rights::from_bits(RIGHT_READ)).unwrap();
        // Requesting GRANT when only READ present
        assert_eq!(
            slot_lookup(&slots, 0, Rights::from_bits(RIGHT_GRANT)),
            Err(HandleError::InsufficientRights)
        );
        // Requesting READ|GRANT when only READ present
        assert_eq!(
            slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ | RIGHT_GRANT)),
            Err(HandleError::InsufficientRights)
        );
    }

    #[test]
    fn lookup_no_required_rights_always_passes() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, ObjectType::Endpoint, Rights::none()).unwrap();
        // Requiring no rights should always succeed on occupied slot
        assert!(slot_lookup(&slots, 0, Rights::none()).is_ok());
    }

    // --- remove ---

    #[test]
    fn remove_valid_entry_returns_it() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, ObjectType::PageSet, Rights::from_bits(RIGHT_READ)).unwrap();
        let removed = slot_remove(&mut slots, 0).unwrap();
        assert_eq!(removed.object_paddr, 0x1000);
        assert_eq!(removed.obj_type, ObjectType::PageSet);
        assert_eq!(removed.rights, Rights::from_bits(RIGHT_READ));
    }

    #[test]
    fn remove_zeros_slot() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, ObjectType::Endpoint, Rights::none()).unwrap();
        slot_remove(&mut slots, 0).unwrap();
        assert_eq!(slots[0].object_paddr, 0);
    }

    #[test]
    fn remove_empty_slot_returns_invalid() {
        let mut slots = empty_table(4);
        assert_eq!(slot_remove(&mut slots, 0), Err(HandleError::InvalidHandle));
    }

    #[test]
    fn remove_out_of_bounds_returns_invalid() {
        let mut slots = empty_table(4);
        assert_eq!(slot_remove(&mut slots, 99), Err(HandleError::InvalidHandle));
    }

    // --- remove_all_by_object ---

    #[test]
    fn remove_all_by_object_clears_matching() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, ObjectType::PageSet, Rights::none()).unwrap();
        slot_insert(&mut slots, 0x2000, ObjectType::Endpoint, Rights::none()).unwrap();
        slot_insert(&mut slots, 0x1000, ObjectType::PageSet, Rights::none()).unwrap();
        let count = slot_remove_all_by_object(&mut slots, 0x1000);
        assert_eq!(count, 2);
        assert_eq!(slots[0].object_paddr, 0);
        assert_eq!(slots[2].object_paddr, 0);
    }

    #[test]
    fn remove_all_by_object_preserves_others() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, ObjectType::PageSet, Rights::none()).unwrap();
        slot_insert(&mut slots, 0x2000, ObjectType::Endpoint, Rights::none()).unwrap();
        slot_remove_all_by_object(&mut slots, 0x1000);
        assert_eq!(slots[1].object_paddr, 0x2000);
    }

    // --- mapped_va ---

    #[test]
    fn set_get_mapped_va_round_trip() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, ObjectType::PageSet, Rights::none()).unwrap();
        slot_set_mapped_va(&mut slots, 0, 0x400).unwrap();
        assert_eq!(slot_get_mapped_va(&slots, 0), Ok(0x400));
    }

    #[test]
    fn mapped_va_on_empty_slot_returns_invalid() {
        let mut slots = empty_table(4);
        assert_eq!(slot_get_mapped_va(&slots, 0), Err(HandleError::InvalidHandle));
        assert_eq!(slot_set_mapped_va(&mut slots, 0, 0x400), Err(HandleError::InvalidHandle));
    }

    // --- HandleEntry::EMPTY ---

    #[test]
    fn empty_entry_has_zero_paddr() {
        assert_eq!(HandleEntry::EMPTY.object_paddr, 0);
        assert_eq!(HandleEntry::EMPTY.mapped_va_page, 0);
    }
}
