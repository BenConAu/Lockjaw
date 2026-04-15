use crate::cap::object::{ObjectType, HandleTableHeader};
use crate::cap::rights::Rights;
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use core::ptr;

/// A single entry in a handle table. Stored in donated pages immediately
/// after the HandleTableHeader.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct HandleEntry {
    /// Physical address of the kernel object. 0 = empty slot.
    pub object_paddr: u64,
    /// Type of the referenced object.
    pub obj_type: ObjectType,
    /// Access rights for this handle.
    pub rights: Rights,
    _padding: [u8; 6],
}

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
    let (header, slots_base) = table_ptrs(table_paddr);
    let slot_count = (*header).slot_count as usize;

    // Find first empty slot
    for i in 0..slot_count {
        let slot = &mut *slot_ptr(slots_base, i);
        if slot.object_paddr == 0 {
            slot.object_paddr = object_paddr.as_u64();
            slot.obj_type = obj_type;
            slot.rights = rights;
            return Ok(i as u32);
        }
    }

    Err(HandleError::TableFull)
}

/// Look up a handle by index, checking that the required rights are present.
pub unsafe fn handle_lookup(
    table_paddr: PhysAddr,
    handle: u32,
    required: Rights,
) -> Result<HandleEntry, HandleError> {
    let (header, slots_base) = table_ptrs(table_paddr);
    let slot_count = (*header).slot_count;

    if handle >= slot_count {
        return Err(HandleError::InvalidHandle);
    }

    let slot = *slot_ptr(slots_base, handle as usize);
    if slot.object_paddr == 0 {
        return Err(HandleError::InvalidHandle);
    }

    // Check every required right is present
    if required.bits() & !slot.rights.bits() != 0 {
        return Err(HandleError::InsufficientRights);
    }

    Ok(slot)
}

/// Remove a handle, returning what was in the slot.
pub unsafe fn handle_remove(
    table_paddr: PhysAddr,
    handle: u32,
) -> Result<HandleEntry, HandleError> {
    let (header, slots_base) = table_ptrs(table_paddr);
    let slot_count = (*header).slot_count;

    if handle >= slot_count {
        return Err(HandleError::InvalidHandle);
    }

    let slot = &mut *slot_ptr(slots_base, handle as usize);
    if slot.object_paddr == 0 {
        return Err(HandleError::InvalidHandle);
    }

    let removed = *slot;
    // SAFETY: kernel object at known VA
    ptr::write_bytes(slot as *mut HandleEntry, 0, 1);
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

unsafe fn table_ptrs(table_paddr: PhysAddr) -> (*const HandleTableHeader, u64) {
    let base_va = table_paddr.as_u64() + KERNEL_VA_OFFSET;
    // SAFETY: kernel object at known VA
    let header = base_va as *const HandleTableHeader;
    let slots_base = base_va + core::mem::size_of::<HandleTableHeader>() as u64;
    (header, slots_base)
}

unsafe fn slot_ptr(slots_base: u64, index: usize) -> *mut HandleEntry {
    let slot_size = core::mem::size_of::<HandleEntry>();
    // SAFETY: kernel object at known VA
    (slots_base + (index * slot_size) as u64) as *mut HandleEntry
}
