// Re-export pure types from lockjaw-types.
pub use lockjaw_types::object::*;

use crate::mm::kernel_ptr::KernelMut;
use core::ptr;
use lockjaw_types::addr::KernelVa;

// ---------------------------------------------------------------------------
// Kernel-only: create_handle_table (unsafe, writes to KVM-mapped memory)
// ---------------------------------------------------------------------------

/// Initialize a HandleTable in a kernel-writable page.
///
/// `base_kva` may live in either the KVM pool (NK3-era /
/// bootstrap-time paths) OR the TTBR1 direct map at
/// `paddr + KERNEL_VA_OFFSET` (NK4+ sys_create_process via
/// donate_process_pages). create_handle_table only writes through
/// the KVA; the free path matches the create regime.
///
/// # Safety
/// `base_kva` must point to a kernel-owned page (or pages, for
/// multi-page tables) of at least `query_handle_table_size(info).pages`
/// pages, with no live references into it.
pub unsafe fn create_handle_table(
    info: &HandleTableCreateInfo,
    base_kva: KernelVa,
) -> Result<(), CreateError> {
    if info.slot_count == 0 || info.slot_count > HANDLE_SLOTS_PER_PAGE {
        return Err(CreateError::InvalidParameter);
    }

    let required = query_handle_table_size(info);
    let mut header_km = KernelMut::<HandleTableHeader>::from_kva(base_kva);

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
