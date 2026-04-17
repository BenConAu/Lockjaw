use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::ipc::ep_queue;
use crate::ipc::reply::{REPLY_STATE_BOUND, REPLY_STATE_FRESH, ReplyObject};
use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;
use core::ptr;

// ---------------------------------------------------------------------------
// Endpoint state tags — mirror lockjaw_types::ipc_state::EpState
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
/// The state is simply "do we have waiters, a blocked receiver, or nobody."
/// Waiters link themselves into the queue via `ipc_queue_next` in their own
/// TCBs; messages travel via the waiter's `ipc_msg`. Per-call caller
/// identity lives on each client's Reply object, not on the endpoint.
#[repr(C)]
pub struct EndpointObject {
    pub header: ObjectHeader,
    /// EP_IDLE, EP_HAS_WAITERS, or EP_HAS_RECEIVER.
    pub state: u8,
    /// Thread waiting via sys_wait_any for readiness.
    pub readiness_waiter: lockjaw_types::wait::ReadinessWaiter,
    /// Head of the intrusive waiter queue (paddr of first queued TCB,
    /// 0 = empty). TCBs link via their `ipc_queue_next` field.
    pub queue_head: u64,
    /// Tail of the intrusive waiter queue.
    pub queue_tail: u64,
}

/// Initialize an endpoint object in a donated page. The `ObjectInitPage`
/// newtype guarantees the page is kernel-owned and not mapped elsewhere.
pub fn create_endpoint(page: crate::mm::addr::ObjectInitPage) -> Result<(), CreateError> {
    // SAFETY: ObjectInitPage guarantees owned storage; KernelMut::from_paddr
    // performs the one VA cast.
    let mut slot = unsafe { KernelMut::<EndpointObject>::from_paddr(page.paddr()) };
    // SAFETY: writing into freshly donated, uninitialized storage.
    unsafe {
        ptr::write(slot.as_mut_ptr(), EndpointObject {
            header: ObjectHeader {
                obj_type: ObjectType::Endpoint,
                page_count: 1,
            },
            state: EP_IDLE,
            readiness_waiter: lockjaw_types::wait::ReadinessWaiter::empty(),
            queue_head: 0,
            queue_tail: 0,
        });
    }
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
// Each operation mirrors a transition in lockjaw_types::ipc_state.
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
    let mut ep = KernelMut::<EndpointObject>::from_paddr(endpoint_paddr);

    if ep.get().state == EP_HAS_RECEIVER {
        // Fast path: receiver is queued. Dequeue, transfer, unblock.
        let receiver = ep_queue::dequeue(ep.get_mut())
            .expect("HasReceiver without queued waiter");
        let mut receiver_tcb = KernelMut::<Tcb>::from_paddr(receiver);
        {
            let r = receiver_tcb.get_mut();
            r.ipc_msg = msg;
            r.ipc_wait_kind = WAIT_KIND_NONE;
            // Receiver's ipc_blocked_on was set when it queued itself; clear
            // it so teardown/diagnostics never see a runnable thread that
            // looks blocked on an endpoint.
            r.ipc_blocked_on = 0;
        }
        scheduler::unblock_thread(receiver);
        ep.get_mut().state = EP_IDLE;
        return Ok(());
    }

    // Slow path: queue self as Send, block.
    let mut sender_tcb = KernelMut::<Tcb>::from_paddr(sender_tcb_paddr);
    {
        let s = sender_tcb.get_mut();
        s.ipc_msg = msg;
        s.ipc_wait_kind = WAIT_KIND_SEND;
        s.ipc_blocked_on = endpoint_paddr.as_u64();
    }
    ep_queue::enqueue(ep.get_mut(), sender_tcb_paddr);

    let ep_ref = ep.get_mut();
    let became_has_waiters = ep_ref.state == EP_IDLE;
    if became_has_waiters {
        ep_ref.state = EP_HAS_WAITERS;
        wake_readiness_waiter_if_registered(ep_ref);
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
    let mut ep = KernelMut::<EndpointObject>::from_paddr(endpoint_paddr);
    let mut receiver_tcb = KernelMut::<Tcb>::from_paddr(receiver_tcb_paddr);

    // Single-reply-slot rule: cannot Receive while holding an outstanding Reply.
    if receiver_tcb.get().current_reply_paddr != 0 {
        return Err(IpcError::EndpointBusy);
    }

    match ep.get().state {
        EP_HAS_WAITERS => {
            // Dequeue head — either a Send or Call waiter.
            let head = ep_queue::dequeue(ep.get_mut())
                .expect("HasWaiters without queued waiter");
            let mut head_tcb = KernelMut::<Tcb>::from_paddr(head);
            let msg = head_tcb.get().ipc_msg;
            let kind = head_tcb.get().ipc_wait_kind;
            head_tcb.get_mut().ipc_wait_kind = WAIT_KIND_NONE;

            match kind {
                WAIT_KIND_SEND => {
                    // Sender is done — wake it.
                    head_tcb.get_mut().ipc_blocked_on = 0;
                    scheduler::unblock_thread(head);
                }
                WAIT_KIND_CALL => {
                    // Caller stays blocked awaiting reply. Bind the caller's
                    // Reply object to THIS receiver's current_reply slot.
                    let reply_paddr = head_tcb.get().ipc_call_reply_paddr;
                    head_tcb.get_mut().ipc_call_reply_paddr = 0;
                    receiver_tcb.get_mut().current_reply_paddr = reply_paddr;
                }
                _ => unreachable!("HasWaiters queue only holds Send or Call"),
            }

            // If the queue is now empty, collapse back to Idle.
            if ep.get().queue_head == 0 {
                ep.get_mut().state = EP_IDLE;
            }
            Ok(msg)
        }
        EP_HAS_RECEIVER => {
            // Another receiver already queued — invariant 5 bars us.
            Err(IpcError::EndpointBusy)
        }
        EP_IDLE | _ => {
            // Slow path: queue self as Receive, block.
            {
                let r = receiver_tcb.get_mut();
                r.ipc_wait_kind = WAIT_KIND_RECEIVE;
                r.ipc_blocked_on = endpoint_paddr.as_u64();
            }
            ep_queue::enqueue(ep.get_mut(), receiver_tcb_paddr);
            ep.get_mut().state = EP_HAS_RECEIVER;
            scheduler::block_current();

            // On wake, msg + current_reply_paddr are already populated by
            // whichever sender/caller fast-pathed us.
            Ok(receiver_tcb.get().ipc_msg)
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
    let mut ep = KernelMut::<EndpointObject>::from_paddr(endpoint_paddr);
    let mut reply = KernelMut::<ReplyObject>::from_paddr(reply_paddr);

    // Precondition: the Reply object must be Fresh.
    if reply.get().state != REPLY_STATE_FRESH {
        return Err(IpcError::ReplyBound);
    }

    // Bind the Reply to this caller before touching the endpoint. This
    // pairing is what the new model calls out: the caller's identity
    // lives on the Reply object, not on the endpoint.
    {
        let r = reply.get_mut();
        r.state = REPLY_STATE_BOUND;
        r.caller_tcb_paddr = caller_tcb_paddr.as_u64();
    }

    let mut caller_tcb = KernelMut::<Tcb>::from_paddr(caller_tcb_paddr);

    if ep.get().state == EP_HAS_RECEIVER {
        // Fast path: receiver queued. Dequeue, deliver msg + bind reply,
        // unblock server, block self.
        let receiver = ep_queue::dequeue(ep.get_mut())
            .expect("HasReceiver without queued waiter");
        let mut receiver_tcb = KernelMut::<Tcb>::from_paddr(receiver);
        {
            let r = receiver_tcb.get_mut();
            r.ipc_msg = msg;
            r.ipc_wait_kind = WAIT_KIND_NONE;
            r.current_reply_paddr = reply_paddr.as_u64();
            // Clear the receiver's blocked-on pointer — it's runnable now.
            r.ipc_blocked_on = 0;
        }
        scheduler::unblock_thread(receiver);
        ep.get_mut().state = EP_IDLE;
    } else {
        // Slow path: store msg in own TCB, queue as Call, block.
        {
            let c = caller_tcb.get_mut();
            c.ipc_msg = msg;
            c.ipc_wait_kind = WAIT_KIND_CALL;
            c.ipc_call_reply_paddr = reply_paddr.as_u64();
        }
        ep_queue::enqueue(ep.get_mut(), caller_tcb_paddr);
        let ep_ref = ep.get_mut();
        let became_has_waiters = ep_ref.state == EP_IDLE;
        if became_has_waiters {
            ep_ref.state = EP_HAS_WAITERS;
            wake_readiness_waiter_if_registered(ep_ref);
        }
    }

    caller_tcb.get_mut().ipc_blocked_on = endpoint_paddr.as_u64();
    scheduler::block_current();

    // On wake, the reply has been written into our ipc_msg and the
    // Reply object has been returned to Fresh by the server's ipc_reply.
    Ok(caller_tcb.get().ipc_msg)
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
    let ep = KernelMut::<EndpointObject>::from_paddr(endpoint_paddr);
    if ep.get().state != EP_HAS_WAITERS {
        return Err(IpcError::WouldBlock);
    }
    // Drop the KernelMut before the nested call re-acquires one.
    drop(ep);
    // A Send or Call is queued — reuse the normal receive path (won't block).
    ipc_receive(endpoint_paddr, receiver_tcb_paddr)
}

// ---------------------------------------------------------------------------
// Readiness helpers for sys_wait_any
// ---------------------------------------------------------------------------

/// Read the endpoint's model EpState. Used by sys_wait_any for readiness.
pub fn read_state(ep_paddr: PhysAddr) -> lockjaw_types::ipc_state::EpState {
    use lockjaw_types::ipc_state::EpState;
    // SAFETY: ep_paddr is a trusted kernel object paddr (produced only via
    // handle-table lookup on an Endpoint handle).
    let ep = unsafe { KernelMut::<EndpointObject>::from_paddr(ep_paddr) };
    match ep.get().state {
        EP_HAS_WAITERS => EpState::HasWaiters,
        EP_HAS_RECEIVER => EpState::HasReceiver,
        _ => EpState::Idle,
    }
}

/// Register a thread as a readiness waiter on this endpoint.
/// The thread will be woken (without consuming) when a sender/caller arrives.
///
/// # Safety
/// `ep_paddr` must point to a live `EndpointObject`.
pub unsafe fn set_readiness_waiter(ep_paddr: PhysAddr, waiter_paddr: PhysAddr) {
    let mut ep = KernelMut::<EndpointObject>::from_paddr(ep_paddr);
    let _ = ep.get_mut().readiness_waiter.register(waiter_paddr.as_u64());
}

/// Clear the readiness waiter if it matches the expected thread.
///
/// # Safety
/// `ep_paddr` must point to a live `EndpointObject`.
pub unsafe fn clear_readiness_waiter(ep_paddr: PhysAddr, expected: PhysAddr) {
    let mut ep = KernelMut::<EndpointObject>::from_paddr(ep_paddr);
    ep.get_mut().readiness_waiter.clear_if_match(expected.as_u64());
}

fn wake_readiness_waiter_if_registered(ep: &mut EndpointObject) {
    if ep.readiness_waiter.is_registered() {
        scheduler::unblock_thread(PhysAddr::new(ep.readiness_waiter.paddr));
        ep.readiness_waiter.paddr = 0;
    }
}
