# Pattern: Plan / Apply

The kernel builds up a plan over many inputs, validates the plan as a
whole, and only then executes the side effects. The plan is a pure
data structure in lockjaw-types; the kernel feeds it observations and
queries it for decisions. Once validation passes, the kernel iterates
the plan and applies each step mechanically.

Reach for this pattern when correctness depends on a multi-step
operation succeeding *as a whole* — when partial success is worse than
no success, and when sequencing matters.

## When to reach for it

- The operation has a **point of no return**: after a certain step
  succeeds, you cannot abort cleanly.
- Multiple inputs **accumulate** into the plan (a list of mappings to
  transfer, a list of resources to free).
- Validation requires **looking at all inputs together** — checking
  totals, dedup, uniformity, consistency.
- The kernel must be able to **fail before committing** if any input
  proves the plan invalid.
- Steps must execute in a **specific order** the plan can encode.

If the operation is a single decision, use [pure
decisions](pure-decisions.md). If it iterates one input at a time
without accumulating, use a [pure state
machine](pure-state-machines.md).

## Canonical example: `ProcessCreationPlanBuilder`

`sys_create_process` consumes PageSets from the parent and transfers
them to the child. Each user mapping might reference the same PageSet
multiple times; the child's single-page handle table has limited
slots; the scheduler's run queue might be full; any of these is a
reason to fail *before* the irreversible PageSet consume.

This is exactly the plan/apply shape: build, validate, then commit.
The pattern splits into two pure pieces: `dedup_add_header` (the
per-mapping deduplication primitive) and `ProcessCreationPlanBuilder`
(the outer-orchestration builder whose `validate` produces an owned
apply-phase token).

**lockjaw-types side** — the dedup primitive
(`lockjaw-types/src/process.rs:130`):

```rust
/// Returns Ok(true) if the header was new, Ok(false) if already present.
pub fn dedup_add_header(
    header_paddr: u64,
    headers: &mut [u64],
    count: &mut usize,
) -> Result<bool, TransferError>;
```

The kernel owns the storage (a `&mut [u64]` in the proc page so the
sync-exception stack stays small). The helper is the single API
boundary for "is this PageSet already on the list?".

The outer builder (`lockjaw-types/src/process.rs:224`):

```rust
pub struct ProcessCreationPlanBuilder { /* mapping_count, stack_pages, … */ }

impl ProcessCreationPlanBuilder {
    pub fn record_mapping(&mut self);
    pub fn record_stack(&mut self, stack_pages: usize) -> Result<(), CreateProcessPlanError>;
    pub fn record_parent_copy(&mut self, entry: HandleEntry) -> Result<(), CreateProcessPlanError>;

    /// Consumes the builder. Produces a `ValidatedProcessCreationPlan`
    /// (owned token) only if every structural precondition holds.
    pub fn validate(
        self,
        scratch_capacity: usize,
        scheduler_has_room: bool,
        unique_header_count: usize,
    ) -> Result<ValidatedProcessCreationPlan, CreateProcessPlanError>;
}

pub struct ValidatedProcessCreationPlan { /* the apply-phase token */ }
```

The builder tracks only the structural facts (mapping count, stack
pages, optional parent copy). It does NOT carry the header list — the
kernel-side storage in the proc page is the single source of truth,
and `unique_header_count` is passed to `validate` directly so the two
counts cannot drift.

**Kernel side** (`src/process.rs:152`): build, validate (twice),
then apply. Edited for length, but the shape matches the live code:

```rust
let mut plan_builder = ProcessCreationPlanBuilder::new();
let proc_kva = /* allocate the child's proc page */;

// Build phase: walk user mappings, record each through the single API
// boundary (record_mapping_into_plan wraps both halves — the proc-page
// dedup write AND the builder count — so callers can't drift them).
for i in 0..mapping_count {
    let user_mapping: ProcessMapping = addr_space.read(...)?;
    let ps_entry = ht.lookup(user_mapping.pageset_id as u32, ...)?;
    record_mapping_into_plan(
        &mut plan_builder,
        proc_kva,
        ps_entry.object_paddr,
    )?;
}
record_stack_into_plan(&mut plan_builder, proc_kva, stack_hdr, stack_pages)?;

// Parent-copy is optional (sentinel u64::MAX = none). Resolved by
// handle table here, recorded on the builder directly — no
// *_into_plan wrapper because parent_copy touches only the builder
// (no proc-page dedup half to pair).
if parent_handle_to_copy != u64::MAX {
    let entry = ht.lookup_any(parent_handle_to_copy as u32, Rights::none())?;
    plan_builder.record_parent_copy(entry)?;
}

// Pure structural validate — consumes builder, yields owned token.
// unique_header_count comes from the proc-page (single source of
// truth) so the builder cannot drift from the actual dedup state.
let unique_header_count =
    process_consumed_header_count(proc_kva) as usize;
let plan = plan_builder.validate(
    scratch_capacity,
    scheduler::has_room(),
    unique_header_count,
)?;

// Kernel-state validate: revoke walk + refcount checks against live
// state. Read headers by typed index — proc-page storage is private.
for idx in 0..plan.unique_header_count() {
    let hdr = process_consumed_header(proc_kva, idx).unwrap();
    consume_pageset_validate(hdr)?;
}

// === Point of no return ===

// Apply phase: per-header consume + parent-copy insert +
// scheduler add. Inlined directly (no single `apply()` wrapper) —
// each step touches distinct kernel state (handle table, scheduler)
// and the inlining keeps the invariant ("token's existence ⇒ both
// validates passed") visible at every step.
for idx in 0..plan.unique_header_count() {
    let hdr = process_consumed_header(proc_kva, idx).unwrap();
    consume_pageset_apply(hdr);
}
if let Some(parent) = plan.parent_copy() {
    child_ht.insert(parent.rights, parent.kind)
        .expect("fresh empty table; capacity checked in pure validate");
}
// (defuse drop guards — child now owns its resources)
if !scheduler::add_thread(tcb_kva) {
    panic!("has_room() returned true but add_thread failed (GKL invariant)");
}
```

Three things are worth noticing:

1. The builder doesn't execute. It accumulates structural facts and
   validates. Side effects (the consume loop, the parent-copy insert,
   the scheduler add) live in the kernel.
2. There are TWO validate gates before any apply runs. The pure
   structural `plan_builder.validate(...)` produces the apply-phase
   token; the kernel-state `consume_pageset_validate(...)` runs after
   it but before any consume. Both must pass.
3. Dedup happens in `dedup_add_header` (the lockjaw-types primitive,
   tested in isolation) but the kernel reaches it through
   `process_record_consumed_header` (the typed boundary at
   `src/cap/process_obj.rs:180`) which `record_mapping_into_plan`
   wraps. Three layers, each its own job: primitive, typed boundary,
   single-call helper.

The plan layer is exhaustively host-tested
(`lockjaw-types/src/process.rs:469+`): `dedup_add_header` covers
new-vs-duplicate, storage exhaustion, ordering; the builder covers
each `CreateProcessPlanError` variant, the
double-stack and double-parent-copy guards, and the
`ValidatedProcessCreationPlan` capacity preconditions.

## Variants

### Plan from observations: `ProcessTeardownPlan`

When the plan is *derived from state* rather than built up over
inputs, `build_teardown_plan` is the shape to use. The kernel observes
the dying process's state (`owned_page_count`, `has_address_space`,
`has_handle_table`, `handle_table_page_count`) and hands those facts
to a pure builder. The builder returns a sequence of `TeardownStep`s
the kernel iterates and applies.

`lockjaw-types/src/process.rs:376` (TeardownStep), `:401`
(ProcessTeardownPlan), `:436` (build_teardown_plan):

```rust
pub enum TeardownStep {
    FreeOwnedPages { count: u32 },
    FreeAddressSpace,
    CleanupHandleEntriesPtesGone,
    CleanupHandleEntriesNoAddressSpace,
    FreeHandleTable { page_count: u8 },
    FreeProcessPage,
}

pub fn build_teardown_plan(
    owned_page_count: u32,
    has_address_space: bool,
    has_handle_table: bool,
    handle_table_page_count: u8,
) -> ProcessTeardownPlan;
```

Two distinct variants for handle cleanup (`PtesGone` vs
`NoAddressSpace`) instead of a boolean: the illegal combination
"address space exists *and* PTEs are gone" can't be expressed. Compare
to the alternative shape (`CleanupHandleEntries { ptes_gone: bool }`)
which would have allowed it. This is why plan/apply rewards careful
enum design.

The kernel iterates `plan.iter()` and `match`es on each step. Cleanup
sequencing — owned pages before address space, address space before
handle cleanup, process page last — is encoded in the plan's
construction order.

## Common pitfalls

- **Side effects during the build phase.** It's tempting to do the
  unmap when the kernel records the unmap result. Don't. The plan's
  job is to decide whether the operation is committable; if the kernel
  starts mutating before validation passes, partial state leaks on
  failure. Keep build, validate, and apply strictly separated.

- **Validation that doesn't gate apply.** If `validate()` returns
  `Ok(())` for a plan with an unrecorded result, validation is hiding
  a bug. Make every observation explicit (`record_unmap` with full
  totals, not just successes) and check the recorded values match
  expectations.

- **Ordering hidden in kernel code.** If the plan's apply phase
  depends on "free pages before address space, handle cleanup before
  free table," that order needs to live *in the plan* — either by the
  order of steps, or by step variants that encode the dependency. A
  comment in the kernel saying "must run after X" rots the moment
  someone reorders the apply loop.

- **Mutable plan that survives validation.** Once `validate()` returns
  `Ok`, the plan should not be mutated before apply. If the plan keeps
  accepting `record_unmap` calls between validate and apply, a stale
  validation can become invalid. The current code naturally avoids
  this — validation is the last fallible step before the point of no
  return — but it's a discipline to maintain.

- **Plans built over user input without bounded capacity.** The
  `dedup_add_header` primitive and `ProcessTeardownPlan` use
  fixed-size storage (caller-supplied `[u64; MAX_CONSUMED_HEADERS]`
  with `MAX_CONSUMED_HEADERS = 32`, `[Option<TeardownStep>; 5]`
  respectively). The kernel can't allocate from the heap; the plan
  must declare its bounds at compile time and return errors when an
  input would exceed them. Don't design a plan that needs `Vec`.

## Recognizing push-shaped code that wants this pattern

The strongest signal is multi-phase orchestration with a "point of no
return" buried in the middle. Look for:

- **A function with a long preamble of validation and resource
  allocation, followed by an irreversible commit.** The classic shape:
  if the commit fails after partial side effects, the kernel state is
  corrupt. Plan/apply pulls all the validation in front of the commit.

- **Loops that mutate kernel state and then check for consistency at
  the end.** "We did the unmaps; now check if any failed; if so, undo"
  is rollback-shaped. Plan/apply replaces it with "record what would
  happen; check it's consistent; *then* do it."

- **Comments documenting step ordering.** "// Must come after X, before
  Y" is the kernel telling you the sequencing should be in a plan, not
  in human memory.

- **Sequencing bugs found in code review.** From journal-6: every
  lifecycle review bug was push-shaped. If reviewers keep finding
  "you forgot a step" or "you did this in the wrong order," the
  function is begging to become a plan.

The refactor: identify the inputs the operation depends on. Identify
the irreversible commit step. Build a plan struct in lockjaw-types
that accumulates the inputs, validates the whole, and exposes the
commit-time data to the kernel. Move all the orchestration into the
plan; leave the kernel with build, validate-and-bail-on-error, then
apply.
