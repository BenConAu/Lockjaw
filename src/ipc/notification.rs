use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::{PhysAddr, paddr_of};
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;
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
    pub blocked_tcb_paddr: u64,
    /// Thread waiting via sys_wait_any for readiness.
    pub readiness_waiter: lockjaw_types::wait::ReadinessWaiter,
    /// Threshold the readiness waiter is waiting for (value >= this means ready).
    pub readiness_threshold: u64,
}

/// Initialize a Notification in a donated page.
pub fn create_notification(page: crate::mm::addr::ObjectInitPage) -> Result<(), CreateError> {
    // SAFETY: ObjectInitPage guarantees owned storage.
    let mut slot = unsafe { KernelMut::<NotificationObject>::from_paddr(page.paddr()) };
    unsafe {
        ptr::write(slot.as_mut_ptr(), NotificationObject {
            header: ObjectHeader {
                obj_type: ObjectType::Notification,
                page_count: 1,
            },
            state: NotificationState::new(),
            blocked_tcb_paddr: 0,
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
            let waiter = PhysAddr::new(obj.blocked_tcb_paddr);
            obj.blocked_tcb_paddr = 0;
            scheduler::unblock_thread(waiter);
        }
    }

    // Check readiness waiter (registered via sys_wait_any).
    // Wake if the new value meets the readiness threshold.
    let should_wake = obj.readiness_waiter.is_registered()
        && lockjaw_types::wait::is_notification_ready(new_value, obj.readiness_threshold);
    if should_wake {
        let waiter = PhysAddr::new(obj.readiness_waiter.paddr);
        obj.readiness_waiter.paddr = 0;
        obj.readiness_threshold = 0;
        scheduler::unblock_thread(waiter);
    }

    Ok(())
}

/// Wait on a notification until the timeline value reaches the threshold.
/// Returns immediately if the counter is already >= threshold.
/// Otherwise blocks the calling thread.
pub fn notification_wait(
    obj: &mut NotificationObject,
    threshold: u64,
) -> Result<u64, NotificationError> {
    debug_assert_eq!(obj.header.obj_type, ObjectType::Notification);

    match obj.state.wait(threshold)? {
        WaitResult::Ready => {
            // Counter already past threshold — return current value
            Ok(obj.state.value)
        }
        WaitResult::Block => {
            // Block until signaled
            let caller_tcb_paddr = scheduler::current_tcb_paddr();
            obj.blocked_tcb_paddr = caller_tcb_paddr.as_u64();

            // SAFETY: scheduler guarantees current_tcb_paddr is a valid, live TCB.
            let mut caller_tcb = unsafe { KernelMut::<Tcb>::from_paddr(caller_tcb_paddr) };
            let notif_paddr = paddr_of(obj);
            caller_tcb.get_mut().ipc_blocked_on = notif_paddr.as_u64();

            scheduler::block_current();

            // When we wake up, return the current value
            Ok(obj.state.value)
        }
    }
}

// ---------------------------------------------------------------------------
// Readiness helpers for sys_wait_any
// ---------------------------------------------------------------------------

/// Read the current timeline value from this notification.
/// Safe because PhysAddr is a trusted kernel address.
pub fn read_value(notif_paddr: PhysAddr) -> u64 {
    // SAFETY: notif_paddr is a trusted kernel-object paddr (handle-table origin).
    let obj = unsafe { KernelMut::<NotificationObject>::from_paddr(notif_paddr) };
    obj.get().state.value
}

/// Register a thread as a readiness waiter on this notification.
/// The thread will be woken when the value reaches the threshold.
///
/// # Safety
/// `notif_paddr` must be a live `NotificationObject`.
pub unsafe fn set_readiness_waiter(notif_paddr: PhysAddr, waiter_paddr: PhysAddr, threshold: u64) {
    let mut obj = KernelMut::<NotificationObject>::from_paddr(notif_paddr);
    let o = obj.get_mut();
    let _ = o.readiness_waiter.register(waiter_paddr.as_u64());
    o.readiness_threshold = threshold;
}

/// Clear the readiness waiter if it matches the expected thread.
///
/// # Safety
/// `notif_paddr` must be a live `NotificationObject`.
pub unsafe fn clear_readiness_waiter(notif_paddr: PhysAddr, expected: PhysAddr) {
    let mut obj = KernelMut::<NotificationObject>::from_paddr(notif_paddr);
    let o = obj.get_mut();
    if o.readiness_waiter.clear_if_match(expected.as_u64()) {
        o.readiness_threshold = 0;
    }
}
