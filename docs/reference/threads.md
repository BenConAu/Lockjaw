# Threads and Context Switching

## What is a context switch?

A context switch saves one thread's CPU state and restores another's.
On AArch64 that means swapping the stack pointer and a set of
registers. The thread that was running is suspended mid-execution,
and another thread picks up exactly where it left off.

Lockjaw's `context_switch` assembly function (declared at
`src/sched/context.rs:15`) is minimal: it saves only the
callee-saved registers (x19-x30) because those are the ones the
C/Rust calling convention requires a function to preserve. The
caller-saved registers (x0-x18) are either already on the stack
(saved by the compiler as part of normal function calls) or saved
by the exception vector `SAVE_REGS` macro (for preemptive switches
triggered by interrupts).

## How preemptive switching works

Every 10 ms the virtual timer fires and the CPU traps into the
exception vector:

```text
Thread A running on CPU N
  -> Timer IRQ
    -> SAVE_REGS (saves x0-x30, ELR, SPSR onto A's saved frame)
      -> gkl_lock (acquire Giant Kernel Lock — single-writer
                   invariant for all scheduler state)
        -> irq_dispatch -> handle_tick -> scheduler::tick -> schedule
          -> context_switch(A, B)
            [save A's callee-saved regs, store A's SP in its TCB]
            [load B's SP from its TCB, restore B's callee-saved regs]
            [ret — "returns" into B's call chain]
        <- schedule <- handle_tick <- irq_dispatch
      -> gkl_unlock (B will re-take on next syscall/IRQ entry)
    -> RESTORE_REGS (from B's saved frame — restores B's x0-x30, ELR, SPSR)
  -> eret (resumes B at B's saved PC)
  -> Thread B continues running on CPU N
```

The key insight: both threads went through the same IRQ handler
chain. Thread B was previously preempted at the same point. Swapping
SP in the middle makes the return path unwind on B's stack instead
of A's.

## The Giant Kernel Lock

`src/sched/gkl.rs` is the project's single-writer serialization
mechanism. Every kernel entry (syscall handler, IRQ handler) acquires
the GKL before touching kernel state and releases it on `eret` back
to EL0. The contract:

- **GKL is held with IRQs masked.** Code that needs IRQs (`wfi`, EL0
  return) must release the GKL first. This prevents the deadlock
  where a timer tick on a core holding the GKL would re-enter the
  handler and spin forever.
- **The GKL is the model the kernel's `unsafe impl Sync` annotations
  rely on.** Every kernel global serialized "by GKL" is documented
  in [`../tracking/tech-debt.md`](../tracking/tech-debt.md) under
  "UnsafeCell globals serialized only by GKL" — the eventual fix is
  per-subsystem locks, but the GKL is the regime today.

## How new threads start

When a thread is created, its stack gets a synthetic frame — fake
saved registers that look like the thread was previously
context-switched out. The saved LR (link register) points to a
`thread_entry` trampoline, and x19 holds the thread's entry function
pointer.

When the scheduler first switches to a new thread:

1. `context_switch` restores the synthetic callee-saved regs
   (x19 = entry fn, LR = trampoline).
2. `ret` jumps to the trampoline.
3. Trampoline unmasks IRQs (new threads start with interrupts
   disabled).
4. Trampoline calls the entry function via `blr x19`.
5. The entry function runs until the next preemption point.

## Thread entry and the indirect-call discipline

Each TCB stores a `fn() -> !` entry function pointer. The trampoline
calls it via an indirect branch (`blr x19`). This is one of the few
places in the kernel where we use an indirect call.

The stack-depth analysis tool (`cargo xtask check-stack`) can't
trace through indirect calls — it doesn't know which function
`blr x19` resolves to. Every indirect call site must be annotated in
`xtask/stack-annotations.toml` with its known targets. **The build
fails if any indirect call is unannotated.** No silent
underestimation. See [`stack-budget.md`](stack-budget.md) for the
full annotation discipline.

## Scheduler — pure model + executable shell

The scheduling decision lives in `lockjaw-types/src/scheduler.rs` as
a pure function, and the kernel's `src/sched/scheduler.rs` is a thin
shell that executes the decision. Key numbers:

| Constant | Value | Where |
|---|---|---|
| `MAX_THREADS` | 1024 | `lockjaw-types/src/scheduler.rs:178` |
| `MAX_CPUS` | 4 | `:183` |

The pure layer exposes `SchedDecision` (`:39`) and `select_next`
(`:85`); the kernel calls them and mechanically executes the
returned variant (context-switch, idle, etc.). The pure model is
host-tested for the reachable-state space.

On the kernel side, the run queue is per-CPU (each CPU has its own
slot in `SchedState::current_per_cpu`) and the GKL serializes
inter-CPU writes. When a CPU has no Ready thread it parks in
`idle_wait`: release GKL, unmask IRQs, `wfi`, re-acquire GKL on
wake. Any IRQ on that CPU breaks `wfi` and re-enters the scheduler.

## BlockToken — the typed blocking discipline

Holding a `&mut T` to a shared kernel object across a context switch
is a use-after-move bug class (the switch may move the object, run
unrelated paths that mutate it, etc.). Lockjaw enforces "no `&mut`
alive across `block_current`" with a type-state token:

```rust
// src/sched/scheduler.rs:353
pub struct BlockToken(());

pub unsafe fn scoped_mut<'a, T>(ptr: *mut T, _token: &'a mut BlockToken) -> &'a mut T;

pub fn block_current(_token: BlockToken);
```

`scoped_mut` borrows the token; `block_current` consumes the token
by value. Rust's borrow checker prevents moving the token while it
is borrowed, so any `&mut T` derived from a `scoped_mut` call must
be dropped before `block_current` can be reached. The
"no-live-borrows-across-block" invariant becomes a compile error.

## Lifecycle: block, unblock, exit

| Function | What it does |
|---|---|
| `block_current(token)` | Transitions current thread Running -> Blocked, then loops in `schedule` + `wfi` until another path transitions it back to Running. |
| `unblock_thread(tcb)` | Requires the target to be Blocked. If `tcb` is currently parked on some CPU (its index appears in `current_per_cpu`), transitions Blocked -> Running directly so the parked CPU returns from its `wfi` loop. Otherwise transitions Blocked -> Ready and the scheduler picks it up on the next round. The "current must never be Ready" invariant (`SchedState::unblock` at `lockjaw-types/src/scheduler.rs:302`) is what makes the direct-to-Running branch necessary. |
| `exit_current()` -> `finish_exit()` | Two-phase exit: `exit_current` (scheduler.rs:511) transitions current to Exited and stores a PendingExit record; `finish_exit` (`:604`, kernel-internal) drains the slot on the next `schedule` and frees TCB / handle-table resources. |
| `sys_wait_any` (syscall #15) | The deadline-aware blocking primitive. Each TCB carries a `wait_deadline: u64` field that `wake_expired_deadlines` (`:473`) scans once per tick and wakes any thread whose deadline has fired. See [`syscalls.md`](syscalls.md) "wait_any (extended)" for the full surface. |

## Stack safety

Each thread's stack gets a canary value at its base during thread
creation. The scheduler checks the canary of the outgoing thread on
every context switch. If the canary is corrupted (stack overflow),
the kernel panics immediately. See `lockjaw-types/src/constants.rs`
for the canary value.

Per-CPU stacks are 8 KiB usable + a 4 KiB guard page (stride 12 KiB,
`MAX_CPUS = 4`), set up in `src/mm/stack.rs`. Thread stacks (for
threads beyond CPU 0's boot thread) are allocated from the buddy.
The full layout + the `cargo xtask check-stack` budget proof are
documented in [`stack-budget.md`](stack-budget.md).

## What lives where

| Layer | Path | Role |
|---|---|---|
| Pure decision model | `lockjaw-types/src/scheduler.rs` | `SchedDecision`, `SchedState`, `select_next`, reachable-state host tests. |
| Pure TCB layout | `lockjaw-types/src/thread.rs` | `Tcb` struct (`:186`), `TcbCreateInfo` (`:296`), saved-context layout, canary slot. |
| Kernel scheduler shell | `src/sched/scheduler.rs` | `schedule`, `block_current`, `unblock_thread`, `idle_wait`, BlockToken type. |
| GKL | `src/sched/gkl.rs` | `gkl_lock` / `gkl_unlock`, IRQ-mask discipline. |
| Context switch asm | `src/sched/context.rs` | The `context_switch(old_sp_ptr, new_sp_ptr)` declaration; the asm body lives alongside. |
| Per-CPU storage | `src/percpu.rs` | `cpu_id()`, per-CPU TPIDR_EL1 setup. |
| Secondary CPU bringup | `src/arch/aarch64/spin_table.rs`, PSCI in `src/main.rs::secondary_main` | How CPUs 1-3 reach the scheduler. |
