/// Pure round-robin scheduler selection model.
///
/// Given the current thread index, the number of threads, and a function
/// to query each thread's state, selects the next Ready thread. The kernel
/// provides thread states by reading TCBs; the model just does the index
/// math and matching.

/// Thread state as seen by the scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedThreadState {
    Ready,
    Running,
    Blocked,
}

/// Result of a scheduling decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedDecision {
    /// Switch to the thread at this index.
    SwitchTo(usize),
    /// No other thread is ready — stay on the current one.
    StayOnCurrent,
}

/// Select the next thread to run using round-robin.
///
/// Starts searching from `current + 1` and wraps around. Skips threads
/// that are not Ready. If no Ready thread is found (all Blocked or only
/// the current thread is Ready), returns StayOnCurrent.
///
/// `thread_count` must be > 0. `current` must be < `thread_count`.
/// `get_state` returns the state of the thread at the given index.
pub fn select_next<F>(current: usize, thread_count: usize, get_state: F) -> SchedDecision
where
    F: Fn(usize) -> SchedThreadState,
{
    if thread_count < 2 {
        return SchedDecision::StayOnCurrent;
    }

    let mut next = (current + 1) % thread_count;
    loop {
        if get_state(next) == SchedThreadState::Ready {
            return SchedDecision::SwitchTo(next);
        }
        next = (next + 1) % thread_count;
        if next == current {
            return SchedDecision::StayOnCurrent;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_round_robin() {
        // A=Running, B=Ready → selects B
        let states = [SchedThreadState::Running, SchedThreadState::Ready];
        let result = select_next(0, 2, |i| states[i]);
        assert_eq!(result, SchedDecision::SwitchTo(1));
    }

    #[test]
    fn skip_blocked() {
        // A=Running, B=Blocked, C=Ready → selects C
        let states = [SchedThreadState::Running, SchedThreadState::Blocked, SchedThreadState::Ready];
        let result = select_next(0, 3, |i| states[i]);
        assert_eq!(result, SchedDecision::SwitchTo(2));
    }

    #[test]
    fn all_blocked_except_current() {
        // A=Running, B=Blocked, C=Blocked → stay on A
        let states = [SchedThreadState::Running, SchedThreadState::Blocked, SchedThreadState::Blocked];
        let result = select_next(0, 3, |i| states[i]);
        assert_eq!(result, SchedDecision::StayOnCurrent);
    }

    #[test]
    fn wrap_around() {
        // current=2, A=Ready, B=Blocked, C=Running → wraps to A
        let states = [SchedThreadState::Ready, SchedThreadState::Blocked, SchedThreadState::Running];
        let result = select_next(2, 3, |i| states[i]);
        assert_eq!(result, SchedDecision::SwitchTo(0));
    }

    #[test]
    fn single_thread() {
        let states = [SchedThreadState::Running];
        let result = select_next(0, 1, |i| states[i]);
        assert_eq!(result, SchedDecision::StayOnCurrent);
    }

    #[test]
    fn eight_threads_last_is_ready() {
        // current=0, threads 1-6 blocked, thread 7 ready
        let states = [
            SchedThreadState::Running,
            SchedThreadState::Blocked,
            SchedThreadState::Blocked,
            SchedThreadState::Blocked,
            SchedThreadState::Blocked,
            SchedThreadState::Blocked,
            SchedThreadState::Blocked,
            SchedThreadState::Ready,
        ];
        let result = select_next(0, 8, |i| states[i]);
        assert_eq!(result, SchedDecision::SwitchTo(7));
    }
}
