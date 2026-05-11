use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;
use core::ptr;
use lockjaw_types::addr::KernelVa;

// Single source of truth: constants and decisions from types.
use lockjaw_types::ipc_state::{
    IpcError, REPLY_STATE_FRESH, REPLY_STATE_BOUND,
    ReplyDecision, decide_reply,
};

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
    pub caller_tcb_kva: u64,
}

/// Initialize a Reply object in a donated page.
pub fn create_reply(page: crate::mm::addr::ObjectInitPage) -> Result<(), CreateError> {
    // SAFETY: ObjectInitPage guarantees owned storage.
    let mut slot = unsafe { KernelMut::<ReplyObject>::from_paddr(page.paddr()) };
    unsafe {
        ptr::write(slot.as_mut_ptr(), ReplyObject {
            header: ObjectHeader {
                obj_type: ObjectType::Reply,
                page_count: 1,
                refcount: 0, // incremented by first handle_insert
            },
            state: REPLY_STATE_FRESH,
            caller_tcb_kva: 0,
        });
    }
    Ok(())
}

/// Reply to the call currently bound on the current thread. Reads the
/// replier's `current_reply_kva` (set earlier by `ipc_receive` when it
/// dequeued a Call waiter), writes `msg` into the caller's TCB, returns
/// the Reply object to Fresh, and unblocks the caller.
///
/// Replies never touch endpoint state — they go directly from the server's
/// TCB to the caller's TCB via the Reply object. This is the property that
/// eliminates the multi-caller race: two outstanding callers' Replies are
/// independent objects with independent bindings.
pub fn ipc_reply(
    msg: [u64; 4],
) -> Result<(), IpcError> {
    let replier_tcb_kva = scheduler::current_tcb_kva();
    // SAFETY: scheduler guarantees current_tcb_kva is a valid, live TCB.
    let replier_tcb = unsafe { KernelMut::<Tcb>::from_kva(replier_tcb_kva) };
    let reply_kva_u64 = replier_tcb.get().current_reply_kva;

    let has_reply = reply_kva_u64 != 0;
    let reply_is_bound = if has_reply {
        // SAFETY: current_reply_kva was set by ipc_receive from a valid Reply
        // KVA produced by `kvm::map_existing` in `sys_create_reply`.
        let reply = unsafe { KernelMut::<ReplyObject>::from_kva(KernelVa::new(reply_kva_u64)) };
        debug_assert_eq!(reply.get().header.obj_type, ObjectType::Reply);
        reply.get().state == REPLY_STATE_BOUND
    } else {
        false
    };

    match decide_reply(has_reply, reply_is_bound) {
        ReplyDecision::Deliver => {
            let mut reply = unsafe { KernelMut::<ReplyObject>::from_kva(KernelVa::new(reply_kva_u64)) };
            let caller_kva = KernelVa::new(reply.get().caller_tcb_kva);
            // SAFETY: Reply.caller_tcb_kva was set by ipc_call from a valid TCB.
            let mut caller_tcb = unsafe { KernelMut::<Tcb>::from_kva(caller_kva) };

            // Deliver the reply message straight to the caller's ipc_msg.
            {
                let c = caller_tcb.get_mut();
                c.ipc_msg = msg;
                c.ipc_blocked_on = 0;
            }

            // Unblock BEFORE clearing the Reply (ordering rule: UnblockThread
            // precedes ClearReply, because unblock reads reply.caller).
            scheduler::unblock_thread(caller_kva);

            // Return the Reply to Fresh and detach this server from the call.
            {
                let r = reply.get_mut();
                r.state = REPLY_STATE_FRESH;
                r.caller_tcb_kva = 0;
            }
            let mut replier_tcb = unsafe { KernelMut::<Tcb>::from_kva(replier_tcb_kva) };
            replier_tcb.get_mut().current_reply_kva = 0;

            Ok(())
        }
        ReplyDecision::Error(e) => Err(e),
    }
}
