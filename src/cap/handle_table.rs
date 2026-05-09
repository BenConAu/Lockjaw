use crate::cap::object::ObjectType;
use crate::cap::rights::Rights;
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use lockjaw_types::object::{HandleTableHeader, HandleEntry, HandleKind};
use lockjaw_types::handle_ops::{self, HandleError, SlotRevokeAction};
use lockjaw_types::syscall::SyscallError;

// ---------------------------------------------------------------------------
// HandleTableRef — safe typed wrapper over handle table operations
// ---------------------------------------------------------------------------

/// A reference to a handle table in kernel memory. Constructed from
/// `CurrentThread::handle_table()` or by wrapping a known-valid
/// handle-table PhysAddr. All methods are safe — the PhysAddr validity
/// is established at construction time.
pub struct HandleTableRef(PhysAddr);

impl HandleTableRef {
    /// Wrap a known-valid handle table physical address.
    ///
    /// # Safety
    /// `paddr` must point to a live HandleTableObject in a kernel-owned page.
    pub unsafe fn from_paddr(paddr: PhysAddr) -> Self {
        HandleTableRef(paddr)
    }

    /// Look up a handle by index without type checking (for export/transfer).
    pub fn lookup_any(&self, handle: u32, required_rights: Rights) -> Result<HandleEntry, SyscallError> {
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            handle_ops::slot_lookup(slots, handle, required_rights)
                .map_err(|_| SyscallError::INVALID_HANDLE)
        }
    }

    /// Look up a handle by index with type and rights checking.
    /// Returns the HandleEntry on success.
    pub fn lookup(&self, handle: u32, required_rights: Rights, expected_type: ObjectType) -> Result<HandleEntry, SyscallError> {
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            let entry = handle_ops::slot_lookup(slots, handle, required_rights)
                .map_err(|_| SyscallError::INVALID_HANDLE)?;
            if entry.kind.obj_type() != expected_type {
                return Err(SyscallError::INVALID_PARAMETER);
            }
            Ok(entry)
        }
    }

    /// Insert a new handle into the table. Returns the slot index.
    /// Returns HANDLE_TABLE_FULL if no empty slot is available.
    pub fn insert(&self, object_paddr: PhysAddr, rights: Rights, kind: HandleKind) -> Result<u32, SyscallError> {
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            handle_ops::slot_insert(slots, object_paddr.as_u64(), rights, kind)
                .map_err(|e| {
                    if matches!(e, HandleError::TableFull) {
                        crate::kprintln!("HANDLE TABLE FULL: ", slots.len(), " slots, all occupied");
                    }
                    SyscallError::HANDLE_TABLE_FULL
                })
        }
    }

    /// Remove a single handle by index. Returns the removed entry.
    pub fn remove(&self, handle: u32) -> Result<HandleEntry, SyscallError> {
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            handle_ops::slot_remove(slots, handle)
                .map_err(|_| SyscallError::INVALID_HANDLE)
        }
    }

    /// Phase-1 revoke walk: read-only. For each handle in this table
    /// referencing `object_paddr`, invoke `on_action(&action)` exactly
    /// once with the slot's pre-clear snapshot. Returns the number of
    /// matching slots seen.
    ///
    /// **No state mutated.** The caller's callback may read but must
    /// not write the slot. Used by `revoke::validate` to walk every
    /// process's table without disturbing them; the caller pairs each
    /// action with `validate_pte_match` against the right TTBR0 to
    /// confirm the apply phase will succeed.
    pub fn revoke_validate(
        &self,
        object_paddr: u64,
        on_action: impl FnMut(&SlotRevokeAction),
    ) -> usize {
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            handle_ops::slot_revoke_validate(slots, object_paddr, on_action)
        }
    }

    /// Phase-2 revoke walk: write. For each handle in this table
    /// referencing `object_paddr`, invoke `on_action(&action)` with
    /// the slot's pre-clear snapshot, then zero the slot.
    ///
    /// `on_action` runs BEFORE the slot is cleared so the caller's
    /// PTE clear (`clear_validated_pte` against the right TTBR0),
    /// `dec_map_count`, and `dec_refcount` all read the action's
    /// `mapped_va_page` and `kind`.
    ///
    /// MUST be called only after a successful matching
    /// `revoke_validate` against the same `object_paddr` within the
    /// same critical section (GKL held).
    pub fn revoke_apply(
        &self,
        object_paddr: u64,
        on_action: impl FnMut(&SlotRevokeAction),
    ) -> usize {
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            handle_ops::slot_revoke_apply(slots, object_paddr, on_action)
        }
    }

    /// Walk all occupied slots and call the callback with each non-empty
    /// HandleEntry. Used during process exit to decrement refcounts and
    /// map_counts before bulk-freeing the handle table pages.
    pub fn for_each_entry(&self, mut cb: impl FnMut(&HandleEntry)) {
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            for slot in slots.iter() {
                if slot.object_paddr != 0 {
                    cb(slot);
                }
            }
        }
    }

    /// Set the mapped_va_page field on a handle entry.
    /// mapped_va_page = 0 means not mapped; nonzero = VA >> 12.
    pub fn set_mapped_va(&self, handle: u32, va_page: u32) -> Result<(), SyscallError> {
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            handle_ops::slot_set_mapped_va(slots, handle, va_page)
                .map_err(|_| SyscallError::INVALID_HANDLE)
        }
    }

    /// Get the mapped_va_page field from a handle entry.
    pub fn get_mapped_va(&self, handle: u32) -> Result<u32, SyscallError> {
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            handle_ops::slot_get_mapped_va(slots, handle)
                .map_err(|_| SyscallError::INVALID_HANDLE)
        }
    }
}

// HandleEntry is defined in lockjaw-types/src/object.rs and imported above.
// HANDLE_SLOTS_PER_PAGE uses size_of::<HandleEntry>() directly.

// HandleTableHeader must be a multiple of 8 bytes so that the HandleEntry
// array following it starts at an 8-byte-aligned offset (HandleEntry's
// first field is u64).
const _: () = assert!(core::mem::size_of::<HandleTableHeader>() % 8 == 0);

// HandleError is defined in lockjaw-types/src/handle_ops.rs and imported above.

// ---------------------------------------------------------------------------
// Handle operations — thin wrappers that delegate to lockjaw-types
// ---------------------------------------------------------------------------

/// Insert a new handle into the table. Returns the handle index (slot number).
pub unsafe fn handle_insert(
    table_paddr: PhysAddr,
    object_paddr: PhysAddr,
    rights: Rights,
    kind: HandleKind,
) -> Result<u32, HandleError> {
    let (_header, slots) = table_slots(table_paddr);
    handle_ops::slot_insert(slots, object_paddr.as_u64(), rights, kind)
        .map_err(|e| {
            if matches!(e, HandleError::TableFull) {
                crate::kprintln!("HANDLE TABLE FULL: ", slots.len(), " slots, all occupied");
            }
            e
        })
}

/// Look up a handle by index, checking that the required rights are present.
pub unsafe fn handle_lookup(
    table_paddr: PhysAddr,
    handle: u32,
    required: Rights,
) -> Result<HandleEntry, HandleError> {
    let (_header, slots) = table_slots(table_paddr);
    handle_ops::slot_lookup(slots, handle, required)
}

/// Remove a handle, returning what was in the slot.
pub unsafe fn handle_remove(
    table_paddr: PhysAddr,
    handle: u32,
) -> Result<HandleEntry, HandleError> {
    let (_header, slots) = table_slots(table_paddr);
    handle_ops::slot_remove(slots, handle)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Get the header pointer and a mutable slice over the handle slots.
/// Replaces manual offset arithmetic with Rust slice indexing.
///
/// # Safety
/// `table_paddr` must point to a live HandleTableObject in a kernel-owned page.
unsafe fn table_slots(table_paddr: PhysAddr) -> (*mut HandleTableHeader, &'static mut [HandleEntry]) {
    let base_va = table_paddr.as_u64() + KERNEL_VA_OFFSET;
    // SAFETY: kernel object at known VA
    let header = base_va as *mut HandleTableHeader;
    let slot_count = (*header).slot_count as usize;
    // SAFETY: slots immediately follow the header in the donated page(s)
    let slots_ptr = (base_va + core::mem::size_of::<HandleTableHeader>() as u64) as *mut HandleEntry;
    (header, core::slice::from_raw_parts_mut(slots_ptr, slot_count))
}
