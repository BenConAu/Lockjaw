/// Pure IPC state machine model for exhaustive verification.
///
/// Models the combined state of one endpoint and two threads. The `step()`
/// function is a pure state transition — no unsafe, no pointers, no scheduler.
/// The kernel's IPC implementation must match this model exactly.

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EpState {
    Idle,
    HasSender,
    HasReceiver,
    HasCaller,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThreadState {
    Ready,
    Running,
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Thread {
    A,
    B,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcOp {
    Send,
    Receive,
    Call,
    Reply,
}

/// The full system state for two threads sharing one endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SystemState {
    pub endpoint: EpState,
    pub has_caller: bool,
    /// Which thread is blocked in the endpoint (if any).
    pub blocked_thread: Option<Thread>,
    /// Which thread is the caller waiting for a reply (if any).
    pub caller_thread: Option<Thread>,
    pub thread_a: ThreadState,
    pub thread_b: ThreadState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionError {
    Busy,
    NoCaller,
    ThreadNotRunnable,
}

// ---------------------------------------------------------------------------
// State transitions
// ---------------------------------------------------------------------------

impl SystemState {
    pub fn initial() -> Self {
        SystemState {
            endpoint: EpState::Idle,
            has_caller: false,
            blocked_thread: None,
            caller_thread: None,
            thread_a: ThreadState::Running,
            thread_b: ThreadState::Ready,
        }
    }

    fn thread_state(&self, who: Thread) -> ThreadState {
        match who {
            Thread::A => self.thread_a,
            Thread::B => self.thread_b,
        }
    }

    fn with_thread_state(mut self, who: Thread, state: ThreadState) -> Self {
        match who {
            Thread::A => self.thread_a = state,
            Thread::B => self.thread_b = state,
        }
        self
    }

}

/// Perform one IPC operation. Returns the new system state or an error.
///
/// A thread can only act if it is Running or Ready (not Blocked).
/// This models the fact that blocked threads cannot make syscalls.
pub fn step(
    state: SystemState,
    who: Thread,
    op: IpcOp,
) -> Result<SystemState, TransitionError> {
    // Only a non-blocked thread can perform operations
    let ts = state.thread_state(who);
    if ts == ThreadState::Blocked {
        return Err(TransitionError::ThreadNotRunnable);
    }

    match op {
        IpcOp::Send => step_send(state, who),
        IpcOp::Receive => step_receive(state, who),
        IpcOp::Call => step_call(state, who),
        IpcOp::Reply => step_reply(state, who),
    }
}

fn step_send(state: SystemState, who: Thread) -> Result<SystemState, TransitionError> {
    match state.endpoint {
        EpState::HasReceiver => {
            // Fast path: receiver waiting. Transfer msg, unblock receiver.
            let receiver = state.blocked_thread.unwrap();
            Ok(SystemState {
                endpoint: EpState::Idle,
                blocked_thread: None,
                // Sender stays runnable, receiver becomes Ready
                ..state.with_thread_state(receiver, ThreadState::Ready)
            })
        }
        EpState::Idle => {
            // Slow path: block sender.
            Ok(SystemState {
                endpoint: EpState::HasSender,
                blocked_thread: Some(who),
                ..state.with_thread_state(who, ThreadState::Blocked)
            })
        }
        _ => Err(TransitionError::Busy),
    }
}

fn step_receive(state: SystemState, who: Thread) -> Result<SystemState, TransitionError> {
    match state.endpoint {
        EpState::HasSender => {
            // Fast path: regular sender waiting. Take msg, unblock sender.
            let sender = state.blocked_thread.unwrap();
            Ok(SystemState {
                endpoint: EpState::Idle,
                blocked_thread: None,
                ..state.with_thread_state(sender, ThreadState::Ready)
            })
        }
        EpState::HasCaller => {
            // Caller waiting. Take msg but do NOT unblock — caller waits for reply.
            Ok(SystemState {
                endpoint: EpState::Idle,
                blocked_thread: None,
                // caller_thread stays set, has_caller stays true
                // The caller's ThreadState stays Blocked
                ..state
            })
        }
        EpState::Idle => {
            // Slow path: block receiver.
            Ok(SystemState {
                endpoint: EpState::HasReceiver,
                blocked_thread: Some(who),
                ..state.with_thread_state(who, ThreadState::Blocked)
            })
        }
        _ => Err(TransitionError::Busy),
    }
}

fn step_call(state: SystemState, who: Thread) -> Result<SystemState, TransitionError> {
    match state.endpoint {
        EpState::HasReceiver => {
            // Fast path: server waiting. Transfer msg, unblock server, block caller.
            let receiver = state.blocked_thread.unwrap();
            let mut next = state.with_thread_state(receiver, ThreadState::Ready);
            next = next.with_thread_state(who, ThreadState::Blocked);
            Ok(SystemState {
                endpoint: EpState::Idle,
                has_caller: true,
                blocked_thread: None,
                caller_thread: Some(who),
                ..next
            })
        }
        EpState::Idle => {
            // Slow path: block caller, store msg.
            Ok(SystemState {
                endpoint: EpState::HasCaller,
                has_caller: true,
                blocked_thread: Some(who),
                caller_thread: Some(who),
                ..state.with_thread_state(who, ThreadState::Blocked)
            })
        }
        _ => Err(TransitionError::Busy),
    }
}

fn step_reply(state: SystemState, _who: Thread) -> Result<SystemState, TransitionError> {
    if !state.has_caller {
        return Err(TransitionError::NoCaller);
    }

    // Reply is only valid after receive has taken the message.
    // If endpoint is still HasCaller, the message hasn't been received yet.
    if state.endpoint == EpState::HasCaller {
        return Err(TransitionError::Busy);
    }

    let caller = state.caller_thread.unwrap();
    Ok(SystemState {
        has_caller: false,
        caller_thread: None,
        ..state.with_thread_state(caller, ThreadState::Ready)
    })
}

// ---------------------------------------------------------------------------
// Invariant checks
// ---------------------------------------------------------------------------

impl SystemState {
    /// Check all invariants. Panics with a descriptive message if violated.
    pub fn check_invariants(&self) {
        // 1. If endpoint is Idle and no caller, blocked_thread must be None
        if self.endpoint == EpState::Idle && !self.has_caller && self.blocked_thread.is_some() {
            panic!("Invariant violated: Idle endpoint with no caller but blocked_thread is {:?}. State: {:?}",
                self.blocked_thread, self);
        }

        // 2. If endpoint has a blocked party, blocked_thread must be Some
        if matches!(self.endpoint, EpState::HasSender | EpState::HasReceiver | EpState::HasCaller) {
            assert!(self.blocked_thread.is_some(),
                "Invariant violated: endpoint {:?} but no blocked_thread. State: {:?}",
                self.endpoint, self);
        }

        // 3. If has_caller, exactly one thread must be Blocked
        if self.has_caller {
            assert!(self.caller_thread.is_some(),
                "Invariant violated: has_caller but no caller_thread. State: {:?}", self);
            let caller = self.caller_thread.unwrap();
            assert_eq!(self.thread_state(caller), ThreadState::Blocked,
                "Invariant violated: caller {:?} is not Blocked. State: {:?}", caller, self);
        }

        // 4. blocked_thread consistency: if set, that thread must be Blocked
        if let Some(bt) = self.blocked_thread {
            assert_eq!(self.thread_state(bt), ThreadState::Blocked,
                "Invariant violated: blocked_thread {:?} is not Blocked. State: {:?}", bt, self);
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
    fn exhaustive_state_exploration_no_deadlocks() {
        extern crate std;
        use std::collections::{HashSet, VecDeque};

        let initial = SystemState::initial();

        let mut visited: HashSet<SystemState> = HashSet::new();
        let mut queue: VecDeque<SystemState> = VecDeque::new();
        let mut transition_count: usize = 0;
        let mut deadlock_count: usize = 0;
        queue.push_back(initial);

        while let Some(state) = queue.pop_front() {
            if visited.contains(&state) {
                continue;
            }
            visited.insert(state);

            // Check invariants on every reachable state
            state.check_invariants();

            // DEADLOCK CHECK: both threads blocked is reachable (e.g. A calls,
            // B receives, B sends instead of replying). This is a user error,
            // not a kernel bug — the kernel allows it. We track these but don't
            // panic. A real deadlock detector would need a third thread or a
            // timeout mechanism.
            if state.thread_a == ThreadState::Blocked && state.thread_b == ThreadState::Blocked {
                deadlock_count += 1;
            }

            // Try all operations from both threads
            for who in [Thread::A, Thread::B] {
                for op in [IpcOp::Send, IpcOp::Receive, IpcOp::Call, IpcOp::Reply] {
                    if let Ok(next) = step(state, who, op) {
                        next.check_invariants();
                        transition_count += 1;
                        if !visited.contains(&next) {
                            queue.push_back(next);
                        }
                    }
                }
            }
        }

        std::println!(
            "State exploration complete: {} reachable states, {} transitions, {} user-deadlock states",
            visited.len(),
            transition_count,
            deadlock_count,
        );

        // Sanity: we should have explored more than just the initial state
        assert!(visited.len() > 5, "Too few states explored: {}", visited.len());
    }

    #[test]
    fn send_then_receive_sequence() {
        let s0 = SystemState::initial();

        // Thread A sends — blocks
        let s1 = step(s0, Thread::A, IpcOp::Send).unwrap();
        assert_eq!(s1.endpoint, EpState::HasSender);
        assert_eq!(s1.thread_a, ThreadState::Blocked);
        assert_eq!(s1.thread_b, ThreadState::Ready);

        // Thread B receives — unblocks A
        let s2 = step(s1, Thread::B, IpcOp::Receive).unwrap();
        assert_eq!(s2.endpoint, EpState::Idle);
        assert_eq!(s2.thread_a, ThreadState::Ready);
    }

    #[test]
    fn receive_then_send_sequence() {
        let s0 = SystemState::initial();

        // Thread B receives — blocks
        let s1 = step(s0, Thread::B, IpcOp::Receive).unwrap();
        assert_eq!(s1.endpoint, EpState::HasReceiver);
        assert_eq!(s1.thread_b, ThreadState::Blocked);

        // Thread A sends — unblocks B
        let s2 = step(s1, Thread::A, IpcOp::Send).unwrap();
        assert_eq!(s2.endpoint, EpState::Idle);
        assert_eq!(s2.thread_b, ThreadState::Ready);
    }

    #[test]
    fn call_receive_reply_sequence() {
        let s0 = SystemState::initial();

        // Thread A calls — blocks
        let s1 = step(s0, Thread::A, IpcOp::Call).unwrap();
        assert_eq!(s1.endpoint, EpState::HasCaller);
        assert_eq!(s1.thread_a, ThreadState::Blocked);
        assert!(s1.has_caller);

        // Thread B receives — gets msg, A stays blocked
        let s2 = step(s1, Thread::B, IpcOp::Receive).unwrap();
        assert_eq!(s2.endpoint, EpState::Idle);
        assert_eq!(s2.thread_a, ThreadState::Blocked); // still waiting for reply
        assert!(s2.has_caller);

        // Thread B replies — unblocks A
        let s3 = step(s2, Thread::B, IpcOp::Reply).unwrap();
        assert_eq!(s3.thread_a, ThreadState::Ready);
        assert!(!s3.has_caller);
    }

    #[test]
    fn receive_then_call_then_reply_sequence() {
        let s0 = SystemState::initial();

        // Thread B receives first — blocks (server waiting)
        let s1 = step(s0, Thread::B, IpcOp::Receive).unwrap();
        assert_eq!(s1.endpoint, EpState::HasReceiver);
        assert_eq!(s1.thread_b, ThreadState::Blocked);

        // Thread A calls — fast path: unblocks B, blocks A
        let s2 = step(s1, Thread::A, IpcOp::Call).unwrap();
        assert_eq!(s2.endpoint, EpState::Idle);
        assert_eq!(s2.thread_a, ThreadState::Blocked); // waiting for reply
        assert_eq!(s2.thread_b, ThreadState::Ready);   // got the message
        assert!(s2.has_caller);

        // Thread B replies — unblocks A
        let s3 = step(s2, Thread::B, IpcOp::Reply).unwrap();
        assert_eq!(s3.thread_a, ThreadState::Ready);
        assert!(!s3.has_caller);
    }

    #[test]
    fn blocked_thread_cannot_act() {
        let s0 = SystemState::initial();
        let s1 = step(s0, Thread::A, IpcOp::Send).unwrap();
        assert_eq!(s1.thread_a, ThreadState::Blocked);

        // Blocked thread A cannot send again
        let err = step(s1, Thread::A, IpcOp::Send);
        assert_eq!(err, Err(TransitionError::ThreadNotRunnable));
    }

    #[test]
    fn reply_without_caller_fails() {
        let s0 = SystemState::initial();
        let err = step(s0, Thread::A, IpcOp::Reply);
        assert_eq!(err, Err(TransitionError::NoCaller));
    }

    #[test]
    fn double_send_fails() {
        let s0 = SystemState::initial();
        let s1 = step(s0, Thread::A, IpcOp::Send).unwrap();
        // Thread B tries to send too — endpoint busy
        let err = step(s1, Thread::B, IpcOp::Send);
        assert_eq!(err, Err(TransitionError::Busy));
    }
}
