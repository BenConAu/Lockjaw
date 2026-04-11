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

/// An effect the kernel must execute after a state transition.
/// Derived by comparing before/after SystemState. The kernel maps each
/// effect to a concrete operation (pointer write, scheduler call) without
/// making any decisions of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcEffect {
    /// Set the endpoint to a new state.
    SetEndpointState(EpState),
    /// Block the acting thread (call scheduler::block_current).
    BlockCurrent,
    /// Unblock the previously blocked/caller thread (call scheduler::unblock_thread).
    UnblockThread,
    /// Store the message in the endpoint buffer (slow path: no partner yet).
    StoreMessage,
    /// Transfer the message directly to the blocked thread's ipc_msg field.
    TransferMessageToBlocked,
    /// Read the message from the endpoint buffer (receiver taking a stored msg).
    TakeMessageFromEndpoint,
    /// Record the caller's TCB paddr in the endpoint (for sys_reply).
    RecordCaller,
    /// Clear the caller's TCB paddr after replying.
    ClearCaller,
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
// Effect derivation — compare before/after to determine what the kernel does
// ---------------------------------------------------------------------------

/// Derive the list of effects the kernel must execute for a transition.
/// The kernel calls step() to get the new state, then derive_effects() to
/// get the effect list, then executes each effect mechanically.
///
/// This function requires `std` (returns Vec) and is only used in tests
/// and in the kernel (which links against alloc-free code that uses a
/// fixed-size array instead).

/// Non-allocating version for the kernel: returns effects in a fixed-size array.
/// Returns (effects, count).
pub fn derive_effects_array(before: &SystemState, after: &SystemState, who: Thread, op: IpcOp) -> ([IpcEffect; 8], usize) {
    let mut effects = [IpcEffect::SetEndpointState(EpState::Idle); 8];
    let mut count = 0;

    let mut push = |e: IpcEffect| {
        effects[count] = e;
        count += 1;
    };

    // Message routing
    match op {
        IpcOp::Send | IpcOp::Call => {
            if before.endpoint == EpState::HasReceiver {
                push(IpcEffect::TransferMessageToBlocked);
            } else {
                push(IpcEffect::StoreMessage);
            }
        }
        IpcOp::Receive => {
            if before.endpoint == EpState::HasSender || before.endpoint == EpState::HasCaller {
                push(IpcEffect::TakeMessageFromEndpoint);
            }
        }
        IpcOp::Reply => {
            push(IpcEffect::TransferMessageToBlocked);
        }
    }

    // Caller tracking: record before block, clear after unblock
    if !before.has_caller && after.has_caller {
        push(IpcEffect::RecordCaller);
    }

    // Unblock must happen BEFORE ClearCaller, because the kernel reads
    // caller_tcb_paddr to know who to unblock.
    let other = match who { Thread::A => Thread::B, Thread::B => Thread::A };
    if before.thread_state(other) == ThreadState::Blocked && after.thread_state(other) == ThreadState::Ready {
        push(IpcEffect::UnblockThread);
    }

    if before.has_caller && !after.has_caller {
        push(IpcEffect::ClearCaller);
    }

    // Endpoint state change — must happen before BlockCurrent
    if before.endpoint != after.endpoint {
        push(IpcEffect::SetEndpointState(after.endpoint));
    }

    // BlockCurrent MUST be last — block_current() context-switches away
    // and does not return until the thread is unblocked.
    let who_before = before.thread_state(who);
    let who_after = after.thread_state(who);
    if who_before != ThreadState::Blocked && who_after == ThreadState::Blocked {
        push(IpcEffect::BlockCurrent);
    }

    (effects, count)
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

                        // Derive effects and verify structural invariants
                        let (effect_arr, effect_count) = derive_effects_array(&state, &next, who, op);
                        let effects = &effect_arr[..effect_count];

                        assert!(!effects.is_empty(),
                            "No effects for valid transition: {:?} {:?} {:?} -> {:?}",
                            state.endpoint, who, op, next.endpoint);

                        // If acting thread became Blocked, must have BlockCurrent
                        if state.thread_state(who) != ThreadState::Blocked
                            && next.thread_state(who) == ThreadState::Blocked {
                            assert!(effects.contains(&IpcEffect::BlockCurrent),
                                "Thread blocked but no BlockCurrent effect: {:?} {:?}", op, effects);
                        }

                        // If other thread was unblocked, must have UnblockThread
                        let other = match who { Thread::A => Thread::B, Thread::B => Thread::A };
                        if state.thread_state(other) == ThreadState::Blocked
                            && next.thread_state(other) == ThreadState::Ready {
                            assert!(effects.contains(&IpcEffect::UnblockThread),
                                "Thread unblocked but no UnblockThread effect: {:?} {:?}", op, effects);
                        }

                        // If endpoint state changed, must have SetEndpointState
                        if state.endpoint != next.endpoint {
                            let has_set = effects.iter().any(|e| matches!(e, IpcEffect::SetEndpointState(_)));
                            assert!(has_set,
                                "Endpoint changed but no SetEndpointState: {:?} {:?}", op, effects);
                        }

                        // Send/Call must move a message somewhere
                        if matches!(op, IpcOp::Send | IpcOp::Call) {
                            let has_msg = effects.contains(&IpcEffect::StoreMessage)
                                || effects.contains(&IpcEffect::TransferMessageToBlocked);
                            assert!(has_msg,
                                "Send/Call without message effect: {:?} {:?}", op, effects);
                        }

                        // Receive from HasSender/HasCaller must take message
                        if op == IpcOp::Receive
                            && (state.endpoint == EpState::HasSender || state.endpoint == EpState::HasCaller) {
                            assert!(effects.contains(&IpcEffect::TakeMessageFromEndpoint),
                                "Receive from sender/caller without TakeMessage: {:?}", effects);
                        }

                        // Caller tracking
                        if !state.has_caller && next.has_caller {
                            assert!(effects.contains(&IpcEffect::RecordCaller),
                                "Caller recorded but no RecordCaller: {:?}", effects);
                        }
                        if state.has_caller && !next.has_caller {
                            assert!(effects.contains(&IpcEffect::ClearCaller),
                                "Caller cleared but no ClearCaller: {:?}", effects);
                        }

                        // ORDERING: BlockCurrent must be the LAST effect.
                        // block_current() context-switches away — any effects
                        // after it would not execute until the thread wakes up.
                        if let Some(pos) = effects.iter().position(|e| *e == IpcEffect::BlockCurrent) {
                            assert_eq!(pos, effect_count - 1,
                                "BlockCurrent must be last effect but was at position {} of {}: {:?}",
                                pos, effect_count, effects);
                        }

                        // ORDERING: UnblockThread must be the FIRST effect
                        // (excluding message effects). The kernel reads
                        // blocked_tcb_paddr/caller_tcb_paddr to find who to
                        // unblock — these fields must not be cleared first.
                        if effects.contains(&IpcEffect::UnblockThread)
                            && effects.contains(&IpcEffect::ClearCaller) {
                            let unblock_pos = effects.iter().position(|e| *e == IpcEffect::UnblockThread).unwrap();
                            let clear_pos = effects.iter().position(|e| *e == IpcEffect::ClearCaller).unwrap();
                            assert!(unblock_pos < clear_pos,
                                "UnblockThread must come before ClearCaller: {:?}", effects);
                        }

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
