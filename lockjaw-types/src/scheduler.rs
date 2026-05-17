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

/// Selector for a CPU that has no current thread (idle CPU, no current
/// to demote / preempt). Returns the index of the first thread reported
/// as `Ready`, or `None` if no thread is runnable.
///
/// Unlike `select_next`, there is no `current` to rotate from; this scan
/// starts at 0 and goes monotonically. The caller is responsible for
/// the "Running on another CPU" predicate — typically by treating those
/// threads as `Blocked` in the closure (same trick `decide` uses with
/// `select_next`).
///
/// Used by the scheduler-refactor's `schedule_from_idle` path. Pure;
/// host-tested.
pub fn select_for_idle_cpu<F>(thread_count: usize, get_state: F) -> Option<usize>
where
    F: Fn(usize) -> SchedThreadState,
{
    for i in 0..thread_count {
        if get_state(i) == SchedThreadState::Ready {
            return Some(i);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// State transitions (the model owns these)
// ---------------------------------------------------------------------------

/// The scheduler's abstract state: per-CPU current thread and each
/// thread's state. The kernel's real state (a static array of Option<PhysAddr>
/// TCBs) is a concrete realization of this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedState {
    /// Per-CPU current thread index. `Some(idx)` means this CPU is running
    /// thread `idx`. `None` means the CPU has no thread assigned yet.
    pub current_per_cpu: [Option<usize>; MAX_CPUS],
    pub states: [Option<SchedThreadState>; MAX_THREADS],
}

/// Maximum number of threads the kernel can track simultaneously.
/// Single source of truth for both the pure scheduler model in this
/// module and the kernel's concrete `Scheduler.threads` slot table
/// (`src/sched/scheduler.rs`). The two used to be MAX_THREADS_MODEL
/// (here) and MAX_THREADS (kernel) with the same value — split was
/// historical, not architectural.
///
/// Sized at 1024: matches what aarch64 hardware can comfortably
/// schedule. Fixed-array storage cost: 1024 * 16 bytes ≈ 16 KB BSS
/// in the slot table + 1-2 KB BSS in the model's states array. TCB
/// content pages are allocated dynamically (1 page each via the KVM
/// pool); empty slots cost nothing.
pub const MAX_THREADS: usize = 1024;

/// Maximum number of CPUs the platform can have. Single source of
/// truth: kernel-side platform.rs and the pure scheduler model both
/// import from here.
pub const MAX_CPUS: usize = 4;

impl SchedState {
    /// Create an empty state with no threads registered. Const-fn so it
    /// can be used for static initialization in the kernel.
    pub const fn new_const() -> Self {
        Self {
            current_per_cpu: [None; MAX_CPUS],
            states: [None; MAX_THREADS],
        }
    }

    /// Create an initial state with one Running thread at index 0 on CPU 0.
    /// Kept for tests and simple callers.
    pub fn new() -> Self {
        let mut s = Self::new_const();
        s.states[0] = Some(SchedThreadState::Running);
        s.current_per_cpu[0] = Some(0);
        s
    }

    /// Return the current thread index for a CPU.
    ///
    /// **Precondition**: `current_per_cpu[cpu_id].is_some()`. This holds in:
    /// syscall handlers (caller's thread is current), IRQ handlers entered
    /// from a running thread (preempted thread is current), and any kernel
    /// path executing on a CPU it was scheduled onto. It does NOT hold for
    /// idle CPUs (no thread assigned) — those callers MUST use
    /// `try_current_for` and handle `None` (typically by routing to
    /// `step_from_idle` or a no-op return).
    ///
    /// Panics if the precondition is violated. Treat the panic as a contract
    /// violation, not a runtime condition.
    pub fn current_for(&self, cpu_id: usize) -> usize {
        self.current_per_cpu[cpu_id].unwrap_or_else(|| panic!("no current thread for this CPU"))
    }

    /// Return the current thread index for a CPU, or `None` if no thread
    /// is assigned. Used by code paths that may legitimately run on an
    /// idle CPU (currently: `schedule()` defensive panic message; the
    /// new `schedule_from_idle` path in the scheduler refactor).
    pub fn try_current_for(&self, cpu_id: usize) -> Option<usize> {
        self.current_per_cpu[cpu_id]
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

    /// Read-only precheck: true iff `add_thread` would currently
    /// succeed. Used by `sys_create_process` to verify the run queue
    /// has room before any destructive transfer work runs in the
    /// apply phase. Stable until the next `add_thread` (caller must
    /// hold the GKL across the precheck and the eventual add).
    pub fn has_free_slot(&self) -> bool {
        self.states.iter().any(|s| s.is_none())
    }

    /// Mark the current thread on `cpu_id` as Running. Used once at boot
    /// when the first thread is already executing, and for secondary CPUs
    /// when their idle thread is assigned.
    pub fn mark_initial_running(&mut self, cpu_id: usize) {
        let idx = self.current_for(cpu_id);
        self.states[idx] = Some(SchedThreadState::Running);
    }

    /// Number of registered threads.
    pub fn thread_count(&self) -> usize {
        self.states.iter().filter(|s| s.is_some()).count()
    }

    /// Get thread state at index (None if not registered).
    pub fn get(&self, idx: usize) -> Option<SchedThreadState> {
        self.states.get(idx).copied().flatten()
    }

    /// Transition the current thread on `cpu_id` to Blocked. Must be called
    /// before step(cpu_id, Block). Fails if the thread isn't Running.
    pub fn block_current(&mut self, cpu_id: usize) -> Result<(), SchedError> {
        let current = self.current_for(cpu_id);
        match self.get(current) {
            Some(SchedThreadState::Running) => {
                self.states[current] = Some(SchedThreadState::Blocked);
                Ok(())
            }
            _ => Err(SchedError::InvalidTransition),
        }
    }

    /// Mark a Blocked thread as ready to run. If the thread is current
    /// on some CPU (i.e., that CPU is parked in `block_current`'s wfi
    /// on this thread's stack), it transitions Blocked → **Running**
    /// directly — no context_switch needed, the CPU just returns from
    /// wfi onto its own stack. Otherwise transitions Blocked → **Ready**
    /// and the scheduler picks it on the next round.
    ///
    /// This enforces the invariant "`current_per_cpu[cpu]`'s state is
    /// never `Ready`" — see `check_invariants`. Without that invariant,
    /// `step(Block)` (which asserts current is Blocked or Exited)
    /// would panic if a self-wake left current in Ready state. Used
    /// by IPC endpoints, notifications, and the per-tick deadline scan.
    /// Rejects Exited threads — once exited, a thread cannot be woken.
    pub fn unblock(&mut self, idx: usize) -> Result<(), SchedError> {
        match self.get(idx) {
            Some(SchedThreadState::Blocked) => {
                let is_current = self.current_per_cpu.iter().any(|c| *c == Some(idx));
                self.states[idx] = Some(if is_current {
                    SchedThreadState::Running
                } else {
                    SchedThreadState::Ready
                });
                Ok(())
            }
            Some(_) => Err(SchedError::InvalidTransition),
            None => Err(SchedError::InvalidIndex),
        }
    }

    /// Remove an Exited thread from the scheduler, freeing its slot for
    /// reuse by a future `add_thread`. Only valid for Exited threads
    /// that are not current on ANY CPU.
    pub fn remove_thread(&mut self, idx: usize) -> Result<(), SchedError> {
        for cpu_current in &self.current_per_cpu {
            if *cpu_current == Some(idx) {
                return Err(SchedError::InvalidTransition);
            }
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
    pub fn step(&mut self, cpu_id: usize, reason: SchedReason) -> SchedDecision {
        let current = self.current_for(cpu_id);
        // Validate entry preconditions for the reason.
        match reason {
            SchedReason::Preempt => {
                assert!(
                    self.get(current) == Some(SchedThreadState::Running),
                    "step(Preempt) requires current thread to be Running"
                );
            }
            SchedReason::Block => {
                let cur = self.get(current);
                assert!(
                    cur == Some(SchedThreadState::Blocked) || cur == Some(SchedThreadState::Exited),
                    "step(Block) requires current thread to be Blocked or Exited"
                );
            }
            SchedReason::Exit => {
                assert!(
                    self.get(current) == Some(SchedThreadState::Running),
                    "step(Exit) requires current thread to be Running"
                );
            }
        }

        let decision = self.decide(cpu_id, reason);
        self.apply_decision(cpu_id, reason, decision)
            .unwrap_or_else(|_| panic!("internal: decide() produced a decision rejected by apply_decision"));

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
        cpu_id: usize,
        reason: SchedReason,
        decision: SchedDecision,
    ) -> Result<(), SchedError> {
        let current = self.current_for(cpu_id);
        match decision {
            SchedDecision::SwitchTo(new_idx) => {
                if self.get(new_idx) != Some(SchedThreadState::Ready) {
                    return Err(SchedError::InvalidTransition);
                }
                // Demote current Running -> Ready (only on Preempt;
                // on Block the caller already set it to Blocked).
                if reason == SchedReason::Preempt {
                    if self.get(current) != Some(SchedThreadState::Running) {
                        return Err(SchedError::InvalidTransition);
                    }
                    self.states[current] = Some(SchedThreadState::Ready);
                } else {
                    // Block: current must already be Blocked or Exited.
                    let cur = self.get(current);
                    if cur != Some(SchedThreadState::Blocked) && cur != Some(SchedThreadState::Exited) {
                        return Err(SchedError::InvalidTransition);
                    }
                }
                self.states[new_idx] = Some(SchedThreadState::Running);
                self.current_per_cpu[cpu_id] = Some(new_idx);
                Ok(())
            }
            SchedDecision::StayOnCurrent => {
                if reason != SchedReason::Preempt {
                    return Err(SchedError::InvalidTransition);
                }
                if self.get(current) != Some(SchedThreadState::Running) {
                    return Err(SchedError::InvalidTransition);
                }
                // No other thread may be Ready (StayOnCurrent must only
                // be chosen when there's no one to switch to).
                for (i, s) in self.states.iter().enumerate() {
                    if i != current && *s == Some(SchedThreadState::Ready) {
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
                self.current_per_cpu[cpu_id] = Some(next);
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
                // No thread to switch to. current_per_cpu stays pointing
                // at the exited thread until an interrupt wakes one.
                Ok(())
            }
        }
    }

    /// Invariants of a valid scheduler state:
    /// - For each active CPU (current_per_cpu[c] = Some(idx)):
    ///   - idx < MAX_THREADS (in bounds)
    ///   - states[idx] is registered (Some)
    /// - No two CPUs have the same current thread
    /// - Every Running thread is current on exactly one CPU
    /// - Running thread count == number of CPUs with Running current
    pub fn check_invariants(&self) -> bool {
        // Check each active CPU's current is valid
        for (c, cpu_current) in self.current_per_cpu.iter().enumerate() {
            if let Some(idx) = cpu_current {
                if *idx >= MAX_THREADS {
                    return false;
                }
                if self.states[*idx].is_none() {
                    return false;
                }
                // No two CPUs share the same current
                for (c2, other) in self.current_per_cpu.iter().enumerate() {
                    if c2 != c && *other == *cpu_current {
                        return false;
                    }
                }
            }
        }

        // Every Running thread must be current on exactly one CPU
        for (i, s) in self.states.iter().enumerate() {
            if *s == Some(SchedThreadState::Running) {
                let cpu_count = self.current_per_cpu.iter()
                    .filter(|c| **c == Some(i))
                    .count();
                if cpu_count != 1 {
                    return false;
                }
            }
        }

        // The thread that is current on a CPU must never be in state
        // Ready. Ready means "eligible to run, waiting for a CPU" —
        // but if you're already current on a CPU, you have one. The
        // legal current-states are Running (executing), Blocked (parked
        // in block_current's wfi on this CPU's stack), and Exited
        // (parked in exit_current's halt on this CPU's stack). `unblock`
        // upholds this by transitioning Blocked → Running for current
        // threads (not Blocked → Ready), so the scheduler's
        // `step(Block)` assert ("current must be Blocked or Exited")
        // cannot be tripped by an inline self-wake.
        for cpu_current in &self.current_per_cpu {
            if let Some(idx) = cpu_current {
                if self.states[*idx] == Some(SchedThreadState::Ready) {
                    return false;
                }
            }
        }

        true
    }

    /// Compute the scheduling decision for a given reason without mutating.
    /// Private — callers use `step` instead, which computes + applies atomically.
    fn decide(&self, cpu_id: usize, reason: SchedReason) -> SchedDecision {
        let current = self.current_for(cpu_id);
        select_next(current, MAX_THREADS, reason, |i| {
            let state = self.get(i).unwrap_or(SchedThreadState::Blocked);
            // A Ready thread that is Running on another CPU must be
            // skipped — it cannot be selected by this CPU.
            if state == SchedThreadState::Running && i != current {
                return SchedThreadState::Blocked; // treat as unavailable
            }
            state
        })
    }

    /// Assign a thread as the initial current for a CPU. Used during
    /// secondary CPU initialization — this is scheduler state initialization,
    /// not a normal scheduling decision. The thread must be Ready; it is
    /// transitioned to Running and set as current_per_cpu[cpu_id].
    ///
    /// Panics if the CPU already has a current thread or the thread is
    /// not Ready.
    pub fn set_initial_current(&mut self, cpu_id: usize, thread_idx: usize) {
        assert!(
            self.current_per_cpu[cpu_id].is_none(),
            "CPU {} already has a current thread", cpu_id
        );
        assert!(
            self.get(thread_idx) == Some(SchedThreadState::Ready),
            "thread {} is not Ready", thread_idx
        );
        self.states[thread_idx] = Some(SchedThreadState::Running);
        self.current_per_cpu[cpu_id] = Some(thread_idx);
    }

    /// Step the scheduler from an idle CPU (no current thread).
    ///
    /// Counterpart to `step` for the case where `current_per_cpu[cpu_id]`
    /// is `None`. Searches for a Ready thread not currently Running on any
    /// CPU; if found, transitions it to Running and assigns it as this
    /// CPU's current. Returns `Some(SwitchTo(idx))` for the caller to
    /// context-switch into, or `None` if no Ready thread exists (caller
    /// re-enters WFI).
    ///
    /// Asserts the CPU has no current on entry (use `step` if it does)
    /// and that `check_invariants()` holds on exit.
    ///
    /// The `Running`-on-another-CPU predicate is folded into the closure
    /// passed to `select_for_idle_cpu`: those threads are treated as
    /// unavailable (Blocked) for selection.
    pub fn step_from_idle(&mut self, cpu_id: usize) -> Option<SchedDecision> {
        assert!(
            self.current_per_cpu[cpu_id].is_none(),
            "step_from_idle: CPU {} already has a current thread", cpu_id
        );
        let picked = select_for_idle_cpu(MAX_THREADS, |i| {
            let state = self.get(i).unwrap_or(SchedThreadState::Blocked);
            // Hide threads that are Running on some other CPU — they
            // cannot be selected here. (We have no `current` to compare
            // against, so any Running thread is "on another CPU".)
            if state == SchedThreadState::Running {
                return SchedThreadState::Blocked;
            }
            state
        });
        let next_idx = match picked {
            Some(idx) => idx,
            None => return None,
        };
        self.states[next_idx] = Some(SchedThreadState::Running);
        self.current_per_cpu[cpu_id] = Some(next_idx);
        debug_assert!(self.check_invariants(),
            "scheduler invariants violated after step_from_idle");
        Some(SchedDecision::SwitchTo(next_idx))
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
        assert_eq!(s.current_for(0), 0);
        assert!(s.check_invariants());
    }

    #[test]
    fn try_current_for_none_when_unassigned() {
        // `new()` only assigns CPU 0. Other CPUs have no current thread
        // and `try_current_for` must observe that as `None` rather than
        // panicking — that's what the scheduler refactor relies on for
        // the idle-CPU path.
        let s = SchedState::new();
        assert_eq!(s.try_current_for(0), Some(0));
        assert_eq!(s.try_current_for(1), None);
        assert_eq!(s.try_current_for(2), None);
        assert_eq!(s.try_current_for(3), None);
    }

    #[test]
    fn try_current_for_some_after_set_initial_current() {
        // None -> Some(idx) transition via set_initial_current on a
        // CPU whose slot has not been pre-populated by SchedState::new
        // (CPU 1 here; CPU 0 already gets Some(0) from new()).
        let mut s = SchedState::new();
        let idx = s.add_thread().unwrap();
        s.set_initial_current(1, idx);
        assert_eq!(s.try_current_for(1), Some(idx));
    }

    // --- Idle-CPU selector tests (scheduler refactor Stage 2) ---

    #[test]
    fn select_for_idle_cpu_returns_first_ready() {
        // Linear scan from 0; first Ready wins. No rotation, no
        // current-skip — distinct from select_next.
        let states = [
            SchedThreadState::Blocked,
            SchedThreadState::Ready,
            SchedThreadState::Ready,
        ];
        assert_eq!(select_for_idle_cpu(3, |i| states[i]), Some(1));
    }

    #[test]
    fn select_for_idle_cpu_none_when_nothing_ready() {
        // All threads either Blocked or Running on other CPUs (caller's
        // closure maps those to Blocked). select returns None; caller
        // re-enters wfi.
        let states = [
            SchedThreadState::Blocked,
            SchedThreadState::Blocked,
            SchedThreadState::Blocked,
        ];
        assert_eq!(select_for_idle_cpu(3, |i| states[i]), None);
    }

    #[test]
    fn select_for_idle_cpu_skips_running_via_predicate() {
        // The selector itself doesn't filter Running — the caller
        // does, by mapping Running-on-another-CPU to Blocked in the
        // closure. Mirror that pattern here.
        let raw = [
            SchedThreadState::Running, // running on another CPU
            SchedThreadState::Ready,
        ];
        let picked = select_for_idle_cpu(2, |i| match raw[i] {
            SchedThreadState::Running => SchedThreadState::Blocked,
            other => other,
        });
        assert_eq!(picked, Some(1));
    }

    #[test]
    fn step_from_idle_transitions_to_running() {
        // Set up: CPU 0 has a Running thread (idx 0, from `new()`),
        // add a Ready thread (idx 1). CPU 1 has no current. step_from_idle
        // on CPU 1 picks idx 1 (the Ready one), transitions it to
        // Running, and assigns it as CPU 1's current.
        let mut s = SchedState::new();
        let idx = s.add_thread().unwrap();
        assert_eq!(s.try_current_for(1), None);
        let decision = s.step_from_idle(1);
        assert_eq!(decision, Some(SchedDecision::SwitchTo(idx)));
        assert_eq!(s.get(idx), Some(SchedThreadState::Running));
        assert_eq!(s.try_current_for(1), Some(idx));
        assert!(s.check_invariants());
    }

    #[test]
    fn step_from_idle_returns_none_when_no_ready() {
        // CPU 0's thread is Running; no other Ready threads exist.
        // step_from_idle on CPU 1 finds nothing to pick.
        let mut s = SchedState::new();
        // Add a thread then immediately block it so it's not Ready.
        let idx = s.add_thread().unwrap();
        s.states[idx] = Some(SchedThreadState::Blocked);
        assert_eq!(s.step_from_idle(1), None);
        assert_eq!(s.try_current_for(1), None);
    }

    #[test]
    fn step_from_idle_hides_running_on_other_cpu() {
        // CPU 0's current (idx 0) is Running. step_from_idle on CPU 1
        // must NOT select it (would be a multi-CPU invariant violation).
        let mut s = SchedState::new();
        assert_eq!(s.get(0), Some(SchedThreadState::Running));
        // No other thread exists → step_from_idle returns None.
        assert_eq!(s.step_from_idle(1), None);
        assert_eq!(s.try_current_for(1), None);
        assert_eq!(s.get(0), Some(SchedThreadState::Running)); // unchanged
    }

    #[test]
    #[should_panic(expected = "already has a current thread")]
    fn step_from_idle_panics_when_cpu_has_current() {
        // Precondition: CPU must have no current. Asserts.
        let mut s = SchedState::new();
        let _ = s.step_from_idle(0); // CPU 0 has current → panic
    }

    #[test]
    fn add_thread_makes_ready() {
        let mut s = SchedState::new();
        let idx = s.add_thread().unwrap();
        assert_eq!(s.get(idx), Some(SchedThreadState::Ready));
        assert!(s.check_invariants());
    }

    #[test]
    fn has_free_slot_initially_true() {
        let s = SchedState::new();
        assert!(s.has_free_slot());
    }

    #[test]
    fn has_free_slot_false_when_full() {
        let mut s = SchedState::new();
        // SchedState::new fills slot 0 (boot thread). Add until full.
        while s.add_thread().is_some() {}
        assert!(!s.has_free_slot());
        assert!(s.add_thread().is_none(), "add must agree with has_free_slot");
    }

    #[test]
    fn has_free_slot_true_after_partial_fill() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        assert!(s.has_free_slot());
    }

    #[test]
    fn block_current_valid_from_running() {
        let mut s = SchedState::new();
        assert!(s.block_current(0).is_ok());
        assert_eq!(s.get(0), Some(SchedThreadState::Blocked));
        assert!(s.check_invariants());
    }

    #[test]
    fn block_current_invalid_when_not_running() {
        let mut s = SchedState::new();
        s.block_current(0).unwrap();
        assert_eq!(s.block_current(0), Err(SchedError::InvalidTransition));
    }

    #[test]
    fn unblock_non_current_to_ready() {
        // Non-current Blocked thread → Ready (normal wake path).
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // idx 1, Ready by default
        s.states[1] = Some(SchedThreadState::Blocked); // simulate block
        assert!(s.unblock(1).is_ok());
        assert_eq!(s.get(1), Some(SchedThreadState::Ready));
        assert!(s.check_invariants());
    }

    #[test]
    fn unblock_current_to_running() {
        // Current Blocked thread → Running (self-wake path). The
        // bug class this prevents: if unblock left current = Ready,
        // step(Block) on the next block_current loop iteration
        // would panic ("requires current Blocked or Exited").
        let mut s = SchedState::new();
        s.block_current(0).unwrap();                  // CPU 0 current → Blocked
        assert_eq!(s.get(0), Some(SchedThreadState::Blocked));
        assert!(s.unblock(0).is_ok());
        assert_eq!(s.get(0), Some(SchedThreadState::Running)); // not Ready!
        assert!(s.check_invariants());
    }

    #[test]
    fn unblock_invalid_when_ready() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        assert_eq!(s.unblock(1), Err(SchedError::InvalidTransition));
    }

    #[test]
    fn check_invariants_rejects_ready_current() {
        // Direct construction of the forbidden state — proves
        // check_invariants catches it. Real callers cannot reach
        // this state because `unblock` upholds the invariant.
        let mut s = SchedState::new();
        s.states[0] = Some(SchedThreadState::Ready); // tampered: current is Ready
        assert!(!s.check_invariants());
    }

    #[test]
    fn apply_switch_to_preserves_one_running() {
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // idx 1 Ready
        let d = s.decide(0, SchedReason::Preempt);
        assert_eq!(d, SchedDecision::SwitchTo(1));
        s.apply_decision(0, SchedReason::Preempt, d).unwrap();
        assert_eq!(s.get(0), Some(SchedThreadState::Ready));
        assert_eq!(s.get(1), Some(SchedThreadState::Running));
        assert_eq!(s.current_for(0), 1);
        assert!(s.check_invariants());
    }

    #[test]
    fn apply_stay_on_current_rejects_block_reason() {
        let mut s = SchedState::new();
        let r = s.apply_decision(0, SchedReason::Block, SchedDecision::StayOnCurrent);
        assert_eq!(r, Err(SchedError::InvalidTransition));
    }

    #[test]
    fn apply_switch_to_non_ready_rejected() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        s.states[1] = Some(SchedThreadState::Blocked);
        let r = s.apply_decision(0, SchedReason::Preempt, SchedDecision::SwitchTo(1));
        assert_eq!(r, Err(SchedError::InvalidTransition));
    }

    // --- Atomic step() API ---

    #[test]
    fn step_preempt_switches_to_ready() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        let d = s.step(0, SchedReason::Preempt);
        assert_eq!(d, SchedDecision::SwitchTo(1));
        assert_eq!(s.get(0), Some(SchedThreadState::Ready));
        assert_eq!(s.get(1), Some(SchedThreadState::Running));
        assert_eq!(s.current_for(0), 1);
    }

    #[test]
    fn step_preempt_stays_when_alone_running() {
        let mut s = SchedState::new();
        let d = s.step(0, SchedReason::Preempt);
        assert_eq!(d, SchedDecision::StayOnCurrent);
        assert_eq!(s.get(0), Some(SchedThreadState::Running));
    }

    #[test]
    fn step_block_switches_to_ready() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        s.block_current(0).unwrap();
        let d = s.step(0, SchedReason::Block);
        assert_eq!(d, SchedDecision::SwitchTo(1));
        assert_eq!(s.get(0), Some(SchedThreadState::Blocked));
        assert_eq!(s.get(1), Some(SchedThreadState::Running));
        assert_eq!(s.current_for(0), 1);
    }

    #[test]
    fn step_block_waits_when_no_ready() {
        let mut s = SchedState::new();
        s.block_current(0).unwrap();
        let d = s.step(0, SchedReason::Block);
        assert_eq!(d, SchedDecision::WaitForInterrupt);
        // State unchanged
        assert_eq!(s.get(0), Some(SchedThreadState::Blocked));
    }

    #[test]
    #[should_panic(expected = "step(Preempt) requires current thread to be Running")]
    fn step_preempt_panics_if_current_not_running() {
        let mut s = SchedState::new();
        s.block_current(0).unwrap();
        // current is Blocked, Preempt precondition violated
        let _ = s.step(0, SchedReason::Preempt);
    }

    #[test]
    #[should_panic(expected = "step(Block) requires current thread to be Blocked")]
    fn step_block_panics_if_current_not_blocked() {
        let mut s = SchedState::new();
        // current is Running, Block precondition violated
        let _ = s.step(0, SchedReason::Block);
    }

    #[test]
    fn timer_tick_while_idle_is_safe() {
        // Regression: when current is Blocked (idling in block_current's
        // wfi loop) and a timer IRQ fires, the kernel's tick() must
        // guard — it cannot call step(Preempt) because the precondition
        // requires current=Running. The kernel returns early; the model
        // is unchanged.
        let mut s = SchedState::new();
        s.block_current(0).unwrap();
        // Verify: current is Blocked
        assert_eq!(s.get(s.current_for(0)), Some(SchedThreadState::Blocked));
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
        s.current_per_cpu[0] = Some(5); // points at unregistered slot
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
        current_cpu0: Option<usize>,
        states: [Option<SchedThreadState>; 4], // bounded to 4 for BFS
    }

    impl ReachableState {
        fn from_sched(s: &SchedState) -> Self {
            let mut states = [None; 4];
            for i in 0..4 {
                states[i] = s.get(i);
            }
            Self { current_cpu0: s.current_per_cpu[0], states }
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
        if s.current_per_cpu[0].is_some() && s.get(s.current_for(0)) == Some(SchedThreadState::Running) {
            let mut s2 = s.clone();
            if s2.block_current(0).is_ok() {
                // Follow up with step(Block) which must complete
                let _ = s2.step(0, SchedReason::Block);
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
            if s.current_per_cpu[0].is_some() && s.get(s.current_for(0)) == Some(SchedThreadState::Running) {
                let _ = s2.step(0, SchedReason::Preempt);
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
        if s.current_per_cpu[0].is_some() && s.get(s.current_for(0)) == Some(SchedThreadState::Running)
            && s.thread_count() >= 2
        {
            let mut s2 = s.clone();
            let decision = s2.step(0, SchedReason::Exit);
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
        let d = s.step(0, SchedReason::Exit);
        assert_eq!(d, SchedDecision::ExitAndSwitch { exited: 0, next: 1 });
        assert_eq!(s.get(0), Some(SchedThreadState::Exited));
        assert_eq!(s.get(1), Some(SchedThreadState::Running));
        assert_eq!(s.current_for(0), 1);
        assert!(s.check_invariants());
    }

    #[test]
    fn step_exit_halts_when_alone() {
        let mut s = SchedState::new();
        let d = s.step(0, SchedReason::Exit);
        assert_eq!(d, SchedDecision::ExitAndHalt { exited: 0 });
        assert_eq!(s.get(0), Some(SchedThreadState::Exited));
        assert!(s.check_invariants());
    }

    #[test]
    fn exited_thread_never_selected() {
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // idx 1 Ready
        s.step(0, SchedReason::Exit); // exit thread 0, switch to 1
        // Now thread 0 is Exited, thread 1 is Running
        // Preempt should stay on current (no Ready threads)
        let d = s.step(0, SchedReason::Preempt);
        assert_eq!(d, SchedDecision::StayOnCurrent);
    }

    #[test]
    fn unblock_rejects_exited() {
        let mut s = SchedState::new();
        s.add_thread().unwrap();
        s.step(0, SchedReason::Exit); // thread 0 Exited
        assert_eq!(s.unblock(0), Err(SchedError::InvalidTransition));
    }

    #[test]
    fn remove_thread_on_exited_frees_slot() {
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // idx 1
        s.step(0, SchedReason::Exit); // exit thread 0
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
        s.step(0, SchedReason::Exit); // ExitAndHalt, current=0, states[0]=Exited
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
        let d = s.step(0, SchedReason::Exit);
        assert_eq!(d, SchedDecision::ExitAndHalt { exited: 0 });
        assert_eq!(s.get(0), Some(SchedThreadState::Exited));
        assert_eq!(s.current_for(0), 0); // still current — can't remove yet

        // remove_thread must fail while exited thread is still current
        assert_eq!(s.remove_thread(0), Err(SchedError::InvalidTransition));

        // Simulate interrupt unblocking thread 1
        s.unblock(1).unwrap();

        // step(Block) with Exited current should find Ready thread 1
        let d2 = s.step(0, SchedReason::Block);
        assert_eq!(d2, SchedDecision::SwitchTo(1));
        assert_eq!(s.current_for(0), 1); // current changed — now remove works

        // NOW remove_thread succeeds (exited thread is no longer current)
        assert!(s.remove_thread(0).is_ok());
        assert_eq!(s.get(0), None);
        assert!(s.check_invariants());
    }

    #[test]
    #[should_panic(expected = "step(Exit) requires current thread to be Running")]
    fn step_exit_panics_if_current_not_running() {
        let mut s = SchedState::new();
        s.block_current(0).unwrap();
        let _ = s.step(0, SchedReason::Exit);
    }

    #[test]
    fn bfs_no_deadlock_without_ready_thread_is_wait_for_interrupt() {
        // Specifically verify: when the current thread just blocked and
        // no other thread is Ready, decide(Block) returns WaitForInterrupt
        // (never StayOnCurrent), and apply_decision leaves state unchanged.
        let mut s = SchedState::new();
        s.add_thread().unwrap(); // thread 1 Ready
        s.states[1] = Some(SchedThreadState::Blocked); // force both not runnable
        s.block_current(0).unwrap();
        let before = s.clone();
        let d = s.step(0, SchedReason::Block);
        assert_eq!(d, SchedDecision::WaitForInterrupt);
        assert_eq!(s, before, "WaitForInterrupt must not mutate state");
    }
}
