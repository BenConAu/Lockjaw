use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::kva_of_raw;
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::scheduler::{self, BlockToken, scoped_mut};
use crate::sched::tcb::Tcb;
use lockjaw_types::addr::KernelVa;
use lockjaw_types::notification_state::{NotificationState, SignalResult, WaitResult, NotificationError};
use core::ptr;

/// Kernel-side Notification object. Lives in a donated page.
/// The state machine logic is in lockjaw-types (tested on host).
/// The kernel reads/writes the state and executes side effects (block/unblock).
#[repr(C)]
pub struct NotificationObject {
    pub header: ObjectHeader,
    /// Timeline semaphore state (value, has_waiter, threshold). Pure logic in lockjaw-types.
    pub state: NotificationState,
    /// TCB paddr of the thread blocked via sys_wait_notification (0 = none).
    pub blocked_tcb_kva: u64,
    /// Thread waiting via sys_wait_any for readiness.
    pub readiness_waiter: lockjaw_types::wait::ReadinessWaiter,
    /// Threshold the readiness waiter is waiting for (value >= this means ready).
    pub readiness_threshold: u64,
}

/// Initialize a Notification in a donated page.
///
/// Init writes through the linear map (paddr); the same bytes are
/// visible at the KVA after `kvm::map_existing` runs in
/// `create_kernel_object_kvm`.
pub fn create_notification(page: crate::mm::addr::ObjectInitPage) -> Result<(), CreateError> {
    // SAFETY: ObjectInitPage guarantees owned storage.
    let mut slot = unsafe { KernelMut::<NotificationObject>::from_paddr(page.paddr()) };
    unsafe {
        ptr::write(slot.as_mut_ptr(), NotificationObject {
            header: ObjectHeader {
                obj_type: ObjectType::Notification,
                page_count: 1,
                refcount: 0, // incremented by first handle_insert
            },
            state: NotificationState::new(),
            blocked_tcb_kva: 0,
            readiness_waiter: lockjaw_types::wait::ReadinessWaiter::empty(),
            readiness_threshold: 0,
        });
    }
    Ok(())
}

/// Signal a notification with a new timeline value.
/// If a waiter's threshold is met, unblocks it.
pub fn notification_signal(
    obj: &mut NotificationObject,
    new_value: u64,
) -> Result<(), NotificationError> {
    debug_assert_eq!(obj.header.obj_type, ObjectType::Notification);

    match obj.state.signal(new_value)? {
        SignalResult::Updated => {}
        SignalResult::WakeWaiter => {
            let waiter = KernelVa::new(obj.blocked_tcb_kva);
            obj.blocked_tcb_kva = 0;
            scheduler::unblock_thread(waiter);
        }
    }

    // Check readiness waiter (registered via sys_wait_any).
    // Wake if the new value meets the readiness threshold.
    let should_wake = obj.readiness_waiter.is_registered()
        && lockjaw_types::wait::is_notification_ready(new_value, obj.readiness_threshold);
    if should_wake {
        // ReadinessWaiter.paddr now holds a TCB KVA — TCBs live in KVM.
        let waiter = KernelVa::new(obj.readiness_waiter.paddr);
        obj.readiness_waiter.paddr = 0;
        obj.readiness_threshold = 0;
        scheduler::unblock_thread(waiter);
    }

    Ok(())
}

/// Wait on a notification until the timeline value reaches the threshold.
/// Returns immediately if the counter is already >= threshold.
/// Otherwise blocks the calling thread.
///
/// Takes `*mut NotificationObject` — see `ipc_send` doc for rationale.
pub fn notification_wait(
    obj: *mut NotificationObject,
    threshold: u64,
) -> Result<u64, NotificationError> {
    // Check state in a scoped block — state.wait() mutates has_waiter.
    let wait_result = {
        let obj_ref = unsafe { &mut *obj };
        debug_assert_eq!(obj_ref.header.obj_type, ObjectType::Notification);
        obj_ref.state.wait(threshold)?
    };

    match wait_result {
        WaitResult::Ready => {
            // Counter already past threshold — return current value
            Ok(unsafe { (*obj).state.value })
        }
        WaitResult::Block => {
            // Block until signaled
            let mut tok = BlockToken::new();
            let caller_tcb_kva = scheduler::current_tcb_kva();
            {
                let obj_ref = unsafe { scoped_mut(obj, &mut tok) };
                obj_ref.blocked_tcb_kva = caller_tcb_kva.as_u64();
            }
            {
                // SAFETY: scheduler guarantees current_tcb_kva is a valid, live TCB.
                let caller_tcb = unsafe { KernelMut::<Tcb>::from_kva(caller_tcb_kva) };
                // Notification lives in KVM; carry its KVA through TCB.ipc_blocked_on.
                let notif_kva = kva_of_raw(obj);
                let t = unsafe { scoped_mut(caller_tcb.raw_ptr(), &mut tok) };
                t.ipc_blocked_on = notif_kva.as_u64();
            }
            // Token consumed — compiler proved no &mut references alive.
            scheduler::block_current(tok);

            // When we wake up, return the current value
            Ok(unsafe { (*obj).state.value })
        }
    }
}

// ---------------------------------------------------------------------------
// Readiness helpers for sys_wait_any
// ---------------------------------------------------------------------------

/// Read the current timeline value from this notification.
/// Safe because the kva is a trusted kernel-object KVA.
pub fn read_value(notif_kva: KernelVa) -> u64 {
    // SAFETY: notif_kva is a trusted kernel-object KVA (handle-table origin).
    let obj = unsafe { KernelMut::<NotificationObject>::from_kva(notif_kva) };
    obj.get().state.value
}

/// Register a thread as a readiness waiter on this notification.
/// The thread will be woken when the value reaches the threshold.
///
/// # Safety
/// `notif_kva` must be a live `NotificationObject` mapped in KVM.
/// `waiter_kva` must be a live `Tcb` mapped in KVM.
pub unsafe fn set_readiness_waiter(notif_kva: KernelVa, waiter_kva: KernelVa, threshold: u64) {
    let mut obj = KernelMut::<NotificationObject>::from_kva(notif_kva);
    let o = obj.get_mut();
    let _ = o.readiness_waiter.register(waiter_kva.as_u64());
    o.readiness_threshold = threshold;
}

/// Clear the readiness waiter if it matches the expected thread.
///
/// # Safety
/// `notif_kva` must be a live `NotificationObject` mapped in KVM.
pub unsafe fn clear_readiness_waiter(notif_kva: KernelVa, expected: KernelVa) {
    let mut obj = KernelMut::<NotificationObject>::from_kva(notif_kva);
    let o = obj.get_mut();
    if o.readiness_waiter.clear_if_match(expected.as_u64()) {
        o.readiness_threshold = 0;
    }
}
