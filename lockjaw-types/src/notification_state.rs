/// Pure notification state machine model (Vulkan-style timeline semaphore).
///
/// A notification has a monotonically increasing u64 counter.
/// signal(value) sets the counter (must be > current).
/// wait(threshold) blocks until counter >= threshold.

/// The state of a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationState {
    /// Current timeline value (monotonically increasing).
    pub value: u64,
    /// Whether a thread is blocked waiting.
    pub has_waiter: bool,
    /// The value the waiter needs before it can proceed.
    pub wait_threshold: u64,
}

/// Result of a signal operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalResult {
    /// Counter updated, no waiter to wake.
    Updated,
    /// Counter updated, waiter's threshold met — wake it.
    WakeWaiter,
}

/// Result of a wait operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitResult {
    /// Counter already >= threshold, return immediately.
    Ready,
    /// Counter < threshold, caller must block.
    Block,
}

/// Errors from notification operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationError {
    /// Signal value is not greater than current counter.
    ValueNotMonotonic,
    /// A thread is already waiting (single-waiter limit).
    AlreadyHasWaiter,
}

impl NotificationState {
    /// Create a new notification with counter at 0, no waiter.
    pub const fn new() -> Self {
        Self {
            value: 0,
            has_waiter: false,
            wait_threshold: 0,
        }
    }

    /// Signal the notification with a new value.
    /// Returns the action to take, or an error if the value isn't monotonic.
    pub fn signal(&mut self, new_value: u64) -> Result<SignalResult, NotificationError> {
        if new_value <= self.value {
            return Err(NotificationError::ValueNotMonotonic);
        }

        self.value = new_value;

        if self.has_waiter && self.value >= self.wait_threshold {
            self.has_waiter = false;
            self.wait_threshold = 0;
            Ok(SignalResult::WakeWaiter)
        } else {
            Ok(SignalResult::Updated)
        }
    }

    /// Check if a wait would block or return immediately.
    /// If it blocks, records the waiter state.
    pub fn wait(&mut self, threshold: u64) -> Result<WaitResult, NotificationError> {
        if self.value >= threshold {
            return Ok(WaitResult::Ready);
        }

        if self.has_waiter {
            return Err(NotificationError::AlreadyHasWaiter);
        }

        self.has_waiter = true;
        self.wait_threshold = threshold;
        Ok(WaitResult::Block)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_notification_starts_at_zero() {
        let n = NotificationState::new();
        assert_eq!(n.value, 0);
        assert!(!n.has_waiter);
    }

    #[test]
    fn signal_updates_counter() {
        let mut n = NotificationState::new();
        assert_eq!(n.signal(1), Ok(SignalResult::Updated));
        assert_eq!(n.value, 1);
        assert_eq!(n.signal(5), Ok(SignalResult::Updated));
        assert_eq!(n.value, 5);
    }

    #[test]
    fn signal_not_monotonic_fails() {
        let mut n = NotificationState::new();
        n.signal(10).unwrap();
        assert_eq!(n.signal(5), Err(NotificationError::ValueNotMonotonic));
        assert_eq!(n.signal(10), Err(NotificationError::ValueNotMonotonic));
    }

    #[test]
    fn wait_ready_immediately() {
        let mut n = NotificationState::new();
        n.signal(5).unwrap();
        assert_eq!(n.wait(3), Ok(WaitResult::Ready));
        assert_eq!(n.wait(5), Ok(WaitResult::Ready));
        assert!(!n.has_waiter);
    }

    #[test]
    fn wait_blocks_when_below_threshold() {
        let mut n = NotificationState::new();
        n.signal(3).unwrap();
        assert_eq!(n.wait(5), Ok(WaitResult::Block));
        assert!(n.has_waiter);
        assert_eq!(n.wait_threshold, 5);
    }

    #[test]
    fn signal_wakes_waiter_when_threshold_met() {
        let mut n = NotificationState::new();
        n.signal(3).unwrap();
        n.wait(5).unwrap(); // blocks
        assert!(n.has_waiter);

        assert_eq!(n.signal(5), Ok(SignalResult::WakeWaiter));
        assert!(!n.has_waiter);
    }

    #[test]
    fn signal_does_not_wake_if_threshold_not_met() {
        let mut n = NotificationState::new();
        n.signal(3).unwrap();
        n.wait(10).unwrap(); // blocks, needs value >= 10
        assert!(n.has_waiter);

        assert_eq!(n.signal(7), Ok(SignalResult::Updated));
        assert!(n.has_waiter); // still waiting
    }

    #[test]
    fn double_wait_fails() {
        let mut n = NotificationState::new();
        n.wait(1).unwrap(); // first wait blocks
        assert_eq!(n.wait(2), Err(NotificationError::AlreadyHasWaiter));
    }

    #[test]
    fn wait_zero_threshold_always_ready() {
        let mut n = NotificationState::new();
        assert_eq!(n.wait(0), Ok(WaitResult::Ready));
    }

    #[test]
    fn irq_delivery_pattern() {
        // Simulates how IRQ delivery works: kernel increments by 1,
        // driver waits for current+1
        let mut n = NotificationState::new();

        // First IRQ
        assert_eq!(n.signal(1), Ok(SignalResult::Updated)); // no waiter yet

        // Driver starts waiting for next IRQ
        assert_eq!(n.wait(1), Ok(WaitResult::Ready)); // already at 1
        assert_eq!(n.wait(2), Ok(WaitResult::Block));  // needs value 2

        // Second IRQ fires
        assert_eq!(n.signal(2), Ok(SignalResult::WakeWaiter));
        assert!(!n.has_waiter);

        // Driver waits for third
        assert_eq!(n.wait(3), Ok(WaitResult::Block));

        // Third IRQ
        assert_eq!(n.signal(3), Ok(SignalResult::WakeWaiter));
    }
}
