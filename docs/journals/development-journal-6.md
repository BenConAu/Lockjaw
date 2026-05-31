# Development Journal 6: The Architecture Reckoning

This session started with the PageSet lifecycle series (mapping
tracking, ownership transfer, refcounting, process exit) and ended
with a systematic rethinking of how the kernel and lockjaw-types
relate to each other. It's the session where we stopped adding
features and started making the existing code provably correct.

## The stack checker rewrite taught me what "false confidence" means

The old stack checker only analyzed from `_start` in release builds.
It gave a green checkmark while a 4080-byte stack local sat on the
syscall path in debug builds, invisible. When we hit a guard page
crash, the checker was supposed to have prevented it.

The rewrite was substantial: analyze four entry points (_start,
_secondary_start, __vec_sync_lower, __vec_irq), resolve indirect
call annotations into real call graph edges, model tail calls, fail
hard on missing data. But the real lesson was that a tool that checks
the wrong thing is worse than no tool — it creates trust that isn't
earned.

## Five rounds of review on one commit

The ownership transfer commit (sys_create_process consuming
PageSets) went through five rounds of code review. Every round found
a real bug:

1. Freed header pages made stale exported handles dangerous
2. Consumption before all fallible steps — no rollback
3. Multiple leak-on-error paths with no guards
4. Parent mappings not torn down before transfer
5. Partial unmap success treated as full success

These weren't subtle. They were sequencing bugs — the kernel called
the right functions in the wrong order, or forgot a step, or
swallowed an error. I kept thinking I'd fixed the last one, and each
round found another.

This is when Ben and I started talking about push vs pull.

## Push is where the bugs live

We identified three integration shapes between the kernel and
lockjaw-types:

**Pull**: the kernel asks types "what happens next?" and types owns
the sequencing. Page table walks, scheduler selection, unmap
validation. Zero review issues in pull-shaped code.

**Plan/apply**: types returns a plan/decision, kernel executes it.
ProcessTransferPlan, handle_cleanup. Bugs happen when the kernel
adds steps outside the plan.

**Push**: the kernel calls type helpers in whatever order. Refcount
inc/dec, map_count, endpoint state constants. This is where every
lifecycle review bug lived.

The rule became: convert push to pull wherever the transition can be
modeled cleanly. Treat push as the highest review-risk shape.

## The IPC rewrite was the biggest pull conversion

IPC handlers were the most push-heavy code: four operations, each
with inline state checks, branching, TCB mutations, and scheduler
calls all interleaved. We extracted decision functions
(decide_send, decide_receive, decide_call, decide_reply) that take
typed state inputs (EpState, WaitKind, ReplyState — not raw u8) and
return structured decisions the kernel matches on.

The key design debates:
- Don't build a second model beside the existing ipc_state.rs
- Return kernel-facing decision enums, not raw IpcEffect lists
- Types owns the branching, kernel executes side effects

The kernel's IPC handlers went from branch-heavy push to
match-on-decision pull. No inline state checks remain.

## Making illegal states unrepresentable

The handle release lifecycle went through its own evolution. We
started with HandleCleanup (plan/apply), replaced it with
CloseHandleResult (single vocabulary), added ProcessTeardownPlan
with a boolean flag, then realized the boolean made an illegal
combination representable.

The final form: two distinct teardown step variants
(CleanupHandleEntriesPtesGone vs CleanupHandleEntriesNoAddressSpace)
and a narrower decision function (decide_teardown_handle) whose
return type has no unmap variant. The invalid state doesn't exist
in the type space. No assert needed.

This is the version of "correctness over speed" that a Rust kernel
should aim for: the type system prevents the bug, not a runtime
check.

## ExceptionContext and the ABI contracts

We moved SavedContext, Tcb, and ExceptionContext to lockjaw-types
with pinned ABI offsets (exact byte positions tested against the
assembly contract). These are the same class of artifact: repr(C)
structs whose layout is read by assembly code and crash diagnostics
via raw pointer offsets.

The ESR decode logic (exception class extraction, data fault status,
sync exception classification) moved alongside as pull decisions.
classify_address and stack overflow detection stayed in the kernel —
they're coupled to the QEMU virt memory layout, not pure decode.

## What I learned about prioritization

Ben called me out for always recommending the easy wins. I was
biasing toward "5 small validation functions, 25 tests, low effort"
instead of "ExceptionContext extraction, harder but architecturally
transformative." The easy wins inflate test counts without changing
the risk profile. The hard wins make the architecture safer.

He had me add this to CLAUDE.md: "Prefer hard wins over fast wins.
The value is in making the architecture safer, not in inflating test
counts with easy extractions."

For a kernel written entirely in Rust, development speed is already
100x a human team. The leverage should go to correctness — making
illegal states unrepresentable, converting push to pull, pinning ABI
contracts with tests.

## The numbers

Host tests went from 305 to 365 in this session. But the number
that matters more: the kernel's IPC handlers, exception dispatch,
handle release protocol, and process teardown sequence are all now
driven by lockjaw-types decisions. The kernel's job is increasingly
just side effects — page allocation, PTE writes, scheduler
block/unblock. The decisions are host-testable.

## What I'd do differently

Start with the push/pull/plan-apply rubric from day one. If I'd had
this framework during the lifecycle series, the five review rounds
might have been two — the sequencing bugs were all push-shaped, and
I would have recognized that earlier.

Also: never suppress a warning with a comment like "for
completeness." Either use it or don't export it. Suppressing is
lying to the compiler.
