use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::ipc::ep_queue;
use crate::ipc::reply::{REPLY_STATE_BOUND, REPLY_STATE_FRESH, ReplyObject};
use crate::mm::addr::{PhysAddr, paddr_of_raw};
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::scheduler::{self, BlockToken, scoped_mut};
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
/// Takes `*mut EndpointObject` instead of `&mut` because the slow path
/// blocks via `block_current()`. A `&mut` parameter would create a Stacked
/// Borrows tag that lives across the context switch; another thread
/// accessing the same endpoint would invalidate it, producing UB. Raw
/// pointers do not create tags, so scoped `&mut` references derived from
/// them can be dropped before blocking without aliasing violations.
pub fn ipc_send(
    ep: *mut EndpointObject,
    msg: [u64; 4],
) -> Result<(), IpcError> {
    // Fast path: receiver is queued. Dequeue, transfer, unblock.
    // Entirely within one scoped block — no blocking, returns early.
    {
        let ep_ref = unsafe { &mut *ep };
        debug_assert_eq!(ep_ref.header.obj_type, ObjectType::Endpoint);

        if ep_ref.state == EP_HAS_RECEIVER {
            let receiver = ep_queue::dequeue(ep_ref)
                .expect("HasReceiver without queued waiter");
            // SAFETY: receiver paddr from the endpoint queue — enqueue contract
            // guarantees it is a valid TCB.
            let mut receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver) };
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
            ep_ref.state = EP_IDLE;
            return Ok(());
        }
    }

    // Slow path: queue self as Send, block.
    let mut tok = BlockToken::new();
    let sender_tcb_paddr = scheduler::current_tcb_paddr();
    let ep_paddr = paddr_of_raw(ep);
    {
        // SAFETY: scheduler guarantees current_tcb_paddr is a valid, live TCB.
        let sender_tcb = unsafe { KernelMut::<Tcb>::from_paddr(sender_tcb_paddr) };
        let s = unsafe { scoped_mut(sender_tcb.raw_ptr(), &mut tok) };
        s.ipc_msg = msg;
        s.ipc_wait_kind = WAIT_KIND_SEND;
        s.ipc_blocked_on = ep_paddr.as_u64();
    }
    {
        let ep_ref = unsafe { scoped_mut(ep, &mut tok) };
        // SAFETY: sender_tcb_paddr is the current thread, not already queued.
        unsafe { ep_queue::enqueue(ep_ref, sender_tcb_paddr) };
        let became_has_waiters = ep_ref.state == EP_IDLE;
        if became_has_waiters {
            ep_ref.state = EP_HAS_WAITERS;
            wake_readiness_waiter_if_registered(ep_ref);
        }
    }
    // Token consumed — compiler proved no &mut references alive.
    scheduler::block_current(tok);
    Ok(())
}

/// Receive a message from an endpoint. Blocks if the queue is empty of
/// senders/callers. Returns the received 4-word message.
///
/// Takes `*mut EndpointObject` — see `ipc_send` doc for rationale.
pub fn ipc_receive(
    ep: *mut EndpointObject,
) -> Result<[u64; 4], IpcError> {
    let receiver_tcb_paddr = scheduler::current_tcb_paddr();

    // Single-reply-slot rule: cannot Receive while holding an outstanding Reply.
    {
        let receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver_tcb_paddr) };
        if receiver_tcb.get().current_reply_paddr != 0 {
            return Err(IpcError::EndpointBusy);
        }
    }

    let state = unsafe { (*ep).state };
    debug_assert_eq!(unsafe { (*ep).header.obj_type }, ObjectType::Endpoint);

    match state {
        EP_HAS_WAITERS => {
            // Non-blocking path — single scoped block for all mutations.
            let ep_ref = unsafe { &mut *ep };
            let head = ep_queue::dequeue(ep_ref)
                .expect("HasWaiters without queued waiter");
            // SAFETY: head paddr from the endpoint queue — valid TCB.
            let mut head_tcb = unsafe { KernelMut::<Tcb>::from_paddr(head) };
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
                    let mut receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver_tcb_paddr) };
                    receiver_tcb.get_mut().current_reply_paddr = reply_paddr;
                }
                _ => unreachable!("HasWaiters queue only holds Send or Call"),
            }

            // If the queue is now empty, collapse back to Idle.
            if ep_ref.queue_head == 0 {
                ep_ref.state = EP_IDLE;
            }
            Ok(msg)
        }
        EP_HAS_RECEIVER => {
            // Another receiver already queued — invariant 5 bars us.
            Err(IpcError::EndpointBusy)
        }
        EP_IDLE | _ => {
            // Slow path: queue self as Receive, block.
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
                ep_ref.state = EP_HAS_RECEIVER;
            }
            // Token consumed — compiler proved no &mut references alive.
            scheduler::block_current(tok);

            // On wake, msg + current_reply_paddr are already populated by
            // whichever sender/caller fast-pathed us.
            let receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver_tcb_paddr) };
            Ok(receiver_tcb.get().ipc_msg)
        }
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
) -> Result<[u64; 4], IpcError> {
    // Precondition checks via raw reads — no &mut created.
    debug_assert_eq!(unsafe { (*ep).header.obj_type }, ObjectType::Endpoint);
    debug_assert_eq!(unsafe { (*reply).header.obj_type }, ObjectType::Reply);
    if unsafe { (*reply).state } != REPLY_STATE_FRESH {
        return Err(IpcError::ReplyBound);
    }

    let mut tok = BlockToken::new();
    let caller_tcb_paddr = scheduler::current_tcb_paddr();
    let ep_paddr = paddr_of_raw(ep);
    let reply_paddr = paddr_of_raw(reply);

    // Bind the Reply to this caller before touching the endpoint. This
    // pairing is what the new model calls out: the caller's identity
    // lives on the Reply object, not on the endpoint.
    {
        let reply_ref = unsafe { scoped_mut(reply, &mut tok) };
        reply_ref.state = REPLY_STATE_BOUND;
        reply_ref.caller_tcb_paddr = caller_tcb_paddr.as_u64();
    }

    // Read state via raw pointer to avoid holding a scoped borrow across
    // both branches (each branch borrows tok independently).
    let ep_state = unsafe { (*ep).state };

    if ep_state == EP_HAS_RECEIVER {
        // Fast path: receiver queued. Dequeue, deliver msg + bind reply,
        // unblock server, block self.
        let ep_ref = unsafe { scoped_mut(ep, &mut tok) };
        let receiver = ep_queue::dequeue(ep_ref)
            .expect("HasReceiver without queued waiter");
        // SAFETY: receiver paddr from the endpoint queue — valid TCB.
        let mut receiver_tcb = unsafe { KernelMut::<Tcb>::from_paddr(receiver) };
        {
            let r = receiver_tcb.get_mut();
            r.ipc_msg = msg;
            r.ipc_wait_kind = WAIT_KIND_NONE;
            r.current_reply_paddr = reply_paddr.as_u64();
            // Clear the receiver's blocked-on pointer — it's runnable now.
            r.ipc_blocked_on = 0;
        }
        scheduler::unblock_thread(receiver);
        ep_ref.state = EP_IDLE;
    } else {
        // Slow path: store msg in own TCB, then enqueue on endpoint.
        {
            // SAFETY: scheduler guarantees current_tcb_paddr is valid.
            let caller_tcb = unsafe { KernelMut::<Tcb>::from_paddr(caller_tcb_paddr) };
            let c = unsafe { scoped_mut(caller_tcb.raw_ptr(), &mut tok) };
            c.ipc_msg = msg;
            c.ipc_wait_kind = WAIT_KIND_CALL;
            c.ipc_call_reply_paddr = reply_paddr.as_u64();
        }
        {
            let ep_ref = unsafe { scoped_mut(ep, &mut tok) };
            // SAFETY: caller_tcb_paddr is the current thread, not already queued.
            unsafe { ep_queue::enqueue(ep_ref, caller_tcb_paddr) };
            let became_has_waiters = ep_ref.state == EP_IDLE;
            if became_has_waiters {
                ep_ref.state = EP_HAS_WAITERS;
                wake_readiness_waiter_if_registered(ep_ref);
            }
        }
    }

    // Set blocked-on (both paths) in its own scope.
    {
        let caller_tcb = unsafe { KernelMut::<Tcb>::from_paddr(caller_tcb_paddr) };
        let c = unsafe { scoped_mut(caller_tcb.raw_ptr(), &mut tok) };
        c.ipc_blocked_on = ep_paddr.as_u64();
    }
    // Token consumed — compiler proved no &mut references alive.
    scheduler::block_current(tok);

    // On wake, the reply has been written into our ipc_msg and the
    // Reply object has been returned to Fresh by the server's ipc_reply.
    let caller_tcb = unsafe { KernelMut::<Tcb>::from_paddr(caller_tcb_paddr) };
    Ok(caller_tcb.get().ipc_msg)
}

/// Non-blocking receive. Returns `WouldBlock` if the queue has nothing
/// for a receiver to take.
pub fn ipc_receive_nb(ep: &mut EndpointObject) -> Result<[u64; 4], IpcError> {
    debug_assert_eq!(ep.header.obj_type, ObjectType::Endpoint);
    if ep.state != EP_HAS_WAITERS {
        return Err(IpcError::WouldBlock);
    }
    // A Send or Call is queued — reuse the normal receive path (won't block
    // because ep.state == EP_HAS_WAITERS takes the non-blocking branch).
    // SAFETY: ep is a valid &mut, cast to *mut for ipc_receive's raw-pointer signature.
    ipc_receive(ep as *mut EndpointObject)
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
