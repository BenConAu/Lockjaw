# SMP Support (Phase 11)

## Context

Lockjaw is single-core. All kernel globals use `UnsafeCell + unsafe impl Sync`
justified by "single-core, IRQs masked." The scheduler has a single `current`
index, GIC init wakes one redistributor, and there is one 4KB kernel stack.

Phase 11 adds multi-core support using a **Giant Kernel Lock (GKL) first**
strategy: get secondary CPUs booting, add one coarse lock that serializes all
kernel execution, prove it works, then optimize later.

Target: QEMU virt with `-smp 4`, GICv3, cortex-a53.

## Prerequisites

Before SMP work begins, the scheduler must properly handle idle threads.
Currently, threads that call `sys_yield` in a loop remain Ready and consume
a full time slice every scheduling round. The scheduler needs a `sys_exit`
or `sys_park` syscall that marks a thread as Blocked so idle threads do not
burn CPU cycles. See `docs/tracking/tech-debt.md` for the tracking entry.

## Architecture decisions

### Giant Kernel Lock first

A single raw ticket lock serializes all kernel-mode execution, exactly as
the current "IRQs masked" invariant does for one core but extended to N
cores. This means:

- **All existing kernel code works unchanged.** The UnsafeCell singletons,
  the scheduler, IPC, the page allocator -- all remain single-threaded under
  the GKL. No per-subsystem SpinLock wrappers needed yet.
- **The only multi-core parallelism is userspace-to-userspace.** Different
  cores run different user processes concurrently; kernel entry serializes.
- **Fine-grained locking is Phase E (future).** Per-subsystem SpinLocks and
  per-object IPC locks are deferred until the GKL baseline is proven stable.

### GKL placement

The GKL is **not** in the exception vectors. It is acquired/released in the
handlers that touch kernel state:

- **Syscall handler**: `gkl_lock()` -> dispatch -> `gkl_unlock()`
- **IRQ handler**: `gkl_lock()` -> `irq_dispatch()` -> `gkl_unlock()`
- **`block_current`**: `gkl_unlock()` before wfi, `gkl_lock()` after wfi

Context_switch happens within the locked region. The resumed thread inherits
the lock obligation and releases it when its handler returns.

### GKL implementation -- token-enforced

Raw ticket lock using two `AtomicU32`s (`next_ticket`, `now_serving`).
Ownership tracked by a `GklGuard` token (zero-sized, non-Copy, non-Clone)
that flows through the call chain. The compiler enforces that every lock is
released and no path forgets to unlock.

`GklGuard::inherit()` is restricted to exactly two sites:
1. `thread_entry` -- first run of a newly-created thread
2. post-`context_switch` resume -- the returned guard in `schedule()`

This is a scheduler-internal type. `inherit()` should be `pub(crate)` at
most, ideally in a scheduler-private submodule.

### Context switch under the GKL -- proof obligation

Four paths through the kernel that interact with the GKL. Each is a proof
obligation -- one missed path deadlocks or unlocks without ownership.

- **Path 1 -- Syscall, no block**: lock -> dispatch -> unlock. 1:1.
- **Path 2 -- Syscall with block**: lock -> block_current -> schedule ->
  context_switch -> resumed thread unlocks. 1:1 across threads.
- **Path 3 -- Timer preemption**: lock -> tick -> schedule -> context_switch
  -> resumed thread unlocks. 1:1 across threads.
- **Path 4 -- Thread first run**: inherit -> unlock. 0:1, balances the lock
  from the schedule() that context-switched to this thread.

### Per-CPU data

`TPIDR_EL1` stores a pointer to a `PerCpu` struct in a static array.
Narrow per-field accessors only (no `&mut PerCpu` exposed to safe code).
Contains `cpu_id: u32`, `current_thread_idx: usize`.

### Scheduler adaptation

- `SchedState.current` -> `current_per_cpu: [Option<usize>; MAX_CPUS_MODEL]`
- `step(cpu_id, reason)` operates on `current_per_cpu[cpu_id]`
- Invariants:
  - A thread is Running on at most one CPU total
  - No two `current_per_cpu` entries point at the same thread index
  - `Ready -> Running` transitions are atomic under the GKL
  - For every `current_per_cpu[i] = Some(idx)`, `states[idx] == Running`

### Secondary boot

PSCI `CPU_ON` (function ID `0xC400_0003`) via `hvc #0`. **This is
QEMU-virt-specific** -- real hardware may use SMC or a different PSCI conduit.

### Cross-core wakeup

`unblock_thread()` sends a broadcast SGI (INTID 0, IRM=1) via
`ICC_SGI1_EL1`. Receiving cores wake from wfi, acquire GKL, call `tick()`.

### Memory ordering

`SH_INNER` stays correct for QEMU virt. `DSB ISH` + `TLBI vmalle1is`
already broadcasts. No changes needed until real hardware.

## Commit sequence

### Commit 1: Per-CPU stacks + TPIDR_EL1 per-CPU data

Infrastructure for multiple cores. No behavior change on one core.
Per-CPU stacks in linker script (2MB-aligned, 4 guard+stack pairs).
`PerCpu` struct with narrow accessors via TPIDR_EL1. `MAX_CPUS = 4`.
Guard page setup unmaps all 4 guard pages.

### Commit 2: Secondary core boot via PSCI

All CPUs online and idling. Secondaries do NOT unmask interrupts or enable
the timer (GKL not yet in place). `_secondary_start` assembly entry,
`secondary_main()` in Rust, `secondary_mmu_init()`, PSCI `cpu_on()`.

### Commits 3+4: Giant Kernel Lock + SMP scheduler

One conceptual unit, two git commits. Commit 3 adds GKL mechanism and
activates secondary GIC/timer. Commit 4 adapts scheduler for per-CPU
current threads with per-CPU idle threads. `MAX_THREADS` 8 -> 16.

Commit 3 is the danger zone -- keep it small and brutally explicit.
Bring-up audit checklist for all four GKL paths.

### Commit 5: Cross-core wakeup via SGI

`send_sgi_broadcast()` in gic.rs, SGI dispatch in IRQ handler,
`unblock_thread()` sends SGI after marking Ready.

### Commit 6: Integration test + cleanup

SMP integration test, README update, tech-debt cleanup.

## Future phases (not in this PR)

**Phase E: Fine-grained locking.** Replace GKL with per-subsystem locks,
per-object IPC locks, scheduler-specific lock for context-switch handoff.

**Phase F: Scheduler scalability.** Per-CPU run queues, thread affinity,
work stealing.
