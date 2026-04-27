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
- **object.rs** (320+ lines): HandleEntry, HandleEntry::EMPTY,
  CloseHandleResult, decide_close_handle, TeardownHandleAction,
  decide_teardown_handle, HANDLE_SLOTS_PER_PAGE
- **handle_ops.rs**: HandleError, slot_insert/lookup/remove,
  find_empty_slot, slot_remove_all_by_object, slot_get/set_mapped_va
- **ipc_state.rs**: EpState model, NotificationState, IPC decision
  functions (decide_send/receive/call/reply), raw constants,
  typed conversions, IpcError
- **scheduler.rs**: RoundRobinState, SchedDecision, select_next
- **wait.rs**: compute_ready_mask, readiness checks
- **thread.rs**: SavedContext, Tcb, TcbCreateInfo, ThreadBootstrap,
  Tcb::init_in_place, crash-sensitive offset tests

Host test count as of this audit: 305 unit + 1 doctest.
Updated after extraction work: 387 unit + 1 doctest.

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
- **CloseHandleResult** — DONE. Replaces HandleCleanup as single
  cleanup vocabulary for both sys_close_handle and finish_exit.
  7 host tests.
- **ProcessTeardownPlan** — DONE. Conditional step sequence with
  construction-safe narrowing: CleanupHandleEntriesPtesGone vs
  CleanupHandleEntriesNoAddressSpace. TeardownHandleAction has
  no unmap variant, making illegal state unrepresentable.
  8 + 3 host tests.
- **PageSetHeader refcount/map_count** — DONE (commit 4). inc/dec
  with free-on-zero, 7 host tests.

Current: 387 host tests + 1 doctest.

### Also done since audit

- **#1 IPC decision enums** — DONE. decide_send/receive/call/reply
  in ipc_state.rs. Kernel IPC handlers rewritten to match-on-decision.
  Raw constants, IpcError, typed conversions all moved to types.
  22 new host tests. Push→pull conversion complete.
- **#2 Handle release lifecycle** — DONE. CloseHandleResult replaces
  HandleCleanup. ProcessTeardownPlan with construction-safe teardown
  step variants. Narrow helpers (dec_refcount_and_maybe_free,
  dec_both_and_maybe_free) single-sourced across both paths.
- **#2 Handle table slot operations** — DONE. Pure slot-array logic
  (insert, lookup, remove, rights checking) moved to
  lockjaw-types/src/handle_ops.rs. Kernel functions are now thin
  wrappers: table_slots() for PhysAddr→slice, then delegate.
  Rights::contains() and HandleEntry::EMPTY added. 22 new host tests.
- **#4 SavedContext + TCB layout** — DONE. Moved to thread.rs with
  ThreadBootstrap, Tcb::init_in_place. 10 host tests with pinned
  crash-sensitive offsets.
- **#11 Endpoint state constants** — DONE (part of IPC extraction).
  EP_IDLE, WAIT_KIND_*, REPLY_STATE_* moved to ipc_state.rs.
- **ExceptionContext + ESR decode** — DONE. ExceptionContext with
  pinned ABI offsets, ESR decode functions, classify_sync_exception
  in lockjaw-types/src/exception.rs. 16 host tests.
- **syscall_name** — DONE. Moved to lockjaw-types/src/syscall.rs.

### Remaining

---

## Tier 1: High-value structural extractions

These move meaningful state machines or data structures to
lockjaw-types, enabling host tests for bug classes we've hit.

### 1. IPC decision enums — DONE

Moved to `lockjaw-types/src/ipc_state.rs`. SendDecision,
ReceiveDecision, CallDecision, ReplyDecision with typed inputs
(EpState, WaitKind, ReplyState). Kernel handlers rewritten to
match-on-decision. 22 host tests.

### 2. Handle table slot operations — DONE

Moved to `lockjaw-types/src/handle_ops.rs`. HandleError, find_empty_slot,
slot_insert, slot_lookup, slot_remove, slot_remove_all_by_object,
slot_get/set_mapped_va. Rights::contains() added to rights.rs.
HandleEntry::EMPTY constant in object.rs. Kernel handle_table.rs
functions are thin wrappers: table_slots() then delegate. 22 host tests.

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
| 1 | 5 structural extractions | 5 done (#1, #2, #4, #11 + ExceptionContext) | 0 remaining | — |
| 2 | 5 validation functions | 0 done | ~25 | Low |
| 3 | 6 constants/helpers | 3 done (#11, #13, ExceptionContext), 3 remaining | ~8 remaining | Very low |
| **Total** | **16 items** | **8 done, 8 remaining** | **~33 remaining** | |

Current: 387 host tests + 1 doctest.

---

## Re-evaluation: ranked by push-to-pull conversion value

With #4 (SavedContext/TCB) and the lifecycle series done, rank
remaining items by the rubric: convert raw push to pull or
plan/apply first.

### Priority 1: Push → Pull/Plan-Apply conversions — ALL DONE

1. ~~IPC decision enums (#1)~~ — **DONE.**
2. ~~Handle release lifecycle (#2)~~ — **DONE.** CloseHandleResult,
   ProcessTeardownPlan, construction-safe teardown narrowing.
3. ~~Process/thread teardown~~ — **DONE** (part of #2).

### Priority 2: Layout and decision extractions — ALL DONE

4. ~~ExceptionContext + ESR decode (Codex #1)~~ — **DONE.**
   exception.rs with pinned ABI offsets and classify_sync_exception.
5. ~~Handle table slot operations (#2)~~ — **DONE.** handle_ops.rs
   with pure slot-array operations. 22 host tests.

### Priority 3: Remaining extractions

6. **IRQ binding table (#3)** — clean generic data structure.
   Almost entirely pure. ~6 tests.

7. **Syscall validation batch (#6-10)** — five pure functions.
   Already naturally pull (return Ok/Err). ~25 tests. Low effort.

8. ~~Endpoint state constants (#11)~~ — **DONE** (part of IPC).
9. ~~syscall_name (#13)~~ — **DONE.** Moved to syscall.rs.
10. **Platform constants (#12)** — eliminate duplication
