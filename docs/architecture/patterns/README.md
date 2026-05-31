# Lockjaw Pattern Catalog

This is the technique catalog for the Lockjaw architecture. Each
pattern is a *shape* — a recurring way that pure logic in
lockjaw-types and side effects in the kernel hand work back and forth.
Together they implement the project's central thesis: the kernel's
job is mechanical execution; every decision that can be a pure
function lives in lockjaw-types and is host-tested.

For *why* this architecture exists — the bugs that motivated it, the
taxonomy that names it, the principles behind it — read
[`../01-architecture.md`](../01-architecture.md).
This catalog is the *how*.

## The five patterns

| Pattern | Shape | Use when |
|---|---|---|
| [Pure Decisions](pure-decisions.md) | Kernel asks once, types answer once | One-shot decision from inputs you have |
| [Pure State Machines](pure-state-machines.md) | Types own iteration, kernel does I/O | Multi-step walk where each step depends on the last |
| [Plan / Apply](plan-apply.md) | Kernel builds, types validates, kernel commits | Multi-input operation with a point-of-no-return |
| [Pure Data Structures](pure-data-structures.md) | Types own layout and ops, kernel owns memory | Non-trivial operations on a kernel-owned data structure |
| [Operation Envelope](operation-envelope.md) | Typestate-gated operation layer + capability token + generic ordering envelope | A device with both a multi-step protocol and a DMA path |

## How to pick

Four questions, in order:

1. **Is the work driving an external device with both a multi-step
   protocol AND a DMA path** (e.g., SDHCI, NICs)?
   - **Yes** — [Operation Envelope](operation-envelope.md)
   - **No** — go to question 2

2. **Does the work iterate?**
   - **No** — single decision → [Pure Decisions](pure-decisions.md)
   - **Yes** — go to question 3

3. **Does the iteration build up state across many inputs that all
   need to commit together?**
   - **Yes** — [Plan / Apply](plan-apply.md)
   - **No** — go to question 4

4. **Does each iteration step depend on a value the previous step
   produced (e.g., reading a PTE to know the next address)?**
   - **Yes** — [Pure State Machine](pure-state-machines.md)
   - **No** — you probably want to operate on a data structure
     directly → [Pure Data Structures](pure-data-structures.md)

The five patterns blur at the edges — a plan is a data structure with
operations, a state machine is a sequence of decisions, an operation
envelope contains a typestate and a plan/apply. The shapes above
describe the *primary* axis of each pattern. When in doubt, pick
the pattern whose canonical example most resembles your problem and
read its "Recognizing push-shaped code that wants this pattern"
section.

## What ties them together

Every pattern follows the same outer shape:

- **Pure logic in lockjaw-types** — no `unsafe`, no MMIO, no kernel
  APIs. Anything that returns the same answer for the same inputs.
- **Host tests cover all branches** — the test suite runs on the
  developer's laptop, not in QEMU. Bugs in pure logic should never
  reach kernel test runs.
- **Side effects in the kernel** — page allocation, PTE writes,
  scheduler block/unblock, IPC delivery, register writes. Anything
  that mutates global state or touches hardware.
- **A narrow interface between them** — an enum, a result, a slice,
  a closure. The narrower this interface, the easier each side is to
  evolve independently.

When you see a kernel function that's hard to read, hard to test, or
keeps surfacing review-found bugs — look for the shape. Inline
match-on-input is a pure decision waiting to be extracted. A loop
with level/index tracking is a state machine. A multi-phase function
with "must come before X" comments is a plan/apply.

## What this catalog isn't

- It isn't a list of every reusable type in lockjaw-types. Some types
  (`PhysAddr`, `Rights`, `HandleEntry`) are pure data — useful to
  share, but no architectural pattern.
- It isn't a roadmap of what to extract next. See
  [`../../tracking/extraction-roadmap.md`](../../tracking/extraction-roadmap.md) for that.
- It isn't a substitute for reading the canonical examples. Each
  pattern doc points you at one — the code is the most precise
  teaching available.

## Adding a new pattern doc

If you discover a recurring shape that doesn't fit one of the four
above, write a new pattern doc using the existing ones as templates.
Each doc has the same sections in the same order:

1. Short definition (2-3 sentences).
2. *When to reach for it* — bulleted trigger conditions.
3. *Canonical example* — the strongest existing example, with
   side-by-side lockjaw-types and kernel snippets, plus a pointer to
   the host tests.
4. *Variants* — 2-3 supporting examples with file:line, briefly.
5. *Common pitfalls* — bulleted, with the bug class each prevents.
6. *Recognizing push-shaped code that wants this pattern* — what to
   look for during refactoring.

Update this README's table and decision flowchart when adding.
