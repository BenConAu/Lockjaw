/// A single entry in a handle table. Stored in the donated pages
/// immediately after the HandleTableHeader.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct HandleEntry {
    /// Physical address of the kernel object. 0 = empty slot.
    pub object_paddr: u64,
    /// Type of the referenced object.
    pub obj_type: crate::cap::object::ObjectType,
    /// Access rights for this handle.
    pub rights: u8,
    _padding: [u8; 6],
}
