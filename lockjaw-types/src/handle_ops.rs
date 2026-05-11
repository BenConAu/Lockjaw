/// Pure handle-table slot operations on `&[HandleEntry]` slices.
///
/// These functions contain the array-scanning, bounds-checking, and
/// rights-checking logic extracted from `src/cap/handle_table.rs`.
/// The kernel calls `table_slots()` to obtain the slice from a
/// PhysAddr, then delegates to these functions. No kernel APIs
/// (alloc, PTE, scheduler) are needed here.

use crate::addr::KernelVa;
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
/// A slot is empty when `kind == HandleKind::Empty`.
pub fn find_empty_slot(slots: &[HandleEntry]) -> Option<usize> {
    slots.iter().position(|s| matches!(s.kind, HandleKind::Empty))
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
    if matches!(slot.kind, HandleKind::Empty) {
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
/// Rejects `HandleKind::Empty` — an inserted handle must carry the
/// object's typed address inside its kind variant.
///
/// For PageSet handles, `mapped_va_page` is forced to 0 regardless
/// of the input. Mapping state is per-address-space and must not
/// leak across handle copies (sys_export_handle, create_process).
pub fn slot_insert(
    slots: &mut [HandleEntry],
    rights: Rights,
    kind: HandleKind,
) -> Result<u32, HandleError> {
    if matches!(kind, HandleKind::Empty) {
        return Err(HandleError::InvalidHandle);
    }
    let idx = find_empty_slot(slots).ok_or(HandleError::TableFull)?;
    // Clear per-address-space state on PageSet handles.
    let kind = match kind {
        HandleKind::PageSet { kva, .. } => HandleKind::PageSet { kva, mapped_va_page: 0 },
        other => other,
    };
    slots[idx] = HandleEntry { rights, kind };
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
    if matches!(slot.kind, HandleKind::Empty) {
        return Err(HandleError::InvalidHandle);
    }
    let removed = *slot;
    *slot = HandleEntry::EMPTY;
    Ok(removed)
}

/// Get the `mapped_va_page` for a PageSet handle entry by index.
/// Returns InvalidHandle if the slot is empty or not a PageSet.
pub fn slot_get_mapped_va(
    slots: &[HandleEntry],
    index: u32,
) -> Result<u32, HandleError> {
    let slot = slots.get(index as usize)
        .ok_or(HandleError::InvalidHandle)?;
    match slot.kind {
        HandleKind::PageSet { mapped_va_page, .. } => Ok(mapped_va_page),
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
    match &mut slot.kind {
        HandleKind::PageSet { mapped_va_page, .. } => {
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

/// Phase 1: read-only walk. For each PageSet slot whose `kva == target`,
/// build a `SlotRevokeAction` and pass it to `on_action`. Returns the
/// number of matching slots seen.
///
/// PageSet revocation is the only revoke shape today — non-PageSet
/// kernel objects don't go through cross-process revoke walks. The
/// target is typed `KernelVa` so callers can't accidentally pass a
/// `PhysAddr` here.
///
/// **No mutation.** The caller's callback may inspect the action but
/// must not mutate the slots. Designed for revoke's validate phase
/// (count slots, validate corresponding PTEs read-only).
pub fn slot_revoke_validate<F>(
    slots: &[HandleEntry],
    target: KernelVa,
    mut on_action: F,
) -> usize
where
    F: FnMut(&SlotRevokeAction),
{
    let mut count = 0;
    for slot in slots.iter() {
        let HandleKind::PageSet { kva, mapped_va_page } = slot.kind else { continue };
        if kva != target {
            continue;
        }
        let action = SlotRevokeAction {
            had_mapping: mapped_va_page != 0,
            mapped_va_page,
            kind: slot.kind,
        };
        on_action(&action);
        count += 1;
    }
    count
}

/// Phase 2: write walk. For each PageSet slot whose `kva == target`,
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
    target: KernelVa,
    mut on_action: F,
) -> usize
where
    F: FnMut(&SlotRevokeAction),
{
    let mut count = 0;
    for slot in slots.iter_mut() {
        let HandleKind::PageSet { kva, mapped_va_page } = slot.kind else { continue };
        if kva != target {
            continue;
        }
        let action = SlotRevokeAction {
            had_mapping: mapped_va_page != 0,
            mapped_va_page,
            kind: slot.kind,
        };
        on_action(&action);
        *slot = HandleEntry::EMPTY;
        count += 1;
    }
    count
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use crate::addr::{KernelVa, PhysAddr};
    use crate::rights::{RIGHT_READ, RIGHT_WRITE, RIGHT_GRANT};
    use alloc::vec;

    fn empty_table(n: usize) -> alloc::vec::Vec<HandleEntry> {
        vec![HandleEntry::EMPTY; n]
    }

    fn ep(paddr: u64, token: u64) -> HandleKind {
        HandleKind::Endpoint { paddr: PhysAddr::new(paddr), caller_token: token }
    }
    fn notif(kva: u64) -> HandleKind {
        HandleKind::Notification { kva: KernelVa::new(kva) }
    }
    fn reply(kva: u64) -> HandleKind {
        HandleKind::Reply { kva: KernelVa::new(kva) }
    }
    fn ps(kva: u64) -> HandleKind {
        HandleKind::PageSet { kva: KernelVa::new(kva), mapped_va_page: 0 }
    }
    const KVA_A: u64 = 0xFFFF_8000_0000_1000;
    const KVA_B: u64 = 0xFFFF_8000_0000_2000;

    // --- insert ---

    #[test]
    fn insert_into_empty_table_returns_index_0() {
        let mut slots = empty_table(4);
        let idx = slot_insert(&mut slots, Rights::from_bits(RIGHT_READ), ep(0x1000, 0));
        assert_eq!(idx, Ok(0));
        assert_eq!(slots[0].kind, ep(0x1000, 0));
    }

    #[test]
    fn insert_fills_sequentially() {
        let mut slots = empty_table(4);
        assert_eq!(slot_insert(&mut slots, Rights::none(), ep(0x1000, 0)), Ok(0));
        assert_eq!(slot_insert(&mut slots, Rights::none(), notif(KVA_B)), Ok(1));
        assert_eq!(slot_insert(&mut slots, Rights::none(), reply(KVA_A)), Ok(2));
        assert_eq!(slot_insert(&mut slots, Rights::none(), ps(KVA_A)), Ok(3));
    }

    #[test]
    fn insert_reuses_removed_slot() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, Rights::none(), ep(0x1000, 0)).unwrap();
        slot_insert(&mut slots, Rights::none(), ep(0x2000, 0)).unwrap();
        slot_remove(&mut slots, 0).unwrap();
        let idx = slot_insert(&mut slots, Rights::none(), ep(0x3000, 0)).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(slots[0].kind, ep(0x3000, 0));
    }

    #[test]
    fn insert_full_table_returns_table_full() {
        let mut slots = empty_table(2);
        slot_insert(&mut slots, Rights::none(), ep(0x1000, 0)).unwrap();
        slot_insert(&mut slots, Rights::none(), ep(0x2000, 0)).unwrap();
        assert_eq!(
            slot_insert(&mut slots, Rights::none(), ep(0x3000, 0)),
            Err(HandleError::TableFull)
        );
    }

    // --- lookup ---

    #[test]
    fn lookup_valid_entry() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, Rights::from_bits(RIGHT_READ | RIGHT_WRITE), ep(0x1000, 0)).unwrap();
        let entry = slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ)).unwrap();
        assert_eq!(entry.kind, ep(0x1000, 0));
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
        slot_insert(&mut slots, Rights::from_bits(RIGHT_READ | RIGHT_WRITE), ep(0x1000, 0)).unwrap();
        assert!(slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ)).is_ok());
        assert!(slot_lookup(&slots, 0, Rights::from_bits(RIGHT_READ | RIGHT_WRITE)).is_ok());
    }

    #[test]
    fn lookup_missing_rights_returns_insufficient() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, Rights::from_bits(RIGHT_READ), ep(0x1000, 0)).unwrap();
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
        slot_insert(&mut slots, Rights::none(), ep(0x1000, 0)).unwrap();
        assert!(slot_lookup(&slots, 0, Rights::none()).is_ok());
    }

    // --- remove ---

    #[test]
    fn remove_valid_entry_returns_it() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, Rights::from_bits(RIGHT_READ), ps(KVA_A)).unwrap();
        let removed = slot_remove(&mut slots, 0).unwrap();
        assert!(removed.kind.is_pageset());
        assert_eq!(removed.rights, Rights::from_bits(RIGHT_READ));
    }

    #[test]
    fn remove_zeros_slot() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, Rights::none(), ep(0x1000, 0)).unwrap();
        slot_remove(&mut slots, 0).unwrap();
        assert_eq!(slots[0].kind, HandleKind::Empty);
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

    // --- mapped_va ---

    #[test]
    fn set_get_mapped_va_round_trip() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, Rights::none(), ps(KVA_A)).unwrap();
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
        slot_insert(&mut slots, Rights::none(), ep(0x1000, 0)).unwrap();
        assert_eq!(slot_get_mapped_va(&slots, 0), Err(HandleError::InvalidHandle));
        assert_eq!(slot_set_mapped_va(&mut slots, 0, 0x400), Err(HandleError::InvalidHandle));
    }

    // --- insert guards ---

    #[test]
    fn insert_rejects_empty_kind() {
        let mut slots = empty_table(4);
        assert_eq!(
            slot_insert(&mut slots, Rights::none(), HandleKind::Empty),
            Err(HandleError::InvalidHandle)
        );
    }

    #[test]
    fn insert_clears_pageset_mapped_va() {
        // Simulates export/copy: source handle has mapped_va_page = 0x400,
        // but the inserted copy must start at 0 (mapping is per-address-space).
        let mut slots = empty_table(4);
        slot_insert(&mut slots, Rights::none(),
            HandleKind::PageSet { kva: KernelVa::new(KVA_A), mapped_va_page: 0x400 }).unwrap();
        assert_eq!(slot_get_mapped_va(&slots, 0), Ok(0));
    }

    // --- HandleEntry::EMPTY ---

    #[test]
    fn empty_entry_kind_is_empty() {
        assert_eq!(HandleEntry::EMPTY.kind, HandleKind::Empty);
    }

    // --- slot_revoke_validate / slot_revoke_apply ---

    #[test]
    fn revoke_validate_does_not_mutate() {
        let mut slots = empty_table(4);
        // PageSet inserts zero mapped_va_page (per-address-space rule).
        // Set the mapping VA explicitly to model an active mapping.
        let h0 = slot_insert(&mut slots, Rights::none(), ps(KVA_A)).unwrap();
        slot_set_mapped_va(&mut slots, h0, 0x400).unwrap();
        slot_insert(&mut slots, Rights::none(), ep(0x2000, 0)).unwrap();
        slot_insert(&mut slots, Rights::none(), ps(KVA_A)).unwrap();

        let snapshot = slots.clone();
        let mut actions = vec![];
        let n = slot_revoke_validate(&slots, KernelVa::new(KVA_A), |a| actions.push(*a));
        assert_eq!(n, 2);
        assert_eq!(slots, snapshot);

        // Action stream reflects pre-clear state.
        assert!(actions[0].had_mapping);
        assert_eq!(actions[0].mapped_va_page, 0x400);
        assert!(matches!(actions[0].kind, HandleKind::PageSet { mapped_va_page: 0x400, .. }));
        assert!(!actions[1].had_mapping);
        assert!(matches!(actions[1].kind, HandleKind::PageSet { mapped_va_page: 0, .. }));
    }

    #[test]
    fn revoke_apply_clears_matching_slots() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, Rights::none(),
            HandleKind::PageSet { kva: KernelVa::new(KVA_A), mapped_va_page: 0x400 }).unwrap();
        slot_insert(&mut slots, Rights::none(), ep(0x2000, 0)).unwrap();
        slot_insert(&mut slots, Rights::none(), ps(KVA_A)).unwrap();

        let n = slot_revoke_apply(&mut slots, KernelVa::new(KVA_A), |_| {});
        assert_eq!(n, 2);

        // Both KVA_A slots cleared; the 0x2000 slot survives.
        assert_eq!(slots[0], HandleEntry::EMPTY);
        assert_eq!(slots[1].kind, ep(0x2000, 0));
        assert_eq!(slots[2], HandleEntry::EMPTY);
    }

    #[test]
    fn revoke_apply_yields_action_before_clearing() {
        // Verifies the on_action callback observes pre-clear state.
        let mut slots = empty_table(2);
        let h = slot_insert(&mut slots, Rights::none(), ps(KVA_A)).unwrap();
        slot_set_mapped_va(&mut slots, h, 0x400).unwrap();

        let mut seen = None;
        slot_revoke_apply(&mut slots, KernelVa::new(KVA_A), |a| seen = Some(*a));

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
        let h0 = slot_insert(&mut slots, Rights::none(), ps(KVA_A)).unwrap();
        slot_set_mapped_va(&mut slots, h0, 0x100).unwrap();
        slot_insert(&mut slots, Rights::none(), notif(KVA_B)).unwrap();
        slot_insert(&mut slots, Rights::none(), ps(KVA_A)).unwrap();
        let h2 = slot_insert(&mut slots, Rights::none(), ps(KVA_A)).unwrap();
        slot_set_mapped_va(&mut slots, h2, 0x200).unwrap();

        let mut validate_actions = vec![];
        slot_revoke_validate(&slots, KernelVa::new(KVA_A), |a| validate_actions.push(*a));

        let mut apply_actions = vec![];
        slot_revoke_apply(&mut slots, KernelVa::new(KVA_A), |a| apply_actions.push(*a));

        assert_eq!(validate_actions, apply_actions);
        assert_eq!(validate_actions.len(), 3);
    }

    #[test]
    fn revoke_no_match_yields_zero() {
        let mut slots = empty_table(4);
        slot_insert(&mut slots, Rights::none(), ep(0x1000, 0)).unwrap();

        let mut count = 0;
        let v = slot_revoke_validate(&slots, KernelVa::new(KVA_B), |_| count += 1);
        assert_eq!(v, 0);
        assert_eq!(count, 0);

        let snapshot = slots.clone();
        let a = slot_revoke_apply(&mut slots, KernelVa::new(KVA_B), |_| count += 1);
        assert_eq!(a, 0);
        assert_eq!(count, 0);
        assert_eq!(slots, snapshot);
    }

    #[test]
    fn revoke_skips_non_pageset_slots() {
        // Endpoint slots are not PageSet — the typed target means revoke
        // never matches them at all, regardless of the underlying paddr.
        let mut slots = empty_table(4);
        slot_insert(&mut slots, Rights::none(), ep(KVA_A, 7)).unwrap();

        let mut count = 0;
        slot_revoke_apply(&mut slots, KernelVa::new(KVA_A), |_| count += 1);
        assert_eq!(count, 0);
    }
}
