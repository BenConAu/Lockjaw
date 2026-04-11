use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;
use lockjaw_types::ipc_state::{
    EpState, IpcEffect, IpcOp, SystemState, Thread, ThreadState as ModelThreadState,
    derive_effects_array, step,
};
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
// IPC operations — driven entirely by the state machine model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum IpcError {
    EndpointBusy,
    NoCaller,
    InvalidTransition,
}

impl From<lockjaw_types::ipc_state::TransitionError> for IpcError {
    fn from(e: lockjaw_types::ipc_state::TransitionError) -> Self {
        match e {
            lockjaw_types::ipc_state::TransitionError::Busy => IpcError::EndpointBusy,
            lockjaw_types::ipc_state::TransitionError::NoCaller => IpcError::NoCaller,
            lockjaw_types::ipc_state::TransitionError::ThreadNotRunnable => IpcError::InvalidTransition,
        }
    }
}

/// Build the model SystemState from the live endpoint.
unsafe fn build_model_state(ep: *const EndpointObject) -> SystemState {
    let current = u8_to_ep_state((*ep).state);
    let has_caller = (*ep).caller_tcb_paddr != 0;
    let has_blocked = (*ep).blocked_tcb_paddr != 0;

    SystemState {
        endpoint: current,
        has_caller,
        blocked_thread: if has_blocked { Some(Thread::B) } else { None },
        caller_thread: if has_caller { Some(Thread::B) } else { None },
        thread_a: ModelThreadState::Running,
        thread_b: if has_blocked || has_caller {
            ModelThreadState::Blocked
        } else {
            ModelThreadState::Ready
        },
    }
}

/// Execute an IPC operation by consulting the state machine model, deriving
/// effects, and executing each effect mechanically. The kernel makes zero
/// decisions — the model decides everything.
unsafe fn execute_ipc(
    endpoint_paddr: PhysAddr,
    op: IpcOp,
    acting_tcb_paddr: PhysAddr,
    msg: Option<[u64; 4]>,
) -> Result<Option<[u64; 4]>, IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);
    let before = build_model_state(ep);

    // Ask the model: is this transition valid? What's the new state?
    let after = step(before, Thread::A, op)?;

    // Derive effects from the state diff
    let (effect_arr, effect_count) = derive_effects_array(&before, &after, Thread::A, op);

    // Execute each effect mechanically.
    // BlockCurrent must be last because it context-switches away — any effects
    // after it would not execute until the thread is unblocked.
    let mut received_msg: Option<[u64; 4]> = None;
    let mut needs_block = false;

    for i in 0..effect_count {
        match effect_arr[i] {
            IpcEffect::BlockCurrent => {
                needs_block = true;
                continue; // defer to after all other effects
            }
            IpcEffect::SetEndpointState(new_state) => {
                (*ep).state = ep_state_to_u8(new_state);
            }
            IpcEffect::StoreMessage => {
                (*ep).msg = msg.unwrap();
                (*ep).blocked_tcb_paddr = acting_tcb_paddr.as_u64();
            }
            IpcEffect::TransferMessageToBlocked => {
                if op == IpcOp::Reply {
                    // Reply: transfer to the caller
                    let caller_paddr = PhysAddr::new((*ep).caller_tcb_paddr);
                    let caller_tcb = tcb_ptr_mut(caller_paddr);
                    (*caller_tcb).ipc_msg = msg.unwrap();
                } else {
                    // Send/Call: transfer to the blocked receiver
                    let blocked_paddr = PhysAddr::new((*ep).blocked_tcb_paddr);
                    let blocked_tcb = tcb_ptr_mut(blocked_paddr);
                    (*blocked_tcb).ipc_msg = msg.unwrap();
                }
            }
            IpcEffect::TakeMessageFromEndpoint => {
                received_msg = Some((*ep).msg);
                (*ep).msg = [0; 4];
                (*ep).blocked_tcb_paddr = 0;
            }
            IpcEffect::RecordCaller => {
                (*ep).caller_tcb_paddr = acting_tcb_paddr.as_u64();
            }
            IpcEffect::ClearCaller => {
                (*ep).caller_tcb_paddr = 0;
            }
            IpcEffect::UnblockThread => {
                if op == IpcOp::Reply {
                    let caller_paddr = PhysAddr::new((*ep).caller_tcb_paddr);
                    scheduler::unblock_thread(caller_paddr);
                    // If blocked_tcb_paddr points to the caller (set by
                    // deferred BlockCurrent during Call fast path), clear it.
                    // Otherwise a stale paddr poisons build_model_state on
                    // the next round.
                    if (*ep).blocked_tcb_paddr == (*ep).caller_tcb_paddr {
                        (*ep).blocked_tcb_paddr = 0;
                    }
                } else {
                    let blocked_paddr = PhysAddr::new((*ep).blocked_tcb_paddr);
                    scheduler::unblock_thread(blocked_paddr);
                    (*ep).blocked_tcb_paddr = 0;
                }
            }
        }
    }

    // Execute BlockCurrent last — it context-switches away and doesn't
    // return until the thread is unblocked.
    if needs_block {
        // Record this thread as blocked on the endpoint (so the partner can find us)
        if (*ep).blocked_tcb_paddr == 0 {
            (*ep).blocked_tcb_paddr = acting_tcb_paddr.as_u64();
        }
        let tcb = tcb_ptr_mut(acting_tcb_paddr);
        (*tcb).ipc_blocked_on = endpoint_paddr.as_u64();
        scheduler::block_current();
    }

    // For receive: if we blocked (slow path), message is in our TCB when we wake
    if op == IpcOp::Receive && received_msg.is_none() {
        let tcb = tcb_ptr(acting_tcb_paddr);
        received_msg = Some((*tcb).ipc_msg);
    }

    // For call: we always block waiting for reply, message is in TCB when we wake
    if op == IpcOp::Call {
        let tcb = tcb_ptr(acting_tcb_paddr);
        received_msg = Some((*tcb).ipc_msg);
    }

    Ok(received_msg)
}

// ---------------------------------------------------------------------------
// Public API — thin wrappers over execute_ipc
// ---------------------------------------------------------------------------

pub unsafe fn ipc_send(
    endpoint_paddr: PhysAddr,
    msg: [u64; 4],
    sender_tcb_paddr: PhysAddr,
) -> Result<(), IpcError> {
    execute_ipc(endpoint_paddr, IpcOp::Send, sender_tcb_paddr, Some(msg))?;
    Ok(())
}

pub unsafe fn ipc_receive(
    endpoint_paddr: PhysAddr,
    receiver_tcb_paddr: PhysAddr,
) -> Result<[u64; 4], IpcError> {
    let result = execute_ipc(endpoint_paddr, IpcOp::Receive, receiver_tcb_paddr, None)?;
    Ok(result.unwrap_or([0; 4]))
}

pub unsafe fn ipc_call(
    endpoint_paddr: PhysAddr,
    msg: [u64; 4],
    caller_tcb_paddr: PhysAddr,
) -> Result<[u64; 4], IpcError> {
    let result = execute_ipc(endpoint_paddr, IpcOp::Call, caller_tcb_paddr, Some(msg))?;
    Ok(result.unwrap_or([0; 4]))
}

pub unsafe fn ipc_reply(
    endpoint_paddr: PhysAddr,
    reply_msg: [u64; 4],
) -> Result<(), IpcError> {
    // For reply, the "acting TCB" is the server — but we only need the endpoint
    // to find the caller. Pass a dummy for acting_tcb_paddr since reply doesn't
    // block the acting thread.
    execute_ipc(endpoint_paddr, IpcOp::Reply, PhysAddr::new(0), Some(reply_msg))?;
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
