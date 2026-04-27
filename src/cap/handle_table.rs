use crate::cap::object::ObjectType;
use crate::cap::rights::Rights;
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use lockjaw_types::object::{HandleTableHeader, HandleEntry, HandleKind};
use lockjaw_types::handle_ops::{self, HandleError};
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
                        crate::kprintln!("HANDLE TABLE FULL: {} slots, all occupied", slots.len());
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

    /// For each handle pointing at the given object with a non-zero
    /// mapped_va_page, call the callback with the mapping VA. The callback
    /// returns true if the unmap succeeded — only then is mapped_va_page
    /// cleared. Returns (total_mapped, successfully_unmapped). If these
    /// differ, some mappings could not be torn down.
    pub fn unmap_for_object(&self, object_paddr: u64, mut cb: impl FnMut(u64) -> bool) -> (usize, usize) {
        let mut total = 0;
        let mut unmapped = 0;
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            for slot in slots.iter_mut() {
                if let HandleKind::PageSet { mapped_va_page } = &mut slot.kind {
                    if slot.object_paddr == object_paddr && *mapped_va_page != 0 {
                        total += 1;
                        let va = (*mapped_va_page as u64) << 12;
                        if cb(va) {
                            *mapped_va_page = 0;
                            unmapped += 1;
                        }
                    }
                }
            }
        }
        (total, unmapped)
    }

    /// Remove ALL handles pointing at a given object physical address.
    /// Used when consuming a PageSet for object creation — invalidates
    /// any duplicate handles in the same table to prevent stale access.
    /// Cross-process exported handles are not affected (requires
    /// revocation infrastructure, tracked in tech-debt).
    pub fn remove_all_by_object(&self, object_paddr: u64) {
        // SAFETY: self.0 was validated at construction.
        unsafe {
            let (_header, slots) = table_slots(self.0);
            handle_ops::slot_remove_all_by_object(slots, object_paddr);
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
                crate::kprintln!("HANDLE TABLE FULL: {} slots, all occupied", slots.len());
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
