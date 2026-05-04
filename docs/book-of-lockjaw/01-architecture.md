# The Lockjaw Architecture

> *Push is where the bugs live.*
>
> — development-journal-6.md

Lockjaw is a microkernel written entirely in Rust. The standard
selling point of a Rust kernel — memory safety — is real but
under-leveraging the language. Once unsafe is contained at the
boundary, the next leverage point is the type system itself. Lockjaw's
architecture pushes that idea harder than convention demands: every
decision that *can* be a pure function lives in pure code, and the
kernel becomes a thin shell of inline assembly and side effects around
a host-testable core.

This chapter is the philosophical foundation. It explains *why* the
architecture is shaped the way it is, what bugs that shape prevents,
and what the taxonomy of integration patterns means in practice. The
sibling [pattern catalog](../patterns/) shows you *how* to apply it.

## The realization came from a review cycle

The shape of this architecture was not designed up front. It emerged
from one commit that took five rounds of code review to ship: the
ownership-transfer commit that taught `sys_create_process` to consume
PageSets from the parent.

Every round found a real bug:

1. Freed header pages made stale exported handles dangerous.
2. PageSets were consumed before all fallible steps — no rollback if
   later steps failed.
3. Multiple leak-on-error paths had no guards.
4. Parent mappings were not torn down before transfer.
5. Partial unmap success was treated as full success.

These were not subtle bugs. They were not memory-safety bugs. They
were sequencing bugs — *the kernel called the right functions in the
wrong order, or forgot a step, or swallowed an error.* Each round
fixed the last one and the next round found another.

When a class of bugs keeps recurring through review, the right
question is not "how do I write more careful code." It is "what shape
of code keeps producing this class of bug, and what shape would
prevent it?"

## Three shapes for pure code and the kernel

The kernel and lockjaw-types interact in three integration shapes:

**Pull**: the kernel asks types "what happens next?" and types owns
the sequencing. Page table walks, scheduler selection, unmap
validation. *Zero review issues in pull-shaped code.*

**Plan / apply**: types returns a plan or decision; the kernel
executes it. ProcessTransferPlan, the handle-cleanup vocabulary,
ProcessTeardownPlan. *Bugs happen when the kernel adds steps outside
the plan.*

**Push**: the kernel calls type helpers in whatever order it likes.
Refcount inc/dec, map_count manipulation, endpoint state constants.
*This is where every lifecycle review bug lived.*

The five-review commit was push-shaped at every layer. The kernel had
the helpers it needed; it just had to orchestrate them in the correct
order. The reviewer would find one missed step, the author would fix
it, the next reviewer would find another. The shape of the code
*allowed* the bug class. No amount of careful coding would have
eliminated it without also eliminating the shape.

The rule that came out of this: **convert push to pull wherever the
transition can be modeled cleanly. Treat push as the highest
review-risk shape.**

## The IPC rewrite was the biggest pull conversion

IPC handlers were the most push-heavy code in the kernel: four
operations (send, receive, call, reply), each with inline state
checks, branching on endpoint state, TCB mutations, and scheduler
calls all interleaved in single functions. Every change had a
sequencing risk.

The conversion pulled the decision logic out into pure functions —
`decide_send`, `decide_receive`, `decide_call`, `decide_reply` — that
take typed state inputs (`EpState`, `WaitKind`, `ReplyState`, not
raw u8s) and return structured decision enums the kernel matches on.

The kernel's IPC handlers went from branch-heavy push to
match-on-decision pull. No inline state checks remain. The decisions
are host-tested without standing up a real endpoint.

The IPC rewrite is the worked example of every pattern in this
codebase. Read it once you understand the patterns; it shows what
happens when push-shaped code is rewritten end-to-end into pull.

## The payoff: making illegal states unrepresentable

The shape prevents a bug class. The next move is to make the bug
*impossible to write*.

The handle release lifecycle went through four iterations:

1. `HandleCleanup` (plan/apply): a list of steps the kernel
   executes.
2. `CloseHandleResult`: a single decision vocabulary, replacing the
   plan with a per-handle decision.
3. `ProcessTeardownPlan` with a `mappings_already_cleared: bool`
   flag.
4. **The flag was wrong.** Two booleans means four states; one
   combination — "address space exists *and* PTEs are gone" — is
   illegal.

The final form: two distinct teardown step variants
(`CleanupHandleEntriesPtesGone` and
`CleanupHandleEntriesNoAddressSpace`) and a narrower decision
function (`decide_teardown_handle`) whose return type has *no unmap
variant*. The illegal state cannot be expressed.

> The invalid state doesn't exist in the type space. No assert
> needed.

This is the version of "correctness over speed" that a Rust kernel
should aim for: *the type system prevents the bug, not a runtime
check.* If you find yourself writing `assert!(!(a && b))`, ask
whether `a` and `b` should be variants of an enum instead.

## What the kernel is for

After enough conversions, the kernel's role narrows. The kernel does:

- **Inline assembly.** Context switches, sysreg reads/writes, IRQ
  masking, eret. No pure model can replace these.
- **MMIO.** GIC, UART, timer registers. Volatile writes to specific
  addresses.
- **Raw pointer manipulation.** Page table writes through
  KERNEL_VA_OFFSET, intrusive list operations, PhysAddr -> VA
  translation.
- **Scheduler effects.** Block, unblock, switch — the actual mutation
  of TCB state and run queues.
- **IPC delivery.** Copying message payloads, transferring caller
  tokens, waking partners.

The kernel does not:

- Decide which thread to run next. (`select_next` does.)
- Decide what to do with a handle on close. (`decide_close_handle`
  does.)
- Decide whether a process can be torn down. (`build_teardown_plan`
  does.)
- Decide what page table action to take at an L2 slot.
  (`map_action_for_l2` does.)
- Walk a page table. (`PageTableWalk` does.)
- Validate a syscall input. (Pure validators in lockjaw-types do.)

> The kernel's job is increasingly just side effects — page
> allocation, PTE writes, scheduler block/unblock. The decisions are
> host-testable.

The asymmetry is the point. Pure logic gets host tests, host
debuggers, host iteration speed. Side effects get the slow path:
QEMU, integration tests, the discipline of inline assembly comments.
The architecture biases toward putting load on the fast side.

## What this means for development speed

For a kernel written entirely in Rust, with assistance from frontier
language models, raw output speed is no longer the bottleneck.
Lines-per-day is comically high; correctness-per-line is the
constraint.

So the project's discipline is calibrated accordingly:

- **Prefer hard wins over fast wins.** The leverage is in making the
  architecture safer, not in inflating test counts with easy
  extractions. Five small validators is less valuable than one
  ExceptionContext extraction with pinned ABI offsets.
- **Make illegal states unrepresentable.** If the type system can
  prevent a bug class, prefer that over runtime assertions or
  documentation. Narrow return types, distinct enum variants for
  distinct conditions.
- **The kernel should be thin.** Inline assembly + mechanical
  execution of lockjaw-types decisions. Every decision that can be a
  pure function in types should be.
- **Convert push to pull.** Push is the highest-risk shape; treat it
  as a refactoring target.

These principles are recorded in `CLAUDE.md` at the top of the
repository. They are the load-bearing summary; this chapter is the
context behind them.

## Where to go next

If you're refactoring existing code into this architecture, start
with [`../patterns/README.md`](../patterns/README.md) — the catalog
has a decision flowchart and four pattern docs, each with a canonical
example.

If you're choosing what to extract next, see
[`../extraction-roadmap.md`](../extraction-roadmap.md) — the ranked
list of remaining push-shaped subsystems.

If you want the historical narrative — how the architecture was
discovered, the bugs that motivated it, the design debates — read
`../development-journal-6.md`. This chapter is the synthesis;
journal-6 is the primary source.
