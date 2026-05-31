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

## Canonical example: `ProcessTransferPlan`

`sys_create_process` consumes PageSets from the parent and transfers
them to the child. Each user mapping might reference the same PageSet
multiple times; some PageSets must be torn down from the parent's
address space before the child can claim them; if any teardown fails,
the entire operation must abort with the parent intact.

This is exactly the plan/apply shape: build, validate, then commit.

**lockjaw-types side** (`lockjaw-types/src/process.rs:130-207`):

```rust
pub struct ProcessTransferPlan {
    headers: [u64; MAX_CONSUMED_HEADERS],
    header_count: usize,
    /// Per-header: (total_mapped_handles, successfully_unmapped).
    unmap_results: [(usize, usize); MAX_CONSUMED_HEADERS],
}

impl ProcessTransferPlan {
    /// Record a PageSet header for consumption. Deduplicates —
    /// multiple mappings from the same PageSet produce one entry.
    pub fn add_header(&mut self, header_paddr: u64) -> Result<HeaderIndex, TransferError>;

    /// Record the result of unmapping parent handles for a header.
    pub fn record_unmap(&mut self, header_idx: HeaderIndex, total: usize, unmapped: usize);

    /// Can we commit? All unmaps must have fully succeeded.
    pub fn validate(&self) -> Result<(), TransferError>;

    /// The deduplicated list of PageSet headers to consume.
    pub fn headers(&self) -> &[u64];
}
```

The plan owns three pieces of state: deduplicated headers, the unmap
results recorded against each header index, and the constants that
determine validity. Nothing in here touches the kernel.

**Kernel side** (`src/process.rs`): build, then validate, then commit.
Edited for length:

```rust
let mut plan = ProcessTransferPlan::new();

// Build phase: walk user mappings, dedup headers into the plan.
for i in 0..mapping_count {
    let user_mapping: ProcessMapping = addr_space.read(...)?;
    let ps_entry = ht.lookup(user_mapping.pageset_id as u32, ...)?;
    // ... resolve to physical page, record in transfer plan ...
    plan.add_header(ps_entry.object_paddr).map_err(|_| "too many PageSets")?;
}
plan.add_header(stack_entry.object_paddr).map_err(|_| "too many PageSets")?;

// (... lots of other fallible setup: address space, handle table, TCB ...)

// Phase 1: tear down parent's VA mappings — fallible per-header.
for i in 0..plan.headers().len() {
    let (idx, hdr) = plan.header_at(i).unwrap();
    // ...
    let (total_mapped, unmapped) = ht.unmap_for_object(hdr, |va| { ... });
    plan.record_unmap(idx, total_mapped, unmapped);
    // (decrement map_count for successful unmaps)
}

// Validation gate: any unmap failure aborts before we do anything irreversible.
plan.validate().map_err(|_| "parent unmap failed during ownership transfer")?;

// === Point of no return ===

// Phase 2: consume PageSets — infallible because we validated.
for &hdr in plan.headers() {
    crate::cap::pageset_table::consume_pageset(hdr, &ht);
}
```

Three things are worth noticing:

1. The plan doesn't execute. It accumulates and validates. Side effects
   (the actual unmaps in Phase 1, the consumption in Phase 2) live in
   the kernel.
2. Validation comes between the fallible phase and the irreversible
   phase. If anything in Phase 1 returned a partial unmap, `validate()`
   detects it and the function returns `Err` — the kernel never reaches
   Phase 2.
3. Dedup happens in `add_header`. The kernel doesn't have to know that
   two `ProcessMapping`s share a PageSet header; it adds, the plan
   merges.

The plan is exhaustively host-tested
(`lockjaw-types/src/process.rs:357-484`): dedup, capacity overflow,
partial unmap detection, empty plan, ordering of unmap records.

## Variants

### Plan from observations: `ProcessTeardownPlan`

When the plan is *derived from state* rather than built up over
inputs, `build_teardown_plan` is the shape to use. The kernel observes
the dying process's state (`owned_page_count`, `has_address_space`,
`has_handle_table`, `handle_table_page_count`) and hands those facts
to a pure builder. The builder returns a sequence of `TeardownStep`s
the kernel iterates and applies.

`lockjaw-types/src/process.rs:217-303`:

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

- **Plans built over user input without bounded capacity.** Both
  `ProcessTransferPlan` and `ProcessTeardownPlan` use fixed-size
  arrays (`MAX_CONSUMED_HEADERS = 32`, `[Option<TeardownStep>; 5]`).
  The kernel can't allocate from the heap; the plan must declare its
  bounds at compile time and return errors when an input would exceed
  them. Don't design a plan that needs `Vec`.

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
