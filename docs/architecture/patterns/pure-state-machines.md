# Pattern: Pure State Machines

A type that owns iteration state and returns "what to do next" on each
step. The kernel does the I/O between steps but never advances the
machine itself — `step()` decides when to read the next address, when
the walk is done, and when something faulted.

Reach for this when the decision logic is iterative (multi-step walk,
flush-and-advance, table scan) and you want the iteration logic to be
host-testable without faking the I/O.

## When to reach for it

- The work is a **walk** or **iteration** with multiple steps before a
  terminal result.
- Each step depends on a value the previous step couldn't see (a PTE
  read from memory, a field on a structure not yet examined).
- The state and the transition rules are pure logic, but the data on
  each step comes from the kernel's I/O.
- Errors at any level need to be detected and returned as a terminal
  state.

If a single decision is enough, use [pure decisions](pure-decisions.md).
If you build up state across many inputs and execute it later, use
[plan/apply](plan-apply.md).

## Canonical example: `PageTableWalk`

A 4-level page table walk needs to read four PTEs from memory in
sequence — the address of each read depends on the value of the
previous PTE. The kernel can't predict the second PTE address before
reading the first, so the iteration is genuinely sequential.

Push-shaped code would interleave the level tracking, PTE
interpretation, and memory reads in a single function. Pull-style
moves the state machine into types: types decides "go read this
address," kernel reads it and feeds the value back.

**lockjaw-types side** (`lockjaw-types/src/page_table.rs:191-285`):

```rust
/// Result of each step in a page table walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkResult {
    /// Read the u64 at this physical address and pass it to step().
    Continue(u64),
    /// Walk complete: the VA maps to this physical address.
    Done(u64),
    /// Walk faulted: the VA is unmapped at some level.
    Fault,
}

pub struct PageTableWalk {
    level: u8,
    va: u64,
    indices: [usize; 4],
}

impl PageTableWalk {
    /// Begin a walk. Returns the walker and the first physical address to read.
    pub fn start(ttbr0_paddr: u64, va: u64) -> (Self, WalkResult) { ... }

    /// Feed a raw PTE value read from the address returned by the previous step.
    pub fn step(&mut self, pte_raw: u64) -> WalkResult {
        let pte = PageTableEntry::from_raw(pte_raw);
        match self.level {
            0 => {
                if !pte.is_table() { return WalkResult::Fault; }
                let next_table = pte.output_addr().as_u64();
                self.level = 1;
                WalkResult::Continue(next_table + (self.indices[1] as u64) * 8)
            }
            // ... levels 1, 2, 3 ...
        }
    }
}
```

**Kernel side** (`src/arch/aarch64/vmem.rs:218-236`, `translate_user_va`):

```rust
pub unsafe fn translate_user_va(ttbr0_paddr: PhysAddr, user_va: u64) -> Option<u64> {
    let (mut walk, mut result) = PageTableWalk::start(ttbr0_paddr.as_u64(), user_va);
    loop {
        match result {
            WalkResult::Continue(pte_paddr) => {
                let pte_va = pte_paddr + KERNEL_VA_OFFSET;
                let pte_raw = core::ptr::read_volatile(pte_va as *const u64);
                result = walk.step(pte_raw);
            }
            WalkResult::Done(phys_addr) => return Some(phys_addr + KERNEL_VA_OFFSET),
            WalkResult::Fault => return None,
        }
    }
}
```

The kernel loop is mechanical: read the address types asks for, hand
the value back, repeat. PTE interpretation, level tracking, fault
detection — none of it is in the kernel.

The walk is exhaustively host-tested
(`lockjaw-types/src/page_table.rs:674-786`): full 4-level walks, 1 GB
and 2 MB block resolution, faults at every level, correct page-offset
preservation. None of these tests touch a real address space.

## Variants

### Walk that stops early: `MapWalk`

`MapWalk` (`lockjaw-types/src/page_table.rs:317-379`) walks
L0→L1→L2 — but stops at L2 instead of resolving the full address. It
returns `MapWalkResult::ReachedL2 { state, ... }` so the caller can
decide what to do with the L2 slot (allocate, reuse, error). Same
state machine pattern, different terminal condition.

### Pagination cursor: `ScratchCursor`

`ScratchCursor` (`lockjaw-types/src/vmem.rs:259-328`) is a state
machine for writing into a multi-page scratch buffer. After each
write, the kernel calls `advance()` and gets either `Continue` (keep
writing into the current page) or `FlushAndAdvance { next_page_idx }`
(current page is full — flush it, switch to the next page). The
kernel never tracks page indices or offsets directly.

This is a different shape from a walk: the state machine doesn't
direct memory reads, it directs page transitions. But the kernel's
loop is the same: act on the result, ask for the next decision.

### Read-and-write walk: `unmap_validated`

`unmap_validated` (`lockjaw-types/src/page_table.rs:495-564`) wraps
two walks behind a higher-level pure operation. It validates each PTE
matches an expected physical page, then clears all matching PTEs in a
second pass. The caller passes both `read_pte` and `write_pte`
closures. Same shape as the canonical walk, but the kernel passes two
I/O capabilities instead of one.

## Common pitfalls

- **Mutable state machine, kernel keeps a copy of the inputs.** If
  `step()` mutates the walker but the kernel also caches the level or
  index, the two go out of sync at the first off-by-one. Trust the
  walker: never duplicate its internal state in the kernel.

- **Closures that aren't transactional.** `unmap_validated` works
  because the address space isn't being mutated by another thread
  during the walk. If your I/O closure could observe state from a
  different thread mid-walk, the walker's results are stale by the
  time the kernel acts. Either lock for the duration of the walk
  (current approach: GKL) or design the state machine to tolerate
  partial reads.

- **Conflating valid empty with fault.** A zero-page PTE looks like a
  fault but might be a legitimate "unmapped" answer in some queries.
  Be explicit about which terminal states the machine returns and what
  they mean. `WalkResult::Fault` and `MapWalkResult::Fault` are
  documented narrowly: an invalid descriptor at a level that requires
  one. Other "valid but unexpected" states get distinct variants
  (`InvalidMapping`, `ReachedL2 { state: IsBlock }`).

- **Step ordering ambiguity.** Document whether `step()` is called
  *before* or *after* the I/O that uses its returned address. The
  convention in lockjaw-types is "the result tells you what to do
  next" — `Continue(addr)` means "read this address and call `step`
  with the value." If a state machine breaks that convention, say so
  in its doc comment.

## Recognizing push-shaped code that wants this pattern

A loop in the kernel that:

- Reads a value, branches on it, computes the next address to read,
  reads that, branches again — that's a state machine inlined.
- Has level/index/offset tracking interleaved with the I/O.
- Can fault at multiple points and needs to bail with the same error
  type.
- Would be hard to test because the I/O is volatile reads, MMIO, or
  page table accesses.

The refactor: name the iterator state in a struct, give it a `step`
method that returns the next-action enum, write host tests with a fake
backing store (see `FakePT` in
`lockjaw-types/src/page_table.rs:887-940` for an example pattern), and
reduce the kernel to a `loop { match result { ... } }`.
