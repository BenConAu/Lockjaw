use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::ipc::ep_queue;
use crate::ipc::reply::{REPLY_STATE_BOUND, REPLY_STATE_FRESH, ReplyObject};
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;
use core::ptr;

// ---------------------------------------------------------------------------
// Endpoint state tags — mirror lockjaw_types::ipc_state_reply::EpState
// ---------------------------------------------------------------------------

pub const EP_IDLE: u8 = 0;
pub const EP_HAS_WAITERS: u8 = 1;
pub const EP_HAS_RECEIVER: u8 = 2;

// ---------------------------------------------------------------------------
// Wait-kind tags stored in TCB.ipc_wait_kind
// ---------------------------------------------------------------------------

pub const WAIT_KIND_NONE: u8 = 0;
pub const WAIT_KIND_SEND: u8 = 1;
pub const WAIT_KIND_RECEIVE: u8 = 2;
pub const WAIT_KIND_CALL: u8 = 3;

// ---------------------------------------------------------------------------
// Endpoint object — stored in a donated page
// ---------------------------------------------------------------------------

/// Kernel-side endpoint object stored in a donated page.
///
/// The `state`, `queue_head`, and `queue_tail` fields drive all current
/// IPC. The legacy `blocked_tcb_paddr` / `msg` / `caller_tcb_paddr`
/// fields are unused after the Reply-object redesign; they will be
/// removed in the cleanup commit.
#[repr(C)]
pub struct EndpointObject {
    pub header: ObjectHeader,
    /// EP_IDLE, EP_HAS_WAITERS, or EP_HAS_RECEIVER.
    pub state: u8,
    /// Legacy — unused. Kept to preserve struct layout until cleanup.
    pub blocked_tcb_paddr: u64,
    /// Legacy — unused. Messages travel via TCB.ipc_msg now.
    pub msg: [u64; 4],
    /// Legacy — unused. Per-call identity lives in Reply objects.
    pub caller_tcb_paddr: u64,
    /// Thread waiting via sys_wait_any for readiness.
    pub readiness_waiter: lockjaw_types::wait::ReadinessWaiter,
    /// Head of the intrusive waiter queue (paddr of first queued TCB,
    /// 0 = empty). TCBs link via their ipc_queue_next field.
    pub queue_head: u64,
    /// Tail of the intrusive waiter queue.
    pub queue_tail: u64,
}

/// Initialize an endpoint object in donated memory.
///
/// # Safety
/// `base_paddr` must point to a donated page not mapped by userspace.
pub unsafe fn create_endpoint(base_paddr: PhysAddr) -> Result<(), CreateError> {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let ep_va = (base_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut EndpointObject;
    ptr::write(ep_va, EndpointObject {
        header: ObjectHeader {
            obj_type: ObjectType::Endpoint,
            page_count: 1,
        },
        state: EP_IDLE,
        blocked_tcb_paddr: 0,
        msg: [0; 4],
        caller_tcb_paddr: 0,
        readiness_waiter: lockjaw_types::wait::ReadinessWaiter::empty(),
        queue_head: 0,
        queue_tail: 0,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// IPC operation errors
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum IpcError {
    EndpointBusy,
    NoCaller,
    WouldBlock,
    ReplyBound,
}

// ---------------------------------------------------------------------------
// IPC operations
// ---------------------------------------------------------------------------
//
// Each operation mirrors a transition in lockjaw_types::ipc_state_reply.
// The model proves the state-machine shape; the kernel mechanically
// implements the same transitions over live pointers.

/// Send a message on an endpoint. Blocks the sender if no receiver is waiting.
///
/// # Safety
/// `endpoint_paddr` and `sender_tcb_paddr` must point to valid kernel objects.
pub unsafe fn ipc_send(
    endpoint_paddr: PhysAddr,
    msg: [u64; 4],
    sender_tcb_paddr: PhysAddr,
) -> Result<(), IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);

    if (*ep).state == EP_HAS_RECEIVER {
        // Fast path: receiver is queued. Dequeue, transfer, unblock.
        let receiver = ep_queue::dequeue(ep).expect("HasReceiver without queued waiter");
        let receiver_tcb = tcb_ptr_mut(receiver);
        (*receiver_tcb).ipc_msg = msg;
        (*receiver_tcb).ipc_wait_kind = WAIT_KIND_NONE;
        scheduler::unblock_thread(receiver);
        (*ep).state = EP_IDLE;
        return Ok(());
    }

    // Slow path: queue self as Send, block.
    let sender_tcb = tcb_ptr_mut(sender_tcb_paddr);
    (*sender_tcb).ipc_msg = msg;
    (*sender_tcb).ipc_wait_kind = WAIT_KIND_SEND;
    (*sender_tcb).ipc_blocked_on = endpoint_paddr.as_u64();
    ep_queue::enqueue(ep, sender_tcb_paddr);

    let became_has_waiters = (*ep).state == EP_IDLE;
    if became_has_waiters {
        (*ep).state = EP_HAS_WAITERS;
        wake_readiness_waiter_if_registered(ep);
    }

    scheduler::block_current();
    Ok(())
}

/// Receive a message from an endpoint. Blocks if the queue is empty of
/// senders/callers. Returns the received 4-word message.
///
/// # Safety
/// `endpoint_paddr` and `receiver_tcb_paddr` must point to valid kernel objects.
pub unsafe fn ipc_receive(
    endpoint_paddr: PhysAddr,
    receiver_tcb_paddr: PhysAddr,
) -> Result<[u64; 4], IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);
    let receiver_tcb = tcb_ptr_mut(receiver_tcb_paddr);

    // Single-reply-slot rule: cannot Receive while holding an outstanding Reply.
    if (*receiver_tcb).current_reply_paddr != 0 {
        return Err(IpcError::EndpointBusy);
    }

    match (*ep).state {
        EP_HAS_WAITERS => {
            // Dequeue head — either a Send or Call waiter.
            let head = ep_queue::dequeue(ep).expect("HasWaiters without queued waiter");
            let head_tcb = tcb_ptr_mut(head);
            let msg = (*head_tcb).ipc_msg;
            let kind = (*head_tcb).ipc_wait_kind;
            (*head_tcb).ipc_wait_kind = WAIT_KIND_NONE;

            match kind {
                WAIT_KIND_SEND => {
                    // Sender is done — wake it.
                    (*head_tcb).ipc_blocked_on = 0;
                    scheduler::unblock_thread(head);
                }
                WAIT_KIND_CALL => {
                    // Caller stays blocked awaiting reply. Bind the caller's
                    // Reply object to THIS receiver's current_reply slot.
                    let reply_paddr = (*head_tcb).ipc_call_reply_paddr;
                    (*head_tcb).ipc_call_reply_paddr = 0;
                    (*receiver_tcb).current_reply_paddr = reply_paddr;
                }
                _ => unreachable!("HasWaiters queue only holds Send or Call"),
            }

            // If the queue is now empty, collapse back to Idle.
            if (*ep).queue_head == 0 {
                (*ep).state = EP_IDLE;
            }
            Ok(msg)
        }
        EP_HAS_RECEIVER => {
            // Another receiver already queued — invariant 5 bars us.
            Err(IpcError::EndpointBusy)
        }
        EP_IDLE | _ => {
            // Slow path: queue self as Receive, block.
            (*receiver_tcb).ipc_wait_kind = WAIT_KIND_RECEIVE;
            (*receiver_tcb).ipc_blocked_on = endpoint_paddr.as_u64();
            ep_queue::enqueue(ep, receiver_tcb_paddr);
            (*ep).state = EP_HAS_RECEIVER;
            scheduler::block_current();

            // On wake, msg + current_reply_paddr are already populated by
            // whichever sender/caller fast-pathed us.
            Ok((*receiver_tcb).ipc_msg)
        }
    }
}

/// Send a message and block waiting for a reply (client call).
/// `reply_paddr` must point to a Fresh ReplyObject owned by the caller;
/// on successful return the reply has been delivered and the object is
/// Fresh again, ready for the next call.
///
/// # Safety
/// `endpoint_paddr`, `reply_paddr`, and `caller_tcb_paddr` must point to
/// valid kernel objects.
pub unsafe fn ipc_call(
    endpoint_paddr: PhysAddr,
    reply_paddr: PhysAddr,
    msg: [u64; 4],
    caller_tcb_paddr: PhysAddr,
) -> Result<[u64; 4], IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);
    let reply = reply_ptr_mut(reply_paddr);

    // Precondition: the Reply object must be Fresh.
    if (*reply).state != REPLY_STATE_FRESH {
        return Err(IpcError::ReplyBound);
    }

    // Bind the Reply to this caller before touching the endpoint. This
    // pairing is what the new model calls out: the caller's identity
    // lives on the Reply object, not on the endpoint.
    (*reply).state = REPLY_STATE_BOUND;
    (*reply).caller_tcb_paddr = caller_tcb_paddr.as_u64();

    let caller_tcb = tcb_ptr_mut(caller_tcb_paddr);

    if (*ep).state == EP_HAS_RECEIVER {
        // Fast path: receiver queued. Dequeue, deliver msg + bind reply,
        // unblock server, block self.
        let receiver = ep_queue::dequeue(ep).expect("HasReceiver without queued waiter");
        let receiver_tcb = tcb_ptr_mut(receiver);
        (*receiver_tcb).ipc_msg = msg;
        (*receiver_tcb).ipc_wait_kind = WAIT_KIND_NONE;
        (*receiver_tcb).current_reply_paddr = reply_paddr.as_u64();
        scheduler::unblock_thread(receiver);
        (*ep).state = EP_IDLE;
    } else {
        // Slow path: store msg in own TCB, queue as Call, block.
        (*caller_tcb).ipc_msg = msg;
        (*caller_tcb).ipc_wait_kind = WAIT_KIND_CALL;
        (*caller_tcb).ipc_call_reply_paddr = reply_paddr.as_u64();
        ep_queue::enqueue(ep, caller_tcb_paddr);
        let became_has_waiters = (*ep).state == EP_IDLE;
        if became_has_waiters {
            (*ep).state = EP_HAS_WAITERS;
            wake_readiness_waiter_if_registered(ep);
        }
    }

    (*caller_tcb).ipc_blocked_on = endpoint_paddr.as_u64();
    scheduler::block_current();

    // On wake, the reply has been written into our ipc_msg and the
    // Reply object has been returned to Fresh by the server's ipc_reply.
    Ok((*caller_tcb).ipc_msg)
}

/// Non-blocking receive. Returns `WouldBlock` if the queue has nothing
/// for a receiver to take.
///
/// # Safety
/// `endpoint_paddr` and `receiver_tcb_paddr` must point to valid kernel objects.
pub unsafe fn ipc_receive_nb(
    endpoint_paddr: PhysAddr,
    receiver_tcb_paddr: PhysAddr,
) -> Result<[u64; 4], IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);
    if (*ep).state != EP_HAS_WAITERS {
        return Err(IpcError::WouldBlock);
    }
    // A Send or Call is queued — reuse the normal receive path (won't block).
    ipc_receive(endpoint_paddr, receiver_tcb_paddr)
}

// ---------------------------------------------------------------------------
// Readiness helpers for sys_wait_any
// ---------------------------------------------------------------------------

/// Read the endpoint's model EpState. Used by sys_wait_any for readiness.
pub fn read_state(ep_paddr: PhysAddr) -> lockjaw_types::ipc_state_reply::EpState {
    use lockjaw_types::ipc_state_reply::EpState;
    unsafe {
        match (*ep_ptr(ep_paddr)).state {
            EP_HAS_WAITERS => EpState::HasWaiters,
            EP_HAS_RECEIVER => EpState::HasReceiver,
            _ => EpState::Idle,
        }
    }
}

/// Register a thread as a readiness waiter on this endpoint.
/// The thread will be woken (without consuming) when a sender/caller arrives.
pub unsafe fn set_readiness_waiter(ep_paddr: PhysAddr, waiter_paddr: PhysAddr) {
    let ep = ep_ptr_mut(ep_paddr);
    let _ = (*ep).readiness_waiter.register(waiter_paddr.as_u64());
}

/// Clear the readiness waiter if it matches the expected thread.
pub unsafe fn clear_readiness_waiter(ep_paddr: PhysAddr, expected: PhysAddr) {
    let ep = ep_ptr_mut(ep_paddr);
    (*ep).readiness_waiter.clear_if_match(expected.as_u64());
}

unsafe fn wake_readiness_waiter_if_registered(ep: *mut EndpointObject) {
    if (*ep).readiness_waiter.is_registered() {
        scheduler::unblock_thread(PhysAddr::new((*ep).readiness_waiter.paddr));
        (*ep).readiness_waiter.paddr = 0;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

unsafe fn ep_ptr(paddr: PhysAddr) -> *const EndpointObject {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *const EndpointObject
}

unsafe fn ep_ptr_mut(paddr: PhysAddr) -> *mut EndpointObject {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *mut EndpointObject
}

unsafe fn tcb_ptr_mut(paddr: PhysAddr) -> *mut Tcb {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *mut Tcb
}

unsafe fn reply_ptr_mut(paddr: PhysAddr) -> *mut ReplyObject {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *mut ReplyObject
}
