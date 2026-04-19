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
    /// Thread has exited. Permanently removed from scheduling — never
    /// selected by select_next, never unblocked. The scheduler slot is
    /// cleared to None by remove_thread after cleanup completes.
    Exited,
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
    /// Current thread is exiting. Transitions Running -> Exited and
    /// selects the next thread. The exiting thread is never scheduled
    /// again. The kernel uses the returned exited index for teardown.
    Exit,
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
    /// Thread exited and a new thread was selected. The kernel must:
    /// 1. Store `exited` index + TCB paddr for deferred cleanup
    /// 2. Context-switch to `next`
    /// The exited thread is already marked Exited and will never run again.
    ExitAndSwitch { exited: usize, next: usize },
    /// Thread exited but no other thread is Ready. The kernel must store
    /// the exited index for cleanup and halt (wfi) until an interrupt
    /// wakes a thread.
    ExitAndHalt { exited: usize },
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
                return match reason {
                    SchedReason::Exit => SchedDecision::ExitAndSwitch { exited: current, next },
                    _ => SchedDecision::SwitchTo(next),
                };
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
        SchedReason::Exit => SchedDecision::ExitAndHalt { exited: current },
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
    /// Rejects Exited threads — once exited, a thread cannot be woken.
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

    /// Remove an Exited thread from the scheduler, freeing its slot for
    /// reuse by a future `add_thread`. Only valid for Exited threads
    /// that are not the current thread (the kernel calls this from the
    /// next thread's context after cleanup).
    pub fn remove_thread(&mut self, idx: usize) -> Result<(), SchedError> {
        if idx == self.current {
            return Err(SchedError::InvalidTransition);
        }
        match self.get(idx) {
            Some(SchedThreadState::Exited) => {
                self.states[idx] = None;
                Ok(())
            }
            Some(_) => Err(SchedError::InvalidTransition),
            None => Err(SchedError::InvalidIndex),
        }
    }

    /// Atomic scheduler step: compute the decision from the current state
    /// and the reason, update the state accordingly, and return what the
    /// kernel must do. This is the ONLY public way to transition the
    /// scheduler between thread-switch points — `decide` and
    /// `apply_decision` are not exposed so the kernel cannot call them
    /// out of order or with forged arguments.
    ///
    /// Preconditions enforced for `reason`:
    /// - `Preempt`: current must be Running (timer tick / yield)
    /// - `Block`: current must be Blocked (caller already called
    ///   `block_current`)
    ///
    /// Returns a SchedDecision describing the action taken. The model
    /// state has already been updated to reflect the decision.
    pub fn step(&mut self, reason: SchedReason) -> SchedDecision {
        // Validate entry preconditions for the reason.
        match reason {
            SchedReason::Preempt => {
                assert!(
                    self.get(self.current) == Some(SchedThreadState::Running),
                    "step(Preempt) requires current thread to be Running"
                );
            }
            SchedReason::Block => {
                let cur = self.get(self.current);
                assert!(
                    cur == Some(SchedThreadState::Blocked) || cur == Some(SchedThreadState::Exited),
                    "step(Block) requires current thread to be Blocked or Exited"
                );
            }
            SchedReason::Exit => {
                assert!(
                    self.get(self.current) == Some(SchedThreadState::Running),
                    "step(Exit) requires current thread to be Running"
                );
            }
        }

        let decision = self.decide(reason);
        self.apply_decision(reason, decision)
            .expect("internal: decide() produced a decision rejected by apply_decision");

        // Post-condition: invariants must hold.
        debug_assert!(self.check_invariants(),
            "scheduler invariants violated after step");

        decision
    }

    /// Apply a scheduling decision, updating states accordingly.
    /// Private — callers use `step` instead.
    ///
    /// For SwitchTo(idx): transitions old current (if Running) to Ready,
    /// transitions idx from Ready to Running, updates current.
    /// For StayOnCurrent: validates current is Running, reason is Preempt,
    /// no other thread is Ready.
    /// For WaitForInterrupt: validates no thread is Ready.
    fn apply_decision(
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
                if reason == SchedReason::Preempt {
                    if self.get(self.current) != Some(SchedThreadState::Running) {
                        return Err(SchedError::InvalidTransition);
                    }
                    self.states[self.current] = Some(SchedThreadState::Ready);
                } else {
                    // Block: current must already be Blocked or Exited.
                    let cur = self.get(self.current);
                    if cur != Some(SchedThreadState::Blocked) && cur != Some(SchedThreadState::Exited) {
                        return Err(SchedError::InvalidTransition);
                    }
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
                // No other thread may be Ready (StayOnCurrent must only
                // be chosen when there's no one to switch to).
                for (i, s) in self.states.iter().enumerate() {
                    if i != self.current && *s == Some(SchedThreadState::Ready) {
                        return Err(SchedError::InvalidTransition);
                    }
                }
                Ok(())
            }
            SchedDecision::WaitForInterrupt => {
                // No thread may be Ready (WaitForInterrupt must only be
                // chosen when the CPU genuinely has nothing to run).
                for s in self.states.iter() {
                    if *s == Some(SchedThreadState::Ready) {
                        return Err(SchedError::InvalidTransition);
                    }
                }
                Ok(())
            }
            SchedDecision::ExitAndSwitch { exited, next } => {
                if reason != SchedReason::Exit {
                    return Err(SchedError::InvalidTransition);
                }
                if self.get(exited) != Some(SchedThreadState::Running) {
                    return Err(SchedError::InvalidTransition);
                }
                if self.get(next) != Some(SchedThreadState::Ready) {
                    return Err(SchedError::InvalidTransition);
                }
                self.states[exited] = Some(SchedThreadState::Exited);
                self.states[next] = Some(SchedThreadState::Running);
                self.current = next;
                Ok(())
            }
            SchedDecision::ExitAndHalt { exited } => {
                if reason != SchedReason::Exit {
                    return Err(SchedError::InvalidTransition);
                }
                if self.get(exited) != Some(SchedThreadState::Running) {
                    return Err(SchedError::InvalidTransition);
                }
                self.states[exited] = Some(SchedThreadState::Exited);
                // No thread to switch to. current stays pointing at the
                // exited thread (the kernel will wfi until an interrupt).
                Ok(())
            }
        }
    }

    /// Invariants of a valid scheduler state:
    /// - `current < MAX_THREADS_MODEL` (in bounds)
    /// - `states[current]` is registered (Some) — may be Exited after
    ///   ExitAndHalt when no other thread is Ready
    /// - At most one thread is Running
    /// - If any thread is Running, it is the current thread
    pub fn check_invariants(&self) -> bool {
        if self.current >= MAX_THREADS_MODEL {
            return false;
        }
        if self.states[self.current].is_none() {
            return false;
        }
        let mut running_count = 0;
        let mut running_idx = None;
        for (i, s) in self.states.iter().enumerate() {
            if *s == Some(SchedThreadState::Running) {
                running_count += 1;
                running_idx = Some(i);
            }
        }
        if running_count > 1 {
            return false;
        }
        if let Some(idx) = running_idx {
            if idx != self.current {
                return false;
            }
        }
        true
    }

    /// Compute the scheduling decision for a given reason without mutating.
    /// Private — callers use `step` instead, which computes + applies atomically.
    fn decide(&self, reason: SchedReason) -> SchedDecision {
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

    // --- Atomic step() API ---

    #[test]
    fn step_preempt_switches_to_ready() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        let d = s.step(SchedReason::Preempt);
        assert_eq!(d, SchedDecision::SwitchTo(1));
        assert_eq!(s.get(0), Some(SchedThreadState::Ready));
        assert_eq!(s.get(1), Some(SchedThreadState::Running));
        assert_eq!(s.current, 1);
    }

    #[test]
    fn step_preempt_stays_when_alone_running() {
        let mut s = SchedState::new();
        let d = s.step(SchedReason::Preempt);
        assert_eq!(d, SchedDecision::StayOnCurrent);
        assert_eq!(s.get(0), Some(SchedThreadState::Running));
    }

    #[test]
    fn step_block_switches_to_ready() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        s.block_current().unwrap();
        let d = s.step(SchedReason::Block);
        assert_eq!(d, SchedDecision::SwitchTo(1));
        assert_eq!(s.get(0), Some(SchedThreadState::Blocked));
        assert_eq!(s.get(1), Some(SchedThreadState::Running));
        assert_eq!(s.current, 1);
    }

    #[test]
    fn step_block_waits_when_no_ready() {
        let mut s = SchedState::new();
        s.block_current().unwrap();
        let d = s.step(SchedReason::Block);
        assert_eq!(d, SchedDecision::WaitForInterrupt);
        // State unchanged
        assert_eq!(s.get(0), Some(SchedThreadState::Blocked));
    }

    #[test]
    #[should_panic(expected = "step(Preempt) requires current thread to be Running")]
    fn step_preempt_panics_if_current_not_running() {
        let mut s = SchedState::new();
        s.block_current().unwrap();
        // current is Blocked, Preempt precondition violated
        let _ = s.step(SchedReason::Preempt);
    }

    #[test]
    #[should_panic(expected = "step(Block) requires current thread to be Blocked")]
    fn step_block_panics_if_current_not_blocked() {
        let mut s = SchedState::new();
        // current is Running, Block precondition violated
        let _ = s.step(SchedReason::Block);
    }

    #[test]
    fn timer_tick_while_idle_is_safe() {
        // Regression: when current is Blocked (idling in block_current's
        // wfi loop) and a timer IRQ fires, the kernel's tick() must
        // guard — it cannot call step(Preempt) because the precondition
        // requires current=Running. The kernel returns early; the model
        // is unchanged.
        let mut s = SchedState::new();
        s.block_current().unwrap();
        // Verify: current is Blocked
        assert_eq!(s.get(s.current), Some(SchedThreadState::Blocked));
        // Verify: calling step(Preempt) in this state would panic
        // (we don't actually call it; the kernel's tick() guard prevents it)
        // But check_invariants still holds (even with Blocked current).
        assert!(s.check_invariants());
    }

    // --- Strengthened check_invariants ---

    #[test]
    fn invariants_reject_running_not_current() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        // Two Running threads at once is invalid
        s.states[1] = Some(SchedThreadState::Running);
        assert!(!s.check_invariants());
    }

    #[test]
    fn invariants_reject_running_is_not_current() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        // current=0 Ready, thread 1 Running — Running is not current
        s.states[0] = Some(SchedThreadState::Ready);
        s.states[1] = Some(SchedThreadState::Running);
        // current still 0
        assert!(!s.check_invariants());
    }

    #[test]
    fn invariants_reject_current_not_registered() {
        let mut s = SchedState::new();
        s.current = 5; // points at unregistered slot
        assert!(!s.check_invariants());
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
                // Follow up with step(Block) which must complete
                let _ = s2.step(SchedReason::Block);
                out.push(("block".into(), s2));
            }
        }

        // Event 3: timer IRQ (matches kernel tick() semantics). If
        // current is Running, this is a Preempt. If current is Blocked
        // (idle loop inside block_current), the kernel's tick() guards
        // and returns without stepping — state is unchanged. This
        // models the real-world interrupt-while-idling case.
        {
            let mut s2 = s.clone();
            if s.get(s.current) == Some(SchedThreadState::Running) {
                let _ = s2.step(SchedReason::Preempt);
            }
            // Otherwise no-op (kernel's tick() returns early)
            out.push(("timer_irq".into(), s2));
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

        // Event 5: current thread exits (if Running and not the only thread)
        if s.get(s.current) == Some(SchedThreadState::Running)
            && s.thread_count() >= 2
        {
            let mut s2 = s.clone();
            let decision = s2.step(SchedReason::Exit);
            match decision {
                SchedDecision::ExitAndSwitch { exited, .. } => {
                    // Also test remove_thread cleanup
                    let mut s3 = s2.clone();
                    let _ = s3.remove_thread(exited);
                    out.push(("exit+remove".into(), s3));
                    out.push(("exit".into(), s2));
                }
                SchedDecision::ExitAndHalt { .. } => {
                    // Cannot remove: exited thread is still current
                    // (no other thread to switch to). Cleanup deferred
                    // until an interrupt wakes another thread.
                    out.push(("exit_halt".into(), s2));
                }
                _ => {
                    out.push(("exit".into(), s2));
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

    // --- Exit tests ---

    #[test]
    fn step_exit_switches_to_ready() {
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // idx 1 Ready
        let d = s.step(SchedReason::Exit);
        assert_eq!(d, SchedDecision::ExitAndSwitch { exited: 0, next: 1 });
        assert_eq!(s.get(0), Some(SchedThreadState::Exited));
        assert_eq!(s.get(1), Some(SchedThreadState::Running));
        assert_eq!(s.current, 1);
        assert!(s.check_invariants());
    }

    #[test]
    fn step_exit_halts_when_alone() {
        let mut s = SchedState::new();
        let d = s.step(SchedReason::Exit);
        assert_eq!(d, SchedDecision::ExitAndHalt { exited: 0 });
        assert_eq!(s.get(0), Some(SchedThreadState::Exited));
        assert!(s.check_invariants());
    }

    #[test]
    fn exited_thread_never_selected() {
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // idx 1 Ready
        s.step(SchedReason::Exit); // exit thread 0, switch to 1
        // Now thread 0 is Exited, thread 1 is Running
        // Preempt should stay on current (no Ready threads)
        let d = s.step(SchedReason::Preempt);
        assert_eq!(d, SchedDecision::StayOnCurrent);
    }

    #[test]
    fn unblock_rejects_exited() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        s.step(SchedReason::Exit); // thread 0 Exited
        assert_eq!(s.unblock(0), Err(SchedError::InvalidTransition));
    }

    #[test]
    fn remove_thread_on_exited_frees_slot() {
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // idx 1
        s.step(SchedReason::Exit); // exit thread 0
        assert!(s.remove_thread(0).is_ok());
        assert_eq!(s.get(0), None);
        // Slot is reusable
        let idx = s.add_thread().unwrap();
        assert_eq!(idx, 0);
        assert_eq!(s.get(0), Some(SchedThreadState::Ready));
    }

    #[test]
    fn remove_thread_rejects_non_exited() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        assert_eq!(s.remove_thread(1), Err(SchedError::InvalidTransition));
    }

    #[test]
    fn remove_thread_rejects_current() {
        // ExitAndHalt leaves the exited thread as current. remove_thread
        // must reject it — cleanup can only happen after switching to a
        // different thread.
        let mut s = SchedState::new();
        s.step(SchedReason::Exit); // ExitAndHalt, current=0, states[0]=Exited
        assert_eq!(s.remove_thread(0), Err(SchedError::InvalidTransition));
    }

    #[test]
    fn exit_and_halt_then_unblock_schedules() {
        // Regression: ExitAndHalt with blocked threads must not deadlock.
        // After exit, an unblock + step(Block) should switch to the
        // newly-Ready thread. Cleanup (remove_thread) must only succeed
        // AFTER current has changed away from the exited thread.
        let mut s = SchedState::new();
        let idx1 = s.add_thread().unwrap(); // idx 1 Ready
        s.states[idx1] = Some(SchedThreadState::Blocked); // force blocked

        // Thread 0 exits — no Ready thread, so ExitAndHalt
        let d = s.step(SchedReason::Exit);
        assert_eq!(d, SchedDecision::ExitAndHalt { exited: 0 });
        assert_eq!(s.get(0), Some(SchedThreadState::Exited));
        assert_eq!(s.current, 0); // still current — can't remove yet

        // remove_thread must fail while exited thread is still current
        assert_eq!(s.remove_thread(0), Err(SchedError::InvalidTransition));

        // Simulate interrupt unblocking thread 1
        s.unblock(1).unwrap();

        // step(Block) with Exited current should find Ready thread 1
        let d2 = s.step(SchedReason::Block);
        assert_eq!(d2, SchedDecision::SwitchTo(1));
        assert_eq!(s.current, 1); // current changed — now remove works

        // NOW remove_thread succeeds (exited thread is no longer current)
        assert!(s.remove_thread(0).is_ok());
        assert_eq!(s.get(0), None);
        assert!(s.check_invariants());
    }

    #[test]
    #[should_panic(expected = "step(Exit) requires current thread to be Running")]
    fn step_exit_panics_if_current_not_running() {
        let mut s = SchedState::new();
        s.block_current().unwrap();
        let _ = s.step(SchedReason::Exit);
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
        let before = s.clone();
        let d = s.step(SchedReason::Block);
        assert_eq!(d, SchedDecision::WaitForInterrupt);
        assert_eq!(s, before, "WaitForInterrupt must not mutate state");
    }
}
