use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::ipc::ep_queue;
use crate::ipc::reply::ReplyObject;
use crate::mm::addr::{PhysAddr, paddr_of_raw};
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::scheduler::{self, BlockToken, scoped_mut};
use crate::sched::tcb::Tcb;
use core::ptr;

// Single source of truth: constants and decision functions from types.
pub use lockjaw_types::ipc_state::{
    EP_IDLE,
    WAIT_KIND_NONE, WAIT_KIND_SEND, WAIT_KIND_RECEIVE, WAIT_KIND_CALL,
    REPLY_STATE_FRESH, REPLY_STATE_BOUND,
    IpcError,
};
use lockjaw_types::ipc_state::{
    EpState, WaitKind,
    SendDecision, ReceiveDecision, CallDecision,
    decide_send, decide_receive, decide_call,
};

// ---------------------------------------------------------------------------
// Endpoint object — stored in a donated page
// ---------------------------------------------------------------------------

/// Kernel-side endpoint object stored in a donated page.
///
/// The state is simply "do we have waiters, a blocked receiver, or nobody."
/// Waiters link themselves into the queue via `ipc_queue_next` in their own
/// TCBs; messages travel via the waiter's `ipc_msg`. Per-call caller
/// identity is carried as a kernel-assigned caller token on each handle
/// and delivered to the server via sys_query_caller_token.
#[repr(C)]
pub struct EndpointObject {
    pub header: ObjectHeader,
    /// EP_IDLE, EP_HAS_WAITERS, or EP_HAS_RECEIVER.
    pub state: u8,
    /// Monotonic counter for assigning caller tokens on export.
    /// Starts at 1; token 0 means "no token" (server's own handle).
    pub next_token: u64,
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
                refcount: 0, // incremented by first handle_insert
            },
            state: EP_IDLE,
            next_token: 1,
            readiness_waiter: lockjaw_types::wait::ReadinessWaiter::empty(),
            queue_head: 0,
            queue_tail: 0,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// IPC operations
// ---------------------------------------------------------------------------
//
// Each operation reads kernel state, converts to typed enums, calls the
// decision function in lockjaw-types, then mechanically executes side
// effects based on the returned decision. No inline state branching.

/// Send a message on an endpoint. Blocks the sender if no receiver is waiting.
///
/// Takes `*mut EndpointObject` instead of `&mut` because the slow path
/// blocks via `block_current()`. A `&mut` parameter would create a Stacked
/// Borrows tag that lives across the context switch; another thread
/// accessing the same endpoint would invalidate it, producing UB. Raw
/// pointers do not create tags, so scoped `&mut` references derived from
/// them can be dropped before blocking without aliasing violations.
pub fn ipc_send(
    ep: *mut EndpointObject,
    msg: [u64; 4],
    caller_token: u64,
) -> Result<(), IpcError> {
    let ep_state = EpState::from_raw(unsafe { (*ep).state })
        .unwrap_or_else(|| panic!("corrupted endpoint state"));
    debug_assert_eq!(unsafe { (*ep).header.obj_type }, ObjectType::Endpoint);

    match decide_send(ep_state) {
        SendDecision::DeliverToReceiver { next_ep_state } => {
            // Fast path: receiver is queued. Dequeue, transfer, unblock.
            let ep_ref = unsafe { &mut *ep };
            let receiver = ep_queue::dequeue(ep_ref)
                .unwrap_or_else(|| panic!("HasReceiver without queued waiter"));
            // SAFETY: receiver paddr from the endpoint queue — enqueue contract
            // guarantees it is a valid TCB.
            let mut receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver) };
            {
                let r = receiver_tcb.get_mut();
                r.ipc_msg = msg;
                r.last_caller_token = caller_token;
                r.ipc_wait_kind = WAIT_KIND_NONE;
                // Receiver's ipc_blocked_on was set when it queued itself; clear
                // it so teardown/diagnostics never see a runnable thread that
                // looks blocked on an endpoint.
                r.ipc_blocked_on = 0;
            }
            scheduler::unblock_thread(receiver);
            // Write endpoint state at commit point (after all effects)
            ep_ref.state = next_ep_state.to_raw();
            Ok(())
        }
        SendDecision::QueueAndBlock { next_ep_state, wake_readiness } => {
            // Slow path: queue self as Send, block.
            let mut tok = BlockToken::new();
            let sender_tcb_paddr = scheduler::current_tcb_paddr();
            let ep_paddr = paddr_of_raw(ep);
            {
                // SAFETY: scheduler guarantees current_tcb_paddr is a valid, live TCB.
                let sender_tcb = unsafe { KernelMut::<Tcb>::from_paddr(sender_tcb_paddr) };
                let s = unsafe { scoped_mut(sender_tcb.raw_ptr(), &mut tok) };
                s.ipc_msg = msg;
                s.ipc_caller_token = caller_token;
                s.ipc_wait_kind = WAIT_KIND_SEND;
                s.ipc_blocked_on = ep_paddr.as_u64();
            }
            {
                let ep_ref = unsafe { scoped_mut(ep, &mut tok) };
                // SAFETY: sender_tcb_paddr is the current thread, not already queued.
                unsafe { ep_queue::enqueue(ep_ref, sender_tcb_paddr) };
                ep_ref.state = next_ep_state.to_raw();
                if wake_readiness {
                    wake_readiness_waiter_if_registered(ep_ref);
                }
            }
            // Token consumed — compiler proved no &mut references alive.
            scheduler::block_current(tok);
            Ok(())
        }
    }
}

/// Receive a message from an endpoint. Blocks if the queue is empty of
/// senders/callers. Returns the received 4-word message.
///
/// Takes `*mut EndpointObject` — see `ipc_send` doc for rationale.
pub fn ipc_receive(
    ep: *mut EndpointObject,
) -> Result<[u64; 4], IpcError> {
    let receiver_tcb_paddr = scheduler::current_tcb_paddr();
    debug_assert_eq!(unsafe { (*ep).header.obj_type }, ObjectType::Endpoint);

    // Read kernel state for the decision function.
    let ep_state = EpState::from_raw(unsafe { (*ep).state })
        .unwrap_or_else(|| panic!("corrupted endpoint state"));
    let has_outstanding_reply = unsafe {
        KernelMut::<Tcb>::from_paddr(receiver_tcb_paddr).get().current_reply_paddr != 0
    };
    // Read queue head's wait kind (if there is a head).
    let (head_wait_kind, queue_has_more) = if ep_state == EpState::HasWaiters {
        let head_paddr = unsafe { (*ep).queue_head };
        if head_paddr != 0 {
            let head_tcb = unsafe { KernelMut::<Tcb>::from_paddr(PhysAddr::new(head_paddr)) };
            let kind = WaitKind::from_raw(head_tcb.get().ipc_wait_kind);
            let more = head_tcb.get().ipc_queue_next != 0;
            (kind, more)
        } else {
            (None, false)
        }
    } else {
        (None, false)
    };

    match decide_receive(ep_state, has_outstanding_reply, head_wait_kind, queue_has_more) {
        ReceiveDecision::DequeueSend { next_ep_state } => {
            // Dequeue head (was Send). Deliver msg, unblock sender.
            let ep_ref = unsafe { &mut *ep };
            let head = ep_queue::dequeue(ep_ref)
                .unwrap_or_else(|| panic!("HasWaiters without queued waiter"));
            // SAFETY: head paddr from the endpoint queue — valid TCB.
            let mut head_tcb = unsafe { KernelMut::<Tcb>::from_paddr(head) };
            let msg = head_tcb.get().ipc_msg;
            let sender_token = head_tcb.get().ipc_caller_token;
            head_tcb.get_mut().ipc_wait_kind = WAIT_KIND_NONE;
            head_tcb.get_mut().ipc_blocked_on = 0;
            // Write sender's caller token to receiver's last_caller_token.
            let mut receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver_tcb_paddr) };
            receiver_tcb.get_mut().last_caller_token = sender_token;
            scheduler::unblock_thread(head);
            ep_ref.state = next_ep_state.to_raw();
            Ok(msg)
        }
        ReceiveDecision::DequeueCall { next_ep_state } => {
            // Dequeue head (was Call). Deliver msg, bind reply.
            // Caller stays blocked awaiting reply.
            let ep_ref = unsafe { &mut *ep };
            let head = ep_queue::dequeue(ep_ref)
                .unwrap_or_else(|| panic!("HasWaiters without queued waiter"));
            // SAFETY: head paddr from the endpoint queue — valid TCB.
            let mut head_tcb = unsafe { KernelMut::<Tcb>::from_paddr(head) };
            let msg = head_tcb.get().ipc_msg;
            let sender_token = head_tcb.get().ipc_caller_token;
            head_tcb.get_mut().ipc_wait_kind = WAIT_KIND_NONE;
            // Bind the caller's Reply object to THIS receiver's current_reply slot.
            let reply_paddr = head_tcb.get().ipc_call_reply_paddr;
            head_tcb.get_mut().ipc_call_reply_paddr = 0;
            let mut receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver_tcb_paddr) };
            receiver_tcb.get_mut().current_reply_paddr = reply_paddr;
            receiver_tcb.get_mut().last_caller_token = sender_token;
            ep_ref.state = next_ep_state.to_raw();
            Ok(msg)
        }
        ReceiveDecision::QueueAndBlock { next_ep_state } => {
            // Queue empty — enqueue self as Receiver, block.
            let mut tok = BlockToken::new();
            let ep_paddr = paddr_of_raw(ep);
            {
                let receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver_tcb_paddr) };
                let r = unsafe { scoped_mut(receiver_tcb.raw_ptr(), &mut tok) };
                r.ipc_wait_kind = WAIT_KIND_RECEIVE;
                r.ipc_blocked_on = ep_paddr.as_u64();
            }
            {
                let ep_ref = unsafe { scoped_mut(ep, &mut tok) };
                // SAFETY: receiver_tcb_paddr is the current thread, not already queued.
                unsafe { ep_queue::enqueue(ep_ref, receiver_tcb_paddr) };
                ep_ref.state = next_ep_state.to_raw();
            }
            // Token consumed — compiler proved no &mut references alive.
            scheduler::block_current(tok);

            // On wake, msg + current_reply_paddr are already populated by
            // whichever sender/caller fast-pathed us.
            let receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver_tcb_paddr) };
            Ok(receiver_tcb.get().ipc_msg)
        }
        ReceiveDecision::Error(e) => Err(e),
    }
}

/// Send a message and block waiting for a reply (client call).
/// The Reply object must be Fresh; on successful return the reply has
/// been delivered and the object is Fresh again, ready for the next call.
///
/// Takes `*mut` for both endpoint and reply — see `ipc_send` doc for
/// rationale. This function always blocks (both fast and slow paths
/// converge to `block_current()`).
pub fn ipc_call(
    ep: *mut EndpointObject,
    reply: *mut ReplyObject,
    msg: [u64; 4],
    caller_token: u64,
) -> Result<[u64; 4], IpcError> {
    debug_assert_eq!(unsafe { (*ep).header.obj_type }, ObjectType::Endpoint);
    debug_assert_eq!(unsafe { (*reply).header.obj_type }, ObjectType::Reply);

    let reply_is_fresh = unsafe { (*reply).state } == REPLY_STATE_FRESH;
    let ep_state = EpState::from_raw(unsafe { (*ep).state })
        .unwrap_or_else(|| panic!("corrupted endpoint state"));

    match decide_call(reply_is_fresh, ep_state) {
        CallDecision::DeliverToReceiver { next_ep_state } => {
            // Fast path: receiver queued. Dequeue, deliver msg + bind reply,
            // unblock server, block self.
            let mut tok = BlockToken::new();
            let caller_tcb_paddr = scheduler::current_tcb_paddr();
            let ep_paddr = paddr_of_raw(ep);
            let reply_paddr = paddr_of_raw(reply);

            // Bind the Reply to this caller before delivering.
            {
                let reply_ref = unsafe { scoped_mut(reply, &mut tok) };
                reply_ref.state = REPLY_STATE_BOUND;
                reply_ref.caller_tcb_paddr = caller_tcb_paddr.as_u64();
            }
            {
                let ep_ref = unsafe { scoped_mut(ep, &mut tok) };
                let receiver = ep_queue::dequeue(ep_ref)
                    .unwrap_or_else(|| panic!("HasReceiver without queued waiter"));
                // SAFETY: receiver paddr from the endpoint queue — valid TCB.
                let mut receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver) };
                {
                    let r = receiver_tcb.get_mut();
                    r.ipc_msg = msg;
                    r.last_caller_token = caller_token;
                    r.ipc_wait_kind = WAIT_KIND_NONE;
                    r.current_reply_paddr = reply_paddr.as_u64();
                    r.ipc_blocked_on = 0;
                }
                scheduler::unblock_thread(receiver);
                ep_ref.state = next_ep_state.to_raw();
            }
            // Set caller's blocked-on.
            {
                let caller_tcb = unsafe { KernelMut::<Tcb>::from_paddr(caller_tcb_paddr) };
                let c = unsafe { scoped_mut(caller_tcb.raw_ptr(), &mut tok) };
                c.ipc_blocked_on = ep_paddr.as_u64();
            }
            // Caller always blocks — this is structural to Call.
            scheduler::block_current(tok);

            let caller_tcb = unsafe { KernelMut::<Tcb>::from_paddr(caller_tcb_paddr) };
            Ok(caller_tcb.get().ipc_msg)
        }
        CallDecision::QueueAndBlock { next_ep_state, wake_readiness } => {
            // Slow path: store msg in own TCB, then enqueue on endpoint.
            let mut tok = BlockToken::new();
            let caller_tcb_paddr = scheduler::current_tcb_paddr();
            let ep_paddr = paddr_of_raw(ep);
            let reply_paddr = paddr_of_raw(reply);

            // Bind the Reply to this caller.
            {
                let reply_ref = unsafe { scoped_mut(reply, &mut tok) };
                reply_ref.state = REPLY_STATE_BOUND;
                reply_ref.caller_tcb_paddr = caller_tcb_paddr.as_u64();
            }
            {
                // SAFETY: scheduler guarantees current_tcb_paddr is valid.
                let caller_tcb = unsafe { KernelMut::<Tcb>::from_paddr(caller_tcb_paddr) };
                let c = unsafe { scoped_mut(caller_tcb.raw_ptr(), &mut tok) };
                c.ipc_msg = msg;
                c.ipc_caller_token = caller_token;
                c.ipc_wait_kind = WAIT_KIND_CALL;
                c.ipc_call_reply_paddr = reply_paddr.as_u64();
                c.ipc_blocked_on = ep_paddr.as_u64();
            }
            {
                let ep_ref = unsafe { scoped_mut(ep, &mut tok) };
                // SAFETY: caller_tcb_paddr is the current thread, not already queued.
                unsafe { ep_queue::enqueue(ep_ref, caller_tcb_paddr) };
                ep_ref.state = next_ep_state.to_raw();
                if wake_readiness {
                    wake_readiness_waiter_if_registered(ep_ref);
                }
            }
            // Caller always blocks — this is structural to Call.
            scheduler::block_current(tok);

            let caller_tcb = unsafe { KernelMut::<Tcb>::from_paddr(caller_tcb_paddr) };
            Ok(caller_tcb.get().ipc_msg)
        }
        CallDecision::Error(e) => Err(e),
    }
}

/// Non-blocking receive. Returns `WouldBlock` if the queue has nothing
/// for a receiver to take.
pub fn ipc_receive_nb(ep: &mut EndpointObject) -> Result<[u64; 4], IpcError> {
    debug_assert_eq!(ep.header.obj_type, ObjectType::Endpoint);
    let ep_state = EpState::from_raw(ep.state)
        .unwrap_or_else(|| panic!("corrupted endpoint state"));
    if ep_state != EpState::HasWaiters {
        return Err(IpcError::WouldBlock);
    }
    // A Send or Call is queued — reuse the normal receive path (won't block
    // because HasWaiters takes the non-blocking branch).
    // SAFETY: ep is a valid &mut, cast to *mut for ipc_receive's raw-pointer signature.
    ipc_receive(ep as *mut EndpointObject)
}

// ---------------------------------------------------------------------------
// Readiness helpers for sys_wait_any
// ---------------------------------------------------------------------------

/// Read the endpoint's model EpState. Used by sys_wait_any for readiness.
pub fn read_state(ep_paddr: PhysAddr) -> EpState {
    // SAFETY: ep_paddr is a trusted kernel object paddr (produced only via
    // handle-table lookup on an Endpoint handle).
    let ep = unsafe { KernelMut::<EndpointObject>::from_paddr(ep_paddr) };
    EpState::from_raw(ep.get().state).unwrap_or_else(|| panic!("corrupted endpoint state"))
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
