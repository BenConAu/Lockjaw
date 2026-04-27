# lockjaw-types extraction plan

Audit conducted 2026-04-26. Six parallel agents examined every kernel
subsystem to identify pure state, logic, and algorithms that can move
from the kernel to lockjaw-types for host testing.

Guiding principle: the kernel should consist of inline assembly and
thin wrappers around lockjaw-types objects and functions. All decision
logic, state machines, validation, and data structures that don't
require kernel APIs (alloc, PTE ops, scheduler, unsafe pointers)
belong in lockjaw-types.

## Integration shapes (rubric for prioritization)

Three shapes describe how the kernel integrates with lockjaw-types,
from safest to riskiest:

### Pull

The kernel asks a pure model "what happens next?" The model owns
sequencing and branching. The kernel executes the returned effects.

Examples: PageTableWalk (step returns Continue/Done/Fault),
scheduler select_next, unmap_validated (two-pass validate-then-clear
driven by types).

**Bugs are rare here.** The model protects sequencing.

### Plan/Apply

The model builds or returns a plan/decision object. The kernel
applies it in the live system. The model doesn't drive sequencing
but it concentrates the decision into one testable point.

Examples: ProcessTransferPlan (build plan, validate, then kernel
applies), handle_cleanup() (returns HandleCleanup struct, kernel
executes), ThreadBootstrap (returns saved_context + saved_sp
together).

**Bugs happen when the kernel adds steps outside the plan.** The
lifecycle review bugs (double-decrement, partial commit) were all
cases where the kernel did something the plan didn't tell it to.

### Push (raw)

The kernel calls helper functions one at a time in whatever order
it chooses. The model provides tools but does not protect sequencing.

Examples: inc_refcount/dec_refcount (kernel must call at the right
points), on_thread_exit (kernel must call at the right lifecycle
moment), EP_IDLE/EP_HAS_RECEIVER constants (kernel branches inline).

**This is where most bugs live.** Every lifecycle review finding was
push-side: wrong order, skipped step, double call.

### The rule

- Prefer **pull** when the whole transition can be modeled cleanly.
- Otherwise prefer **plan/apply** over raw push.
- Treat **raw push as the weakest form** and the highest review-risk.
- When evaluating extraction priority, convert raw push to pull or
  plan/apply first — that is where the biggest quality wins are.

## Current state

lockjaw-types already contains substantial extracted logic:

- **page_table.rs** (1100+ lines): PageTableEntry, PageTableWalk,
  MapWalk, unmap_validated — gold standard for extraction
- **vmem.rs** (360+ lines): page_table_indices, validate_mapping,
  classify_l2_entry, select_attrs, build_user_page
- **buddy.rs** (470 lines): full buddy allocator with 25+ tests
- **pageset_table.rs** (500+ lines): PageSetHeader, PageSetTable,
  refcount/map_count lifecycle
- **process.rs** (350+ lines): ProcessTransferPlan, ProcessLifecycle,
  on_thread_exit/create
- **object.rs** (320+ lines): HandleEntry, HandleCleanup,
  handle_cleanup, HANDLE_SLOTS_PER_PAGE
- **ipc_state.rs**: EpState model, NotificationState
- **scheduler.rs**: RoundRobinState, SchedDecision, select_next
- **wait.rs**: compute_ready_mask, readiness checks

Host test count as of this audit: 305 unit + 1 doctest.

---

## Status (updated 2026-04-26)

### Done

- **#4 SavedContext + TCB layout** — DONE. SavedContext, Tcb,
  TcbCreateInfo, ThreadBootstrap moved to lockjaw-types/src/thread.rs.
  Crash-sensitive offsets pinned with exact numeric tests (name@256,
  current_syscall@216, current_syscall_args@224). Tcb::init_in_place
  for zero-copy initialization. 10 new host tests.
- **ProcessTransferPlan** — DONE (commit 3 series). Pure ownership
  transfer decisions with 11 host tests.
- **HandleCleanup** — DONE (commit 4 series). Single-authority
  cleanup decision for handle release with 6 host tests.
- **PageSetHeader refcount/map_count** — DONE (commit 4). inc/dec
  with free-on-zero, 7 host tests.

Current: 315 host tests + 1 doctest.

### Remaining

---

## Tier 1: High-value structural extractions

These move meaningful state machines or data structures to
lockjaw-types, enabling host tests for bug classes we've hit.

### 1. IPC decision enums

**Source**: `src/ipc/endpoint.rs`, `src/ipc/reply.rs`

The kernel's IPC handlers (ipc_send, ipc_receive, ipc_call,
ipc_reply) contain state-machine decisions interleaved with TCB
mutations and scheduler calls. The decisions are pure:

- **SendDecision**: EP_HAS_RECEIVER -> deliver immediately, else
  queue and block
- **ReceiveDecision**: EP_HAS_WAITERS -> dequeue (Send vs Call
  determines unblock vs bind-reply), EP_IDLE -> queue as receiver
- **CallDecision**: reply must be Fresh, then same as send with
  reply binding
- **ReplyDecision**: reply must be Bound, deliver to caller

Extract as pure functions in lockjaw-types that take endpoint state
and return a decision enum. Kernel applies the side effects.

**Tests**: all state transitions, error conditions (double-receive,
reply-not-bound, reply-already-bound).

### 2. Handle table slot operations

**Source**: `src/cap/handle_table.rs` (handle_insert, handle_lookup,
handle_remove)

The slot-finding and rights-checking logic is pure array/bitmask
operations on `&[HandleEntry]` slices:

- `find_empty_slot(slots) -> Option<usize>`
- `check_rights(required, present) -> bool`

Currently interleaved with unsafe KernelMut access via table_slots().

**Tests**: slot reuse after removal, full-table rejection, rights
bitmask edge cases (requesting READ|GRANT when only READ|WRITE).

### 3. IRQ binding table

**Source**: `src/arch/aarch64/irq_bind.rs`

Almost entirely pure data structure logic. 62 lines. Extract as
generic `IrqBindingTable<T>` with bind/lookup/unbind. Kernel keeps
the UnsafeCell singleton wrapper.

**Tests**: bind/lookup/rebind, full table, INTID bounds, duplicate
detection.

### 4. SavedContext and TCB initialization — DONE

Moved to `lockjaw-types/src/thread.rs`. SavedContext, Tcb,
TcbCreateInfo, ThreadBootstrap, Tcb::init_in_place. 10 host tests
including pinned crash-sensitive offsets.

### 5. ProcessObject struct

**Source**: `src/cap/process_obj.rs`

The struct definition should move to lockjaw-types (same pattern as
HandleEntry). The kernel keeps the narrow accessors that use
KernelRef/KernelMut.

**Tests**: size fits in page, field layout, owned_pages array
bounds.

---

## Tier 2: Syscall validation extraction

Small pure functions for parameter validation. Bundle into
`lockjaw-types/src/syscall_validation.rs`. These catch the exact
bugs we've hit in review.

### 6. validate_map_va(va, user_va_end)

**Source**: `src/syscall/handler.rs` sys_map_pages

Rejects VA=0 (sentinel for "not mapped"), unaligned VAs, and
kernel-space addresses.

**Tests**: aligned user VA passes, zero rejected, 0x1001 rejected,
USER_VA_END rejected.

### 7. validate_thread_va(entry, stack_top, stack_base, user_va_end)

**Source**: `src/syscall/handler.rs` sys_create_thread

Validates entry point in user range, stack_base < stack_top, stack_top
16-byte aligned (AArch64 ABI).

**Tests**: valid config passes, stack_base >= stack_top rejected
(regression for a real bug), unaligned stack rejected, OOB entry
rejected.

### 8. validate_alloc_flags(flags)

**Source**: `src/syscall/handler.rs` sys_alloc_pages

Rejects unknown flag bits. Returns AllocationMode enum (Contiguous
vs Scattered).

**Tests**: flags=0 -> Scattered, flags=1 -> Contiguous, flags=2 ->
error (reserved bit).

### 9. validate_unmap_va(va, mapped_va_page)

**Source**: `src/syscall/handler.rs` sys_unmap_pages

Rejects unmap of unmapped handle (mapped_va_page=0) and VA mismatch
(va != mapped_va_page << 12).

**Tests**: not-mapped rejected, correct VA passes, wrong VA rejected.

### 10. validate_query_va(va, user_va_end)

**Source**: `src/syscall/handler.rs` sys_query_mapping

Rejects unaligned and out-of-range VAs.

**Tests**: aligned user VA passes, unaligned rejected, kernel VA
rejected.

---

## Tier 3: Constants and small helpers

Quick wins — move constants to types, eliminate duplication, extract
trivial predicates.

### 11. Endpoint state constants

**Source**: `src/ipc/endpoint.rs`

`EP_IDLE`, `EP_HAS_WAITERS`, `EP_HAS_RECEIVER`, `WAIT_KIND_NONE`,
`WAIT_KIND_SEND`, `WAIT_KIND_RECEIVE`, `WAIT_KIND_CALL` — move to
`lockjaw-types/src/ipc_state.rs`. Verify parity with EpState enum
in tests.

### 12. Platform constants

**Source**: `src/arch/aarch64/platform.rs`

`UART0_BASE_PHYS`, `GICD_BASE_PHYS`, `GICR_BASE_PHYS`, `RAM_BASE`,
`DEVICE_MMIO_BASE`, `VIRTUAL_TIMER_INTID`, `MAX_CPUS` — move to
lockjaw-types. `KERNEL_LOAD_ADDR` stays in kernel (linker-specific).

### 13. syscall_name()

**Source**: `src/crash.rs`

Already pure (`match num { 0 => "sys_debug_putc", ... }`). Move to
`lockjaw-types/src/syscall.rs` where the syscall numbers are defined.

### 14. Scheduler predicates

**Source**: `src/sched/scheduler.rs`

- `can_preempt(active, thread_count, current_state) -> bool`
- `should_flush_tlb(old_process, new_process) -> bool`

Small decision functions currently inline in tick() and schedule().

### 15. Stack canary validation

**Source**: `src/mm/stack.rs`

`validate_canary(value) -> Result<(), u64>` — compare against
STACK_CANARY constant. Kernel keeps the volatile read and panic.

### 16. Buddy allocator helpers

**Source**: `src/mm/page_alloc.rs`

`page_index(addr)`, `round_up_page(addr)`, `index_to_page(idx)` —
pure address arithmetic. Move to `lockjaw-types/src/addr.rs` or
`buddy.rs`.

**Tests**: round-trip conversion, page boundary rounding.

---

## What stays in the kernel

These categories of code are inherently kernel-side:

- **Inline assembly**: context_switch, exception vectors, TTBR0/TLB
  manipulation, MSR/MRS instructions
- **Page allocation**: buddy allocator state, alloc/dealloc calls
- **Unsafe pointer access**: KernelRef/KernelMut, table_slots()
- **Hardware registers**: GIC, timer, UART, MAIR/TCR/SCTLR
- **Scheduler orchestration**: block_current/schedule loops, wfi,
  GKL acquire/release
- **Drop guards**: PageGuard, Ttbr0Guard (tied to kernel dealloc)

---

## Estimated test coverage gain

| Tier | Items | Status | New tests | Effort |
|------|-------|--------|-----------|--------|
| 1 | 5 structural extractions | 1 done (#4), 4 remaining | ~30 remaining | Medium |
| 2 | 5 validation functions | 0 done | ~25 | Low |
| 3 | 6 constants/helpers | 0 done | ~15 | Very low |
| **Total** | **16 items** | **1 done, 15 remaining** | **~70 remaining** | |

Current: 315 host tests. After: ~385 host tests.

---

## Re-evaluation: ranked by push-to-pull conversion value

With #4 (SavedContext/TCB) and the lifecycle series done, rank
remaining items by the rubric: convert raw push to pull or
plan/apply first.

### Priority 1: Push → Pull conversions (highest bug-prevention ROI)

1. **IPC decision enums (#1)** — currently the most push-heavy code
   in the kernel. Four operations, each with inline state checks,
   branching, and subtle interactions. Convert to pull: types returns
   SendDecision/ReceiveDecision/CallDecision/ReplyDecision, kernel
   executes side effects. Both docs agree. ~15 tests.

2. **PageSet/handle release lifecycle** — sys_close_handle and
   finish_exit are still "kernel sequences a delicate protocol
   around small pure helpers." handle_cleanup() is plan/apply but
   the kernel still owns unmap-before-remove ordering. Needs a
   stronger plan/apply shape where the decision object captures
   the full sequence. Both docs flag this (our #2, Codex #3/#16).

3. **Process/thread teardown** — finish_exit's LastThread arm is
   push: kernel sequences owned_pages free, address space free,
   handle table walk, process page free. Should be plan/apply:
   types returns a CleanupPlan, kernel executes. Codex #5.

### Priority 2: Plan/Apply improvements

4. **ExceptionContext + ESR decode (Codex #1)** — frame layout is
   as critical as SavedContext. ESR classification (SVC vs data
   abort vs instruction abort) is pure decision logic currently
   inline in the exception handler. Pull candidate.

5. **Handle table slot operations (#2)** — rights checking and
   slot finding. Currently push (kernel calls at the right time).
   Simple enough that plan/apply is sufficient. ~8 tests.

### Priority 3: Easy wins (already pull-shaped, just not extracted)

6. **Syscall validation batch (#6-10)** — five pure functions.
   Already naturally pull (return Ok/Err). Just not extracted yet.
   ~25 tests. Low effort.

7. **Endpoint state constants (#11)** — move to types
8. **syscall_name (#13)** — trivial move
9. **Platform constants (#12)** — eliminate duplication
10. **IRQ binding table (#3)** — clean data structure extraction
