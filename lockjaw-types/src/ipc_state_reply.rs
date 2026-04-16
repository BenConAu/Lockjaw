//! IPC state machine model for the Reply-object + intrusive-queue design.
//!
//! This module runs in parallel with [`crate::ipc_state`] during the IPC
//! redesign. The kernel still consumes the old module; this one is exercised
//! only by host tests. The cutover commit will swap the kernel over to this
//! module, and the subsequent cleanup commit will remove the old one.
//!
//! ### Why a new model
//!
//! The old model assumes at most one caller per endpoint (a single
//! `caller_tcb_paddr` slot). Two clients calling the same endpoint before
//! the server replies silently overwrite each other — a real corruption bug
//! hit during ramfb/uart-driver bring-up. The new model eliminates that
//! failure mode by:
//!
//! 1. Making the endpoint a queue of waiters rather than a single-slot
//!    rendezvous.
//! 2. Giving each client a Reply object whose binding to the call is
//!    independent of endpoint state, so a reply targets a specific caller
//!    rather than whatever paddr the endpoint happens to hold.
//!
//! ### Threads
//!
//! Three modeled threads: [`ThreadId::Client1`], [`ThreadId::Client2`],
//! [`ThreadId::Server`]. Two clients is the minimum to make the overwrite
//! bug reachable; a server rounds out the rendezvous. The kernel itself
//! supports many more threads — the model's job is only to prove the
//! state-machine shape is correct.

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Endpoint state. Collapses the old `HasSender`/`HasCaller` split into
/// a single `HasWaiters` — each waiter carries its kind in the queue, so
/// the endpoint itself doesn't need to distinguish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EpState {
    Idle,
    HasWaiters,
    HasReceiver,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThreadState {
    Ready,
    Running,
    Blocked,
}

/// All threads in the model. `Client1`/`Client2` each own a Reply object;
/// `Server` does not (servers reply via clients' Replies).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThreadId {
    Client1,
    Client2,
    Server,
}

impl ThreadId {
    pub const ALL: [ThreadId; 3] = [ThreadId::Client1, ThreadId::Client2, ThreadId::Server];

    pub fn as_index(self) -> usize {
        match self {
            ThreadId::Client1 => 0,
            ThreadId::Client2 => 1,
            ThreadId::Server => 2,
        }
    }

    pub fn is_client(self) -> bool {
        matches!(self, ThreadId::Client1 | ThreadId::Client2)
    }

    pub fn as_client(self) -> Option<ClientId> {
        match self {
            ThreadId::Client1 => Some(ClientId::One),
            ThreadId::Client2 => Some(ClientId::Two),
            ThreadId::Server => None,
        }
    }
}

/// A client's identity, narrower than [`ThreadId`]. Used where only a client
/// makes sense (e.g. Reply ownership).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientId {
    One,
    Two,
}

impl ClientId {
    pub const ALL: [ClientId; 2] = [ClientId::One, ClientId::Two];

    pub fn as_index(self) -> usize {
        match self {
            ClientId::One => 0,
            ClientId::Two => 1,
        }
    }

    pub fn as_thread(self) -> ThreadId {
        match self {
            ClientId::One => ThreadId::Client1,
            ClientId::Two => ThreadId::Client2,
        }
    }
}

/// Kind of wait a queued thread holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WaitKind {
    Send,
    Receive,
    Call,
}

/// Reply object state. A Reply belongs to exactly one client and is either
/// free for the next call or bound to an outstanding one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReplyState {
    Fresh,
    /// Bound to this client's outstanding call. The invariant chain
    /// "`reply_state[c] == Bound{c}`" is load-bearing — we track the
    /// caller field to prove that overwriting can never happen.
    Bound { caller: ClientId },
}

/// IPC operation requested by a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcOp {
    Send,
    Receive,
    Call,
    Reply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionError {
    /// Endpoint cannot accept this operation in its current state
    /// (e.g. two receivers, reply without a bound call).
    Busy,
    /// Sys_reply with no bound call on the server.
    NoCaller,
    /// Sys_call attempted with a Reply object that is already Bound.
    ReplyAlreadyBound,
    /// Thread is already queued and cannot issue a new op while blocked.
    ThreadNotRunnable,
}

// ---------------------------------------------------------------------------
// Queue — fixed-size FIFO of waiter entries
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QueueEntry {
    pub thread: ThreadId,
    pub kind: WaitKind,
}

/// Fixed-size FIFO. Index 0 is head; entries[0..count] are valid. At most
/// 3 entries, matching the thread count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Queue {
    pub entries: [Option<QueueEntry>; 3],
    pub count: usize,
}

impl Queue {
    pub const fn empty() -> Self {
        Queue { entries: [None; 3], count: 0 }
    }

    pub fn enqueue(&mut self, entry: QueueEntry) {
        assert!(self.count < 3, "queue full");
        self.entries[self.count] = Some(entry);
        self.count += 1;
    }

    pub fn dequeue(&mut self) -> Option<QueueEntry> {
        if self.count == 0 {
            return None;
        }
        let head = self.entries[0];
        // Shift remaining entries forward.
        for i in 0..(self.count - 1) {
            self.entries[i] = self.entries[i + 1];
        }
        self.entries[self.count - 1] = None;
        self.count -= 1;
        head
    }

    pub fn head(&self) -> Option<QueueEntry> {
        if self.count == 0 { None } else { self.entries[0] }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn contains(&self, t: ThreadId) -> bool {
        self.entries.iter().take(self.count).any(|e| {
            matches!(e, Some(QueueEntry { thread, .. }) if *thread == t)
        })
    }

    pub fn count_kind(&self, k: WaitKind) -> usize {
        self.entries.iter().take(self.count).filter(|e| {
            matches!(e, Some(QueueEntry { kind, .. }) if *kind == k)
        }).count()
    }
}

// ---------------------------------------------------------------------------
// System state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SystemState {
    pub endpoint: EpState,
    pub queue: Queue,
    /// Reply-object state for each client. Server has no Reply.
    pub reply_state: [ReplyState; 2],
    /// Per-thread binding of "whose Reply am I currently handling?" Mirrors
    /// the kernel's `current_reply_paddr` TCB field. Set on Receive-of-Call,
    /// cleared on the corresponding Reply. Indexed by `ThreadId::as_index`.
    pub current_reply: [Option<ClientId>; 3],
    /// Per-thread scheduler state (indexed by `ThreadId::as_index`).
    pub thread_state: [ThreadState; 3],
}

impl SystemState {
    pub const fn initial() -> Self {
        SystemState {
            endpoint: EpState::Idle,
            queue: Queue::empty(),
            reply_state: [ReplyState::Fresh, ReplyState::Fresh],
            current_reply: [None, None, None],
            // Clients are Ready (either could run), server is Ready.
            // The acting thread is implicitly Running for the transition.
            thread_state: [ThreadState::Ready, ThreadState::Ready, ThreadState::Ready],
        }
    }

    pub fn thread(&self, t: ThreadId) -> ThreadState {
        self.thread_state[t.as_index()]
    }

    fn with_thread(mut self, t: ThreadId, s: ThreadState) -> Self {
        self.thread_state[t.as_index()] = s;
        self
    }

    fn with_reply(mut self, c: ClientId, r: ReplyState) -> Self {
        self.reply_state[c.as_index()] = r;
        self
    }

    pub fn reply(&self, c: ClientId) -> ReplyState {
        self.reply_state[c.as_index()]
    }
}

// ---------------------------------------------------------------------------
// Effects — mechanical operations the kernel executes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcEffect {
    SetEndpointState(EpState),
    /// Acting thread appends itself to the endpoint queue with the given kind.
    EnqueueSelfAs(WaitKind),
    /// Pop the head of the endpoint queue.
    DequeueHead,
    /// Store the acting thread's syscall-args msg into its own TCB (slow path).
    StoreMsgInOwnTcb,
    /// Write the acting thread's msg into the previously-blocked receiver's
    /// TCB. Used on Send/Call fast path.
    WriteMsgToBlockedReceiver,
    /// Write the acting thread's msg into the bound caller's TCB. Used on Reply.
    WriteMsgToBoundCaller,
    /// Take the msg from the just-dequeued head waiter's TCB into the acting
    /// thread's receive buffer. Used on Receive fast path.
    TakeMsgFromDequeuedHead,
    /// Bind a client's Reply object to its outstanding call (Fresh → Bound).
    BindReply { caller: ClientId },
    /// Return a client's Reply object to Fresh (Bound → Fresh). Used on Reply.
    ClearReply { caller: ClientId },
    /// Record on the acting (receiver) TCB which client's Reply is being handled.
    SetCurrentReply { receiver: ThreadId, caller: ClientId },
    /// Clear the acting TCB's current-reply pointer.
    ClearCurrentReply { receiver: ThreadId },
    /// Move a thread from Blocked to Ready.
    UnblockThread { who: ThreadId },
    /// Move the acting thread from Running to Blocked and context-switch.
    /// Must be the last effect — nothing after it runs until we wake up.
    BlockCurrent,
}

/// Fixed-size array of effects for `no_std` use. 10 slots cover the
/// worst-case transition (Call fast path emits 7 effects).
pub const MAX_EFFECTS: usize = 10;

pub fn empty_effects() -> [IpcEffect; MAX_EFFECTS] {
    [IpcEffect::SetEndpointState(EpState::Idle); MAX_EFFECTS]
}

// ---------------------------------------------------------------------------
// Transitions
// ---------------------------------------------------------------------------

/// Entry point: validate the op in the given state, compute the new state,
/// and return both the new state and the effect list.
pub fn step(
    state: SystemState,
    who: ThreadId,
    op: IpcOp,
) -> Result<(SystemState, [IpcEffect; MAX_EFFECTS], usize), TransitionError> {
    // Acting thread must not be Blocked.
    if state.thread(who) == ThreadState::Blocked {
        return Err(TransitionError::ThreadNotRunnable);
    }

    match op {
        IpcOp::Send => step_send(state, who),
        IpcOp::Receive => step_receive(state, who),
        IpcOp::Call => step_call(state, who),
        IpcOp::Reply => step_reply(state, who),
    }
}

fn step_send(
    state: SystemState,
    who: ThreadId,
) -> Result<(SystemState, [IpcEffect; MAX_EFFECTS], usize), TransitionError> {
    match state.endpoint {
        EpState::HasReceiver => {
            // Fast path — receiver is queued, dequeue & deliver.
            let mut next = state;
            let head = next.queue.dequeue().expect("HasReceiver without queued receiver");
            debug_assert_eq!(head.kind, WaitKind::Receive);
            let receiver = head.thread;
            next = next.with_thread(receiver, ThreadState::Ready);
            next.endpoint = EpState::Idle;

            let mut effects = empty_effects();
            let mut n = 0;
            effects[n] = IpcEffect::DequeueHead; n += 1;
            effects[n] = IpcEffect::WriteMsgToBlockedReceiver; n += 1;
            effects[n] = IpcEffect::UnblockThread { who: receiver }; n += 1;
            effects[n] = IpcEffect::SetEndpointState(EpState::Idle); n += 1;
            Ok((next, effects, n))
        }
        EpState::Idle | EpState::HasWaiters => {
            // Slow path — queue self as Send, block.
            let mut next = state;
            next.queue.enqueue(QueueEntry { thread: who, kind: WaitKind::Send });
            next = next.with_thread(who, ThreadState::Blocked);
            let endpoint_changed = next.endpoint == EpState::Idle;
            if endpoint_changed {
                next.endpoint = EpState::HasWaiters;
            }

            let mut effects = empty_effects();
            let mut n = 0;
            effects[n] = IpcEffect::StoreMsgInOwnTcb; n += 1;
            effects[n] = IpcEffect::EnqueueSelfAs(WaitKind::Send); n += 1;
            if endpoint_changed {
                effects[n] = IpcEffect::SetEndpointState(EpState::HasWaiters); n += 1;
            }
            effects[n] = IpcEffect::BlockCurrent; n += 1;
            Ok((next, effects, n))
        }
    }
}

fn step_receive(
    state: SystemState,
    who: ThreadId,
) -> Result<(SystemState, [IpcEffect; MAX_EFFECTS], usize), TransitionError> {
    // Per-receiver single-reply-slot rule: if this thread still holds a
    // current_reply from a prior Receive-of-Call, it must Reply before
    // receiving again. (seL4 MCS would allow "save reply" to defer; that's
    // a future extension — see plan non-goals.)
    if state.current_reply[who.as_index()].is_some() {
        return Err(TransitionError::Busy);
    }

    match state.endpoint {
        EpState::HasReceiver => {
            // Another receiver already queued — invariant 5 bars a second.
            Err(TransitionError::Busy)
        }
        EpState::HasWaiters => {
            // Fast path — dequeue head, act on its kind.
            let mut next = state;
            let head = next.queue.dequeue().expect("HasWaiters without queued waiter");
            debug_assert!(matches!(head.kind, WaitKind::Send | WaitKind::Call));

            let mut effects = empty_effects();
            let mut n = 0;
            effects[n] = IpcEffect::DequeueHead; n += 1;
            effects[n] = IpcEffect::TakeMsgFromDequeuedHead; n += 1;

            match head.kind {
                WaitKind::Send => {
                    next = next.with_thread(head.thread, ThreadState::Ready);
                    effects[n] = IpcEffect::UnblockThread { who: head.thread }; n += 1;
                }
                WaitKind::Call => {
                    let client = head.thread.as_client()
                        .expect("Call waiter must be a client");
                    next.current_reply[who.as_index()] = Some(client);
                    effects[n] = IpcEffect::SetCurrentReply { receiver: who, caller: client }; n += 1;
                    // Caller stays Blocked awaiting Reply.
                }
                WaitKind::Receive => unreachable!("HasWaiters cannot hold a Receive waiter"),
            }

            // Collapse state if queue drained.
            if next.queue.is_empty() {
                next.endpoint = EpState::Idle;
                effects[n] = IpcEffect::SetEndpointState(EpState::Idle); n += 1;
            }
            Ok((next, effects, n))
        }
        EpState::Idle => {
            // Slow path — queue self as Receive, block.
            let mut next = state;
            next.queue.enqueue(QueueEntry { thread: who, kind: WaitKind::Receive });
            next = next.with_thread(who, ThreadState::Blocked);
            next.endpoint = EpState::HasReceiver;

            let mut effects = empty_effects();
            let mut n = 0;
            effects[n] = IpcEffect::EnqueueSelfAs(WaitKind::Receive); n += 1;
            effects[n] = IpcEffect::SetEndpointState(EpState::HasReceiver); n += 1;
            effects[n] = IpcEffect::BlockCurrent; n += 1;
            Ok((next, effects, n))
        }
    }
}

fn step_call(
    state: SystemState,
    who: ThreadId,
) -> Result<(SystemState, [IpcEffect; MAX_EFFECTS], usize), TransitionError> {
    let client = who.as_client().ok_or(TransitionError::Busy)?;
    // Precondition: the client's Reply must be Fresh (no outstanding call).
    // This is the key bug-catcher — under the old model, a second Call
    // silently overwrote the first caller's identity.
    if state.reply(client) != ReplyState::Fresh {
        return Err(TransitionError::ReplyAlreadyBound);
    }

    match state.endpoint {
        EpState::HasReceiver => {
            // Fast path — deliver msg to server, bind reply, block self.
            let mut next = state;
            let head = next.queue.dequeue().expect("HasReceiver without queued receiver");
            debug_assert_eq!(head.kind, WaitKind::Receive);
            let server = head.thread;
            next = next
                .with_thread(server, ThreadState::Ready)
                .with_thread(who, ThreadState::Blocked)
                .with_reply(client, ReplyState::Bound { caller: client });
            next.current_reply[server.as_index()] = Some(client);
            next.endpoint = EpState::Idle;

            let mut effects = empty_effects();
            let mut n = 0;
            effects[n] = IpcEffect::DequeueHead; n += 1;
            effects[n] = IpcEffect::BindReply { caller: client }; n += 1;
            effects[n] = IpcEffect::WriteMsgToBlockedReceiver; n += 1;
            effects[n] = IpcEffect::SetCurrentReply { receiver: server, caller: client }; n += 1;
            effects[n] = IpcEffect::UnblockThread { who: server }; n += 1;
            effects[n] = IpcEffect::SetEndpointState(EpState::Idle); n += 1;
            effects[n] = IpcEffect::BlockCurrent; n += 1;
            Ok((next, effects, n))
        }
        EpState::Idle | EpState::HasWaiters => {
            // Slow path — bind reply, queue self as Call, block.
            let mut next = state;
            next = next.with_reply(client, ReplyState::Bound { caller: client });
            next.queue.enqueue(QueueEntry { thread: who, kind: WaitKind::Call });
            next = next.with_thread(who, ThreadState::Blocked);
            let endpoint_changed = next.endpoint == EpState::Idle;
            if endpoint_changed {
                next.endpoint = EpState::HasWaiters;
            }

            let mut effects = empty_effects();
            let mut n = 0;
            effects[n] = IpcEffect::BindReply { caller: client }; n += 1;
            effects[n] = IpcEffect::StoreMsgInOwnTcb; n += 1;
            effects[n] = IpcEffect::EnqueueSelfAs(WaitKind::Call); n += 1;
            if endpoint_changed {
                effects[n] = IpcEffect::SetEndpointState(EpState::HasWaiters); n += 1;
            }
            effects[n] = IpcEffect::BlockCurrent; n += 1;
            Ok((next, effects, n))
        }
    }
}

fn step_reply(
    state: SystemState,
    who: ThreadId,
) -> Result<(SystemState, [IpcEffect; MAX_EFFECTS], usize), TransitionError> {
    // The replier must have a non-None current_reply. Kernel-side this is
    // the current thread's TCB.current_reply_paddr.
    let client = state.current_reply[who.as_index()].ok_or(TransitionError::NoCaller)?;

    // Reply target must currently be Bound to that client.
    match state.reply(client) {
        ReplyState::Bound { caller } if caller == client => {}
        _ => return Err(TransitionError::Busy),
    }

    let target = client.as_thread();
    let mut next = state;
    next = next
        .with_thread(target, ThreadState::Ready)
        .with_reply(client, ReplyState::Fresh);
    next.current_reply[who.as_index()] = None;

    let mut effects = empty_effects();
    let mut n = 0;
    effects[n] = IpcEffect::WriteMsgToBoundCaller; n += 1;
    // UnblockThread must come BEFORE ClearReply — the kernel reads
    // reply.caller to know whom to unblock.
    effects[n] = IpcEffect::UnblockThread { who: target }; n += 1;
    effects[n] = IpcEffect::ClearReply { caller: client }; n += 1;
    effects[n] = IpcEffect::ClearCurrentReply { receiver: who }; n += 1;
    Ok((next, effects, n))
}

// ---------------------------------------------------------------------------
// Invariants
// ---------------------------------------------------------------------------

impl SystemState {
    /// Check every invariant. Panics with a descriptive message on violation.
    pub fn check_invariants(&self) {
        // 1. Endpoint ↔ queue consistency.
        match self.endpoint {
            EpState::Idle => {
                assert!(self.queue.is_empty(),
                    "Idle endpoint but queue non-empty: {:?}", self.queue);
            }
            EpState::HasReceiver => {
                assert_eq!(self.queue.count, 1,
                    "HasReceiver must have exactly 1 waiter, got {}", self.queue.count);
                let head = self.queue.head().unwrap();
                assert_eq!(head.kind, WaitKind::Receive,
                    "HasReceiver head must be Receive kind, got {:?}", head.kind);
                // Any thread (not just Server) may receive — we don't
                // constrain which thread plays the receiver role.
            }
            EpState::HasWaiters => {
                assert!(!self.queue.is_empty(), "HasWaiters but queue empty");
                // At least one queued entry is Send/Call (no Receive).
                for e in self.queue.entries.iter().take(self.queue.count) {
                    let kind = e.unwrap().kind;
                    assert!(matches!(kind, WaitKind::Send | WaitKind::Call),
                        "HasWaiters cannot hold a Receive waiter: {:?}", kind);
                }
            }
        }

        // 5. At most one Receive waiter anywhere.
        assert!(self.queue.count_kind(WaitKind::Receive) <= 1,
            "Multiple Receive waiters: {:?}", self.queue);

        // 6. Queued threads must be Blocked.
        for e in self.queue.entries.iter().take(self.queue.count) {
            let entry = e.unwrap();
            assert_eq!(self.thread(entry.thread), ThreadState::Blocked,
                "Queued thread {:?} not Blocked", entry.thread);
        }

        // 7. No thread queued twice.
        for t in ThreadId::ALL {
            let count = self.queue.entries.iter().take(self.queue.count)
                .filter(|e| matches!(e, Some(q) if q.thread == t))
                .count();
            assert!(count <= 1, "Thread {:?} queued twice", t);
        }

        // 2 & 3. Reply/caller consistency.
        for c in ClientId::ALL {
            let t = c.as_thread();
            let queued_as_call = self.queue.entries.iter()
                .take(self.queue.count)
                .any(|e| matches!(e, Some(q) if q.thread == t && q.kind == WaitKind::Call));
            let being_handled_by = self.current_reply.iter()
                .filter(|x| **x == Some(c))
                .count();
            // At most one thread can hold this client's Reply in-flight.
            assert!(being_handled_by <= 1,
                "Reply {:?} held by multiple receivers", c);

            match self.reply(c) {
                ReplyState::Bound { caller } => {
                    assert_eq!(caller, c,
                        "Reply {:?} bound to wrong caller {:?}", c, caller);
                    assert_eq!(self.thread(t), ThreadState::Blocked,
                        "Reply {:?} Bound but owner {:?} not Blocked", c, t);
                    // Exactly one of: still queued as Call, or being-handled.
                    assert!(queued_as_call ^ (being_handled_by == 1),
                        "Reply {:?} Bound but neither queued-as-Call nor being-handled (queued={}, handled={})",
                        c, queued_as_call, being_handled_by);
                }
                ReplyState::Fresh => {
                    assert_eq!(being_handled_by, 0,
                        "A receiver holds current_reply = {:?} but its Reply is Fresh", c);
                    assert!(!queued_as_call,
                        "{:?} queued as Call but Reply is Fresh", t);
                }
            }
        }

        // 3. current_reply[t] = Some(c) implies reply_state[c] = Bound{c}.
        for (i, cr) in self.current_reply.iter().enumerate() {
            if let Some(c) = cr {
                assert!(matches!(self.reply(*c), ReplyState::Bound { caller } if caller == *c),
                    "current_reply[{}] = Some({:?}) but reply not Bound to it", i, c);
            }
        }

        // 8. At most one Running thread.
        let running = self.thread_state.iter()
            .filter(|s| **s == ThreadState::Running)
            .count();
        assert!(running <= 1, "More than one Running thread: {:?}", self.thread_state);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The seed test. Two clients Call back-to-back before the server runs;
    /// then the server Receives twice and Replies twice. Every reply must
    /// match its client — that's exactly what the old model failed to
    /// guarantee.
    #[test]
    fn two_callers_no_overwrite() {
        let s0 = SystemState::initial();

        // Client1 calls.
        let (s1, _, _) = step(s0, ThreadId::Client1, IpcOp::Call).unwrap();
        assert_eq!(s1.reply(ClientId::One), ReplyState::Bound { caller: ClientId::One });
        assert_eq!(s1.reply(ClientId::Two), ReplyState::Fresh);
        assert_eq!(s1.endpoint, EpState::HasWaiters);
        assert_eq!(s1.thread(ThreadId::Client1), ThreadState::Blocked);

        // Client2 calls. The old bug: Client1's caller info would be lost
        // here. The new model keeps them distinct.
        let (s2, _, _) = step(s1, ThreadId::Client2, IpcOp::Call).unwrap();
        assert_eq!(s2.reply(ClientId::One), ReplyState::Bound { caller: ClientId::One });
        assert_eq!(s2.reply(ClientId::Two), ReplyState::Bound { caller: ClientId::Two });
        assert_eq!(s2.thread(ThreadId::Client2), ThreadState::Blocked);
        assert_eq!(s2.queue.count, 2);
        assert_eq!(s2.queue.head().unwrap().thread, ThreadId::Client1,
            "FIFO: Client1 is still the head");

        // Server receives. Head is Client1 — FIFO.
        let (s3, _, _) = step(s2, ThreadId::Server, IpcOp::Receive).unwrap();
        assert_eq!(s3.current_reply[ThreadId::Server.as_index()], Some(ClientId::One));
        assert_eq!(s3.queue.count, 1);
        assert_eq!(s3.queue.head().unwrap().thread, ThreadId::Client2);
        // Client1 still Blocked (awaiting reply); Client1's Reply still Bound.
        assert_eq!(s3.thread(ThreadId::Client1), ThreadState::Blocked);
        assert_eq!(s3.reply(ClientId::One), ReplyState::Bound { caller: ClientId::One });
        assert_eq!(s3.reply(ClientId::Two), ReplyState::Bound { caller: ClientId::Two });

        // Server replies — unblocks Client1 specifically.
        let (s4, _, _) = step(s3, ThreadId::Server, IpcOp::Reply).unwrap();
        assert_eq!(s4.thread(ThreadId::Client1), ThreadState::Ready);
        assert_eq!(s4.reply(ClientId::One), ReplyState::Fresh);
        assert_eq!(s4.current_reply[ThreadId::Server.as_index()], None);
        // Client2 still queued, still Blocked, Reply still Bound.
        assert_eq!(s4.thread(ThreadId::Client2), ThreadState::Blocked);
        assert_eq!(s4.reply(ClientId::Two), ReplyState::Bound { caller: ClientId::Two });

        // Server receives Client2's call.
        let (s5, _, _) = step(s4, ThreadId::Server, IpcOp::Receive).unwrap();
        assert_eq!(s5.current_reply[ThreadId::Server.as_index()], Some(ClientId::Two));
        assert_eq!(s5.queue.count, 0);
        assert_eq!(s5.endpoint, EpState::Idle);

        // Server replies — unblocks Client2.
        let (s6, _, _) = step(s5, ThreadId::Server, IpcOp::Reply).unwrap();
        assert_eq!(s6.thread(ThreadId::Client2), ThreadState::Ready);
        assert_eq!(s6.reply(ClientId::Two), ReplyState::Fresh);
        assert_eq!(s6.current_reply[ThreadId::Server.as_index()], None);
    }

    #[test]
    fn call_over_bound_reply_is_error() {
        let s0 = SystemState::initial();
        let (s1, _, _) = step(s0, ThreadId::Client1, IpcOp::Call).unwrap();
        // Client1 is now Blocked — it cannot issue ops while blocked.
        // But the real precondition we want to check is that even a
        // hypothetical re-entrant Call (e.g. if the client were unblocked
        // artificially) would fail because its Reply is Bound.
        let s1_artif = s1.with_thread(ThreadId::Client1, ThreadState::Running);
        let err = step(s1_artif, ThreadId::Client1, IpcOp::Call).unwrap_err();
        assert_eq!(err, TransitionError::ReplyAlreadyBound);
    }

    #[test]
    fn bind_reply_never_over_bound() {
        // BFS-level: across every reachable transition, a BindReply effect
        // is emitted only when the prior reply_state for that client was
        // Fresh. This is the strongest guard against the original bug.
        extern crate std;
        use std::collections::{HashSet, VecDeque};

        let mut visited: HashSet<SystemState> = HashSet::new();
        let mut queue: VecDeque<SystemState> = VecDeque::new();
        queue.push_back(SystemState::initial());

        while let Some(state) = queue.pop_front() {
            if !visited.insert(state) { continue; }
            state.check_invariants();
            for who in ThreadId::ALL {
                for op in [IpcOp::Send, IpcOp::Receive, IpcOp::Call, IpcOp::Reply] {
                    if let Ok((next, effects, n)) = step(state, who, op) {
                        next.check_invariants();
                        for e in &effects[..n] {
                            if let IpcEffect::BindReply { caller } = e {
                                assert_eq!(state.reply(*caller), ReplyState::Fresh,
                                    "BindReply emitted while prior state was {:?} (state={:?}, op={:?})",
                                    state.reply(*caller), state, op);
                            }
                        }
                        queue.push_back(next);
                    }
                }
            }
        }
    }

    #[test]
    fn receive_empty_queue_no_reply_binding() {
        // sys_receive on an empty queue blocks; no Reply state changes.
        let s0 = SystemState::initial();
        let (s1, _, _) = step(s0, ThreadId::Server, IpcOp::Receive).unwrap();
        assert_eq!(s1.endpoint, EpState::HasReceiver);
        assert!(s1.current_reply.iter().all(|x| x.is_none()));
        assert_eq!(s1.reply(ClientId::One), ReplyState::Fresh);
        assert_eq!(s1.reply(ClientId::Two), ReplyState::Fresh);
    }

    #[test]
    fn send_fast_path_unblocks_receiver() {
        let s0 = SystemState::initial();
        let (s1, _, _) = step(s0, ThreadId::Server, IpcOp::Receive).unwrap();
        assert_eq!(s1.endpoint, EpState::HasReceiver);

        let (s2, effs, n) = step(s1, ThreadId::Client1, IpcOp::Send).unwrap();
        assert_eq!(s2.endpoint, EpState::Idle);
        assert_eq!(s2.thread(ThreadId::Server), ThreadState::Ready);
        // Sender stays Ready/Running (Send is non-blocking on fast path).
        assert_ne!(s2.thread(ThreadId::Client1), ThreadState::Blocked);
        // Effects include DequeueHead, WriteMsgToBlockedReceiver, UnblockThread, SetEndpointState(Idle).
        let es = &effs[..n];
        assert!(es.iter().any(|e| matches!(e, IpcEffect::DequeueHead)));
        assert!(es.iter().any(|e| matches!(e, IpcEffect::WriteMsgToBlockedReceiver)));
        assert!(es.iter().any(|e| matches!(e, IpcEffect::UnblockThread { who: ThreadId::Server })));
    }

    #[test]
    fn call_fast_path_binds_and_unblocks_server() {
        let s0 = SystemState::initial();
        let (s1, _, _) = step(s0, ThreadId::Server, IpcOp::Receive).unwrap();
        let (s2, effs, n) = step(s1, ThreadId::Client1, IpcOp::Call).unwrap();
        assert_eq!(s2.thread(ThreadId::Server), ThreadState::Ready);
        assert_eq!(s2.thread(ThreadId::Client1), ThreadState::Blocked);
        assert_eq!(s2.current_reply[ThreadId::Server.as_index()], Some(ClientId::One));
        assert_eq!(s2.reply(ClientId::One), ReplyState::Bound { caller: ClientId::One });
        assert_eq!(s2.endpoint, EpState::Idle);
        // BlockCurrent must be last.
        assert!(matches!(effs[n - 1], IpcEffect::BlockCurrent));
    }

    #[test]
    fn exhaustive_state_exploration() {
        extern crate std;
        use std::collections::{HashSet, VecDeque};

        let initial = SystemState::initial();
        let mut visited: HashSet<SystemState> = HashSet::new();
        let mut queue: VecDeque<SystemState> = VecDeque::new();
        queue.push_back(initial);

        let mut transition_count: usize = 0;

        while let Some(state) = queue.pop_front() {
            if !visited.insert(state) { continue; }
            state.check_invariants();

            for who in ThreadId::ALL {
                for op in [IpcOp::Send, IpcOp::Receive, IpcOp::Call, IpcOp::Reply] {
                    if let Ok((next, effects, n)) = step(state, who, op) {
                        next.check_invariants();
                        transition_count += 1;
                        let es = &effects[..n];

                        // BlockCurrent, if present, must be last.
                        if let Some(pos) = es.iter().position(|e| matches!(e, IpcEffect::BlockCurrent)) {
                            assert_eq!(pos, n - 1,
                                "BlockCurrent must be last effect; state={:?}, op={:?}, effects={:?}",
                                state, op, es);
                        }

                        // UnblockThread (if present with ClearReply) must precede ClearReply.
                        let unblock_pos = es.iter().position(|e| matches!(e, IpcEffect::UnblockThread { .. }));
                        let clear_reply_pos = es.iter().position(|e| matches!(e, IpcEffect::ClearReply { .. }));
                        if let (Some(u), Some(c)) = (unblock_pos, clear_reply_pos) {
                            assert!(u < c,
                                "UnblockThread must precede ClearReply: {:?}", es);
                        }

                        // BindReply, if present, means prior reply was Fresh.
                        for e in es {
                            if let IpcEffect::BindReply { caller } = e {
                                assert_eq!(state.reply(*caller), ReplyState::Fresh);
                            }
                        }

                        queue.push_back(next);
                    }
                }
            }
        }

        std::println!(
            "new-IPC state exploration: {} states, {} transitions",
            visited.len(), transition_count,
        );
        assert!(visited.len() > 10, "Too few reachable states: {}", visited.len());
        // Keep the BFS tractable — blow the whistle if the state space
        // explodes unexpectedly (would warn us to canonicalize client symmetry).
        assert!(visited.len() < 10_000,
            "State space too large: {}", visited.len());
    }
}
