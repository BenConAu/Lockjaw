// Re-export pure types from lockjaw-types.
pub use lockjaw_types::object::*;

use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use core::ptr;

// ---------------------------------------------------------------------------
// Kernel-only: create_handle_table (unsafe, writes to donated memory)
// ---------------------------------------------------------------------------

/// Initialize a HandleTable in donated physical memory.
///
/// # Safety
/// `base_paddr` must point to donated pages (not mapped by userspace).
/// The memory must be at least `query_handle_table_size(info).pages` pages.
pub unsafe fn create_handle_table(
    info: &HandleTableCreateInfo,
    base_paddr: PhysAddr,
) -> Result<(), CreateError> {
    if info.slot_count == 0 {
        return Err(CreateError::InvalidParameter);
    }

    let required = query_handle_table_size(info);
    let base_va = base_paddr.as_u64() + KERNEL_VA_OFFSET;

    // Write the handle table header
    // SAFETY: kernel object at known VA
    let header = base_va as *mut HandleTableHeader;
    ptr::write(
        header,
        HandleTableHeader {
            header: ObjectHeader {
                obj_type: ObjectType::HandleTable,
                page_count: required.pages as u8,
            },
            slot_count: info.slot_count,
        },
    );

    // Zero all handle slots (empty = object_paddr 0)
    // SAFETY: slots immediately follow the header in the donated page(s)
    let slots_ptr = (base_va + core::mem::size_of::<HandleTableHeader>() as u64)
        as *mut crate::cap::handle_table::HandleEntry;
    ptr::write_bytes(slots_ptr, 0, info.slot_count as usize);

    Ok(())
}
