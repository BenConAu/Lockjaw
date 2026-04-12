use crate::addr::PAGE_SIZE;

// ---------------------------------------------------------------------------
// Object types
// ---------------------------------------------------------------------------

/// Kernel object types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjectType {
    HandleTable = 0,
    ThreadControlBlock = 1,
    Endpoint = 2,
    Notification = 3,
}

/// Header written at the start of every kernel object's donated memory.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ObjectHeader {
    pub obj_type: ObjectType,
    pub page_count: u8,
}

/// Result of a size query — how many 4 KB pages an object needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectSize {
    pub pages: usize,
}

// ---------------------------------------------------------------------------
// HandleTable create-info (Vulkan pattern)
// ---------------------------------------------------------------------------

/// Describes a HandleTable to create. Same struct for query and create.
#[derive(Clone, Copy, Debug)]
pub struct HandleTableCreateInfo {
    pub slot_count: u32,
}

/// Full header for a HandleTable object, stored at the start of its pages.
#[repr(C)]
pub struct HandleTableHeader {
    pub header: ObjectHeader,
    pub slot_count: u32,
}

/// How many pages does a HandleTable with this config need?
///
/// Computes: header size + (slot_count * slot_size), rounded up to pages.
/// The slot size is 16 bytes (HandleEntry: u64 paddr + ObjectType + Rights + padding).
pub fn query_handle_table_size(info: &HandleTableCreateInfo) -> ObjectSize {
    let header_size = core::mem::size_of::<HandleTableHeader>();
    // HandleEntry is 16 bytes (u64 + u8 + u8 + 6 padding)
    let slot_size = 16;
    let total_bytes = header_size + slot_size * info.slot_count as usize;
    let pages = (total_bytes + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize;
    ObjectSize { pages }
}

// ---------------------------------------------------------------------------
// Endpoint create-info (Vulkan pattern)
// ---------------------------------------------------------------------------

/// Describes an Endpoint to create. Endpoints are simple rendezvous points
/// with no configuration parameters.
#[derive(Clone, Copy, Debug)]
pub struct EndpointCreateInfo;

/// How many pages does an Endpoint need? Always 1.
pub fn query_endpoint_size(_info: &EndpointCreateInfo) -> ObjectSize {
    ObjectSize { pages: 1 }
}

// ---------------------------------------------------------------------------
// Notification create-info (Vulkan timeline semaphore pattern)
// ---------------------------------------------------------------------------

/// Describes a Notification to create. Notifications are timeline semaphores
/// with a monotonically increasing u64 counter.
#[derive(Clone, Copy, Debug)]
pub struct NotificationCreateInfo;

/// How many pages does a Notification need? Always 1.
pub fn query_notification_size(_info: &NotificationCreateInfo) -> ObjectSize {
    ObjectSize { pages: 1 }
}

/// Error from object creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateError {
    InvalidParameter,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_table_8_slots_fits_in_one_page() {
        let info = HandleTableCreateInfo { slot_count: 8 };
        let size = query_handle_table_size(&info);
        assert_eq!(size.pages, 1);
    }

    #[test]
    fn handle_table_256_slots_fits_in_one_page() {
        // header (~8 bytes) + 256 * 16 = 4104 bytes > 4096 → 2 pages
        let info = HandleTableCreateInfo { slot_count: 256 };
        let size = query_handle_table_size(&info);
        assert_eq!(size.pages, 2);
    }

    #[test]
    fn handle_table_1000_slots() {
        let info = HandleTableCreateInfo { slot_count: 1000 };
        let size = query_handle_table_size(&info);
        // header + 1000 * 16 = ~16008 bytes → 4 pages
        assert_eq!(size.pages, 4);
    }

    #[test]
    fn endpoint_fits_in_one_page() {
        let size = query_endpoint_size(&EndpointCreateInfo);
        assert_eq!(size.pages, 1);
    }

    #[test]
    fn object_type_equality() {
        assert_eq!(ObjectType::HandleTable, ObjectType::HandleTable);
        assert_ne!(ObjectType::HandleTable, ObjectType::ThreadControlBlock);
        assert_ne!(ObjectType::Endpoint, ObjectType::HandleTable);
    }

    #[test]
    fn create_error_equality() {
        assert_eq!(CreateError::InvalidParameter, CreateError::InvalidParameter);
    }
}
