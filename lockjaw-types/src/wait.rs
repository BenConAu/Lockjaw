/// Types and readiness logic for sys_wait_any.
///
/// sys_wait_any blocks until any of N waitable objects is ready.
/// Readiness is polymorphic per object type:
/// - Endpoint: ready when a sender/caller is blocked (HasSender or HasCaller state)
/// - Notification: ready when timeline value >= threshold

/// Maximum number of objects in a single sys_wait_any call.
pub const MAX_WAIT_OBJECTS: usize = 4;

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

/// Check whether a notification is ready given its current value and
/// the wait threshold.
pub fn is_notification_ready(current_value: u64, threshold: u64) -> bool {
    current_value >= threshold
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn notification_not_ready_zero_value_nonzero_threshold() {
        assert!(!is_notification_ready(0, 1));
    }

    #[test]
    fn wait_entry_size() {
        assert_eq!(core::mem::size_of::<WaitEntry>(), 16);
    }

    #[test]
    fn max_wait_objects_is_four() {
        assert_eq!(MAX_WAIT_OBJECTS, 4);
    }
}
