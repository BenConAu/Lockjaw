# SMP

Lockjaw runs on up to `MAX_CPUS = 4` cores
(`lockjaw-types/src/scheduler.rs:183`). This doc covers
SMP-specific concerns — per-CPU memory layout, the CPU-identity
model, secondary-CPU boot methods, cross-CPU coordination — that
the broader [`scheduler.md`](scheduler.md),
[`threads.md`](threads.md), and [`boot-sequence.md`](boot-sequence.md)
each touch slices of.

## Per-CPU stack region

Each CPU gets a 12 KiB stride at `__per_cpu_stacks`:

```text
  +-----------------+ <- __per_cpu_stacks (2 MiB-aligned)
  | guard page  4K  |    (unmapped — overflow into here faults)
  +-----------------+
  | CPU 0 stack 8K  |    
  +-----------------+
  | guard page  4K  |
  +-----------------+
  | CPU 1 stack 8K  |
  +-----------------+
  | guard page  4K  |
  +-----------------+
  | CPU 2 stack 8K  |
  +-----------------+
  | guard page  4K  |
  +-----------------+
  | CPU 3 stack 8K  |
  +-----------------+ <- __per_cpu_stacks_end
```

Constants: `PER_CPU_STACK_STRIDE = 12288` bytes,
`PER_CPU_STACK_SIZE = 8192` bytes
(`src/mm/stack.rs:14,18`). The 2 MiB alignment of the region's
base is what creates the alignment gap between `__kernel_end` and
`__per_cpu_stacks` that the page allocator's `init_with_gap` frees
back to the buddy (see [`memory-model.md`](memory-model.md)).

A stack canary is written at each CPU's stack base by
`stack::init_canary` (CPU 0) or `stack::init_canary_for_cpu(cpu_id)`
(secondaries). The scheduler checks the outgoing canary on every
context switch — a corrupted canary panics immediately. See
[`stack-budget.md`](stack-budget.md) for the budget proof.

## CPU identity — MPIDR vs cpu_id vs TPIDR_EL1

Three distinct identities for the same CPU:

| Identifier | Source | Stable across boots? | Used by |
|---|---|---|---|
| `MPIDR_EL1.Aff0` | Hardware | Yes, board-specific | DTB matching, secondary entry asm |
| `cpu_id: u32` (0..MAX_CPUS) | Lockjaw assignment | Yes (linear, single-cluster) | All kernel code |
| `TPIDR_EL1` (pointer to `PerCpu`) | Per-CPU init | Per-boot | `cpu_id()` lookup, per-CPU state |

The kernel assigns `cpu_id` linearly from `MPIDR_EL1.Aff0` —
single-cluster, linear topology. Multi-cluster (Aff1 nonzero)
would need a different `MPIDR` -> `cpu_id` mapping; that's an
explicit known limitation documented in `boot.rs:91+`'s
secondary entry comments.

`TPIDR_EL1` is the per-CPU kernel pointer. Each CPU stores its own
`&PerCpu` slot's address there via `percpu::init_percpu(cpu_id)`
(`src/percpu.rs:46`). Reading `cpu_id()` is one `mrs TPIDR_EL1`
+ field-load — no memory access, no TLB walk. This is the
hot-path on every kernel entry.

The per-CPU pointer is set **twice** on each CPU during boot:
once pre-pivot (PA pointer, so a panic during the pivot window
still resolves the per-CPU data through BOOT_L0's identity map),
once post-pivot (L0[1] kernel-image VA so it survives a future
user-TTBR0 install). See [`boot-sequence.md`](boot-sequence.md)
phases 3 and 5.

## Per-CPU data structure — `PerCpu`

`src/percpu.rs:17`:

```rust
#[repr(C)]
struct PerCpu {
    cpu_id: u32,
    current_thread_idx: usize,
}
```

Two fields, both per-CPU-mutable. Accessors are
`percpu::cpu_id()`, `percpu::current_thread_idx()`,
`percpu::set_current_thread_idx(idx)`. The struct is not `pub` —
all access goes through these accessors, which return values, not
references, so no safe code can hold two `&mut PerCpu` to the
same slot.

The static `PERCPU_DATA: PerCpuArray` (`:32`) is a four-element
`[UnsafeCell<PerCpu>; MAX_CPUS]`. The `unsafe impl Sync` (`:30`)
documents the discipline: each CPU touches only its own slot
(indexed by the `cpu_id` it stored in `TPIDR_EL1` at init). No
cross-CPU access to the same slot occurs.

The scheduler-side per-CPU state is separate: `SchedState.current_per_cpu`
in `lockjaw-types/src/scheduler.rs:162` is the source-of-truth
mapping (cpu -> currently-running thread index). `percpu::current_thread_idx`
is a per-CPU local cache to avoid the GKL-held scheduler lookup
on every context switch. The two are kept in sync at every
context-switch boundary.

## Secondary CPU boot methods

DTB tells us how the firmware expects to release each CPU. Two
methods are supported, dispatched from `kmain`'s SMP loop
(`src/main.rs:323`):

### PSCI (preferred — QEMU virt, Pi 4B)

`arch::aarch64::psci::cpu_on(target_mpidr, entry_pa, context_id, hvc)`
at `src/arch/aarch64/psci.rs:24`. Wraps SMC (`hvc=false`) or HVC
(`hvc=true`) into the firmware. The firmware starts the target
CPU at `entry_pa` with `x0 = context_id`. Lockjaw passes
`context_id = target_mpidr` as a defensive default for PSCI
consumers — but the secondary entry asm at
`src/arch/aarch64/boot.rs:95-97` **does not consume x0**; it reads
`MPIDR_EL1.Aff0` directly so the same `_secondary_start` symbol
works for both PSCI (x0 set) and spin-table (x0 undefined).

### Spin-table (legacy, some embedded boards)

`arch::aarch64::spin_table::write_release_addr(release_addr, entry_pa)`
at `src/arch/aarch64/spin_table.rs:18`. Each CPU's firmware
prologue parks it in a WFE loop polling a per-CPU release address.
The kernel writes `entry_pa` to that address, then a single `SEV`
after all writes wakes all secondaries at once.

The DTB exposes which method to use via the CPU node's
`enable-method` property; `lockjaw_types::fdt::SmpMethod`
classifies into `Psci { hvc }`, `SpinTable`, or `None`.

## Per-CPU GIC initialization

The GIC has per-CPU state (redistributor on GICv3, banked registers
on GICv2) that each CPU must initialize itself. Secondary CPUs call
`arch::aarch64::gic::init_cpu(cpu_id)` from `secondary_main`. This
is what enables the timer interrupt + any other PPI-class IRQs for
that CPU. Without it, the secondary parks in `wfi` and never wakes.

GIC initialization layering:
- **Global init** (GICD config, SPI defaults) — CPU 0 does this
  once during `kmain` phase 5.
- **Per-CPU init** (`gic::init_cpu(cpu_id)`) — every CPU does this
  for its own redistributor / banked registers. CPU 0 also.

## Cross-CPU coordination

Currently, all cross-CPU coordination is **implicit, via the GKL**.
There is no SGI-based wake-up mechanism: a secondary CPU parked in
`idle_wait` wakes on the next timer tick (which is per-CPU, fires
independently on every CPU's CNTV) and re-enters the scheduler
under GKL.

This means worst-case wake latency for "thread becomes Ready on
CPU A while CPU B is idle" is up to one tick (10 ms). The
follow-up work — SGI broadcast on `unblock_thread` so an idle CPU
wakes immediately — is in
[`../tracking/yagni-parking-lot.md`](../tracking/yagni-parking-lot.md)
as built-then-removed (the SGI broadcast was prototyped early and
removed when GKL serialization made it redundant for the workloads
Lockjaw runs today).

The end-state with per-subsystem locks instead of GKL will need
SGI wake to make cross-CPU thread migration responsive. The
infrastructure is sketched in the YAGNI entry; reviving it is the
right move when the GKL bottleneck shows in real workloads.

## The GKL regime — single-writer SMP

Every kernel entry (syscall, IRQ, exception) acquires the Giant
Kernel Lock before touching kernel state. While the GKL is held,
the holding CPU is the only one mutating scheduler state, handle
tables, page allocators, etc. Other CPUs are either:

- Running EL0 code (no kernel state contention),
- Spinning on `gkl_lock`,
- Or parked in `wfi` because they had nothing to run.

The GKL is held *with IRQs masked* on the holder — this prevents
the timer-re-entry deadlock (a timer tick on a CPU holding the
GKL would re-enter the IRQ handler and spin forever on the lock).
Release happens before `eret` to EL0 in the userspace-sync and
IRQ handlers (see [`exception-handling.md`](exception-handling.md)).

The end-state is per-subsystem locks (scheduler, page allocator,
KVM, handle table). The GKL gets us SMP-safe today with linear
serialization; the breakup happens when the contention shows. See
[`../tracking/tech-debt.md`](../tracking/tech-debt.md)
"UnsafeCell globals serialized only by GKL".

## What's NOT done

Deliberate scope cuts:

- **No SGI cross-CPU wake.** A thread unblocked on CPU A while
  CPU B is idle will wait until CPU B's next tick to pick it up.
  See the yagni-parking-lot entry above.
- **No work stealing.** The run queue is global; CPUs that go
  idle scan the global state via `select_for_idle_cpu`. Per-CPU
  run queues + work stealing land if the GKL breakup ever needs
  them.
- **No CPU hotplug.** All CPUs come up at boot; none are taken
  offline at runtime. PSCI provides `CPU_OFF`/`AFFINITY_INFO`
  but Lockjaw doesn't call them.
- **No NUMA awareness.** Memory allocation is uniform; there is
  no per-CPU buddy or per-CPU KVM pool.
- **Single-cluster, linear MPIDR topology.** Multi-cluster boards
  would need a richer `MPIDR -> cpu_id` mapping than `Aff0`.

Each is a future extension point if a real need surfaces. The
substrate is shaped so each lands as a focused change in its own
layer rather than a cross-cutting rewrite.
