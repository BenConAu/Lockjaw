use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use core::ptr;

/// Reply object state.
///
/// `Fresh` — not bound to any call. sys_call consumes a Fresh Reply.
/// `Bound` — bound to a caller that is currently blocked waiting. The server's
///           sys_reply uses `caller_tcb_paddr` to unblock exactly that caller
///           and return the Reply to Fresh.
pub const REPLY_STATE_FRESH: u8 = 0;
#[allow(dead_code)] // Used in commit 5 cutover (sys_call binds / sys_reply clears).
pub const REPLY_STATE_BOUND: u8 = 1;

/// Kernel-side Reply object. Lives in one donated page.
///
/// Each client thread owns one Reply and reuses it across calls. The Reply
/// carries the identity of the blocked caller independently of endpoint
/// state, so multiple concurrent callers on the same endpoint do not
/// overwrite each other — which is the bug this object fixes.
#[repr(C)]
pub struct ReplyObject {
    pub header: ObjectHeader,
    /// REPLY_STATE_FRESH or REPLY_STATE_BOUND.
    pub state: u8,
    /// TCB paddr of the caller currently bound to this Reply (0 when Fresh).
    pub caller_tcb_paddr: u64,
}

/// Initialize a Reply object in donated physical memory.
///
/// # Safety
/// `base_paddr` must be a donated page (one page, not mapped by userspace).
pub unsafe fn create_reply(base_paddr: PhysAddr) -> Result<(), CreateError> {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let obj_va = (base_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut ReplyObject;
    ptr::write(obj_va, ReplyObject {
        header: ObjectHeader {
            obj_type: ObjectType::Reply,
            page_count: 1,
        },
        state: REPLY_STATE_FRESH,
        caller_tcb_paddr: 0,
    });
    Ok(())
}
