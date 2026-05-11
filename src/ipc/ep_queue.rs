//! Intrusive FIFO queue of waiter TCBs on an endpoint.
//!
//! Callers and senders that block on an endpoint link themselves into
//! this queue via the TCB's `ipc_queue_next` field. The endpoint holds
//! `queue_head`/`queue_tail` KVAs (TCBs live in the KVM pool —
//! kernel-vmem-roadmap.md) for O(1) enqueue/dequeue.
//!
//! No allocation — the storage is the TCB's own page.
//!
//! `remove`, `is_empty`, and `peek_head` have no callers yet — retained
//! for process teardown (see docs/yagni-parking-lot.md).

use crate::ipc::endpoint::EndpointObject;
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::tcb::Tcb;
use lockjaw_types::addr::KernelVa;

/// Append a TCB to the tail of the endpoint's waiter queue.
///
/// # Safety
/// `tcb_kva` must be the KVA of a valid [`Tcb`] that is not already
/// queued on any endpoint.
pub unsafe fn enqueue(ep: &mut EndpointObject, tcb_kva: KernelVa) {
    // SAFETY: caller contract — tcb_kva is a live Tcb.
    let mut tcb = KernelMut::<Tcb>::from_kva(tcb_kva);
    tcb.get_mut().ipc_queue_next = 0;

    if ep.queue_tail == 0 {
        // Empty queue — new entry becomes both head and tail.
        ep.queue_head = tcb_kva.as_u64();
        ep.queue_tail = tcb_kva.as_u64();
    } else {
        // Non-empty — link previous tail's next to us, update tail.
        // SAFETY: prior tail was enqueued under the same contract.
        let mut prev_tail = KernelMut::<Tcb>::from_kva(KernelVa::new(ep.queue_tail));
        prev_tail.get_mut().ipc_queue_next = tcb_kva.as_u64();
        ep.queue_tail = tcb_kva.as_u64();
    }
}

/// Pop the head of the endpoint's waiter queue. Returns the TCB KVA,
/// or `None` if the queue is empty. The popped TCB's `ipc_queue_next`
/// is cleared before return.
pub fn dequeue(ep: &mut EndpointObject) -> Option<KernelVa> {
    let head = ep.queue_head;
    if head == 0 {
        return None;
    }
    // SAFETY: head was set only by enqueue() above, which upholds the
    // Tcb-kva contract.
    let mut head_tcb = unsafe { KernelMut::<Tcb>::from_kva(KernelVa::new(head)) };
    let next = head_tcb.get().ipc_queue_next;
    head_tcb.get_mut().ipc_queue_next = 0;

    ep.queue_head = next;
    if next == 0 {
        // Queue drained — clear tail too.
        ep.queue_tail = 0;
    }
    Some(KernelVa::new(head))
}

/// Remove a specific TCB from the endpoint's waiter queue. O(n) in
/// queue length (intended for process teardown, where a dying thread
/// must be unlinked from any endpoint it was blocked on).
///
/// Returns `true` if the TCB was found and removed, `false` otherwise.
///
/// # Safety
/// `tcb_kva` must refer to a valid [`Tcb`] (whether or not it is
/// actually queued on this endpoint).
#[allow(dead_code)]
pub unsafe fn remove(ep: &mut EndpointObject, tcb_kva: KernelVa) -> bool {
    let target = tcb_kva.as_u64();
    let mut prev: u64 = 0;
    let mut cur = ep.queue_head;

    while cur != 0 {
        // SAFETY: cur comes from the queue, which only holds valid Tcb KVAs.
        let mut cur_tcb = KernelMut::<Tcb>::from_kva(KernelVa::new(cur));
        let next = cur_tcb.get().ipc_queue_next;
        if cur == target {
            // Unlink cur.
            if prev == 0 {
                ep.queue_head = next;
            } else {
                // SAFETY: prev walked past via the same queue invariant.
                let mut prev_tcb = KernelMut::<Tcb>::from_kva(KernelVa::new(prev));
                prev_tcb.get_mut().ipc_queue_next = next;
            }
            if ep.queue_tail == cur {
                ep.queue_tail = prev;
            }
            cur_tcb.get_mut().ipc_queue_next = 0;
            return true;
        }
        prev = cur;
        cur = next;
    }
    false
}

/// Whether the endpoint's queue is empty.
#[allow(dead_code)]
pub fn is_empty(ep: &EndpointObject) -> bool {
    ep.queue_head == 0
}

/// Peek at the head TCB KVA without dequeuing.
#[allow(dead_code)]
pub fn peek_head(ep: &EndpointObject) -> Option<KernelVa> {
    if ep.queue_head == 0 { None } else { Some(KernelVa::new(ep.queue_head)) }
}
