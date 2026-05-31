# Syscall Interface

## Why EL0 matters

AArch64 has four Exception Levels (EL0-EL3). Lockjaw runs the kernel at EL1 and userspace at EL0. The privilege boundary between them is enforced by the CPU hardware:

- **EL0 code cannot access kernel memory.** Page table entries have access permission bits (AP field) that distinguish kernel-only (AP=0b00) from user-accessible (AP=0b01). An EL0 access to a kernel-only page triggers a data abort.
- **EL0 code cannot execute privileged instructions.** MSR/MRS to system registers, ERET, TTBR writes — all trap if attempted from EL0.
- **The only way for EL0 to talk to the kernel is SVC.** The `SVC #0` instruction generates a synchronous exception, which traps to the kernel's exception vector table. The kernel handles the request and returns to EL0 via `ERET`.

Without this separation, a buggy or malicious userspace program could overwrite kernel data structures, disable interrupts, or take over the machine.

## The syscall convention

Lockjaw uses a simple register-based calling convention for syscalls:

```
x8     = syscall number
x0-x5  = arguments (up to 6)
SVC #0 = trap instruction

After return (2-register convention):
x0     = SyscallError (0 = success, nonzero = error code)
x1     = return value (for syscalls that produce one)
x2-x4  = additional return values (for multi-value returns,
         e.g. ret4-flavored receive/call carrying a 4-word msg)
```

This is similar to Linux's AArch64 syscall convention (which also
uses x8 for the number and x0-x5 for arguments) but splits the
error and value across two registers instead of overloading x0.
The split removes the "value happens to look like an error code"
ambiguity that single-register conventions live with — every
syscall that returns a value reads x0 for the OK/error decision
and x1 for the payload.

Wrappers in `lockjaw-userlib` follow the same shape: an
`inlateout("x0") arg => err, inlateout("x1") arg => val` asm block
and `if err == 0 { Ok(val) } else { Err(SyscallError(err)) }`. See
`user/lockjaw-userlib/src/syscall.rs` for canonical examples.

## How the trap works

When userspace executes `SVC #0`, the CPU:

1. Saves the current PC to `ELR_EL1` (Exception Link Register)
2. Saves the current PSTATE to `SPSR_EL1` (Saved Program Status Register)
3. Switches to EL1 and jumps to the exception vector at `VBAR_EL1 + 0x400` (Lower EL, AArch64, Synchronous)

The vector stub saves all 31 general-purpose registers plus ELR/SPSR/ESR onto the kernel stack (272 bytes), then calls the Rust handler with a pointer to this saved context.

The handler reads x8 from the saved context, dispatches to the right syscall function, and writes the result back into the saved register frame: x0 gets the `SyscallError` code (0 on success), x1 gets the return value if the syscall produces one, and x2–x4 carry additional return words for multi-value returns. The assembly stub then restores all registers from the stack and executes `ERET`, which returns to EL0 at the instruction after the SVC.

## Separate exception vectors for userspace

The exception vector table has separate groups for "Current EL" (kernel faults) and "Lower EL" (userspace traps). Before Phase 6, all groups pointed to the same handlers. Now:

- **Offset 0x200 (Current EL, Synchronous):** Kernel fault handler — prints details and halts. This fires if the kernel itself hits a data abort, undefined instruction, etc.
- **Offset 0x400 (Lower EL, Synchronous):** Syscall dispatcher. Checks the Exception Class (EC field of ESR_EL1):
  - EC 0x15 = SVC from AArch64 → dispatch syscall
  - Anything else = userspace fault → print and halt the thread

The IRQ vector (offset 0x480) is shared — timer interrupts work the same regardless of whether they preempt kernel or userspace code.

## The EL1 to EL0 drop

To start running user code, the kernel:

1. Sets up a user page table in TTBR0 with user-accessible pages
2. Writes the user entry point to `ELR_EL1`
3. Writes `SPSR_EL1 = 0` (EL0t mode, interrupts enabled)
4. Writes the user stack pointer to `SP_EL0`
5. Executes `ERET` to drop to EL0

Both TTBR0 (user pages) and TTBR1 (kernel pages) are active simultaneously. User code uses TTBR0 addresses. When an exception occurs, the CPU switches to EL1 and the kernel accesses its code and data via TTBR1 (higher-half addresses) — or via the identity map that is temporarily included in TTBR0 (see the identity map workaround below).

## The identity map workaround

The kernel binary is linked at physical addresses (0x40080000) by the linker script. VBAR_EL1 and all function addresses point to these physical addresses. When TTBR0 is replaced with user page tables, the kernel's physical addresses must still be reachable for exception handling to work.

The current workaround: include the kernel's identity map (RAM and device MMIO) as kernel-only entries (AP_RW_EL1) in the user TTBR0. Userspace cannot access these ranges — they fault with a permission error. This is safe but not pure: ideally the kernel would be linked at higher-half VAs and TTBR0 would contain only user mappings.

## Why yield exists

`sys_yield` (syscall 1) is a permanent part of the syscall API. It voluntarily reschedules the calling thread — the thread goes to the back of the run queue and the next ready thread runs.

Every microkernel has this:
- seL4: `seL4_Yield`
- Zircon: `zx_thread_yield`
- Linux: `sched_yield`

Without yield, a thread waiting for something would spin-wait (burning CPU) until the 10ms timer fires and preempts it. With yield, the thread says "I have nothing useful to do right now" and the scheduler immediately picks someone else. This matters for:

- **Server processes** that handle requests and want to return their time slice after replying
- **Polling patterns** where a thread checks a condition and yields between attempts
- **Fairness** between threads with different workloads

## Current syscalls (32)

Source of truth: `lockjaw-types/src/syscall.rs` (constants) +
`src/syscall/handler.rs` (dispatch + handlers).

| # | Name | Arguments | Returns | Description |
|---|------|-----------|---------|-------------|
| 0 | debug_puts | x0=buf VA, x1=len | — | Print a buffer via UART (debug only). Buffer must fit in one page; rejected if it crosses user VA end. |
| 1 | yield | — | — | Voluntary reschedule |
| 2 | send | x0=ep handle, x1-x4=msg | — | Send message on endpoint (non-blocking) |
| 3 | receive | x0=ep handle | x1-x4=msg | Receive message (blocks if queue empty) |
| 4 | call | x0=ep handle, x1=reply handle, x2-x5=msg | x1-x4=reply | Send + block for reply (call/reply IPC) |
| 5 | reply | x0-x3=msg | — | Reply to current bound caller |
| 6 | alloc_pages | x0=count, x1=flags | x1=handle | Allocate physical pages, return PageSet handle (`ALLOC_FLAG_CONTIGUOUS = 1`) |
| 7 | map_pages | x0=handle, x1=VA, x2=flags | — | Map PageSet pages into caller's address space |
| 8 | create_process | x0=mappings VA, x1=mapping_count, x2=entry_point, x3=stack_pageset_id, x4=scratch_pageset_id, x5=parent_handle_to_copy (u64::MAX for none), x6=name VA | — | Create new process from mapping list. Per-arg breakdown in `src/syscall/handler.rs:549`. |
| 9 | create_notification | x0=PageSet handle | x1=handle | Create notification (timeline semaphore) |
| 10 | signal_notification | x0=handle, x1=value | — | Signal notification (must be monotonic) |
| 11 | wait_notification | x0=handle, x1=threshold | x1=value | Wait until counter >= threshold |
| 12 | bind_irq | x0=INTID, x1=notif handle | — | Bind hardware IRQ to notification |
| 13 | create_endpoint | x0=PageSet handle | x1=handle | Create IPC endpoint |
| 14 | recv_nb | x0=ep handle | x1-x4=msg | Non-blocking receive (WOULD_BLOCK if empty) |
| 15 | wait_any | x0=entries ptr, x1=count, x2=deadline | x1=ready bitmask | Wait on objects and/or until deadline. See **wait_any (extended)** below. |
| 16 | export_handle | x0=handle | x1=new index | Duplicate handle into bound caller's table |
| 17 | get_boot_info | — | x1=PageSet handle | Get boot information (DTB PageSet handle) |
| 18 | register_device_page | x0=phys addr | x1=PageSet handle | Register MMIO page as tracked PageSet |
| 19 | query_pageset_phys | x0=PageSet handle, x1=page index | x1=phys addr | Query physical address of a page |
| 20 | create_reply | x0=PageSet handle | x1=handle | Create Reply object for call/reply IPC |
| 21 | exit | — | (never returns) | Exit current thread, free resources |
| 22 | create_thread | x0=entry, x1=stack_top, x2=stack_base, x3=arg | — | Create thread in calling process (shares address space). VA range validated; mapping not checked (faults at EL0 if unmapped). stack_top must be 16-byte aligned. |
| 23 | query_mapping | x0=VA (page-aligned) | x1=mapped (0/1), x2=run_pages | Query mapping state at a user VA. Returns whether the page is mapped and how many consecutive pages share the same state. |
| 24 | close_handle | x0=handle | — | Remove a handle from caller's table, freeing the slot. Does NOT free backing object or pages (no refcounting). |
| 25 | unmap_pages | x0=handle, x1=VA | — | Unmap a previously-mapped PageSet from caller's address space. |
| 26 | query_caller_token | — | x1=token (u64) | Return the per-call identity token for the currently-bound caller. Server-side primitive — read inside a `receive`/`recv_nb` handler. See [`../architecture/02-handle-identity-tokens.md`](../architecture/02-handle-identity-tokens.md). |
| 27 | alloc_dma_pages | x0=count | x1=handle | Allocate `count` pages from the DMA pool (`DmaPoolOrigin`). Pages are physically contiguous and cacheable; the returned PageSet is the only origin `sys_dma_sync_*` will accept. |
| 28 | sched_telemetry | — | x1=ticks, x2=ctx_switches, x3=ttbr0_writes, x4=tick_max | 4-word message return: scheduler counters since boot + worst-case observed tick handler latency. Intended for kernel-health diagnostics. |
| 29 | dma_sync_for_cpu | x0=handle, x1=offset, x2=len | — | Invalidate cache lines covering `[offset, offset+len)` in a `DmaPoolOrigin` PageSet so a subsequent CPU load sees device-DMA'd bytes. Only callable on DMA-pool origins (rejects Buddy origins with `INVALID_PARAMETER`). Driver-internal — `lockjaw-userlib` wraps via `run_dma_transfer`. |
| 30 | dma_sync_for_device | x0=handle, x1=offset, x2=len | — | Clean (write back) cache lines so a subsequent device DMA read sees CPU-written bytes. Mirror of `dma_sync_for_cpu`; same origin restriction. |
| 31 | unmask_irq | x0=INTID | — | Re-enable a previously-bound level-triggered SPI in the GIC after the userspace driver has cleared the source. Rejects INTID < 32 (PPIs/SGIs aren't userspace-bindable). No-op for edge IRQs (which aren't masked in the first place). |

## wait_any (extended)

`sys_wait_any` is the substrate for every blocking primitive in
Lockjaw. The kernel's job is to put a thread to sleep until one of
two things happens — an object becomes ready, or wall-clock time
crosses an absolute monotonic deadline — and to keep the surface
area for that exactly one syscall wide.

### Signature

| Reg | Meaning |
|---|---|
| x0 | `*const WaitEntry` — entries array in caller memory (`null` allowed when `count == 0`). |
| x1 | `u64` — `count`, the number of valid `WaitEntry` slots. Range: `0..=MAX_WAIT_OBJECTS` (currently 4). |
| x2 | `u64` — absolute monotonic deadline in `CNTVCT_EL0` ticks. `u64::MAX` (= `MonoTicks::NO_DEADLINE`) means "no timeout". |
| x8 | `SYS_WAIT_ANY` (= 15). |

Returns `(err, mask)` per the standard 2-register convention:
- `err == 0` and `mask != 0` → object N became ready (bit N set).
- `err == 0` and `mask == 0` → deadline expired before any object
  fired (timeout encoding — see *Three load-bearing details* below).
- `err != 0` → the syscall itself failed validation (bad pointer,
  count out of range, missing handle).

Userspace wrappers in `lockjaw-userlib`:
- `sys_wait_any(entries)` — no-timeout form, passes `NO_DEADLINE` in x2.
- `sys_wait_any_until(entries, deadline)` — deadline-aware form.
- `time::sleep_until(deadline)` / `time::sleep_for(nanos)` — pure-sleep
  helpers built on `count == 0`.

### Three load-bearing details

These are framing decisions that work today but are not eternal
contracts. A future reader who tightens any of them silently
breaks something. Each is a deliberate scaffolding choice — call
them out before "fixing" them.

**1. `count == 0` is a first-class form, not a corner case.**
`sys_wait_any` deliberately carries two roles on one syscall:
- *wait on objects (with optional timeout)* — `count > 0`
- *pure sleep, deadline-only* — `count == 0`, `deadline != NO_DEADLINE`

The collapse onto a single syscall keeps the kernel substrate
small and matches the long-term shape where every blocking
primitive is "wait until any condition fires, or until a
deadline." A reviewer who tightens `validate_wait_count` back to
`>= 1` will silently break `lockjaw_userlib::time::sleep_until` /
`sleep_for` — userland sleep is implemented exactly as
`sys_wait_any(null, 0, deadline)`.

**2. `mask == 0 == timeout` is deliberate scaffolding.**
The encoding works today because there are exactly two wake
sources — object readiness (sets a bit) and deadline expiry (sets
nothing) — and one is the strict complement of the other. A bare
`mask == 0` return is unambiguously the timeout outcome.

The moment a third wake source appears (cancellation, signal-like
interruption, IPC shutdown notice, any spurious unblock path), the
encoding has to change. The migration is clean: switch to a typed
return like `enum WaitResult { Ready(u64), Timeout, Cancelled, … }`
carried across the existing return registers (e.g. tag in x0,
mask in x1). Until then, "no bits set" means "deadline fired" —
do not assume that's permanent.

**3. Wake-up latency is tick-quantized.**
The kernel scans for expired deadlines once per scheduler tick
(currently 10 ms), inside `handle_tick` *before* `scheduler::tick()`
so a just-expired sleeper participates in the same tick's scheduling
decision. Wake ordering is "wake then schedule," not "schedule then
wake" — see `src/arch/aarch64/timer.rs::handle_tick` for the
rationale (the inverted order doubles worst-case wakeup latency).

In practice `sleep_for(N ns)` returns no sooner than the deadline
and at most ~one to two scheduler-tick periods later (one for
request alignment, one of headroom for the post-deadline scan).
On QEMU virt + cortex-a53 a 50 ms request lands in [50 ms, 70 ms];
the integration test in `tests/qemu_integration.sh` pins this
envelope. This is a *lower-bounded* sleep — fine for any spec
requirement stated as a minimum ("wait at least N"), not a high-
resolution facility. When sub-tick precision becomes load-bearing,
the right move is reprogramming `CNTV_TVAL_EL0` to fire at the
earliest pending deadline (Linux's `tickless` mode), not bolting
on a separate facility.

### EL0-direct counter reads

`monotonic_now()` in userspace is a single `mrs CNTVCT_EL0` —
ARMv8 lets EL0 read the virtual counter directly once
`CNTKCTL_EL1.EL0VCTEN` is set, which the kernel does in
`arch::aarch64::timer::enable_el0_counter_reads` at boot (boot
CPU and every secondary). Likewise `cntfreq_hz()` reads
`CNTFRQ_EL0` directly — no clock-read syscall, no vDSO page,
just an architectural register that EL0 is allowed to touch.

This is the same shape Linux's vDSO uses for
`clock_gettime(CLOCK_MONOTONIC)` on ARM64: read CNTVCT in
userspace, scale by CNTFRQ. We're not inventing a model — we're
using the architectural feature the way it's intended.
