use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;
use lockjaw_types::ipc_state::{EpState, IpcOp, SystemState, TransitionError};
use core::ptr;

// ---------------------------------------------------------------------------
// Endpoint states — must match lockjaw_types::ipc_state::EpState
// ---------------------------------------------------------------------------

const EP_IDLE: u8 = 0;
const EP_HAS_SENDER: u8 = 1;
const EP_HAS_RECEIVER: u8 = 2;
const EP_HAS_CALLER: u8 = 3;

fn ep_state_to_u8(s: EpState) -> u8 {
    match s {
        EpState::Idle => EP_IDLE,
        EpState::HasSender => EP_HAS_SENDER,
        EpState::HasReceiver => EP_HAS_RECEIVER,
        EpState::HasCaller => EP_HAS_CALLER,
    }
}

fn u8_to_ep_state(v: u8) -> EpState {
    match v {
        EP_HAS_SENDER => EpState::HasSender,
        EP_HAS_RECEIVER => EpState::HasReceiver,
        EP_HAS_CALLER => EpState::HasCaller,
        _ => EpState::Idle,
    }
}

// ---------------------------------------------------------------------------
// Endpoint object — stored in a donated page
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct EndpointObject {
    pub header: ObjectHeader,
    pub state: u8,
    pub blocked_tcb_paddr: u64,
    pub msg: [u64; 4],
    pub caller_tcb_paddr: u64,
}

pub unsafe fn create_endpoint(base_paddr: PhysAddr) -> Result<(), CreateError> {
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
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// IPC operations — validated by the lockjaw-types state machine
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum IpcError {
    EndpointBusy,
    NoCaller,
    InvalidTransition,
}

impl From<TransitionError> for IpcError {
    fn from(e: TransitionError) -> Self {
        match e {
            TransitionError::Busy => IpcError::EndpointBusy,
            TransitionError::NoCaller => IpcError::NoCaller,
            TransitionError::ThreadNotRunnable => IpcError::InvalidTransition,
        }
    }
}

/// Validate an IPC operation against the state machine model.
/// Returns the current endpoint state for the caller to act on.
unsafe fn validate_transition(ep: *const EndpointObject, op: IpcOp) -> Result<EpState, IpcError> {
    let current = u8_to_ep_state((*ep).state);
    let has_caller = (*ep).caller_tcb_paddr != 0;

    // Build a SystemState for validation. We model the acting thread as A
    // (Running) and use B as the blocked/caller thread if one exists.
    let has_blocked = (*ep).blocked_tcb_paddr != 0;
    let dummy = SystemState {
        endpoint: current,
        has_caller,
        blocked_thread: if has_blocked { Some(lockjaw_types::ipc_state::Thread::B) } else { None },
        caller_thread: if has_caller { Some(lockjaw_types::ipc_state::Thread::B) } else { None },
        thread_a: lockjaw_types::ipc_state::ThreadState::Running,
        thread_b: if has_blocked || has_caller {
            lockjaw_types::ipc_state::ThreadState::Blocked
        } else {
            lockjaw_types::ipc_state::ThreadState::Ready
        },
    };

    // Validate the transition is legal
    lockjaw_types::ipc_state::step(dummy, lockjaw_types::ipc_state::Thread::A, op)?;
    Ok(current)
}

/// Send a message on an endpoint.
pub unsafe fn ipc_send(
    endpoint_paddr: PhysAddr,
    msg: [u64; 4],
    sender_tcb_paddr: PhysAddr,
) -> Result<(), IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);
    let current = validate_transition(ep, IpcOp::Send)?;

    match current {
        EpState::HasReceiver => {
            // Fast path: receiver waiting. Transfer msg, unblock receiver.
            let receiver_paddr = PhysAddr::new((*ep).blocked_tcb_paddr);
            let receiver_tcb = tcb_ptr_mut(receiver_paddr);
            (*receiver_tcb).ipc_msg = msg;
            scheduler::unblock_thread(receiver_paddr);

            (*ep).state = EP_IDLE;
            (*ep).blocked_tcb_paddr = 0;
            Ok(())
        }
        EpState::Idle => {
            // Slow path: block sender.
            (*ep).state = EP_HAS_SENDER;
            (*ep).blocked_tcb_paddr = sender_tcb_paddr.as_u64();
            (*ep).msg = msg;

            let sender_tcb = tcb_ptr_mut(sender_tcb_paddr);
            (*sender_tcb).ipc_blocked_on = endpoint_paddr.as_u64();
            scheduler::block_current();
            Ok(())
        }
        _ => Err(IpcError::EndpointBusy),
    }
}

/// Receive a message from an endpoint.
pub unsafe fn ipc_receive(
    endpoint_paddr: PhysAddr,
    receiver_tcb_paddr: PhysAddr,
) -> Result<[u64; 4], IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);
    let current = validate_transition(ep, IpcOp::Receive)?;

    match current {
        EpState::HasSender => {
            // Fast path: regular sender waiting. Take msg, unblock sender.
            let msg = (*ep).msg;
            let sender_paddr = PhysAddr::new((*ep).blocked_tcb_paddr);
            scheduler::unblock_thread(sender_paddr);

            (*ep).state = EP_IDLE;
            (*ep).blocked_tcb_paddr = 0;
            (*ep).msg = [0; 4];
            Ok(msg)
        }
        EpState::HasCaller => {
            // Caller waiting. Take msg, do NOT unblock caller.
            let msg = (*ep).msg;
            (*ep).state = EP_IDLE;
            (*ep).blocked_tcb_paddr = 0;
            (*ep).msg = [0; 4];
            Ok(msg)
        }
        EpState::Idle => {
            // Slow path: block receiver.
            (*ep).state = EP_HAS_RECEIVER;
            (*ep).blocked_tcb_paddr = receiver_tcb_paddr.as_u64();

            let receiver_tcb = tcb_ptr_mut(receiver_tcb_paddr);
            (*receiver_tcb).ipc_blocked_on = endpoint_paddr.as_u64();
            scheduler::block_current();

            let receiver_tcb = tcb_ptr(receiver_tcb_paddr);
            Ok((*receiver_tcb).ipc_msg)
        }
        _ => Err(IpcError::EndpointBusy),
    }
}

/// Call: send a message and block waiting for a reply.
pub unsafe fn ipc_call(
    endpoint_paddr: PhysAddr,
    msg: [u64; 4],
    caller_tcb_paddr: PhysAddr,
) -> Result<[u64; 4], IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);
    let current = validate_transition(ep, IpcOp::Call)?;

    (*ep).caller_tcb_paddr = caller_tcb_paddr.as_u64();

    match current {
        EpState::HasReceiver => {
            // Fast path: server waiting. Transfer msg, unblock server, block caller.
            let receiver_paddr = PhysAddr::new((*ep).blocked_tcb_paddr);
            let receiver_tcb = tcb_ptr_mut(receiver_paddr);
            (*receiver_tcb).ipc_msg = msg;
            scheduler::unblock_thread(receiver_paddr);

            (*ep).state = EP_IDLE;
            (*ep).blocked_tcb_paddr = 0;

            let caller_tcb = tcb_ptr_mut(caller_tcb_paddr);
            (*caller_tcb).ipc_blocked_on = endpoint_paddr.as_u64();
            scheduler::block_current();

            let caller_tcb = tcb_ptr(caller_tcb_paddr);
            Ok((*caller_tcb).ipc_msg)
        }
        EpState::Idle => {
            // Slow path: block caller with message.
            (*ep).state = EP_HAS_CALLER;
            (*ep).blocked_tcb_paddr = caller_tcb_paddr.as_u64();
            (*ep).msg = msg;

            let caller_tcb = tcb_ptr_mut(caller_tcb_paddr);
            (*caller_tcb).ipc_blocked_on = endpoint_paddr.as_u64();
            scheduler::block_current();

            let caller_tcb = tcb_ptr(caller_tcb_paddr);
            Ok((*caller_tcb).ipc_msg)
        }
        _ => Err(IpcError::EndpointBusy),
    }
}

/// Reply to the last caller on an endpoint.
pub unsafe fn ipc_reply(
    endpoint_paddr: PhysAddr,
    reply_msg: [u64; 4],
) -> Result<(), IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);
    validate_transition(ep, IpcOp::Reply)?;

    let caller_paddr = PhysAddr::new((*ep).caller_tcb_paddr);
    let caller_tcb = tcb_ptr_mut(caller_paddr);
    (*caller_tcb).ipc_msg = reply_msg;
    scheduler::unblock_thread(caller_paddr);

    let ep = ep_ptr_mut(endpoint_paddr);
    (*ep).caller_tcb_paddr = 0;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

unsafe fn ep_ptr_mut(paddr: PhysAddr) -> *mut EndpointObject {
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *mut EndpointObject
}

unsafe fn tcb_ptr(paddr: PhysAddr) -> *const Tcb {
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *const Tcb
}

unsafe fn tcb_ptr_mut(paddr: PhysAddr) -> *mut Tcb {
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *mut Tcb
}
