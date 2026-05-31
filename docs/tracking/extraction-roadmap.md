# Extraction Roadmap

This roadmap lists the remaining push-shaped kernel code that is a
candidate for moving into lockjaw-types as a pure state machine,
plan/apply structure, decision function, or pure data structure. It
also documents what stays kernel-only and why.

For *why* extraction matters and the philosophy behind the architecture,
read [`../architecture/01-architecture.md`](../architecture/01-architecture.md).
For *how* to apply each pattern, read [`patterns/`](../architecture/patterns/).

This file replaces the older `types-extraction-plan.md` and
`codex-kernel-architecture-work-items.md`. It is the single roadmap.

Last refreshed: 2026-05-04. Host test count at refresh: 523.

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
- **ProcessCreationPlanBuilder + ValidatedProcessCreationPlan + dedup_add_header**
  — `sys_create_process` ownership transfer decisions. Pure structural
  validate produces an owned apply-phase token; kernel-state
  consume_pageset_validate is the second gate before any apply.
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
  kernel and userspace via `lockjaw-types/src/process.rs`.
- **validate_process_mappings** — Buffer-capacity check is in
  `lockjaw-types/src/vmem.rs`; `create_process` calls it directly.
- **wait validation** — `validate_wait_count`, `WaitEntry`,
  `MAX_WAIT_OBJECTS` are in `lockjaw-types/src/wait.rs`; kernel
  imports them directly.

### Userspace extractions (canonical exemplars)

These landed in the posix-server work and prove the patterns scale
beyond the kernel:

- **ElfLoadPlan** (`lockjaw-types/src/elf_loader.rs`) — plan/apply
  shape; loader populates the plan, then applies mappings in a
  separate loop.
- **posix dispatch + compute_va_layout + write_linux_stack**
  (`lockjaw-types/src/posix.rs`) — decision function and two
  plan-like pure functions; all three are host-tested.
- **PROCESS_MAPPINGS_PER_PAGE** (`lockjaw-types/src/process.rs`) —
  single constant shared between kernel and posix-server so both use
  the same per-page capacity.

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

#### 1. `create_process` outer orchestration — DONE

`ProcessCreationPlanBuilder` and `ValidatedProcessCreationPlan` live
in `lockjaw-types/src/process.rs`. `validate(self)` consumes the
builder and returns an owned token; apply takes the token by value.
The pure layer carries only structural facts (counts, parent-copy
metadata) — the deduplicated header list lives in the proc page
(`ProcessObject::consumed_headers`), kept off the kernel sync-
exception stack. `validate` takes `unique_header_count` as an
explicit parameter sourced from the proc-page storage, so the
"how many unique headers were consumed" count has a single source
of truth and cannot drift between the kernel storage and a builder
mirror. Per-mapping/per-stack registration goes through one kernel
helper (`record_mapping_into_plan` / `record_stack_into_plan`) that
wraps the proc-page write and the builder count update — callers
can't accidentally do one without the other.

The kernel splits the work between `provision_resources` (heavy
frame: address-space builder, scratch state, per-iteration locals,
kernel-page allocation) and `create_process` (orchestrator: holds
the builder, the guards, runs validate + apply). The plan token is
constructible only by the pure validate, so a future edit cannot
move a fallible step past the point-of-no-return without
restructuring the type seam.

Test gain: ~16 host tests (at-most-once invariants, post-consume
handle-table capacity, caller_token / rights preservation, dedup
ordering and overflow). Pure dedup logic factored into the free
function `dedup_add_header(header, &mut [u64], &mut usize)` which
the kernel calls with the proc page's storage slice.

#### 2. `sys_map_pages` VA decision

**Source**: `src/syscall/handler.rs` `sys_map_pages()` (line 321)

The handler chains: lookup PageSet handle, check `mapped_va_page == 0`
(not-already-mapped sentinel), validate VA, call `map_pages_in_existing`,
update handle's `mapped_va_page`, increment refcount. The decision
"can this handle be mapped at this VA?" is inline with no pure
vocabulary.

**Suggested shape**: pure decision function returning
`MapHandleDecision { Ok { ... }, AlreadyMapped, InvalidVa, ... }`.
Kernel observes the handle entry and proposed VA, calls the function,
matches on the result.

**Effort**: low — pure function over a `HandleEntry` and VA.

**Test gain**: ~6 tests covering VA reuse, double-mapping, sentinel
collision. This is a class that has required review rounds before.

#### 3. `pageset_table::alloc_pages` rollback

**Source**: `src/cap/pageset_table.rs` `alloc_pages()` (line 104)

The function loops allocating data pages with inline rollback on
failure: `for j in 0..i { dealloc_page(...) }`. The rollback logic is
coupled to the loop counter — if the loop structure changes, the
rollback can drift silently.

**Suggested shape**: a `PageSetAllocPlan` that separates "how many to
allocate" from "how to roll back." The actual allocator still lives in
the kernel; the plan captures the pre-commit decisions.

**Effort**: medium — requires defining clean input/output types.

**Test gain**: ~6 tests covering partial-allocation edge cases. Partial
allocation leaks are a known silent failure mode.

### Priority 2: Layout extractions and shared types

#### 4. Object header constructors

**Source**: `src/cap/object.rs` (line 26), `src/cap/reply.rs` (line 34)

`ObjectHeader` and `HandleTableHeader` are in lockjaw-types but have
no `::new()` constructors. The kernel writes struct literals inline
(e.g. `ObjectHeader { obj_type, page_count: 1, refcount: 0 }`). If a
new required field is added to the header, every literal needs
updating and the compiler won't flag missed sites.

**Suggested shape**: `ObjectHeader::new(obj_type, page_count)` and
`HandleTableHeader::new(slot_count)` in lockjaw-types. Same pattern as
the existing `Tcb::init_in_place`.

**Effort**: low — add two `fn new()` methods, update ~4 callsites.

**Test gain**: ~4 tests (construction-safe: catches missed-init on
field additions).

#### 5. `ProcessObject` struct definition

**Source**: `src/cap/process_obj.rs` (line 15)

The struct definition and its field-layout invariants are kernel-only.
Moving to lockjaw-types (same pattern as `HandleEntry`, `Tcb`) enables
host tests against layout size and field offsets.

**Effort**: low — struct move, kernel keeps `KernelRef`/`KernelMut`
accessors.

**Test gain**: low (~3 layout-pinning tests). Worth doing as a precursor
to item 22.

#### 6. IRQ binding table

**Source**: `src/arch/aarch64/irq_bind.rs` (~62 lines)

Pure data structure logic — bind/lookup/unbind over a fixed-size array.
Wrapped in a `static mut` singleton in the kernel. Extract as generic
`IrqBindingTable<T>`. Kernel keeps the `UnsafeCell` singleton wrapper.

**Effort**: low. **Test gain**: ~6 tests (bind/lookup/unbind, bounds
checking, reserved-INTID rejection).

#### 7. Page-table teardown / free-walk model

**Source**: `src/arch/aarch64/vmem.rs` `free_address_space()` (line 210)

Walks L1/L2/L3 page table entries and calls `page_alloc::dealloc_page`
at each level. A pure `TeardownWalk` state machine could direct
which physical addresses to free at each step (analogous to
`PageTableWalk` directing which addresses to read), making
`free_address_space` a dealloc-only loop.

**Effort**: medium. **Test gain**: high (~10 tests). Page-table teardown
is currently completely dark to host tests — any off-by-one in level
counting silently leaks pages.

#### 8. Boot-memory reservation planning

**Source**: `src/mm/page_alloc.rs` `init_with_gap()` (line 55)

Computes which physical ranges are free vs reserved (kernel image,
stacks, DTB). Pure math over address ranges. Extract to lockjaw-types;
kernel feeds the returned free ranges to the buddy allocator.

**Effort**: medium. **Test gain**: high (~8 tests). Boot memory layout
is currently entirely untested; mistakes here produce silent OOMs or
reserved-range corruption.

#### 9. Stack layout policy

**Source**: `src/mm/stack.rs`

`PER_CPU_STACK_STRIDE` (12288) and `PER_CPU_STACK_SIZE` (8192) are
local constants with a comment "must match linker.ld and boot.rs."
These should be a shared `StackLayout` struct with derived fields so
the linker script, boot.rs, and stack.rs all reference one definition.
The canary read/write stays volatile in-kernel.

**Effort**: low. **Test gain**: ~4 tests (stride/size arithmetic,
guard-page offset). The "must match" comment is a bug waiting to happen.

### Priority 3: Validation batch (low-cost wins)

Bundle into `lockjaw-types/src/syscall_validation.rs`. Each is a small
pure function for a specific syscall parameter check. Each catches
exactly the bug class that has surfaced in review.

| # | Function | Source | Tests |
|---|---|---|---|
| 10 | `validate_thread_va(entry, stack_top, stack_base, user_va_end)` | `sys_create_thread` (line 757) | ~6 |
| 11 | `validate_alloc_flags(flags) -> AllocationMode` | `sys_alloc_pages` (line 279) | ~3 |
| 12 | `validate_unmap_va(va, mapped_va_page)` | `sys_unmap_pages` (line 857) | ~3 |
| 13 | `validate_query_va(va, user_va_end)` | `sys_query_mapping` (line 832) | ~3 |

Effort: low. Total test gain: ~15. Note: `validate_map_va` is partially
subsumed by the `MapHandleDecision` in item 2; check for overlap before
implementing item 12.

### Priority 4: Constants, helpers, cleanups

#### 14. `MAX_CPUS` unification

**Source**: `src/arch/aarch64/platform.rs` (line 24)

`MAX_CPUS = 4` in the kernel and `MAX_CPUS_MODEL = 4` in
`lockjaw-types/src/scheduler.rs` (line 140) are the same constant
with different names and a "must match" comment. Consolidate to one
`MAX_CPUS` in lockjaw-types and delete the kernel duplicate.

Note: the MMIO base addresses (`uart0_base`, `gicd_base`, etc.) are
*not* compile-time constants — they are discovered at runtime from the
DTB and stored in `PlatformInfo`. The previous roadmap item about
extracting `UART0_BASE_PHYS` etc. as constants is obsolete and has
been removed.

**Effort**: low. **Test gain**: ~1 (assertion that the two are equal,
turning a comment invariant into a compile error).

#### 15. Scheduler predicates

**Source**: `src/sched/scheduler.rs`

The preemption and TLB-flush decisions (`can_preempt`, `should_flush_tlb`)
are inline in `tick()` and `schedule()`. Extract as pure predicates
with clear boolean inputs.

**Effort**: low. **Test gain**: ~4 tests.

#### 16. Stack canary validation

**Source**: `src/mm/stack.rs` (line 76), `src/sched/scheduler.rs` (line 681)

`STACK_CANARY` is already in `lockjaw-types/src/constants.rs`. The
comparison `if value != STACK_CANARY` is duplicated in two places
without a shared pure function. Extract `validate_canary(value: u64) ->
Result<(), StackCanaryError>` to lockjaw-types. Kernel keeps the
volatile read and the panic.

**Effort**: low. **Test gain**: ~3 tests.

#### 17. Buddy allocator address arithmetic

**Source**: `src/mm/page_alloc.rs` (lines 145-156)

`page_index(addr)`, `round_up_page(addr)`, `index_to_page(idx)` are
private to `page_alloc.rs` with no host tests. Move to
`lockjaw-types/src/addr.rs`. These are pure arithmetic over `PhysAddr`.

**Effort**: low. **Test gain**: ~6 tests.

#### 18. Timer tick policy

**Source**: `src/arch/aarch64/timer.rs`

Bookkeeping and tick decisions are pure. CNTV register programming and
interrupt ack stay in-kernel.

**Effort**: low. **Test gain**: ~3 tests.

#### 19. GIC geometry

**Source**: `src/arch/aarch64/gic/v3.rs`

INTID-to-register math (e.g. `reg = intid / 32`, line 113) and
redistributor geometry (128 KB stride, line 12) are inline arithmetic.
Extract to lockjaw-types as pure index functions. Volatile register
writes stay in-kernel.

**Effort**: low. **Test gain**: ~6 tests.

#### 20. `wait_any` readiness planner

**Source**: `src/syscall/handler.rs` `sys_wait_any()` (line 507)

`WaitEntry`, `validate_wait_count`, and `MAX_WAIT_OBJECTS` are already
in lockjaw-types. The `check_readiness` helper (called line 553) is
inline in handler.rs. A `WaitReadinessPlan` pure function that takes the
resolved paddr/type/threshold arrays and returns the ready bitmask would
make this testable. The waiter-registration and block/wake loops must
stay in-kernel.

**Effort**: low. **Test gain**: ~4 tests.

#### 21. Remove `src/elf.rs` shim

**Source**: `src/elf.rs` (3 lines: `pub use lockjaw_types::elf::*;`)

The shim is declared in `src/main.rs` (`mod elf;`) but has no callers
(`crate::elf::` appears nowhere in the kernel). Delete both the shim
file and the `mod elf;` declaration.

**Effort**: trivial. **Test gain**: 0. **Bug class**: dead code.

#### 22. `OwnedPageList` value object

**Source**: `src/cap/process_obj.rs` `process_push_owned_page()` (line 109)

The dedup/bounds-check logic for `owned_pages` is inline in the kernel.
`MAX_OWNED_PAGES` is in lockjaw-types but the list operations are not.
Extract as `OwnedPageList` with `push_dedup(page) -> bool` and `iter()`.
Preconditioned on item 5 (ProcessObject struct move).

**Effort**: low (after item 5). **Test gain**: ~5 tests.

#### 23. `PageSet` value object promotion

**Source**: `src/cap/pageset.rs`

`PageSet` struct is in lockjaw-types (`lockjaw-types/src/pageset_table.rs`
is a different, already-extracted piece). The kernel-side `cap/pageset.rs`
has a `PageSet` struct that calls `page_alloc` directly — it is not a
pure value object. Separating the pure struct (pages + count) from the
allocation side effect would match the plan/apply pattern. This is lower
priority than item 3 (`alloc_pages` rollback), which addresses the same
file more directly.

**Effort**: low. **Test gain**: ~3 tests (layout pinning).

#### 24. `ReplyObject` liveness tag alignment

**Source**: `src/ipc/reply.rs` (line 21)

`ReplyState` enum is in `lockjaw-types/src/ipc_state.rs`. The kernel
`ReplyObject` uses `REPLY_STATE_FRESH`/`REPLY_STATE_BOUND` constants
from lockjaw-types (line 10-11 import), which is correct. The residual
issue is that `ReplyObject.state` is a `u8` rather than `ReplyState`,
requiring manual encode/decode. Switch to `ReplyState` directly to
eliminate the encode path.

**Effort**: low. **Test gain**: ~2 tests (construction, state round-trip).

---

## Considered and rejected

These items appeared in earlier drafts but are either obsolete or
the framing was wrong:

- **Platform MMIO constants extraction**: `UART0_BASE_PHYS`, `GICD_BASE_PHYS`,
  etc. do not exist as compile-time constants. All MMIO addresses are
  discovered at runtime via DTB scan (`platform::discover()`) and stored
  in `PlatformInfo`. Extracting non-existent constants is a non-item.

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
| Platform discovery | `src/arch/aarch64/platform.rs` | DTB scan populates `PlatformInfo` at runtime. All MMIO addresses are discovered, not constant. |

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
