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
    Reply = 4,
    Process = 5,
    PageSet = 6,
}

/// Header written at the start of every kernel object's donated memory.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ObjectHeader {
    pub obj_type: ObjectType,
    pub page_count: u8,
    /// Handle reference count. Accounting only for non-PageSet objects
    /// (no free-on-zero). Initialized to 0; incremented by handle_insert.
    pub refcount: u32,
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
    pub slot_count: u64,
}

/// Full header for a HandleTable object, stored at the start of its pages.
#[repr(C)]
pub struct HandleTableHeader {
    pub header: ObjectHeader,
    pub slot_count: u64,
}

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
    pub rights: crate::rights::Rights,
    /// Reserved padding for alignment.
    pub _padding: [u8; 2],
    /// Page number of the VA where this handle's PageSet is mapped.
    /// 0 = not mapped. Set by sys_map_pages, cleared by sys_unmap_pages.
    /// Only meaningful for PageSet handles. Stores VA >> 12.
    pub mapped_va_page: u32,
}

/// Decision for closing/releasing a handle. Single vocabulary for
/// both sys_close_handle and finish_exit handle table cleanup.
/// Replaces the former HandleCleanup struct.
///
/// The kernel matches on the variant and executes mechanically:
/// - RemoveOnly: just remove the handle slot, no accounting.
/// - RemoveAndDecRef: remove + dec refcount + maybe free.
/// - UnmapThenRemove: unmap PTEs first (fallible in sys_close_handle,
///   asserted-already-done in finish_exit), then remove + dec both.
/// - InvalidHandle: entry is empty or absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseHandleResult {
    /// Occupied non-PageSet handle — just remove, no accounting.
    RemoveOnly,
    /// PageSet, not mapped — remove + dec refcount + maybe free.
    RemoveAndDecRef { header_paddr: u64 },
    /// PageSet, mapped — must unmap first, then remove + dec both
    /// counters + maybe free.
    UnmapThenRemove {
        header_paddr: u64,
        mapped_va_page: u32,
    },
    /// Empty slot or absent entry.
    InvalidHandle,
}

/// Decide what to do when closing/releasing a handle entry.
///
/// For sys_close_handle: pass the looked-up entry.
/// For finish_exit: pass each occupied entry from for_each_entry.
/// None input (failed lookup) returns InvalidHandle.
pub fn decide_close_handle(entry: Option<&HandleEntry>) -> CloseHandleResult {
    let entry = match entry {
        Some(e) if e.object_paddr != 0 => e,
        _ => return CloseHandleResult::InvalidHandle,
    };
    if entry.obj_type != ObjectType::PageSet {
        return CloseHandleResult::RemoveOnly;
    }
    if entry.mapped_va_page != 0 {
        CloseHandleResult::UnmapThenRemove {
            header_paddr: entry.object_paddr,
            mapped_va_page: entry.mapped_va_page,
        }
    } else {
        CloseHandleResult::RemoveAndDecRef {
            header_paddr: entry.object_paddr,
        }
    }
}

/// Teardown-specific handle decision for processes without an
/// address space (kernel processes). The return type has no unmap
/// variant, making the illegal state unrepresentable by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeardownHandleAction {
    /// PageSet handle — dec refcount, maybe free.
    DecRef { header_paddr: u64 },
    /// Non-PageSet or empty — nothing to do.
    Skip,
}

/// Decide cleanup for a handle entry during kernel-process teardown.
/// Only returns DecRef or Skip — no unmap action exists.
pub fn decide_teardown_handle(entry: &HandleEntry) -> TeardownHandleAction {
    if entry.object_paddr == 0 || entry.obj_type != ObjectType::PageSet {
        return TeardownHandleAction::Skip;
    }
    // Invariant: kernel processes don't have mapped PageSets
    // (they don't call sys_map_pages). A nonzero mapped_va_page
    // here means the invariant is broken at the source — halt
    // rather than silently skip the map_count decrement.
    assert!(entry.mapped_va_page == 0,
        "mapped PageSet handle in kernel process teardown");
    TeardownHandleAction::DecRef {
        header_paddr: entry.object_paddr,
    }
}

/// Maximum handle slots that fit in a single 4KB page.
pub const HANDLE_SLOTS_PER_PAGE: u64 = ((PAGE_SIZE as usize - core::mem::size_of::<HandleTableHeader>()) / core::mem::size_of::<HandleEntry>()) as u64;

/// How many pages does a HandleTable with this config need?
///
/// Computes: header size + (slot_count * entry_size), rounded up to pages.
pub fn query_handle_table_size(info: &HandleTableCreateInfo) -> ObjectSize {
    let header_size = core::mem::size_of::<HandleTableHeader>();
    let total_bytes = header_size + core::mem::size_of::<HandleEntry>() * info.slot_count as usize;
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

// ---------------------------------------------------------------------------
// Reply create-info
// ---------------------------------------------------------------------------

/// Describes a Reply object to create. Reply objects are per-client mailboxes
/// used for sys_call; each client creates one at startup and reuses it. A
/// Reply holds the blocked caller's TCB paddr so the server's sys_reply can
/// target the specific caller without touching endpoint state.
#[derive(Clone, Copy, Debug)]
pub struct ReplyCreateInfo;

/// How many pages does a Reply need? Always 1.
pub fn query_reply_size(_info: &ReplyCreateInfo) -> ObjectSize {
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
        assert_ne!(ObjectType::Reply, ObjectType::Endpoint);
    }

    #[test]
    fn reply_fits_in_one_page() {
        let size = query_reply_size(&ReplyCreateInfo);
        assert_eq!(size.pages, 1);
    }

    #[test]
    fn create_error_equality() {
        assert_eq!(CreateError::InvalidParameter, CreateError::InvalidParameter);
    }

    #[test]
    fn handle_entry_size_is_16() {
        // HANDLE_SLOTS_PER_PAGE depends on this being exactly 16.
        assert_eq!(core::mem::size_of::<HandleEntry>(), 16);
    }

    #[test]
    fn handle_slots_per_page_uses_full_page() {
        // 255 entries * 16 bytes + 8 byte header = 4088, fits in 4096.
        assert_eq!(HANDLE_SLOTS_PER_PAGE, 255);
        let used = core::mem::size_of::<HandleTableHeader>()
            + HANDLE_SLOTS_PER_PAGE as usize * core::mem::size_of::<HandleEntry>();
        assert!(used <= 4096);
        // One more wouldn't fit.
        let with_one_more = used + core::mem::size_of::<HandleEntry>();
        assert!(with_one_more > 4096);
    }

    #[test]
    fn pageset_in_object_type_enum() {
        // PageSet must be a distinct variant for handle table type checking.
        assert_ne!(ObjectType::PageSet, ObjectType::Endpoint);
        assert_ne!(ObjectType::PageSet, ObjectType::Notification);
        assert_ne!(ObjectType::PageSet, ObjectType::HandleTable);
    }

    // --- decide_close_handle tests ---

    fn make_entry(obj_type: ObjectType, paddr: u64, mapped_va_page: u32) -> HandleEntry {
        HandleEntry {
            object_paddr: paddr,
            obj_type,
            rights: crate::rights::Rights::from_bits(0),
            _padding: [0; 2],
            mapped_va_page,
        }
    }

    #[test]
    fn close_none_returns_invalid() {
        assert_eq!(decide_close_handle(None), CloseHandleResult::InvalidHandle);
    }

    #[test]
    fn close_empty_slot_returns_invalid() {
        let entry = make_entry(ObjectType::PageSet, 0, 0);
        assert_eq!(decide_close_handle(Some(&entry)), CloseHandleResult::InvalidHandle);
    }

    #[test]
    fn close_non_pageset_remove_only() {
        let entry = make_entry(ObjectType::Endpoint, 0x1000, 0);
        assert_eq!(decide_close_handle(Some(&entry)), CloseHandleResult::RemoveOnly);
    }

    #[test]
    fn close_unmapped_pageset_dec_ref() {
        let entry = make_entry(ObjectType::PageSet, 0x1000, 0);
        assert_eq!(decide_close_handle(Some(&entry)), CloseHandleResult::RemoveAndDecRef {
            header_paddr: 0x1000,
        });
    }

    #[test]
    fn close_mapped_pageset_unmap_then_remove() {
        let entry = make_entry(ObjectType::PageSet, 0x1000, 0x400);
        assert_eq!(decide_close_handle(Some(&entry)), CloseHandleResult::UnmapThenRemove {
            header_paddr: 0x1000,
            mapped_va_page: 0x400,
        });
    }

    #[test]
    fn close_notification_remove_only() {
        let entry = make_entry(ObjectType::Notification, 0x2000, 0);
        assert_eq!(decide_close_handle(Some(&entry)), CloseHandleResult::RemoveOnly);
    }

    // --- decide_teardown_handle tests ---

    #[test]
    fn teardown_handle_unmapped_pageset_dec_ref() {
        let entry = make_entry(ObjectType::PageSet, 0x1000, 0);
        assert_eq!(decide_teardown_handle(&entry), TeardownHandleAction::DecRef {
            header_paddr: 0x1000,
        });
    }

    #[test]
    fn teardown_handle_non_pageset_skip() {
        let entry = make_entry(ObjectType::Endpoint, 0x1000, 0);
        assert_eq!(decide_teardown_handle(&entry), TeardownHandleAction::Skip);
    }

    #[test]
    fn teardown_handle_empty_slot_skip() {
        let entry = make_entry(ObjectType::PageSet, 0, 0);
        assert_eq!(decide_teardown_handle(&entry), TeardownHandleAction::Skip);
    }

    // --- close_decision regression tests ---

    #[test]
    fn close_decision_is_sole_authority_for_map_count() {
        // Regression for the double-decrement bug: if the kernel
        // decrements map_count manually AND the decision also implies
        // a map_count decrement, it happens twice. This test verifies
        // that mapped_va_page controls the variant: nonzero produces
        // UnmapThenRemove (which includes map_count dec), zero
        // produces RemoveAndDecRef (which does not).
        let mapped = make_entry(ObjectType::PageSet, 0x1000, 0x400);
        assert!(matches!(decide_close_handle(Some(&mapped)),
            CloseHandleResult::UnmapThenRemove { .. }));

        // After the kernel clears mapped_va_page (simulating the slot
        // state after set_mapped_va(handle, 0)):
        let cleared = make_entry(ObjectType::PageSet, 0x1000, 0);
        assert!(matches!(decide_close_handle(Some(&cleared)),
            CloseHandleResult::RemoveAndDecRef { .. }));
    }
}
