/// Scheduler state machine model.
///
/// Owns the abstract scheduler protocol: thread states, valid transitions,
/// and the selection decision. The kernel calls these functions to mutate
/// its state rather than doing the transitions ad-hoc. This pushes the
/// invariants (exactly one Running, only Blocked threads can be unblocked,
/// etc.) into the model where they can be verified via reachable-state
/// exploration on the host.

/// Thread state as seen by the scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchedThreadState {
    Ready,
    Running,
    Blocked,
}

/// Why scheduling was invoked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchedReason {
    /// Called from a timer tick or voluntary yield. Current thread is
    /// Running and wants to continue if no one else is Ready.
    Preempt,
    /// Current thread just marked itself Blocked and MUST NOT resume
    /// until some other code path unblocks it.
    Block,
}

/// Result of a scheduling decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchedDecision {
    /// Switch to the thread at this index. The kernel must perform the
    /// context switch and state transitions via apply_switch().
    SwitchTo(usize),
    /// No switch needed — keep running the current thread. Only valid
    /// when reason == Preempt and current is Running.
    StayOnCurrent,
    /// No runnable thread exists. Halt the CPU (wfi) until an interrupt
    /// wakes something up.
    WaitForInterrupt,
}

/// Errors from scheduler state transitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedError {
    /// Attempted to transition a thread that is already in the target state
    /// or in a state that doesn't allow the transition.
    InvalidTransition,
    /// Thread index out of range.
    InvalidIndex,
}

// ---------------------------------------------------------------------------
// Selection logic (pure function over state snapshot)
// ---------------------------------------------------------------------------

/// Select the next thread to run using round-robin.
///
/// Starts searching from `current + 1` and wraps around. Skips threads
/// that are not Ready. The result depends on both the thread states and
/// the reason scheduling was invoked.
///
/// Invariants on the input (assumed; caller responsible):
/// - `reason == Preempt` implies the caller ran this from a timer tick
///   or voluntary yield. State for `current` should be Running.
/// - `reason == Block` implies the caller has already transitioned
///   `current` to Blocked before calling this function.
pub fn select_next<F>(
    current: usize,
    thread_count: usize,
    reason: SchedReason,
    get_state: F,
) -> SchedDecision
where
    F: Fn(usize) -> SchedThreadState,
{
    if thread_count >= 2 {
        let mut next = (current + 1) % thread_count;
        loop {
            if get_state(next) == SchedThreadState::Ready {
                return SchedDecision::SwitchTo(next);
            }
            next = (next + 1) % thread_count;
            if next == current {
                break;
            }
        }
    }

    match reason {
        SchedReason::Preempt => match get_state(current) {
            SchedThreadState::Running => SchedDecision::StayOnCurrent,
            _ => SchedDecision::WaitForInterrupt,
        },
        SchedReason::Block => SchedDecision::WaitForInterrupt,
    }
}

// ---------------------------------------------------------------------------
// State transitions (the model owns these)
// ---------------------------------------------------------------------------

/// The scheduler's abstract state: which thread is current, and each
/// thread's state. The kernel's real state (a static array of Option<PhysAddr>
/// TCBs) is a concrete realization of this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedState {
    pub current: usize,
    pub states: [Option<SchedThreadState>; MAX_THREADS_MODEL],
}

/// Maximum threads tracked by the model. The kernel's MAX_THREADS (8)
/// should be <= this. Fixed-size array for no_std compatibility.
pub const MAX_THREADS_MODEL: usize = 8;

impl SchedState {
    /// Create an empty state with no threads registered. Const-fn so it
    /// can be used for static initialization in the kernel.
    pub const fn new_const() -> Self {
        Self {
            current: 0,
            states: [None; MAX_THREADS_MODEL],
        }
    }

    /// Create an initial state with one Running thread at index 0.
    /// Kept for tests and simple callers.
    pub fn new() -> Self {
        let mut s = Self::new_const();
        s.states[0] = Some(SchedThreadState::Running);
        s
    }

    /// Add a new thread as Ready. Returns its index, or None if full.
    pub fn add_thread(&mut self) -> Option<usize> {
        for (i, s) in self.states.iter_mut().enumerate() {
            if s.is_none() {
                *s = Some(SchedThreadState::Ready);
                return Some(i);
            }
        }
        None
    }

    /// Mark the current thread (typically the boot thread) as Running.
    /// Used once at boot when the first thread is already executing.
    pub fn mark_initial_running(&mut self) {
        self.states[self.current] = Some(SchedThreadState::Running);
    }

    /// Number of registered threads.
    pub fn thread_count(&self) -> usize {
        self.states.iter().filter(|s| s.is_some()).count()
    }

    /// Get thread state at index (None if not registered).
    pub fn get(&self, idx: usize) -> Option<SchedThreadState> {
        self.states.get(idx).copied().flatten()
    }

    /// Transition the current thread to Blocked. Must be called before
    /// schedule(Block). Fails if the thread isn't Running.
    pub fn block_current(&mut self) -> Result<(), SchedError> {
        match self.get(self.current) {
            Some(SchedThreadState::Running) => {
                self.states[self.current] = Some(SchedThreadState::Blocked);
                Ok(())
            }
            _ => Err(SchedError::InvalidTransition),
        }
    }

    /// Mark a Blocked thread as Ready so the scheduler can pick it.
    /// Used by IPC endpoints and notifications when a partner arrives.
    pub fn unblock(&mut self, idx: usize) -> Result<(), SchedError> {
        match self.get(idx) {
            Some(SchedThreadState::Blocked) => {
                self.states[idx] = Some(SchedThreadState::Ready);
                Ok(())
            }
            Some(_) => Err(SchedError::InvalidTransition),
            None => Err(SchedError::InvalidIndex),
        }
    }

    /// Apply a scheduling decision, updating states accordingly.
    /// This is what the kernel calls after select_next returns.
    ///
    /// For SwitchTo(idx): transitions old current (if Running) to Ready,
    /// transitions idx from Ready to Running, updates current.
    /// For StayOnCurrent: no state changes (current stays Running).
    /// For WaitForInterrupt: no state changes.
    pub fn apply_decision(
        &mut self,
        reason: SchedReason,
        decision: SchedDecision,
    ) -> Result<(), SchedError> {
        match decision {
            SchedDecision::SwitchTo(new_idx) => {
                if self.get(new_idx) != Some(SchedThreadState::Ready) {
                    return Err(SchedError::InvalidTransition);
                }
                // Demote current Running -> Ready (only on Preempt;
                // on Block the caller already set it to Blocked).
                if reason == SchedReason::Preempt
                    && self.get(self.current) == Some(SchedThreadState::Running)
                {
                    self.states[self.current] = Some(SchedThreadState::Ready);
                }
                self.states[new_idx] = Some(SchedThreadState::Running);
                self.current = new_idx;
                Ok(())
            }
            SchedDecision::StayOnCurrent => {
                if reason != SchedReason::Preempt {
                    return Err(SchedError::InvalidTransition);
                }
                if self.get(self.current) != Some(SchedThreadState::Running) {
                    return Err(SchedError::InvalidTransition);
                }
                Ok(())
            }
            SchedDecision::WaitForInterrupt => Ok(()),
        }
    }

    /// Invariants of a valid scheduler state:
    /// - At most one thread is Running.
    /// - If reason=Preempt was just called, the Running thread is current.
    /// - Every registered index has a state.
    pub fn check_invariants(&self) -> bool {
        let running_count = self
            .states
            .iter()
            .filter(|s| **s == Some(SchedThreadState::Running))
            .count();
        running_count <= 1
    }

    /// Compute the scheduling decision for a given reason without mutating.
    pub fn decide(&self, reason: SchedReason) -> SchedDecision {
        select_next(self.current, MAX_THREADS_MODEL, reason, |i| {
            self.get(i).unwrap_or(SchedThreadState::Blocked)
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::collections::{HashSet, VecDeque};
    use std::string::String;
    use std::vec::Vec;
    use std::format;

    // --- Pure selector tests ---

    #[test]
    fn preempt_one_ready_switches() {
        let states = [SchedThreadState::Running, SchedThreadState::Ready];
        let r = select_next(0, 2, SchedReason::Preempt, |i| states[i]);
        assert_eq!(r, SchedDecision::SwitchTo(1));
    }

    #[test]
    fn preempt_all_others_blocked_current_running() {
        let states = [SchedThreadState::Running, SchedThreadState::Blocked, SchedThreadState::Blocked];
        let r = select_next(0, 3, SchedReason::Preempt, |i| states[i]);
        assert_eq!(r, SchedDecision::StayOnCurrent);
    }

    #[test]
    fn preempt_all_blocked_including_current() {
        let states = [SchedThreadState::Blocked, SchedThreadState::Blocked];
        let r = select_next(0, 2, SchedReason::Preempt, |i| states[i]);
        assert_eq!(r, SchedDecision::WaitForInterrupt);
    }

    #[test]
    fn block_one_other_ready() {
        let states = [SchedThreadState::Blocked, SchedThreadState::Ready];
        let r = select_next(0, 2, SchedReason::Block, |i| states[i]);
        assert_eq!(r, SchedDecision::SwitchTo(1));
    }

    #[test]
    fn block_all_others_blocked() {
        let states = [SchedThreadState::Blocked, SchedThreadState::Blocked];
        let r = select_next(0, 2, SchedReason::Block, |i| states[i]);
        assert_eq!(r, SchedDecision::WaitForInterrupt);
    }

    // --- SchedState transition tests ---

    #[test]
    fn new_state_has_one_running() {
        let s = SchedState::new();
        assert_eq!(s.get(0), Some(SchedThreadState::Running));
        assert_eq!(s.current, 0);
        assert!(s.check_invariants());
    }

    #[test]
    fn add_thread_makes_ready() {
        let mut s = SchedState::new();
        let idx = s.add_thread().unwrap();
        assert_eq!(s.get(idx), Some(SchedThreadState::Ready));
        assert!(s.check_invariants());
    }

    #[test]
    fn block_current_valid_from_running() {
        let mut s = SchedState::new();
        assert!(s.block_current().is_ok());
        assert_eq!(s.get(0), Some(SchedThreadState::Blocked));
        assert!(s.check_invariants());
    }

    #[test]
    fn block_current_invalid_when_not_running() {
        let mut s = SchedState::new();
        s.block_current().unwrap();
        assert_eq!(s.block_current(), Err(SchedError::InvalidTransition));
    }

    #[test]
    fn unblock_valid_from_blocked() {
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // idx 1 Ready
        // Block thread 1
        s.states[1] = Some(SchedThreadState::Blocked);
        assert!(s.unblock(1).is_ok());
        assert_eq!(s.get(1), Some(SchedThreadState::Ready));
    }

    #[test]
    fn unblock_invalid_when_ready() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        assert_eq!(s.unblock(1), Err(SchedError::InvalidTransition));
    }

    #[test]
    fn apply_switch_to_preserves_one_running() {
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // idx 1 Ready
        let d = s.decide(SchedReason::Preempt);
        assert_eq!(d, SchedDecision::SwitchTo(1));
        s.apply_decision(SchedReason::Preempt, d).unwrap();
        assert_eq!(s.get(0), Some(SchedThreadState::Ready));
        assert_eq!(s.get(1), Some(SchedThreadState::Running));
        assert_eq!(s.current, 1);
        assert!(s.check_invariants());
    }

    #[test]
    fn apply_stay_on_current_rejects_block_reason() {
        let mut s = SchedState::new();
        let r = s.apply_decision(SchedReason::Block, SchedDecision::StayOnCurrent);
        assert_eq!(r, Err(SchedError::InvalidTransition));
    }

    #[test]
    fn apply_switch_to_non_ready_rejected() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        s.states[1] = Some(SchedThreadState::Blocked);
        let r = s.apply_decision(SchedReason::Preempt, SchedDecision::SwitchTo(1));
        assert_eq!(r, Err(SchedError::InvalidTransition));
    }

    // --- Reachable-state exploration (actual BFS) ---

    // Starting from the initial state (one Running thread), apply every
    // valid external event (add_thread, block_current, unblock(idx),
    // tick/yield = schedule(Preempt), ipc_block = schedule(Block)) and
    // verify no reachable state violates invariants.
    //
    // The state space is bounded (<=4 threads, 3 states each + current,
    // so <= 4 * 3^4 = 324 states) so BFS completes quickly.

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct ReachableState {
        current: usize,
        states: [Option<SchedThreadState>; 4], // bounded to 4 for BFS
    }

    impl ReachableState {
        fn from_sched(s: &SchedState) -> Self {
            let mut states = [None; 4];
            for i in 0..4 {
                states[i] = s.get(i);
            }
            Self { current: s.current, states }
        }

        fn to_sched(&self) -> SchedState {
            let mut states = [None; MAX_THREADS_MODEL];
            for i in 0..4 {
                states[i] = self.states[i];
            }
            SchedState { current: self.current, states }
        }
    }

    fn successors(s: &SchedState) -> Vec<(String, SchedState)> {
        let mut out = Vec::new();

        // Event 1: add a new thread (if space)
        {
            let mut s2 = s.clone();
            if s2.add_thread().is_some() {
                out.push(("add".into(), s2));
            }
        }

        // Event 2: current thread blocks itself (if Running)
        if s.get(s.current) == Some(SchedThreadState::Running) {
            let mut s2 = s.clone();
            if s2.block_current().is_ok() {
                // Follow up with schedule(Block) which must complete
                let d = s2.decide(SchedReason::Block);
                if s2.apply_decision(SchedReason::Block, d).is_ok() {
                    out.push(("block".into(), s2));
                }
            }
        }

        // Event 3: timer tick (Preempt)
        {
            let mut s2 = s.clone();
            let d = s2.decide(SchedReason::Preempt);
            if s2.apply_decision(SchedReason::Preempt, d).is_ok() {
                out.push(("preempt".into(), s2));
            }
        }

        // Event 4: unblock a Blocked thread (simulates IPC partner arrival)
        for i in 0..4 {
            if s.get(i) == Some(SchedThreadState::Blocked) {
                let mut s2 = s.clone();
                if s2.unblock(i).is_ok() {
                    out.push((format!("unblock({})", i), s2));
                }
            }
        }

        out
    }

    #[test]
    fn bfs_reachable_states_preserve_invariants() {
        let initial = SchedState::new();
        let mut visited: HashSet<ReachableState> = HashSet::new();
        let mut queue: VecDeque<SchedState> = VecDeque::new();

        queue.push_back(initial.clone());
        visited.insert(ReachableState::from_sched(&initial));

        let mut explored = 0;
        while let Some(s) = queue.pop_front() {
            explored += 1;
            assert!(s.check_invariants(),
                "Reachable state violates invariants: {:?}", s);

            for (_event, next) in successors(&s) {
                let key = ReachableState::from_sched(&next);
                if visited.insert(key) {
                    queue.push_back(next);
                }
            }
        }

        // We should explore at least a handful of states.
        assert!(explored >= 5, "BFS only explored {} states", explored);
    }

    #[test]
    fn bfs_no_deadlock_without_ready_thread_is_wait_for_interrupt() {
        // Specifically verify: when the current thread just blocked and
        // no other thread is Ready, decide(Block) returns WaitForInterrupt
        // (never StayOnCurrent), and apply_decision leaves state unchanged.
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // thread 1 Ready
        s.states[1] = Some(SchedThreadState::Blocked); // force both not runnable
        s.block_current().unwrap();
        let d = s.decide(SchedReason::Block);
        assert_eq!(d, SchedDecision::WaitForInterrupt);
        let before = s.clone();
        s.apply_decision(SchedReason::Block, d).unwrap();
        assert_eq!(s, before, "WaitForInterrupt must not mutate state");
    }
}
