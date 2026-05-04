# Extraction Roadmap

This roadmap lists the remaining push-shaped kernel code that is a
candidate for moving into lockjaw-types as a pure state machine,
plan/apply structure, decision function, or pure data structure. It
also documents what stays kernel-only and why.

For *why* extraction matters and the philosophy behind the architecture,
read [`book-of-lockjaw/01-architecture.md`](book-of-lockjaw/01-architecture.md).
For *how* to apply each pattern, read [`patterns/`](patterns/).

This file replaces the older `types-extraction-plan.md` and
`codex-kernel-architecture-work-items.md`. It is the single roadmap.

Last refreshed: 2026-05-03.

---

## What's already done

Major extractions are complete and represent the gold-standard
exemplars in the codebase:

- **IPC decision protocol** — `decide_send/receive/call/reply` in
  `lockjaw-types/src/ipc_state.rs`. Kernel handlers are
  match-on-decision. Push → pull conversion complete.
- **Handle release lifecycle** — `CloseHandleResult`,
  `decide_teardown_handle`, `ProcessTeardownPlan` with
  construction-safe variants. Illegal states unrepresentable.
- **Handle slot operations** — `handle_ops.rs` with pure
  insert/lookup/remove/find. Kernel `HandleTableRef` is a thin
  wrapper.
- **PageTableWalk + MapWalk + unmap_validated** — Page table walks
  fully extracted; kernel does only memory reads.
- **ProcessTransferPlan** — `sys_create_process` ownership transfer
  decisions. Validation gates the irreversible commit.
- **SavedContext + Tcb layout** — `lockjaw-types/src/thread.rs` with
  pinned crash-sensitive offsets.
- **ExceptionContext + ESR decode** — `lockjaw-types/src/exception.rs`
  with pinned ABI offsets, sync exception classification.
- **Scheduler model** — `SchedDecision`, `select_next`, `SchedState`
  with BFS reachable-state tests.
- **PageSetHeader refcount/map_count** — Lifecycle in lockjaw-types.
- **AddressSpaceBuilder + L3RegionTracker + ScratchCursor** —
  Incremental address space construction with pure decisions.
- **ProcessMapping + PROCESS_MAP_FLAG_EXECUTABLE** — Shared between
  kernel and userspace via lockjaw-types/src/process.rs.

Re-verify the current state with `cargo test -p lockjaw-types` (host
tests) before assuming a roadmap item is unstarted.

---

## Active extraction targets

Ranked by push→pull conversion value: bug class prevented, test gain,
relative effort.

### Priority 1: Push-shaped subsystems (highest review-risk)

These are functions where the kernel orchestrates multi-step
sequencing inline. Sequencing bugs in this shape have historically
needed multiple review rounds to catch.

#### 1. `create_process` orchestration

**Source**: `src/process.rs` `create_process()`

The function is now ~240 lines and accumulates a `ProcessTransferPlan`,
walks user mappings, drives the `AddressSpaceBuilder`/`ScratchCursor`,
sets up the handle table, copies the parent handle, builds the TCB,
unmaps the parent's view, and consumes PageSets — interleaved with
drop guards and the point-of-no-return.

The `ProcessTransferPlan` extraction was the first move; the
`ScratchCursor` extraction was the second. The remaining push shape is
the *outer orchestration*: which steps run in which order, where the
fallible boundary sits, what each guard protects.

**Suggested shape**: a `ProcessCreationPlan` state machine. Kernel
feeds observations (resolved mapping, allocated handle table, copied
TCB), plan returns next action and tracks invariants. This is the
single biggest remaining push-shaped kernel function.

**Effort**: medium — touches several existing pure types, requires
careful guard interaction.

**Test gain**: high — every sequencing decision in process creation
becomes host-testable.

#### 2. `sys_map_pages` VA decision

**Source**: `src/syscall/handler.rs` `sys_map_pages` (~lines 262-317)

The handler chains: lookup PageSet handle, check
`mapped_va_page == 0`, validate VA, read header, call `map_pages_in_existing`,
update handle's `mapped_va_page`, increment refcount. The decision
"can this handle be mapped at this VA?" is inline.

**Suggested shape**: pure decision function
`MapHandleDecision { Ok { ... }, AlreadyMapped, InvalidVa, ... }`.
Kernel observes the handle entry and proposed VA, calls the function,
matches on the result.

**Effort**: low — pure function over a `HandleEntry` and VA.

**Test gain**: medium — covers a class of sys_map_pages bugs (VA
reuse, double-mapping).

#### 3. `pageset_table::alloc_pages` rollback

**Source**: `src/cap/pageset_table.rs` `alloc_pages()` (~lines 104-190)

The function loops allocating data pages, rolling back on failure with
inline match on each allocation result. The rollback decision is
brittle — if the loop structure changes, the rollback can drift.

**Suggested shape**: a `PageSetAllocPlan` that pre-computes the
allocation count and reserves it, separating "what to allocate" from
"how to roll back if it fails." Closer to plan/apply.

**Effort**: medium — requires the kernel to allocate without partial
state visible mid-loop.

**Test gain**: medium — partial-allocation leaks are a known risk.

### Priority 2: Layout extractions and shared types

#### 4. Object header constructors

**Source**: `src/cap/object.rs`, `src/cap/process_obj.rs`

Several places write object headers via scattered literal field
assignments. Replace with `ObjectHeader::new(...)`,
`HandleTableHeader::new(...)`, etc., in lockjaw-types. Same shape as
the existing `Tcb::init_in_place`.

**Effort**: low. **Test gain**: medium (catches missed-init bugs).

#### 5. `ProcessObject` struct definition

**Source**: `src/cap/process_obj.rs`

Move the struct definition and field-layout invariants to
lockjaw-types (same pattern as `HandleEntry`, `Tcb`). Kernel keeps the
narrow accessors that use `KernelRef`/`KernelMut`.

**Effort**: low. **Test gain**: low (mostly layout pinning).

#### 6. IRQ binding table

**Source**: `src/arch/aarch64/irq_bind.rs`

~62 lines of pure data structure logic — bind/lookup/unbind. Extract
as generic `IrqBindingTable<T>`. Kernel keeps the `UnsafeCell`
singleton wrapper.

**Effort**: low. **Test gain**: low–medium (~6 tests). **Notes**: was
priority 3 in the prior roadmap; still applicable.

#### 7. Page-table teardown / free-walk model

**Source**: `src/arch/aarch64/vmem.rs` `free_address_space()`

The walk-and-deallocate logic interleaves PTE inspection with
`page_alloc::dealloc_page` calls. A pure `TeardownWalk` state machine
in lockjaw-types could direct the deallocations the same way
`PageTableWalk` directs reads, making `free_address_space()`
deallocate-only.

**Effort**: medium. **Test gain**: high (page-table teardown is hard
to test today).

#### 8. Boot-memory reservation planning

**Source**: `src/mm/page_alloc.rs` `init_with_gap()`

Pure planning of which physical ranges are free vs reserved. Move to
lockjaw-types, return free ranges; kernel feeds them to the buddy
allocator.

**Effort**: medium. **Test gain**: high (boot memory layout is
currently untested).

#### 9. Stack layout policy

**Source**: `src/mm/stack.rs`

Stride, guard-page offset, canary region, fill window — pure layout
math. Extract; kernel writes/checks the computed range.

**Effort**: low. **Test gain**: medium.

### Priority 3: Validation batch (low-cost wins)

Bundle into `lockjaw-types/src/syscall_validation.rs`. Each is a small
pure function for a specific syscall parameter check. Each catches
exactly the bug class that has surfaced in review.

| # | Function | Source | Tests |
|---|---|---|---|
| 10 | `validate_thread_va(entry, stack_top, stack_base, user_va_end)` | `sys_create_thread` | ~6 |
| 11 | `validate_alloc_flags(flags) -> AllocationMode` | `sys_alloc_pages` | ~3 |
| 12 | `validate_unmap_va(va, mapped_va_page)` | `sys_unmap_pages` | ~3 |
| 13 | `validate_query_va(va, user_va_end)` | `sys_query_mapping` | ~3 |

Effort: low. Total test gain: ~15. The `validate_map_va` function from
the prior roadmap may already be subsumed by `MapHandleDecision`
above; check before re-doing.

### Priority 4: Constants, helpers, cleanups

#### 14. Platform constants

**Source**: `src/arch/aarch64/platform.rs`

`UART0_BASE_PHYS`, `GICD_BASE_PHYS`, `GICR_BASE_PHYS`, `RAM_BASE`,
`DEVICE_MMIO_BASE`, `VIRTUAL_TIMER_INTID`, `MAX_CPUS` — move to
lockjaw-types so MMU, GIC, UART, and stack code use one source of
truth. `KERNEL_LOAD_ADDR` stays in kernel (linker-specific).

#### 15. Scheduler predicates

**Source**: `src/sched/scheduler.rs`

- `can_preempt(active, thread_count, current_state) -> bool`
- `should_flush_tlb(old_process, new_process) -> bool`

Inline today in `tick()` and `schedule()`. Pure predicates.

#### 16. Stack canary validation

**Source**: `src/mm/stack.rs`

`validate_canary(value) -> Result<(), u64>` — compare against
`STACK_CANARY` constant. Kernel keeps the volatile read and panic.

#### 17. Buddy allocator address arithmetic

**Source**: `src/mm/page_alloc.rs`

`page_index(addr)`, `round_up_page(addr)`, `index_to_page(idx)` —
pure address arithmetic. Move to `lockjaw-types/src/addr.rs`.

#### 18. Timer tick policy

**Source**: `src/arch/aarch64/timer.rs`

Bookkeeping/tick decisions are pure. CNTV programming and interrupt
ack stay in-kernel.

#### 19. GIC geometry

**Source**: `src/arch/aarch64/gic.rs`

INTID-to-register math, redistributor geometry, priority-byte
addressing — pure index arithmetic that can move to lockjaw-types.

#### 20. `wait_any` planner

**Source**: `src/syscall/handler.rs` `sys_wait_any`

Readiness snapshot, mask computation, waiter registration plan, wake
cleanup plan. The mask computation already has a pure helper; the
planner around it is push-shaped.

#### 21. Remove `src/elf.rs` shim

The kernel-side `elf.rs` is a shim around `lockjaw_types::elf`. Import
the types crate directly and delete the shim.

#### 22. `OwnedPageList` value object

**Source**: `src/cap/process_obj.rs`

Owned-page dedup/bounds semantics. Pure list operations on a
fixed-size array.

#### 23. `PageSet` value object promotion

**Source**: `src/cap/pageset.rs`

Promote the `PageSet` value object into lockjaw-types; allocation and
rollback stay in the kernel (covered by item 3 above).

#### 24. `ReplyObject` liveness tags

**Source**: `src/ipc/reply.rs`

Make `ReplyObject` state tags match `lockjaw_types::ipc_state::ReplyState`
exactly so there is no parallel vocabulary.

---

## Kernel-only boundary

These categories are *correctly* in the kernel. They cannot be
modeled as pure functions without losing essential behavior. Don't
file extraction PRs against them.

| Zone | Files | Reason |
|---|---|---|
| Inline assembly | `src/arch/aarch64/context.rs`, `src/arch/aarch64/exceptions.rs`, `boot.rs` | Context switches, eret, MSR/MRS, SPSR manipulation. No pure model. |
| MMIO | `src/arch/aarch64/uart.rs`, `gic/v*.rs`, `timer.rs` | Volatile writes to specific addresses. Geometry can be pure (item 19); register I/O stays. |
| Page allocator | `src/mm/page_alloc.rs` (alloc/dealloc paths) | Buddy state mutation under IRQ mask. Address arithmetic can be pure (item 17); state mutation stays. |
| TTBR0 swap + TLB | `src/sched/scheduler.rs` (process switch) | `msr ttbr0_el1`, `tlbi vmalle1is` — must stay inline. |
| Intrusive list operations | `src/ipc/ep_queue.rs` | TCB linked-list pointer surgery. The queue *contract* can be pure; the pointer ops can't. |
| Drop guards | `src/process.rs`, `src/cap/pageset_table.rs` | Coupled to kernel `dealloc` and `free_address_space`. The rollback *plan* could be pure; the actual cleanup stays. |
| GKL acquire/release | `src/sched/gkl.rs` | Disable IRQs, atomic acquire, IRQ restore. Inline. |
| `block_current` loop | `src/sched/scheduler.rs` | `wfi` + GKL release/re-acquire around interrupt window. Cannot be pure. |
| Exception vector setup | `src/arch/aarch64/boot.rs` | Writing VBAR_EL1. |
| `KernelRef`/`KernelMut` | `src/mm/kernel_ptr.rs` | Unsafe pointer wrapper. Pointers and lifetimes by definition. |
| `table_slots` and friends | `src/cap/handle_table.rs` | PhysAddr → `&mut [HandleEntry]` conversion. The slice operations are pure (`handle_ops`); the conversion stays. |

---

## Verification

When you finish an extraction:

- `cargo test -p lockjaw-types --target aarch64-apple-darwin --lib`
  — host tests
- `make test` — full integration suite (all GIC variants)
- Update this file: move the item from the active list to the "What's
  already done" section at the top, with a one-line summary of what
  shape it landed as (decision / state machine / plan / data
  structure).
