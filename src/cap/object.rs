use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET, PAGE_SIZE};
use core::ptr;

// ---------------------------------------------------------------------------
// Object types
// ---------------------------------------------------------------------------

/// Kernel object types. PageSet and MappedPages added in later milestones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjectType {
    HandleTable = 0,
}

/// Header written at the start of every kernel object's donated memory.
/// Userspace cannot see this — the pages are exclusively owned by the kernel.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ObjectHeader {
    pub obj_type: ObjectType,
    pub page_count: u8,
}

/// Result of a size query — how many 4 KB pages an object needs.
#[derive(Clone, Copy, Debug)]
pub struct ObjectSize {
    pub pages: usize,
}

// ---------------------------------------------------------------------------
// HandleTable create-info and query/create (Vulkan pattern)
// ---------------------------------------------------------------------------

/// Describes a HandleTable to create. Same struct for query and create.
#[derive(Clone, Copy, Debug)]
pub struct HandleTableCreateInfo {
    pub slot_count: u32,
}

/// How many pages does a HandleTable with this config need?
pub fn query_handle_table_size(info: &HandleTableCreateInfo) -> ObjectSize {
    let header_size = core::mem::size_of::<HandleTableHeader>();
    let slot_size = core::mem::size_of::<crate::cap::handle_table::HandleEntry>();
    let total_bytes = header_size + slot_size * info.slot_count as usize;
    let pages = (total_bytes + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize;
    ObjectSize { pages }
}

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

/// Error from object creation.
#[derive(Clone, Copy, Debug)]
pub enum CreateError {
    InvalidParameter,
}

// ---------------------------------------------------------------------------
// Internal header types (stored in donated memory)
// ---------------------------------------------------------------------------

/// Full header for a HandleTable object, stored at the start of its pages.
#[repr(C)]
pub struct HandleTableHeader {
    pub header: ObjectHeader,
    pub slot_count: u32,
}
