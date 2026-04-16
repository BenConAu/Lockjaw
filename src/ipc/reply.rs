use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::ipc::endpoint::IpcError;
use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;
use core::ptr;

/// Reply object state.
///
/// `Fresh` — not bound to any call. sys_call consumes a Fresh Reply.
/// `Bound` — bound to a caller that is currently blocked waiting. The server's
///           sys_reply uses `caller_tcb_paddr` to unblock exactly that caller
///           and return the Reply to Fresh.
pub const REPLY_STATE_FRESH: u8 = 0;
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
    let mut slot = KernelMut::<ReplyObject>::from_paddr(base_paddr);
    // SAFETY: writing into freshly donated, uninitialized storage.
    ptr::write(slot.as_mut_ptr(), ReplyObject {
        header: ObjectHeader {
            obj_type: ObjectType::Reply,
            page_count: 1,
        },
        state: REPLY_STATE_FRESH,
        caller_tcb_paddr: 0,
    });
    Ok(())
}

/// Reply to the call currently bound on `replier_tcb_paddr`. Reads the
/// replier's `current_reply_paddr` (set earlier by `ipc_receive` when it
/// dequeued a Call waiter), writes `msg` into the caller's TCB, returns
/// the Reply object to Fresh, and unblocks the caller.
///
/// Replies never touch endpoint state — they go directly from the server's
/// TCB to the caller's TCB via the Reply object. This is the property that
/// eliminates the multi-caller race: two outstanding callers' Replies are
/// independent objects with independent bindings.
///
/// # Safety
/// `replier_tcb_paddr` must be a valid TCB. Any Reply object referenced via
/// that TCB's `current_reply_paddr` must be a valid kernel object.
pub unsafe fn ipc_reply(
    replier_tcb_paddr: PhysAddr,
    msg: [u64; 4],
) -> Result<(), IpcError> {
    let mut replier_tcb = KernelMut::<Tcb>::from_paddr(replier_tcb_paddr);
    let reply_paddr_u64 = replier_tcb.get().current_reply_paddr;
    if reply_paddr_u64 == 0 {
        return Err(IpcError::NoCaller);
    }

    let mut reply = KernelMut::<ReplyObject>::from_paddr(PhysAddr::new(reply_paddr_u64));
    if reply.get().state != REPLY_STATE_BOUND {
        // Someone else (or stale state) — shouldn't happen in a coherent kernel.
        return Err(IpcError::NoCaller);
    }
    let caller_paddr = PhysAddr::new(reply.get().caller_tcb_paddr);
    let mut caller_tcb = KernelMut::<Tcb>::from_paddr(caller_paddr);

    // Deliver the reply message straight to the caller's ipc_msg.
    {
        let c = caller_tcb.get_mut();
        c.ipc_msg = msg;
        c.ipc_blocked_on = 0;
    }

    // Unblock BEFORE clearing the Reply (ordering rule: UnblockThread
    // precedes ClearReply, because unblock reads reply.caller).
    scheduler::unblock_thread(caller_paddr);

    // Return the Reply to Fresh and detach this server from the call.
    {
        let r = reply.get_mut();
        r.state = REPLY_STATE_FRESH;
        r.caller_tcb_paddr = 0;
    }
    replier_tcb.get_mut().current_reply_paddr = 0;

    Ok(())
}
