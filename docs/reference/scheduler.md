# Scheduler

The scheduler is split into two layers: a pure decision model in
`lockjaw-types/src/scheduler.rs` and a kernel-side executable shell
in `src/sched/scheduler.rs`. The pure model is host-tested; the
kernel shell is the side-effects shim that turns a `SchedDecision`
into a real context switch.

This doc covers the algorithm. For the thread *lifecycle*
(`block_current`/`unblock_thread`/`exit_current`, BlockToken,
context-switch mechanics, stack budget), see
[`threads.md`](threads.md).

## The two layers

| Layer | Path | Role |
|---|---|---|
| Pure model | `lockjaw-types/src/scheduler.rs` | `SchedState`, `SchedDecision`, `SchedReason`, `select_next`, invariants. Host-tested. No kernel side effects. |
| Kernel shell | `src/sched/scheduler.rs` | Owns the `Scheduler` static, holds `SchedState` behind GKL, performs context switches based on the pure model's verdict. |

The pure model is what `cargo test -p lockjaw-types` exercises with
reachable-state BFS. The kernel shell is the thin layer that
mechanically executes whichever variant the pure `select_next`
returned.

## SchedReason — what's asking for a decision

`lockjaw-types/src/scheduler.rs:24`:

```rust
pub enum SchedReason {
    /// Timer tick or voluntary yield. Current is Running and wants
    /// to keep running if nobody else is Ready.
    Preempt,
    /// Current thread just marked itself Blocked. MUST NOT resume
    /// until another code path unblocks it.
    Block,
    /// Current thread is exiting. Transitions Running -> Exited and
    /// selects a successor.
    Exit,
}
```

Preempt fires from the 10 ms timer tick (`scheduler::tick`) and
from explicit `sys_yield`. Block fires from `block_current` after
the caller has transitioned the current thread Running -> Blocked.
Exit fires from `exit_current` and consumes the current thread.

## SchedDecision — the pure verdict

```rust
pub enum SchedDecision {
    SwitchTo(usize),                          // do a context switch
    StayOnCurrent,                            // no switch, keep running
    WaitForInterrupt,                         // nothing Ready, halt
    ExitAndSwitch { exited: usize, next: usize },
    ExitAndHalt   { exited: usize },
}
```

The kernel shell matches on the variant and executes:
- `SwitchTo(idx)` -> `context_switch` to TCB[idx].
- `StayOnCurrent` -> no-op return.
- `WaitForInterrupt` -> release GKL, unmask IRQ, `wfi`, re-take GKL
  on wake (see `idle_wait` in `src/sched/scheduler.rs:988`).
- `ExitAndSwitch { exited, next }` -> stash `exited` for deferred
  `finish_exit`, context-switch to `next`.
- `ExitAndHalt { exited }` -> stash `exited`, `idle_wait`.

## select_next — the algorithm

`lockjaw-types/src/scheduler.rs:85`. Round-robin: start at
`(current + 1) mod thread_count`, walk forward, return the first
slot whose `get_state(idx) == Ready`. If the walk wraps back to
`current` without finding a Ready thread:

| Reason | No Ready found |
|---|---|
| `Preempt` + current Running | `StayOnCurrent` |
| `Preempt` + current Blocked | `WaitForInterrupt` |
| `Block`   | `WaitForInterrupt` (current was just transitioned Blocked) |
| `Exit`    | `ExitAndHalt { exited: current }` |

When a Ready thread *is* found:

| Reason | Result |
|---|---|
| `Preempt` / `Block` | `SwitchTo(found)` |
| `Exit`              | `ExitAndSwitch { exited: current, next: found }` |

That's the whole policy. It's not BFS — it's the simplest possible
round-robin. The fairness argument is "every Ready thread runs
within `thread_count` ticks of any other"; the priority argument
is "there are no priorities, threads are equal." When (if) the
project needs richer scheduling — strict priorities, deadlines,
WCET budgets — the change lands in `select_next` (host-tested)
and the kernel shell rebuilds against the new decision variants.

The function is pure: it takes a `get_state: Fn(usize) ->
SchedThreadState` closure, so it never touches the live scheduler
state. The kernel side feeds it `SchedState::get` (`:271`).

## select_for_idle_cpu — the other selector

`lockjaw-types/src/scheduler.rs:132`. A CPU with no current thread
(secondary CPUs at boot, or a CPU whose thread just exited via
`ExitAndHalt`) cannot use `select_next` — there's no `current`
to rotate from. The companion function:

```rust
pub fn select_for_idle_cpu<F>(thread_count: usize, get_state: F) -> Option<usize>
```

Scans monotonically from index 0 and returns the first Ready
thread, or `None`. The caller is responsible for treating threads
that are "Running on another CPU" as Blocked in the closure — the
pure function doesn't know about other CPUs' current slots. The
kernel's `schedule_from_idle` path does this filtering before
calling in.

## SchedState — the kernel-side mirror

`lockjaw-types/src/scheduler.rs:159`:

```rust
pub struct SchedState {
    pub(crate) current_per_cpu: [Option<usize>; MAX_CPUS],   // = 4
    pub(crate) states: [Option<SchedThreadState>; MAX_THREADS], // = 1024
}
```

- `current_per_cpu[cpu]` = which thread index is currently running
  on `cpu`, or `None` if the CPU has no thread assigned (idle).
- `states[idx]` = the thread's `SchedThreadState` (Running / Ready
  / Blocked / Exited), or `None` for an empty slot.

`MAX_THREADS = 1024` (`:178`) and `MAX_CPUS = 4` (`:183`) are
single-source-of-truth — the kernel re-exports `MAX_CPUS` from
here per the Phase-3 extraction-roadmap entry.

## Invariants — `check_invariants`

`:488`. The state machine enforces these at runtime in debug builds
and is host-tested for them. Per the function body at `:488-538`:

1. **Bounds.** Every `current_per_cpu[cpu] = Some(idx)` has
   `idx < MAX_THREADS` (`:492`).
2. **Live slot.** Every current index points at a `Some(state)`
   slot — no current CPU references an empty slot (`:495`).
3. **Per-CPU uniqueness.** No two CPUs share the same current
   index (`:498-503`).
4. **Every Running thread is current on exactly one CPU.**
   `states[i] == Running` <=> exactly one CPU has
   `current_per_cpu == Some(i)` (`:507-517`).
5. **A current thread must never be Ready.** Ready means "eligible
   to run, waiting for a CPU" — but a current thread already has
   one. The legal current-states are Running (executing), Blocked
   (parked in `block_current`'s wfi on this CPU's stack), and
   Exited (parked in `exit_current`'s halt on this CPU's stack).
   `unblock` upholds this by transitioning Blocked -> Running for
   current threads (not Blocked -> Ready), which is what makes the
   self-wake branch in [`threads.md`](threads.md)'s
   `unblock_thread` row necessary. (`:529-535`)

`SchedState::check_invariants() -> bool` is called from debug-build
assertions in the kernel shell to catch invariant violations before
they corrupt the run queue.

`SchedState::check_invariants() -> bool` is called from debug-build
assertions in the kernel shell to catch invariant violations before
they corrupt the run queue.

## Preemption points

The scheduler runs at four trigger points:

| Trigger | Source | SchedReason |
|---|---|---|
| Timer tick (~10 ms) | `arch::aarch64::timer::handle_tick` | Preempt |
| Explicit `sys_yield` | `handler::sys_yield` (`syscall/handler.rs:288`) | Preempt |
| `block_current(token)` | from any blocking IPC path (`sys_receive`, `sys_call`, `sys_wait_any`) | Block |
| `exit_current()` | from `sys_exit` (`handler.rs` syscall #21) | Exit |

Tick + yield use `SchedReason::Preempt` — the current thread is
still Running and wants to stay if no one else is Ready. Block + Exit
have already transitioned the current thread out of Running before
calling the scheduler.

## Per-CPU mechanics

The scheduler is per-CPU in the slot sense: `current_per_cpu[cpu]`
is each CPU's own current thread. But the run queue is shared —
every Ready thread is a candidate for any CPU. There is no work
stealing because there's no per-CPU run queue to steal from; a CPU
that goes idle calls `select_for_idle_cpu` against the global
state under the GKL.

When the GKL fragments into per-subsystem locks (tracked in
[`../tracking/tech-debt.md`](../tracking/tech-debt.md) "UnsafeCell
globals serialized only by GKL"), the natural next shape is per-CPU
run queues with a periodic-rebalance pass. The pure-model split
makes that change a `select_next` rewrite + a kernel-shell rewrite,
not a 200-line audit of every site that touches scheduler state.

## Wake-up latency

The deadline scanner `wake_expired_deadlines` (`src/sched/scheduler.rs:473`)
runs once per scheduler tick, *before* `scheduler::tick`'s
preempt call — see the timer-handler ordering note in
[`syscalls.md`](syscalls.md) under "wait_any (extended)". Concretely:

- A `sys_wait_any` thread whose deadline expires returns at most
  one to two scheduler-tick periods after the deadline.
- On QEMU virt + cortex-a53 with a 10 ms tick, a 50 ms request
  lands in `[50ms, 70ms]`. The integration test pins this envelope.
- Sub-tick precision is not provided. When required, the right
  move is reprogramming `CNTV_TVAL_EL0` to fire at the earliest
  pending deadline (Linux's "tickless" mode), not bolting on a
  separate facility.

## Where it lives

| File | Role |
|---|---|
| `lockjaw-types/src/scheduler.rs` | Pure model: SchedState, SchedDecision, SchedReason, select_next, select_for_idle_cpu, check_invariants. Host-tested. |
| `src/sched/scheduler.rs` | Kernel shell: `Scheduler` static, GKL-held mutation, context_switch invocation, `block_current` / `unblock_thread` / `exit_current` / `idle_wait`. |
| `src/sched/gkl.rs` | Giant Kernel Lock. |
| `src/sched/context.rs` | `context_switch(old_sp_ptr, new_sp_ptr)` asm declaration. |
| `src/percpu.rs` | Per-CPU `TPIDR_EL1` setup, `cpu_id()`. |
