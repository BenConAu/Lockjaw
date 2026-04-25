use crate::cap::object::ObjectType;
use crate::cap::rights::Rights;
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use lockjaw_types::object::{HandleTableHeader, HandleEntry};
use lockjaw_types::syscall::SyscallError;
use core::ptr;

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
        unsafe { handle_lookup(self.0, handle, required_rights) }
            .map_err(|_| SyscallError::INVALID_HANDLE)
    }

    /// Look up a handle by index with type and rights checking.
    /// Returns the HandleEntry on success.
    pub fn lookup(&self, handle: u32, required_rights: Rights, expected_type: ObjectType) -> Result<HandleEntry, SyscallError> {
        // SAFETY: self.0 was validated at construction.
        let entry = unsafe { handle_lookup(self.0, handle, required_rights) }
            .map_err(|_| SyscallError::INVALID_HANDLE)?;
        if entry.obj_type != expected_type {
            return Err(SyscallError::INVALID_PARAMETER);
        }
        Ok(entry)
    }

    /// Insert a new handle into the table. Returns the slot index.
    /// Returns HANDLE_TABLE_FULL if no empty slot is available.
    pub fn insert(&self, object_paddr: PhysAddr, obj_type: ObjectType, rights: Rights) -> Result<u32, SyscallError> {
        // SAFETY: self.0 was validated at construction.
        unsafe { handle_insert(self.0, object_paddr, obj_type, rights) }
            .map_err(|_| SyscallError::HANDLE_TABLE_FULL)
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
            for slot in slots.iter_mut() {
                if slot.object_paddr == object_paddr {
                    // SAFETY: zeroing via mutable slice reference to mark slot empty.
                    ptr::write_bytes(slot as *mut HandleEntry, 0, 1);
                }
            }
        }
    }

    /// The underlying physical address (needed by cross-table operations
    /// like sys_export_handle on the caller's table).
    #[allow(dead_code)] // No callers yet — needed for cross-table operations.
    pub fn paddr(&self) -> PhysAddr {
        self.0
    }
}

// HandleEntry is defined in lockjaw-types/src/object.rs and imported above.
// HANDLE_SLOTS_PER_PAGE uses size_of::<HandleEntry>() directly — no
// separate constant or compile-time assertion needed.

/// Errors from handle operations.
#[derive(Clone, Copy, Debug)]
pub enum HandleError {
    TableFull,
    InvalidHandle,
    InsufficientRights,
}

// ---------------------------------------------------------------------------
// Handle operations — work on a HandleTableObject in donated memory
// ---------------------------------------------------------------------------

/// Insert a new handle into the table. Returns the handle index (slot number).
pub unsafe fn handle_insert(
    table_paddr: PhysAddr,
    object_paddr: PhysAddr,
    obj_type: ObjectType,
    rights: Rights,
) -> Result<u32, HandleError> {
    let (_header, slots) = table_slots(table_paddr);

    // Find first empty slot
    for (i, slot) in slots.iter_mut().enumerate() {
        if slot.object_paddr == 0 {
            slot.object_paddr = object_paddr.as_u64();
            slot.obj_type = obj_type;
            slot.rights = rights;
            return Ok(i as u32);
        }
    }

    crate::kprintln!("HANDLE TABLE FULL: {} slots, all occupied", slots.len());
    Err(HandleError::TableFull)
}

/// Look up a handle by index, checking that the required rights are present.
pub unsafe fn handle_lookup(
    table_paddr: PhysAddr,
    handle: u32,
    required: Rights,
) -> Result<HandleEntry, HandleError> {
    let (_header, slots) = table_slots(table_paddr);

    let slot = slots.get(handle as usize)
        .ok_or(HandleError::InvalidHandle)?;
    if slot.object_paddr == 0 {
        return Err(HandleError::InvalidHandle);
    }

    // Check every required right is present
    if required.bits() & !slot.rights.bits() != 0 {
        return Err(HandleError::InsufficientRights);
    }

    Ok(*slot)
}

/// Remove a handle, returning what was in the slot.
pub unsafe fn handle_remove(
    table_paddr: PhysAddr,
    handle: u32,
) -> Result<HandleEntry, HandleError> {
    let (_header, slots) = table_slots(table_paddr);

    let slot = slots.get_mut(handle as usize)
        .ok_or(HandleError::InvalidHandle)?;
    if slot.object_paddr == 0 {
        return Err(HandleError::InvalidHandle);
    }

    let removed = *slot;
    // SAFETY: zeroing via the mutable slice reference to mark slot empty.
    ptr::write_bytes(slot as *mut HandleEntry, 0, 1);
    Ok(removed)
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
