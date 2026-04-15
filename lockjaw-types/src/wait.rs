/// Types, validation, and readiness logic for sys_wait_any.
///
/// All logic here is pure — no pointers, no unsafe, testable on host.
/// The kernel reads object state and calls these functions to determine
/// readiness and validate syscall parameters.

use crate::ipc_state::EpState;

/// Maximum number of objects in a single sys_wait_any call.
pub const MAX_WAIT_OBJECTS: usize = 4;

/// End of the user virtual address range (first 1GB on AArch64).
/// Userspace pointers must be below this.
const USER_VA_END: u64 = 0x4000_0000;

/// A single entry in a sys_wait_any call.
/// Passed from userspace as an array of these structs.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WaitEntry {
    /// Handle index in the caller's handle table.
    pub handle: u64,
    /// Threshold for readiness. Meaning depends on object type:
    /// - Endpoint: ignored (always ready if sender/caller pending)
    /// - Notification: ready when value >= threshold
    pub threshold: u64,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate that a userspace buffer is entirely within the user VA range.
/// Returns false if the pointer is in kernel space, the size overflows,
/// or the buffer extends past USER_VA_END.
pub fn validate_user_buffer(ptr: u64, byte_count: u64) -> bool {
    match ptr.checked_add(byte_count) {
        Some(end) => ptr < USER_VA_END && end <= USER_VA_END,
        None => false, // overflow
    }
}

/// Validate that a wait entry count is within bounds.
pub fn validate_wait_count(count: usize) -> bool {
    count >= 1 && count <= MAX_WAIT_OBJECTS
}

// ---------------------------------------------------------------------------
// Readiness
// ---------------------------------------------------------------------------

/// Check whether an endpoint is ready for a receiver (has a pending sender or caller).
pub fn is_endpoint_ready(state: EpState) -> bool {
    matches!(state, EpState::HasSender | EpState::HasCaller)
}

/// Check whether a notification value meets a wait threshold.
pub fn is_notification_ready(current_value: u64, threshold: u64) -> bool {
    current_value >= threshold
}

/// Check whether an endpoint has a blocked caller (required for sys_export_handle).
/// The exporter can only export a handle into a caller that is waiting for a reply.
/// Note: after sys_receive on a HasCaller endpoint, the state transitions to Idle
/// but the caller remains blocked (caller_tcb_paddr is still set). The check must
/// use the caller paddr, not the endpoint state.
pub fn can_export_to_caller(caller_tcb_paddr: u64) -> bool {
    caller_tcb_paddr != 0
}

// ---------------------------------------------------------------------------
// Bitmask computation
// ---------------------------------------------------------------------------

/// The state of a single waitable object, as seen by the readiness check.
/// The kernel reads object memory and builds these; the pure function computes the mask.
#[derive(Clone, Copy, Debug)]
pub enum ObjectReadiness {
    /// Endpoint with its current IPC state.
    Endpoint(EpState),
    /// Notification with its current timeline value and the wait threshold.
    Notification { value: u64, threshold: u64 },
}

/// Compute the ready bitmask for a set of waitable objects.
/// Bit N is set if object N is ready. Pure function — no pointers, no unsafe.
pub fn compute_ready_mask(objects: &[ObjectReadiness]) -> u64 {
    let mut mask = 0u64;
    for (i, obj) in objects.iter().enumerate() {
        let ready = match obj {
            ObjectReadiness::Endpoint(state) => is_endpoint_ready(*state),
            ObjectReadiness::Notification { value, threshold } => is_notification_ready(*value, *threshold),
        };
        if ready {
            mask |= 1 << i;
        }
    }
    mask
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- WaitEntry ---

    #[test]
    fn wait_entry_size_is_16_bytes() {
        assert_eq!(core::mem::size_of::<WaitEntry>(), 16);
    }

    // --- validate_user_buffer ---

    #[test]
    fn user_buffer_valid_in_range() {
        assert!(validate_user_buffer(0x0040_0000, 4096));
    }

    #[test]
    fn user_buffer_valid_at_start() {
        assert!(validate_user_buffer(0, 100));
    }

    #[test]
    fn user_buffer_valid_at_boundary() {
        // Exactly fills up to USER_VA_END
        assert!(validate_user_buffer(0, USER_VA_END));
    }

    #[test]
    fn user_buffer_invalid_in_kernel_space() {
        assert!(!validate_user_buffer(0x4000_0000, 16));
    }

    #[test]
    fn user_buffer_invalid_crosses_boundary() {
        assert!(!validate_user_buffer(0x3FFF_FFF0, 32));
    }

    #[test]
    fn user_buffer_invalid_overflow() {
        assert!(!validate_user_buffer(u64::MAX - 10, 100));
    }

    #[test]
    fn user_buffer_zero_size_valid() {
        assert!(validate_user_buffer(0x1000, 0));
    }

    #[test]
    fn user_buffer_zero_size_invalid_kernel_ptr() {
        // Even zero-size buffer at kernel address is invalid
        assert!(!validate_user_buffer(0xFFFF_0000_0000_0000, 0));
    }

    // --- can_export_to_caller ---

    #[test]
    fn export_allowed_when_caller_present() {
        assert!(can_export_to_caller(0x4020_0000)); // any nonzero paddr
    }

    #[test]
    fn export_denied_when_no_caller() {
        assert!(!can_export_to_caller(0));
    }

    // --- validate_wait_count ---

    #[test]
    fn wait_count_valid_one() {
        assert!(validate_wait_count(1));
    }

    #[test]
    fn wait_count_valid_max() {
        assert!(validate_wait_count(MAX_WAIT_OBJECTS));
    }

    #[test]
    fn wait_count_invalid_zero() {
        assert!(!validate_wait_count(0));
    }

    #[test]
    fn wait_count_invalid_too_many() {
        assert!(!validate_wait_count(MAX_WAIT_OBJECTS + 1));
    }

    // --- is_endpoint_ready ---

    #[test]
    fn endpoint_ready_has_sender() {
        assert!(is_endpoint_ready(EpState::HasSender));
    }

    #[test]
    fn endpoint_ready_has_caller() {
        assert!(is_endpoint_ready(EpState::HasCaller));
    }

    #[test]
    fn endpoint_not_ready_idle() {
        assert!(!is_endpoint_ready(EpState::Idle));
    }

    #[test]
    fn endpoint_not_ready_has_receiver() {
        assert!(!is_endpoint_ready(EpState::HasReceiver));
    }

    // --- is_notification_ready ---

    #[test]
    fn notification_ready_at_threshold() {
        assert!(is_notification_ready(5, 5));
    }

    #[test]
    fn notification_ready_above_threshold() {
        assert!(is_notification_ready(10, 5));
    }

    #[test]
    fn notification_not_ready_below_threshold() {
        assert!(!is_notification_ready(4, 5));
    }

    #[test]
    fn notification_ready_at_zero() {
        assert!(is_notification_ready(0, 0));
    }

    #[test]
    fn notification_not_ready_zero_below_threshold() {
        assert!(!is_notification_ready(0, 1));
    }

    // --- compute_ready_mask: UART driver scenarios ---

    #[test]
    fn uart_driver_nothing_ready_blocks() {
        // Endpoint idle, notification below threshold → would block
        let objects = [
            ObjectReadiness::Endpoint(EpState::Idle),
            ObjectReadiness::Notification { value: 0, threshold: 1 },
        ];
        assert_eq!(compute_ready_mask(&objects), 0);
    }

    #[test]
    fn uart_driver_ipc_message_arrives() {
        // Sender waiting on endpoint, notification below threshold → endpoint ready
        let objects = [
            ObjectReadiness::Endpoint(EpState::HasSender),
            ObjectReadiness::Notification { value: 0, threshold: 1 },
        ];
        assert_eq!(compute_ready_mask(&objects), 0b01);
    }

    #[test]
    fn uart_driver_irq_fires() {
        // Endpoint idle, notification meets threshold → notification ready
        let objects = [
            ObjectReadiness::Endpoint(EpState::Idle),
            ObjectReadiness::Notification { value: 3, threshold: 3 },
        ];
        assert_eq!(compute_ready_mask(&objects), 0b10);
    }

    #[test]
    fn uart_driver_both_ready() {
        // Caller waiting + notification met → both ready
        let objects = [
            ObjectReadiness::Endpoint(EpState::HasCaller),
            ObjectReadiness::Notification { value: 5, threshold: 3 },
        ];
        assert_eq!(compute_ready_mask(&objects), 0b11);
    }

    #[test]
    fn uart_driver_notification_not_yet() {
        // Endpoint has a caller, notification one below threshold
        let objects = [
            ObjectReadiness::Endpoint(EpState::HasCaller),
            ObjectReadiness::Notification { value: 4, threshold: 5 },
        ];
        assert_eq!(compute_ready_mask(&objects), 0b01);
    }

    // --- compute_ready_mask: general scenarios ---

    #[test]
    fn single_endpoint_ready() {
        let objects = [ObjectReadiness::Endpoint(EpState::HasSender)];
        assert_eq!(compute_ready_mask(&objects), 0b1);
    }

    #[test]
    fn single_endpoint_not_ready() {
        let objects = [ObjectReadiness::Endpoint(EpState::HasReceiver)];
        assert_eq!(compute_ready_mask(&objects), 0);
    }

    #[test]
    fn four_objects_third_ready() {
        let objects = [
            ObjectReadiness::Endpoint(EpState::Idle),
            ObjectReadiness::Notification { value: 0, threshold: 1 },
            ObjectReadiness::Endpoint(EpState::HasSender),
            ObjectReadiness::Notification { value: 0, threshold: 1 },
        ];
        assert_eq!(compute_ready_mask(&objects), 0b0100);
    }

    #[test]
    fn four_objects_all_ready() {
        let objects = [
            ObjectReadiness::Endpoint(EpState::HasSender),
            ObjectReadiness::Notification { value: 10, threshold: 5 },
            ObjectReadiness::Endpoint(EpState::HasCaller),
            ObjectReadiness::Notification { value: 1, threshold: 0 },
        ];
        assert_eq!(compute_ready_mask(&objects), 0b1111);
    }

    #[test]
    fn empty_returns_zero() {
        assert_eq!(compute_ready_mask(&[]), 0);
    }
}
