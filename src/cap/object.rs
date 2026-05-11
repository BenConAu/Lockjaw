// Re-export pure types from lockjaw-types.
pub use lockjaw_types::object::*;

use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::KernelMut;
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
    if info.slot_count == 0 || info.slot_count > HANDLE_SLOTS_PER_PAGE {
        return Err(CreateError::InvalidParameter);
    }

    let required = query_handle_table_size(info);
    let mut header_km = KernelMut::<HandleTableHeader>::from_paddr(base_paddr);

    // Write the handle table header
    ptr::write(
        header_km.as_mut_ptr(),
        HandleTableHeader {
            header: ObjectHeader {
                obj_type: ObjectType::HandleTable,
                page_count: required.pages as u8,
                refcount: 0, // handle tables are not handle-tracked
            },
            slot_count: info.slot_count,
        },
    );

    // Zero all handle slots — `HandleKind::Empty = 0` is the empty
    // sentinel and its discriminant byte is the first byte of the
    // kind field, so an all-zeros slot decodes as Empty.
    // SAFETY: slots immediately follow the header in the donated page(s)
    let slots_ptr = (header_km.as_ptr() as u64 + core::mem::size_of::<HandleTableHeader>() as u64)
        as *mut lockjaw_types::object::HandleEntry;
    ptr::write_bytes(slots_ptr, 0, info.slot_count as usize);

    Ok(())
}
