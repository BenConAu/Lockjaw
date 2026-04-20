/// Process lifetime model.
///
/// Pure decision logic for process thread-count transitions.
/// The kernel calls these functions to determine lifecycle outcomes;
/// this pushes the invariants into testable code.

/// Outcome of a thread exiting from a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessLifecycle {
    /// Process has remaining threads. Contains the new count.
    ThreadsRemaining(u32),
    /// Last thread exited. Caller must free process resources.
    LastThread,
    /// Process is immortal (kernel process). Count is decremented
    /// but process resources are never freed. Contains the new count.
    Immortal(u32),
}

/// Pure decision: what happens when a thread in this process exits?
///
/// Always decrements the count. `immortal` means "do not free process
/// resources," not "do not update count." Panics if `thread_count == 0`
/// (precondition: at least one thread must exist to exit).
pub fn on_thread_exit(thread_count: u32, immortal: bool) -> ProcessLifecycle {
    assert!(thread_count > 0, "on_thread_exit: no threads to exit");
    let new_count = thread_count - 1;
    if immortal {
        return ProcessLifecycle::Immortal(new_count);
    }
    if new_count == 0 {
        ProcessLifecycle::LastThread
    } else {
        ProcessLifecycle::ThreadsRemaining(new_count)
    }
}

/// Pure increment for thread creation. Returns the new count.
/// Panics on overflow.
pub fn on_thread_create(thread_count: u32) -> u32 {
    thread_count.checked_add(1).expect("thread count overflow")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_thread_exits_normal_process() {
        assert_eq!(on_thread_exit(1, false), ProcessLifecycle::LastThread);
    }

    #[test]
    fn threads_remaining_after_exit() {
        assert_eq!(on_thread_exit(2, false), ProcessLifecycle::ThreadsRemaining(1));
        assert_eq!(on_thread_exit(5, false), ProcessLifecycle::ThreadsRemaining(4));
    }

    #[test]
    fn immortal_process_decrements_but_never_freed() {
        assert_eq!(on_thread_exit(1, true), ProcessLifecycle::Immortal(0));
        assert_eq!(on_thread_exit(2, true), ProcessLifecycle::Immortal(1));
    }

    #[test]
    #[should_panic(expected = "no threads to exit")]
    fn exit_with_zero_threads_panics() {
        on_thread_exit(0, false);
    }

    #[test]
    #[should_panic(expected = "no threads to exit")]
    fn exit_with_zero_threads_immortal_panics() {
        on_thread_exit(0, true);
    }

    #[test]
    fn thread_create_increments() {
        assert_eq!(on_thread_create(0), 1);
        assert_eq!(on_thread_create(1), 2);
        assert_eq!(on_thread_create(99), 100);
    }

    #[test]
    #[should_panic(expected = "thread count overflow")]
    fn thread_create_overflow_panics() {
        on_thread_create(u32::MAX);
    }
}
