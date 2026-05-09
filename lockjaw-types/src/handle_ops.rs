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
// Slot revocation walks (two-phase: validate read-only, apply write)
// ---------------------------------------------------------------------------

/// Per-slot info yielded during a revoke walk.
///
/// The kernel-side caller uses this to drive per-kind cleanup
/// (cross-process PTE clear, dec_map_count, dec_refcount) on the
/// matching slot before / as the slot itself is cleared.
///
/// `kind` is the slot's pre-clear HandleKind so callers can distinguish
/// PageSet (refcounted) from non-PageSet kinds without a second lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotRevokeAction {
    /// True iff this is a PageSet handle with `mapped_va_page != 0`.
    /// Caller's PTE-clear callback runs only when this is true.
    pub had_mapping: bool,
    /// VA page of the active mapping (valid only when `had_mapping`).
    pub mapped_va_page: u32,
    /// Original handle kind. PageSet kinds need dec_refcount; other
    /// kinds (Endpoint / Notification / Reply) do not under today's
    /// accounting model.
    pub kind: HandleKind,
}

/// Phase 1: read-only walk. For each slot whose `object_paddr ==
/// target`, build a `SlotRevokeAction` and pass it to `on_action`.
/// Returns the number of matching slots seen.
///
/// **No mutation.** The caller's callback may inspect the action but
/// must not mutate the slots. Designed for revoke's validate phase
/// (count slots, validate corresponding PTEs read-only).
pub fn slot_revoke_validate<F>(
    slots: &[HandleEntry],
    target: u64,
    mut on_action: F,
) -> usize
where
    F: FnMut(&SlotRevokeAction),
{
    let mut count = 0;
    for slot in slots.iter() {
        if slot.object_paddr != target {
            continue;
        }
        let action = action_for(slot);
        on_action(&action);
        count += 1;
    }
    count
}

/// Phase 2: write walk. For each slot whose `object_paddr == target`,
/// build a `SlotRevokeAction`, hand it to `on_action`, then clear
/// the slot. Returns the number of cleared slots.
///
/// `on_action` runs BEFORE the slot is zeroed so the caller's PTE
/// clear and refcount/map_count decrements can read the action's
/// `mapped_va_page` and `kind`.
///
/// MUST be called only after a successful matching `slot_revoke_validate`
/// against the same `(slots, target)` pair within the same critical
/// section (GKL held). The action stream is identical across both
/// passes under that precondition.
pub fn slot_revoke_apply<F>(
    slots: &mut [HandleEntry],
    target: u64,
    mut on_action: F,
) -> usize
where
    F: FnMut(&SlotRevokeAction),
{
    let mut count = 0;
    for slot in slots.iter_mut() {
        if slot.object_paddr != target {
            continue;
        }
        let action = action_for(slot);
        on_action(&action);
        *slot = HandleEntry::EMPTY;
        count += 1;
    }
    count
}

fn action_for(slot: &HandleEntry) -> SlotRevokeAction {
    match slot.kind {
        HandleKind::PageSet { mapped_va_page } => SlotRevokeAction {
            had_mapping: mapped_va_page != 0,
            mapped_va_page,
            kind: slot.kind,
        },
        other => SlotRevokeAction {
            had_mapping: false,
            mapped_va_page: 0,
            kind: other,
        },
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

    // --- slot_revoke_validate / slot_revoke_apply ---

    #[test]
    fn revoke_validate_does_not_mutate() {
        let mut slots = empty_table(4);
        // PageSet inserts zero mapped_va_page (per-address-space rule).
        // Set the mapping VA explicitly to model an active mapping.
        let h0 = slot_insert(&mut slots, 0x1000, Rights::none(),
                             HandleKind::PageSet { mapped_va_page: 0 }).unwrap();
        slot_set_mapped_va(&mut slots, h0, 0x400).unwrap();
        slot_insert(&mut slots, 0x2000, Rights::none(),
                    HandleKind::Endpoint { caller_token: 0 }).unwrap();
        slot_insert(&mut slots, 0x1000, Rights::none(),
                    HandleKind::PageSet { mapped_va_page: 0 }).unwrap();

        let snapshot = slots.clone();
        let mut actions = vec![];
        let n = slot_revoke_validate(&slots, 0x1000, |a| actions.push(*a));
        assert_eq!(n, 2);
        assert_eq!(slots, snapshot);

        // Action stream reflects pre-clear state.
        assert!(actions[0].had_mapping);
        assert_eq!(actions[0].mapped_va_page, 0x400);
        assert!(matches!(actions[0].kind, HandleKind::PageSet { mapped_va_page: 0x400 }));
        assert!(!actions[1].had_mapping);
        assert!(matches!(actions[1].kind, HandleKind::PageSet { mapped_va_page: 0 }));
    }

    #[test]
    fn revoke_apply_clears_matching_slots() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::none(),
                    HandleKind::PageSet { mapped_va_page: 0x400 }).unwrap();
        slot_insert(&mut slots, 0x2000, Rights::none(),
                    HandleKind::Endpoint { caller_token: 0 }).unwrap();
        slot_insert(&mut slots, 0x1000, Rights::none(),
                    HandleKind::PageSet { mapped_va_page: 0 }).unwrap();

        let n = slot_revoke_apply(&mut slots, 0x1000, |_| {});
        assert_eq!(n, 2);

        // Both 0x1000 slots cleared; the 0x2000 slot survives.
        assert_eq!(slots[0], HandleEntry::EMPTY);
        assert_eq!(slots[1].object_paddr, 0x2000);
        assert_eq!(slots[2], HandleEntry::EMPTY);
    }

    #[test]
    fn revoke_apply_yields_action_before_clearing() {
        // Verifies the on_action callback observes pre-clear state.
        let mut slots = empty_table(2);
        let h = slot_insert(&mut slots, 0x1000, Rights::none(),
                            HandleKind::PageSet { mapped_va_page: 0 }).unwrap();
        slot_set_mapped_va(&mut slots, h, 0x400).unwrap();

        let mut seen = None;
        slot_revoke_apply(&mut slots, 0x1000, |a| seen = Some(*a));

        let action = seen.unwrap();
        assert!(action.had_mapping);
        assert_eq!(action.mapped_va_page, 0x400);
        assert_eq!(slots[0], HandleEntry::EMPTY);
    }

    #[test]
    fn revoke_validate_and_apply_yield_same_action_stream() {
        // The two-phase precondition: under stable handle table, the
        // action stream from validate and apply must match exactly so
        // accounting reconciliation in validate predicts apply.
        let mut slots = empty_table(8);
        let h0 = slot_insert(&mut slots, 0x1000, Rights::none(),
                             HandleKind::PageSet { mapped_va_page: 0 }).unwrap();
        slot_set_mapped_va(&mut slots, h0, 0x100).unwrap();
        slot_insert(&mut slots, 0x2000, Rights::none(),
                    HandleKind::Notification).unwrap();
        slot_insert(&mut slots, 0x1000, Rights::none(),
                    HandleKind::PageSet { mapped_va_page: 0 }).unwrap();
        let h2 = slot_insert(&mut slots, 0x1000, Rights::none(),
                             HandleKind::PageSet { mapped_va_page: 0 }).unwrap();
        slot_set_mapped_va(&mut slots, h2, 0x200).unwrap();

        let mut validate_actions = vec![];
        slot_revoke_validate(&slots, 0x1000, |a| validate_actions.push(*a));

        let mut apply_actions = vec![];
        slot_revoke_apply(&mut slots, 0x1000, |a| apply_actions.push(*a));

        assert_eq!(validate_actions, apply_actions);
        assert_eq!(validate_actions.len(), 3);
    }

    #[test]
    fn revoke_no_match_yields_zero() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::none(),
                    HandleKind::Endpoint { caller_token: 0 }).unwrap();

        let mut count = 0;
        let v = slot_revoke_validate(&slots, 0x9999, |_| count += 1);
        assert_eq!(v, 0);
        assert_eq!(count, 0);

        let snapshot = slots.clone();
        let a = slot_revoke_apply(&mut slots, 0x9999, |_| count += 1);
        assert_eq!(a, 0);
        assert_eq!(count, 0);
        assert_eq!(slots, snapshot);
    }

    #[test]
    fn revoke_apply_non_pageset_kind_not_marked_mapped() {
        // Endpoint / Notification / Reply slots set had_mapping = false
        // regardless of slot fields — the action's mapped_va_page is
        // only meaningful for PageSet.
        let mut slots = empty_table(4);
        slot_insert(&mut slots, 0x1000, Rights::none(),
                    HandleKind::Endpoint { caller_token: 7 }).unwrap();

        let mut seen = None;
        slot_revoke_apply(&mut slots, 0x1000, |a| seen = Some(*a));

        let action = seen.unwrap();
        assert!(!action.had_mapping);
        assert_eq!(action.mapped_va_page, 0);
        assert!(matches!(action.kind, HandleKind::Endpoint { caller_token: 7 }));
    }
}
