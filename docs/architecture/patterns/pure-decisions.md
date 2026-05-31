# Pattern: Pure Decisions

A pure function that takes some inputs and returns an enum (or other
deterministic value). The kernel reads the input, calls the pure
function, and matches on the result. All branching logic lives in
lockjaw-types; the kernel's job is to apply each branch's side effects.

This is the simplest of the patterns. Reach for it whenever a decision
can be made from inputs already in hand.

## When to reach for it

- The decision is **one-shot** — there's no iteration.
- The decision is a function of inputs you already have, not state
  the kernel observes during the decision.
- The kernel applies the result in a few lines of side-effect code
  (allocate, write, return error).
- You'd otherwise write a 3-or-more-branch `if`/`match` directly in
  kernel code.

If the decision needs to drive iteration, use a [pure state
machine](pure-state-machines.md). If it accumulates over multiple
inputs, use [plan/apply](plan-apply.md).

## Canonical example: `MapAction`

The kernel walks page tables to map a user page. When it reaches the
target L2 slot, it has to decide: allocate a new L3 table, reuse an
existing one, or error because the slot already holds a 2 MB block.

That decision is pure. Given the L2 slot's classification, exactly one
action is correct.

**lockjaw-types side** (`lockjaw-types/src/vmem.rs:36-65`):

```rust
/// What the kernel found at an L2 slot when trying to map a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum L2SlotState {
    Empty,
    HasL3Table,
    IsBlock,
}

/// What action the kernel should take for a given L2 slot state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapAction {
    AllocateL3,
    UseExistingL3,
    ErrorBlockConflict,
}

/// Determine the action for mapping a page given the L2 slot state.
pub fn map_action_for_l2(state: L2SlotState) -> MapAction {
    match state {
        L2SlotState::Empty => MapAction::AllocateL3,
        L2SlotState::HasL3Table => MapAction::UseExistingL3,
        L2SlotState::IsBlock => MapAction::ErrorBlockConflict,
    }
}
```

**Kernel side** (`src/arch/aarch64/vmem.rs`, in `map_pages_in_existing`):

```rust
let l3_va = match map_action_for_l2(state) {
    MapAction::UseExistingL3 => {
        let l3_paddr = (*l2_va).entries[l2_idx].output_addr();
        (l3_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut PageTable
    }
    MapAction::AllocateL3 => {
        let l3_page = page_alloc::alloc_page().ok_or(VmemError::OutOfPages)?;
        let va = (l3_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut PageTable;
        ptr::write_bytes(va, 0, 1);
        (*l2_va).entries[l2_idx] = PageTableEntry::new_table(l3_page.start_addr());
        va
    }
    MapAction::ErrorBlockConflict => {
        return Err(VmemError::TooManyL3Regions);
    }
};
```

The kernel block contains only side effects: page allocation, pointer
manipulation, PTE writes. The decision about which side effect to run
was made by `map_action_for_l2`. That function is host-tested in
`lockjaw-types/src/vmem.rs:370-383`.

## Variants

The "decide once, kernel applies result" shape supports several useful
return types beyond a bare enum.

### Decision carrying computed data: `validate_mapping`

When the decision computes derived values the kernel will need, fold
them into the enum so the kernel doesn't redo the math.

`lockjaw-types/src/vmem.rs:67-104`:

```rust
pub enum MapValidation {
    Ok { l2_idx: usize, l3_start: usize },
    ErrorOutOfRange,
    ErrorSpansL2Boundary,
    ErrorTooManyPages,
}

pub fn validate_mapping(virt_addr: u64, page_count: usize) -> MapValidation { ... }
```

The kernel either gets `Ok { l2_idx, l3_start }` and uses both fields,
or gets a specific error variant. Indices aren't recomputed at the
call site.

### Decision returning a structured value: `build_process_page`

When the decision is a transformation, the return type can be the
transformed value rather than an enum. `build_process_page`
(`lockjaw-types/src/vmem.rs:234-253`) takes
`(phys, user_accessible, executable)` and returns a `PageTableEntry`
with the correct AP/UXN/PXN bits already set. The kernel writes the
result; it never inspects which permission policy applied.

### Predicate: `validate_process_mappings`

The simplest form is a `bool` predicate. `validate_process_mappings`
(`lockjaw-types/src/vmem.rs:113-118`) checks
`mapping_count > 0 && stack_count > 0 && mapping_count + stack_count <= capacity`.
The kernel uses it as an `if !validate(...) { return Err(...) }` gate.

### Advanced: `SchedDecision`

`SchedDecision` (`lockjaw-types/src/scheduler.rs:37-58`) is a richer
example: 5 variants spanning regular switch, stay-on-current,
wait-for-interrupt, and two exit transitions. The pure decision
function `select_next` (lines 85-118) is wrapped by `SchedState::step`
which validates and applies the result atomically.

This is the same pattern at a larger scale: a pure function returns an
enum, the kernel matches and acts. The wrapping state machine (`step`
+ `apply_decision`) shows how a decision can be paired with state
transitions to enforce invariants — but the core shape is unchanged.
Read `pure-state-machines.md` next if your decision needs to drive
iteration; read this file if it doesn't.

## Common pitfalls

- **Leaking kernel types into the enum.** The decision enum is part of
  the public surface of lockjaw-types. If you put a `*mut PageTable` or
  a kernel handle in a variant, lockjaw-types stops being host-testable.
  Keep enum payloads pure data: indices, counts, addresses-as-u64,
  flags. Let the kernel reconstruct any pointer it needs from those.

- **Hidden state dependency.** A decision function that calls into
  `static mut` or reads global state is no longer pure. If you find
  yourself wanting that, hand the dependency in via a closure (like
  `select_next` does with `get_state: F`) so the test can supply a
  fake.

- **Recomputing in the kernel.** If the decision computed something the
  kernel re-derives (indices, lengths, classified states), fold it into
  the return type. Avoiding the duplication is half the value.

- **Boolean flags that allow illegal combinations.** Two booleans give
  four states; if only three are valid, use an enum with three
  variants. The handle teardown plan started with a boolean and ended
  with `CleanupHandleEntriesPtesGone` vs `CleanupHandleEntriesNoAddressSpace`
  precisely to make the impossible combination unrepresentable. See
  `lockjaw-types/src/process.rs:217-237`.

## Recognizing push-shaped code that wants this pattern

You're looking at code that should be a pure decision when:

- A `match` or `if`/`else if` chain in the kernel selects one of
  several mutually exclusive actions.
- The selection criteria are values the kernel already has — no I/O,
  no allocation, no async.
- The same selection logic appears (or could appear) in more than one
  call site.
- A bug in the selection would be hard to test without running the
  whole kernel.

The refactor: lift the selection into a function in lockjaw-types that
returns an enum, write host tests for each branch, replace the kernel
chain with a single `match` on the enum's variants.
