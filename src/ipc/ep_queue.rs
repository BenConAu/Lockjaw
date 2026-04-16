//! Intrusive FIFO queue of waiter TCBs on an endpoint.
//!
//! Callers and senders that block on an endpoint link themselves into
//! this queue via the TCB's `ipc_queue_next` field. The endpoint holds
//! `queue_head`/`queue_tail` paddrs for O(1) enqueue/dequeue.
//!
//! No allocation — the storage is the TCB's own page.
//!
//! `remove`, `is_empty`, and `peek_head` are used by future process
//! teardown and readiness paths; `#[allow(dead_code)]` on them keeps
//! the build clean until those callers land.

use crate::ipc::endpoint::EndpointObject;
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use crate::sched::tcb::Tcb;

/// Append a TCB to the tail of the endpoint's waiter queue.
///
/// # Safety
/// `ep` must be a valid EndpointObject pointer. `tcb_paddr` must be the
/// paddr of a valid Tcb that is not already queued anywhere.
pub unsafe fn enqueue(ep: *mut EndpointObject, tcb_paddr: PhysAddr) {
    let tcb = tcb_ptr_mut(tcb_paddr);
    (*tcb).ipc_queue_next = 0;

    if (*ep).queue_tail == 0 {
        // Empty queue — new entry becomes both head and tail.
        (*ep).queue_head = tcb_paddr.as_u64();
        (*ep).queue_tail = tcb_paddr.as_u64();
    } else {
        // Non-empty — link previous tail's next to us, update tail.
        let prev_tail = tcb_ptr_mut(PhysAddr::new((*ep).queue_tail));
        (*prev_tail).ipc_queue_next = tcb_paddr.as_u64();
        (*ep).queue_tail = tcb_paddr.as_u64();
    }
}

/// Pop the head of the endpoint's waiter queue. Returns the TCB paddr,
/// or `None` if the queue is empty.
///
/// The popped TCB's `ipc_queue_next` is cleared before return.
///
/// # Safety
/// `ep` must be a valid EndpointObject pointer.
pub unsafe fn dequeue(ep: *mut EndpointObject) -> Option<PhysAddr> {
    let head = (*ep).queue_head;
    if head == 0 {
        return None;
    }
    let head_tcb = tcb_ptr_mut(PhysAddr::new(head));
    let next = (*head_tcb).ipc_queue_next;
    (*head_tcb).ipc_queue_next = 0;

    (*ep).queue_head = next;
    if next == 0 {
        // Queue drained — clear tail too.
        (*ep).queue_tail = 0;
    }
    Some(PhysAddr::new(head))
}

/// Remove a specific TCB from the endpoint's waiter queue. O(n) in
/// queue length (intended for process teardown, where a dying thread
/// must be unlinked from any endpoint it was blocked on).
///
/// Returns `true` if the TCB was found and removed, `false` otherwise.
///
/// # Safety
/// `ep` must be a valid EndpointObject pointer. `tcb_paddr` must be
/// the paddr of a valid Tcb (whether or not it's actually queued here).
#[allow(dead_code)]
pub unsafe fn remove(ep: *mut EndpointObject, tcb_paddr: PhysAddr) -> bool {
    let target = tcb_paddr.as_u64();
    let mut prev: u64 = 0;
    let mut cur = (*ep).queue_head;

    while cur != 0 {
        let cur_tcb = tcb_ptr_mut(PhysAddr::new(cur));
        let next = (*cur_tcb).ipc_queue_next;
        if cur == target {
            // Unlink cur.
            if prev == 0 {
                (*ep).queue_head = next;
            } else {
                let prev_tcb = tcb_ptr_mut(PhysAddr::new(prev));
                (*prev_tcb).ipc_queue_next = next;
            }
            if (*ep).queue_tail == cur {
                (*ep).queue_tail = prev;
            }
            (*cur_tcb).ipc_queue_next = 0;
            return true;
        }
        prev = cur;
        cur = next;
    }
    false
}

/// Whether the endpoint's queue is empty.
///
/// # Safety
/// `ep` must be a valid EndpointObject pointer.
#[allow(dead_code)]
pub unsafe fn is_empty(ep: *const EndpointObject) -> bool {
    (*ep).queue_head == 0
}

/// Peek at the head TCB paddr without dequeuing.
///
/// # Safety
/// `ep` must be a valid EndpointObject pointer.
#[allow(dead_code)]
pub unsafe fn peek_head(ep: *const EndpointObject) -> Option<PhysAddr> {
    let head = (*ep).queue_head;
    if head == 0 { None } else { Some(PhysAddr::new(head)) }
}

/// Resolve a TCB paddr to a mutable kernel-VA pointer.
unsafe fn tcb_ptr_mut(paddr: PhysAddr) -> *mut Tcb {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *mut Tcb
}
