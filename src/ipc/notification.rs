use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET};
use crate::sched::scheduler;
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
    /// TCB paddr of a thread waiting via sys_wait_any for readiness (0 = none).
    pub readiness_waiter_paddr: u64,
    /// Threshold the readiness waiter is waiting for (value >= this means ready).
    pub readiness_threshold: u64,
}

/// Initialize a Notification in donated physical memory.
///
/// # Safety
/// `base_paddr` must be a donated page.
pub unsafe fn create_notification(base_paddr: PhysAddr) -> Result<(), CreateError> {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let obj_va = (base_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut NotificationObject;
    ptr::write(obj_va, NotificationObject {
        header: ObjectHeader {
            obj_type: ObjectType::Notification,
            page_count: 1,
        },
        state: NotificationState::new(),
        blocked_tcb_paddr: 0,
        readiness_waiter_paddr: 0,
        readiness_threshold: 0,
    });
    Ok(())
}

/// Signal a notification with a new timeline value.
/// If a waiter's threshold is met, unblocks it.
///
/// # Safety
/// `notif_paddr` must be a valid NotificationObject.
pub unsafe fn notification_signal(
    notif_paddr: PhysAddr,
    new_value: u64,
) -> Result<(), NotificationError> {
    let obj = obj_ptr_mut(notif_paddr);

    match (*obj).state.signal(new_value)? {
        SignalResult::Updated => {}
        SignalResult::WakeWaiter => {
            let waiter = PhysAddr::new((*obj).blocked_tcb_paddr);
            (*obj).blocked_tcb_paddr = 0;
            scheduler::unblock_thread(waiter);
        }
    }

    // Check readiness waiter (registered via sys_wait_any).
    // Wake if the new value meets the readiness threshold.
    if (*obj).readiness_waiter_paddr != 0
        && lockjaw_types::wait::is_notification_ready(new_value, (*obj).readiness_threshold)
    {
        scheduler::unblock_thread(PhysAddr::new((*obj).readiness_waiter_paddr));
        (*obj).readiness_waiter_paddr = 0;
        (*obj).readiness_threshold = 0;
    }

    Ok(())
}

/// Wait on a notification until the timeline value reaches the threshold.
/// Returns immediately if the counter is already >= threshold.
/// Otherwise blocks the calling thread.
///
/// # Safety
/// `notif_paddr` must be a valid NotificationObject.
/// `caller_tcb_paddr` must be the calling thread's TCB.
pub unsafe fn notification_wait(
    notif_paddr: PhysAddr,
    threshold: u64,
    caller_tcb_paddr: PhysAddr,
) -> Result<u64, NotificationError> {
    let obj = obj_ptr_mut(notif_paddr);

    match (*obj).state.wait(threshold)? {
        WaitResult::Ready => {
            // Counter already past threshold — return current value
            Ok((*obj).state.value)
        }
        WaitResult::Block => {
            // Block until signaled
            (*obj).blocked_tcb_paddr = caller_tcb_paddr.as_u64();

            // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
            let tcb = (caller_tcb_paddr.as_u64() + KERNEL_VA_OFFSET)
                as *mut crate::sched::tcb::Tcb;
            (*tcb).ipc_blocked_on = notif_paddr.as_u64();

            scheduler::block_current();

            // When we wake up, return the current value
            let obj = obj_ptr(notif_paddr);
            Ok((*obj).state.value)
        }
    }
}

// ---------------------------------------------------------------------------
// Readiness helpers for sys_wait_any
// ---------------------------------------------------------------------------

/// Read the current timeline value from this notification.
/// Safe because PhysAddr is a trusted kernel address.
pub fn read_value(notif_paddr: PhysAddr) -> u64 {
    unsafe {
        let obj = obj_ptr(notif_paddr);
        (*obj).state.value
    }
}

/// Register a thread as a readiness waiter on this notification.
/// The thread will be woken when the value reaches the threshold.
pub unsafe fn set_readiness_waiter(notif_paddr: PhysAddr, waiter_paddr: PhysAddr, threshold: u64) {
    let obj = obj_ptr_mut(notif_paddr);
    (*obj).readiness_waiter_paddr = waiter_paddr.as_u64();
    (*obj).readiness_threshold = threshold;
}

/// Clear the readiness waiter registration on this notification.
pub unsafe fn clear_readiness_waiter(notif_paddr: PhysAddr) {
    let obj = obj_ptr_mut(notif_paddr);
    (*obj).readiness_waiter_paddr = 0;
    (*obj).readiness_threshold = 0;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

unsafe fn obj_ptr(paddr: PhysAddr) -> *const NotificationObject {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *const NotificationObject
}

unsafe fn obj_ptr_mut(paddr: PhysAddr) -> *mut NotificationObject {
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    (paddr.as_u64() + KERNEL_VA_OFFSET) as *mut NotificationObject
}
