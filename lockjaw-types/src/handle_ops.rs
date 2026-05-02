/// Pure handle-table slot operations on `&[HandleEntry]` slices.
///
/// These functions contain the array-scanning, bounds-checking, and
/// rights-checking logic extracted from `src/cap/handle_table.rs`.
/// The kernel calls `table_slots()` to obtain the slice from a
/// PhysAddr, then delegates to these functions. No kernel APIs
/// (alloc, PTE, scheduler) are needed here.

use crate::object::{HandleEntry, HandleKind};
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

impl HandleError {
    /// Return the variant name as a static string.
    pub fn name(&self) -> &'static str {
        match self {
            HandleError::TableFull => "TableFull",
            HandleError::InvalidHandle => "InvalidHandle",
            HandleError::InsufficientRights => "InsufficientRights",
        }
    }
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
///
/// Rejects HandleKind::Empty — an occupied slot (object_paddr != 0)
/// with kind = Empty is an illegal state. Use HandleEntry::EMPTY
/// assignment for clearing slots instead.
///
/// For PageSet handles, mapped_va_page is forced to 0 regardless
/// of the input. Mapping state is per-address-space and must not
/// leak across handle copies (sys_export_handle, create_process).
pub fn slot_insert(
    slots: &mut [HandleEntry],
    object_paddr: u64,
    rights: Rights,
    kind: HandleKind,
) -> Result<u32, HandleError> {
    if matches!(kind, HandleKind::Empty) {
        return Err(HandleError::InvalidHandle);
    }
    let idx = find_empty_slot(slots).ok_or(HandleError::TableFull)?;
    // Clear per-address-space state on PageSet handles.
    let kind = match kind {
        HandleKind::PageSet { .. } => HandleKind::PageSet { mapped_va_page: 0 },
        other => other,
    };
    slots[idx] = HandleEntry {
        object_paddr,
        rights,
        kind,
    };
    Ok(idx as u32)
}

/// Remove a handle entry by index. Copies the entry out, then zeros
/// the slot. Returns the removed entry on success.
///
/// Ordering: copy-out BEFORE zeroing — kernel code reads
/// kind and rights from the returned entry.
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

/// Get the `mapped_va_page` for a PageSet handle entry by index.
/// Returns InvalidHandle if the slot is empty or not a PageSet.
pub fn slot_get_mapped_va(
    slots: &[HandleEntry],
    index: u32,
) -> Result<u32, HandleError> {
    let slot = slots.get(index as usize)
        .ok_or(HandleError::InvalidHandle)?;
    if slot.object_paddr == 0 {
        return Err(HandleError::InvalidHandle);
    }
    match slot.kind {
        HandleKind::PageSet { mapped_va_page } => Ok(mapped_va_page),
        _ => Err(HandleError::InvalidHandle),
    }
}

/// Set the `mapped_va_page` on a PageSet handle entry by index.
/// Returns InvalidHandle if the slot is empty or not a PageSet.
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
    match &mut slot.kind {
        HandleKind::PageSet { mapped_va_page } => {
            *mapped_va_page = va_page;
            Ok(())
        }
        _ => Err(HandleError::InvalidHandle),
    }
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
        let idx = slot_insert(&mut slots, 0x1000, Rights::from_bits(RIGHT_READ), HandleKind::Endpoint { caller_token: 0 });
        assert_eq!(idx, Ok(0));
        assert_eq!(slots[0].object_paddr, 0x1000);
    }

    #[test]
    fn insert_fills_sequentially() {
        let mut slots = empty_table(4);
        assert_eq!(slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }), Ok(0));
        assert_eq!(slot_insert(&mut slots, 0x2000, Rights::none(), HandleKind::Notification), Ok(1));
        assert_eq!(slot_insert(&mut slots, 0x3000, Rights::none(), HandleKind::Reply), Ok(2));
        assert_eq!(slot_insert(&mut slots, 0x4000, Rights::none(), HandleKind::PageSet { mapped_va_page: 0 }), Ok(3));
    }

    #[test]
    fn insert_reuses_removed_slot() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        slot_insert(&mut slots, 0x2000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        slot_remove(&mut slots, 0).unwrap();
        let idx = slot_insert(&mut slots, 0x3000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(slots[0].object_paddr, 0x3000);
    }

    #[test]
    fn insert_full_table_returns_table_full() {
        let mut slots = empty_table(2);
        slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        slot_insert(&mut slots, 0x2000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        assert_eq!(
            slot_insert(&mut slots, 0x3000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }),
            Err(HandleError::TableFull)
        );
    }

    // --- lookup ---

    #[test]
    fn lookup_valid_entry() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::from_bits(RIGHT_READ | RIGHT_WRITE), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        let entry = slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ)).unwrap();
        assert_eq!(entry.object_paddr, 0x1000);
        assert_eq!(entry.kind, HandleKind::Endpoint { caller_token: 0 });
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
        slot_insert(&mut slots, 0x1000, Rights::from_bits(RIGHT_READ | RIGHT_WRITE), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        assert!(slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ)).is_ok());
        assert!(slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ | RIGHT_WRITE)).is_ok());
    }

    #[test]
    fn lookup_missing_rights_returns_insufficient() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::from_bits(RIGHT_READ), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        assert_eq!(
            slot_lookup(&slots, 0, Rights::from_bits(RIGHT_GRANT)),
            Err(HandleError::InsufficientRights)
        );
        assert_eq!(
            slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ | RIGHT_GRANT)),
            Err(HandleError::InsufficientRights)
        );
    }

    #[test]
    fn lookup_no_required_rights_always_passes() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        assert!(slot_lookup(&slots, 0, Rights::none()).is_ok());
    }

    // --- remove ---

    #[test]
    fn remove_valid_entry_returns_it() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::from_bits(RIGHT_READ), HandleKind::PageSet { mapped_va_page: 0 }).unwrap();
        let removed = slot_remove(&mut slots, 0).unwrap();
        assert_eq!(removed.object_paddr, 0x1000);
        assert!(removed.kind.is_pageset());
        assert_eq!(removed.rights, Rights::from_bits(RIGHT_READ));
    }

    #[test]
    fn remove_zeros_slot() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }).unwrap();
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
        slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::PageSet { mapped_va_page: 0 }).unwrap();
        slot_insert(&mut slots, 0x2000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::PageSet { mapped_va_page: 0 }).unwrap();
        let count = slot_remove_all_by_object(&mut slots, 0x1000);
        assert_eq!(count, 2);
        assert_eq!(slots[0].object_paddr, 0);
        assert_eq!(slots[2].object_paddr, 0);
    }

    #[test]
    fn remove_all_by_object_preserves_others() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::PageSet { mapped_va_page: 0 }).unwrap();
        slot_insert(&mut slots, 0x2000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        slot_remove_all_by_object(&mut slots, 0x1000);
        assert_eq!(slots[1].object_paddr, 0x2000);
    }

    // --- mapped_va ---

    #[test]
    fn set_get_mapped_va_round_trip() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::PageSet { mapped_va_page: 0 }).unwrap();
        slot_set_mapped_va(&mut slots, 0, 0x400).unwrap();
        assert_eq!(slot_get_mapped_va(&slots, 0), Ok(0x400));
    }

    #[test]
    fn mapped_va_on_empty_slot_returns_invalid() {
        let mut slots = empty_table(4);
        assert_eq!(slot_get_mapped_va(&slots, 0), Err(HandleError::InvalidHandle));
        assert_eq!(slot_set_mapped_va(&mut slots, 0, 0x400), Err(HandleError::InvalidHandle));
    }

    #[test]
    fn mapped_va_on_non_pageset_returns_invalid() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::Endpoint { caller_token: 0 }).unwrap();
        assert_eq!(slot_get_mapped_va(&slots, 0), Err(HandleError::InvalidHandle));
        assert_eq!(slot_set_mapped_va(&mut slots, 0, 0x400), Err(HandleError::InvalidHandle));
    }

    // --- insert guards ---

    #[test]
    fn insert_rejects_empty_kind() {
        let mut slots = empty_table(4);
        assert_eq!(
            slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::Empty),
            Err(HandleError::InvalidHandle)
        );
    }

    #[test]
    fn insert_clears_pageset_mapped_va() {
        // Simulates export/copy: source handle has mapped_va_page = 0x400,
        // but the inserted copy must start at 0 (mapping is per-address-space).
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::none(), HandleKind::PageSet { mapped_va_page: 0x400 }).unwrap();
        assert_eq!(slot_get_mapped_va(&slots, 0), Ok(0));
    }

    // --- HandleEntry::EMPTY ---

    #[test]
    fn empty_entry_has_zero_paddr() {
        assert_eq!(HandleEntry::EMPTY.object_paddr, 0);
        assert_eq!(HandleEntry::EMPTY.kind, HandleKind::Empty);
    }
}
