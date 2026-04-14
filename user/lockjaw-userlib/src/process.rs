/// A mapping entry for sys_create_process.
/// Must match the kernel's process::ProcessMapping layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProcessMapping {
    pub virt_addr: u64,
    pub pageset_id: u64,
    pub page_index: u64,
    pub flags: u64,
}

pub const FLAG_EXECUTABLE: u64 = 1 << 0;
