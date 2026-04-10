use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;
use core::ptr;

// ---------------------------------------------------------------------------
// Endpoint states
// ---------------------------------------------------------------------------

const EP_IDLE: u8 = 0;
const EP_HAS_SENDER: u8 = 1;
const EP_HAS_RECEIVER: u8 = 2;

// ---------------------------------------------------------------------------
// Endpoint object — stored in a donated page
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct EndpointObject {
    pub header: ObjectHeader,
    pub state: u8,
    pub blocked_tcb_paddr: u64,
    pub msg: [u64; 4],
}

/// Initialize an Endpoint in donated physical memory.
///
/// # Safety
/// `base_paddr` must be a donated page.
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
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// IPC operations
// ---------------------------------------------------------------------------

/// IPC error type.
#[derive(Clone, Copy, Debug)]
pub enum IpcError {
    EndpointBusy,
}

/// Send a message on an endpoint. If a receiver is already waiting (fast path),
/// transfer the message directly and unblock the receiver. Otherwise block the
/// sender until a receiver arrives.
///
/// # Safety
/// `endpoint_paddr` must be a valid Endpoint object. `sender_tcb_paddr` must be
/// the calling thread's TCB.
pub unsafe fn ipc_send(
    endpoint_paddr: PhysAddr,
    msg: [u64; 4],
    sender_tcb_paddr: PhysAddr,
) -> Result<(), IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);

    match (*ep).state {
        EP_HAS_RECEIVER => {
            // Fast path: receiver is already waiting. Transfer message directly.
            let receiver_paddr = PhysAddr::new((*ep).blocked_tcb_paddr);
            let receiver_tcb = tcb_ptr_mut(receiver_paddr);

            // Copy message into receiver's IPC buffer
            (*receiver_tcb).ipc_msg = msg;

            // Unblock receiver
            scheduler::unblock_thread(receiver_paddr);

            // Reset endpoint to idle
            (*ep).state = EP_IDLE;
            (*ep).blocked_tcb_paddr = 0;

            Ok(())
        }
        EP_IDLE => {
            // Slow path: no receiver yet. Store message and block sender.
            (*ep).state = EP_HAS_SENDER;
            (*ep).blocked_tcb_paddr = sender_tcb_paddr.as_u64();
            (*ep).msg = msg;

            // Set sender's IPC state and block
            let sender_tcb = tcb_ptr_mut(sender_tcb_paddr);
            (*sender_tcb).ipc_blocked_on = endpoint_paddr.as_u64();

            scheduler::block_current();

            Ok(())
        }
        _ => Err(IpcError::EndpointBusy),
    }
}

/// Receive a message from an endpoint. If a sender is already waiting (fast
/// path), take the message and unblock the sender. Otherwise block the
/// receiver until a sender arrives.
///
/// Returns the 4-word message in the receiver's TCB ipc_msg field.
///
/// # Safety
/// `endpoint_paddr` must be a valid Endpoint object. `receiver_tcb_paddr` must
/// be the calling thread's TCB.
pub unsafe fn ipc_receive(
    endpoint_paddr: PhysAddr,
    receiver_tcb_paddr: PhysAddr,
) -> Result<[u64; 4], IpcError> {
    let ep = ep_ptr_mut(endpoint_paddr);

    match (*ep).state {
        EP_HAS_SENDER => {
            // Fast path: sender is already waiting. Take message from endpoint.
            let msg = (*ep).msg;
            let sender_paddr = PhysAddr::new((*ep).blocked_tcb_paddr);

            // Unblock sender
            scheduler::unblock_thread(sender_paddr);

            // Reset endpoint to idle
            (*ep).state = EP_IDLE;
            (*ep).blocked_tcb_paddr = 0;
            (*ep).msg = [0; 4];

            Ok(msg)
        }
        EP_IDLE => {
            // Slow path: no sender yet. Block receiver.
            (*ep).state = EP_HAS_RECEIVER;
            (*ep).blocked_tcb_paddr = receiver_tcb_paddr.as_u64();

            // Set receiver's IPC state and block
            let receiver_tcb = tcb_ptr_mut(receiver_tcb_paddr);
            (*receiver_tcb).ipc_blocked_on = endpoint_paddr.as_u64();

            scheduler::block_current();

            // When we wake up, the message is in our TCB's ipc_msg
            let receiver_tcb = tcb_ptr(receiver_tcb_paddr);
            Ok((*receiver_tcb).ipc_msg)
        }
        _ => Err(IpcError::EndpointBusy),
    }
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
