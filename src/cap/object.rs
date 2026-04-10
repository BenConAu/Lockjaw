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
    let slots_base = base_va + core::mem::size_of::<HandleTableHeader>() as u64;
    let slot_size = core::mem::size_of::<crate::cap::handle_table::HandleEntry>();
    for i in 0..info.slot_count as usize {
        let slot_ptr = (slots_base + (i * slot_size) as u64) as *mut u8;
        ptr::write_bytes(slot_ptr, 0, slot_size);
    }

    Ok(())
}
