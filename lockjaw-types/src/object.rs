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

/// Object type + per-type metadata for a handle table entry.
///
/// Each non-empty variant carries the address of its underlying kernel
/// object — `paddr: PhysAddr` for objects allocated from the buddy
/// allocator (Endpoint, Notification, Reply, etc.) and `kva: KernelVa`
/// for objects that live in the kernel VA pool (PageSet headers
/// today; eventually all kernel objects). The address regime is part
/// of the variant so the type system rules out crossing them.
///
/// Discriminant values match the legacy `ObjectType` enum so that
/// diagnostic code and tests can compare by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C, u8)]
pub enum HandleKind {
    Empty = 0,
    HandleTable { paddr: crate::addr::PhysAddr } = 1,
    ThreadControlBlock { paddr: crate::addr::PhysAddr } = 2,
    Endpoint { paddr: crate::addr::PhysAddr, caller_token: u64 } = 3,
    Notification { paddr: crate::addr::PhysAddr } = 4,
    Reply { kva: crate::addr::KernelVa } = 5,
    Process { paddr: crate::addr::PhysAddr } = 6,
    PageSet { kva: crate::addr::KernelVa, mapped_va_page: u32 } = 7,
}

impl ObjectType {
    /// Return the variant name as a static string.
    pub fn name(&self) -> &'static str {
        match self {
            ObjectType::HandleTable => "HandleTable",
            ObjectType::ThreadControlBlock => "ThreadControlBlock",
            ObjectType::Endpoint => "Endpoint",
            ObjectType::Notification => "Notification",
            ObjectType::Reply => "Reply",
            ObjectType::Process => "Process",
            ObjectType::PageSet => "PageSet",
        }
    }
}

impl HandleKind {
    /// Return the variant name as a static string.
    pub fn name(&self) -> &'static str {
        match self {
            HandleKind::Empty => "Empty",
            HandleKind::HandleTable { .. } => "HandleTable",
            HandleKind::ThreadControlBlock { .. } => "ThreadControlBlock",
            HandleKind::Endpoint { .. } => "Endpoint",
            HandleKind::Notification { .. } => "Notification",
            HandleKind::Reply { .. } => "Reply",
            HandleKind::Process { .. } => "Process",
            HandleKind::PageSet { .. } => "PageSet",
        }
    }

    /// True if this is the PageSet variant (any mapped_va_page value).
    pub fn is_pageset(&self) -> bool {
        matches!(self, HandleKind::PageSet { .. })
    }

    /// Return the ObjectType equivalent for code that still needs it
    /// (e.g., object header checks, refcount paths).
    pub fn obj_type(&self) -> ObjectType {
        match self {
            HandleKind::Empty => ObjectType::HandleTable, // inert
            HandleKind::HandleTable { .. } => ObjectType::HandleTable,
            HandleKind::ThreadControlBlock { .. } => ObjectType::ThreadControlBlock,
            HandleKind::Endpoint { .. } => ObjectType::Endpoint,
            HandleKind::Notification { .. } => ObjectType::Notification,
            HandleKind::Reply { .. } => ObjectType::Reply,
            HandleKind::Process { .. } => ObjectType::Process,
            HandleKind::PageSet { .. } => ObjectType::PageSet,
        }
    }
}

/// A single entry in a handle table. Stored in donated pages immediately
/// after the HandleTableHeader.
///
/// The address of the underlying kernel object lives inside `kind` —
/// each non-empty `HandleKind` variant carries its own typed address
/// (`PhysAddr` or `KernelVa`). Emptiness is keyed by
/// `kind == HandleKind::Empty`; there is no separate `object_paddr`
/// field whose meaning floats with `kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct HandleEntry {
    /// Access rights for this handle. Inert filler when `kind = Empty`.
    pub rights: crate::rights::Rights,
    /// Object type + per-type metadata, including the typed address.
    pub kind: HandleKind,
}

// Static layout assertions — part of the design contract.
// HandleKind grew from 16 → 24 bytes when each variant absorbed its
// typed address (PhysAddr or KernelVa). HandleEntry stays at 32 bytes
// because the previous u64 `object_paddr` field disappeared.
const _: () = assert!(core::mem::size_of::<HandleEntry>() == 32);
const _: () = assert!(core::mem::align_of::<HandleEntry>() == 8);
const _: () = assert!(core::mem::size_of::<HandleKind>() == 24);

impl HandleEntry {
    /// Empty slot sentinel. `kind == HandleKind::Empty` is the sole
    /// empty-slot test throughout the codebase.
    pub const EMPTY: Self = Self {
        rights: crate::rights::Rights::none(),
        kind: HandleKind::Empty,
    };
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
    RemoveAndDecRef { header_kva: crate::addr::KernelVa },
    /// PageSet, mapped — must unmap first, then remove + dec both
    /// counters + maybe free.
    UnmapThenRemove {
        header_kva: crate::addr::KernelVa,
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
        Some(e) if !matches!(e.kind, HandleKind::Empty) => e,
        _ => return CloseHandleResult::InvalidHandle,
    };
    match entry.kind {
        HandleKind::PageSet { kva, mapped_va_page } if mapped_va_page != 0 => {
            CloseHandleResult::UnmapThenRemove {
                header_kva: kva,
                mapped_va_page,
            }
        }
        HandleKind::PageSet { kva, .. } => {
            CloseHandleResult::RemoveAndDecRef {
                header_kva: kva,
            }
        }
        _ => CloseHandleResult::RemoveOnly,
    }
}

/// Teardown-specific handle decision for processes without an
/// address space (kernel processes). The return type has no unmap
/// variant, making the illegal state unrepresentable by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeardownHandleAction {
    /// PageSet handle — dec refcount, maybe free.
    DecRef { header_kva: crate::addr::KernelVa },
    /// Non-PageSet or empty — nothing to do.
    Skip,
}

/// Decide cleanup for a handle entry during kernel-process teardown.
/// Only returns DecRef or Skip — no unmap action exists.
pub fn decide_teardown_handle(entry: &HandleEntry) -> TeardownHandleAction {
    match entry.kind {
        HandleKind::Empty => TeardownHandleAction::Skip,
        HandleKind::PageSet { kva, mapped_va_page } => {
            // Invariant: kernel processes don't have mapped PageSets
            // (they don't call sys_map_pages). A nonzero mapped_va_page
            // here means the invariant is broken at the source — halt
            // rather than silently skip the map_count decrement.
            assert!(mapped_va_page == 0,
                "mapped PageSet handle in kernel process teardown");
            TeardownHandleAction::DecRef { header_kva: kva }
        }
        _ => TeardownHandleAction::Skip,
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
    fn handle_table_256_slots() {
        // header (8 bytes) + 256 * 32 = 8200 bytes → 3 pages
        let info = HandleTableCreateInfo { slot_count: 256 };
        let size = query_handle_table_size(&info);
        assert_eq!(size.pages, 3);
    }

    #[test]
    fn handle_table_1000_slots() {
        let info = HandleTableCreateInfo { slot_count: 1000 };
        let size = query_handle_table_size(&info);
        // header + 1000 * 32 = 32008 bytes → 8 pages
        assert_eq!(size.pages, 8);
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
    fn handle_entry_size_is_32() {
        assert_eq!(core::mem::size_of::<HandleEntry>(), 32);
    }

    #[test]
    fn handle_kind_size_is_24() {
        // Each non-empty variant carries its typed address (PhysAddr or
        // KernelVa). Endpoint additionally carries caller_token, so its
        // payload is 16 bytes — that determines the enum's footprint
        // under #[repr(C, u8)] (1 tag + 7 padding + 16 payload = 24).
        assert_eq!(core::mem::size_of::<HandleKind>(), 24);
    }

    #[test]
    fn handle_entry_alignment_is_8() {
        assert_eq!(core::mem::align_of::<HandleEntry>(), 8);
    }

    #[test]
    fn handle_slots_per_page() {
        assert_eq!(HANDLE_SLOTS_PER_PAGE, 127);
        let used = core::mem::size_of::<HandleTableHeader>()
            + HANDLE_SLOTS_PER_PAGE as usize * core::mem::size_of::<HandleEntry>();
        assert!(used <= 4096);
        // One more wouldn't fit.
        let with_one_more = used + core::mem::size_of::<HandleEntry>();
        assert!(with_one_more > 4096);
    }

    #[test]
    fn handle_kind_discriminants_match_object_type() {
        let dummy_paddr = crate::addr::PhysAddr::new(0x1000);
        let dummy_kva = crate::addr::KernelVa::new(0xFFFF_8000_0000_1000);
        // Ensure HandleKind discriminant values match ObjectType for diagnostics.
        assert_eq!(HandleKind::Empty.obj_type(), ObjectType::HandleTable); // inert
        assert_eq!(HandleKind::HandleTable { paddr: dummy_paddr }.obj_type(), ObjectType::HandleTable);
        assert_eq!(HandleKind::Endpoint { paddr: dummy_paddr, caller_token: 0 }.obj_type(), ObjectType::Endpoint);
        assert_eq!(HandleKind::PageSet { kva: dummy_kva, mapped_va_page: 0 }.obj_type(), ObjectType::PageSet);
        assert_eq!(HandleKind::Notification { paddr: dummy_paddr }.obj_type(), ObjectType::Notification);
    }

    #[test]
    fn pageset_in_object_type_enum() {
        // PageSet must be a distinct variant for handle table type checking.
        assert_ne!(ObjectType::PageSet, ObjectType::Endpoint);
        assert_ne!(ObjectType::PageSet, ObjectType::Notification);
        assert_ne!(ObjectType::PageSet, ObjectType::HandleTable);
    }

    // --- decide_close_handle tests ---

    fn make_entry(kind: HandleKind) -> HandleEntry {
        HandleEntry {
            rights: crate::rights::Rights::from_bits(0),
            kind,
        }
    }

    fn dummy_paddr() -> crate::addr::PhysAddr {
        crate::addr::PhysAddr::new(0x4000_1000)
    }
    fn dummy_kva() -> crate::addr::KernelVa {
        crate::addr::KernelVa::new(0xFFFF_8000_0000_1000)
    }

    #[test]
    fn close_none_returns_invalid() {
        assert_eq!(decide_close_handle(None), CloseHandleResult::InvalidHandle);
    }

    #[test]
    fn close_empty_slot_returns_invalid() {
        let entry = make_entry(HandleKind::Empty);
        assert_eq!(decide_close_handle(Some(&entry)), CloseHandleResult::InvalidHandle);
    }

    #[test]
    fn close_non_pageset_remove_only() {
        let entry = make_entry(HandleKind::Endpoint { paddr: dummy_paddr(), caller_token: 0 });
        assert_eq!(decide_close_handle(Some(&entry)), CloseHandleResult::RemoveOnly);
    }

    #[test]
    fn close_unmapped_pageset_dec_ref() {
        let kva = dummy_kva();
        let entry = make_entry(HandleKind::PageSet { kva, mapped_va_page: 0 });
        assert_eq!(decide_close_handle(Some(&entry)), CloseHandleResult::RemoveAndDecRef {
            header_kva: kva,
        });
    }

    #[test]
    fn close_mapped_pageset_unmap_then_remove() {
        let kva = dummy_kva();
        let entry = make_entry(HandleKind::PageSet { kva, mapped_va_page: 0x400 });
        assert_eq!(decide_close_handle(Some(&entry)), CloseHandleResult::UnmapThenRemove {
            header_kva: kva,
            mapped_va_page: 0x400,
        });
    }

    #[test]
    fn close_notification_remove_only() {
        let entry = make_entry(HandleKind::Notification { paddr: dummy_paddr() });
        assert_eq!(decide_close_handle(Some(&entry)), CloseHandleResult::RemoveOnly);
    }

    // --- decide_teardown_handle tests ---

    #[test]
    fn teardown_handle_unmapped_pageset_dec_ref() {
        let kva = dummy_kva();
        let entry = make_entry(HandleKind::PageSet { kva, mapped_va_page: 0 });
        assert_eq!(decide_teardown_handle(&entry), TeardownHandleAction::DecRef {
            header_kva: kva,
        });
    }

    #[test]
    fn teardown_handle_non_pageset_skip() {
        let entry = make_entry(HandleKind::Endpoint { paddr: dummy_paddr(), caller_token: 0 });
        assert_eq!(decide_teardown_handle(&entry), TeardownHandleAction::Skip);
    }

    #[test]
    fn teardown_handle_empty_slot_skip() {
        let entry = make_entry(HandleKind::Empty);
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
        let kva = dummy_kva();
        let mapped = make_entry(HandleKind::PageSet { kva, mapped_va_page: 0x400 });
        assert!(matches!(decide_close_handle(Some(&mapped)),
            CloseHandleResult::UnmapThenRemove { .. }));

        // After the kernel clears mapped_va_page (simulating the slot
        // state after set_mapped_va(handle, 0)):
        let cleared = make_entry(HandleKind::PageSet { kva, mapped_va_page: 0 });
        assert!(matches!(decide_close_handle(Some(&cleared)),
            CloseHandleResult::RemoveAndDecRef { .. }));
    }
}
