# Development Journal: Handle Revocation, Variable-Size PageSets, and the mmap Gate

Written after the Phase 2 musl session. A real musl `malloc(8 MiB)`
walks through the shim, the personality server, the kernel's
mmap path, and back. `fopen("/HELLO.TXT", "r") + fread + fclose`
runs the existing Phase 1 gate through the same plumbing instead
of direct syscalls. The malloc path makes a Linux libc actually
useful on Lockjaw — programs can now allocate memory and use
stdio.

It also took a lot of review rounds. Eight on revocation alone,
several more on Phase 2.K, a few on each of 2.0 → 2.5. The
dominant story this session isn't any single phase — it's the
shape of the review feedback and what it kept catching.

## The unblock no one wanted

Phase 2's first plan was straightforward: Linux mmap into musl's
malloc. I drafted `docs/posix-phase2-mmap-plan.md`, started Phase
2.K (variable-size PageSet header), and wrote a `consume_pageset`
that leaked the entire allocated header block as a tombstone. The
buddy allocator requires matched-shape free for contiguous
allocations; I hadn't thought through the case where consume
needed to return memory it didn't own.

Codex caught it. I tried fixing the leak by freeing tail pages
individually — buddy allocator corruption. Tried reverting to
"leak the whole block as tombstone" — Ben pushed back: "Don't be
a lazy bastard Claude, fix the bloody leak."

The fix was handle revocation. The kernel had needed it for
months — the `consume_pageset` doc explicitly flagged the leak as
"the proper fix is handle revocation (future work)". I'd been
deferring it because it touches every process's handle table.

The right call, in retrospect, was Ben's: "I think we are better
off stashing what you have done here and making a plan for handle
revocation. That way we don't accumulate more debt on a weak
point in the kernel." So Phase 2.K stashed, revocation plan
written.

The plan iterated through ~8 Codex review rounds before the first
line of revocation code got written. Each round caught a real
correctness issue that needed a design change, not just a code
change:

- The kernel's existing `unmap_validated` was a single fallible
  step that mutated state. Cross-process revoke needed a
  validate/apply split so we could check every process's table
  read-only first, then do the writes only once everything was
  known to succeed.
- Once we had a validate/apply split, `sys_create_process`'s
  destructive parent-side unmap (which used to happen *before*
  the still-fallible `scheduler::add_thread`) became wrong by
  inheritance. The plan grew a `scheduler::has_room()` precheck
  and a full reorder that pushed every fallible step into the
  validate phase.
- The `parent_handle_to_copy` path in `sys_create_process` had a
  pre-existing bug: it inserted a copied PageSet handle into the
  child's table without `inc_refcount`. Naively fixing that
  inside revocation introduced an ordering trap (the child's
  handle table wasn't yet visible to `scheduler::threads()`).
  Cleanest answer was to reject PageSet kind for
  `parent_handle_to_copy` entirely; users who need that can call
  `sys_export_handle`.

Etc. Codex's reviews kept finding "same class of bug, different
instance." That's a real signal: the architecture lets us reason
about decisions before we commit them.

After ~8 rounds the plan was 1000 lines and self-consistent.
Implementation took two commits and went smoothly because every
decision had been chewed over. Total cost: maybe twice the
straight-line implementation time, but the result is a clean
two-phase revocation that makes consume transactional. The
tombstone leak is gone; cross-process exported handles are
correctly cleared on consume.

## Verifying it actually runs

Before moving to Phase 2.K I asked: does the integration test
actually exercise the new path, or am I trusting "no regression"
to mean "feature works"? `make test` was 83/83. That doesn't
prove `revoke_apply` ever cleared a slot — a silent bypass of the
walker would still pass.

Added a `revoke OK: header=N procs=N slots=N maps=N` kprintln to
`consume_pageset_apply`. Boot output showed 53 emits per boot,
`procs` ranging 2..11 — the walker is firing on every consume,
walking every live process. Locked it down with two integration
assertions: the prefix appears, and at least one walk reports
`procs >= 2` (regex over `[2-9]` or two-digit counts; not pinned
to a specific number after Codex flagged that as brittle).

Lesson reinforced: "make test green" is necessary but not
sufficient. The plan called for this diagnostic explicitly and I
almost forgot.

## Fast fix vs right fix, on Codex's terms

Phase 2.K turned out to be a much bigger design exercise than I
expected. The variable-size `PageSetHeader` is conceptually
simple — 16 bytes of metadata followed by an inline u64 array
spanning multiple physically-contiguous header pages. The fix to
the kernel allocator is mechanical: `alloc_pages_contiguous` for
the header, `dealloc_pages_contiguous` to free.

But the type-system fallout was severe. Once `pages` stops being
an inline `[u64; 510]` array and becomes "the bytes immediately
following the struct," the safe-Rust API I'd had — `pub fn
get_page(&self, i: usize) -> Option<u64>` — becomes unsound on
any non-backed instance. A safe caller could write
`PageSetHeader::empty(); h.init(&[0x1000])` and clobber the
stack.

First fix was the lazy one: mark the four backing-dependent
methods `unsafe fn` and add SAFETY comments at every call site.
11 sites, 11 documented contracts. Tests passed. I shipped it.

Codex caught it in the next round. I'd asked for the wrong
property. The point of `unsafe` isn't that callers are *now*
careful; it's that the type system *prevents* misuse. With
`unsafe fn init`, a future caller who reaches for the convenient
empty value can still write the same broken code, just inside an
`unsafe` block. The type system only catches "did you remember
unsafe"; it doesn't enforce the contract.

Ben asked the right question: "are you doing the fast fix or the
right fix here?" I'd been honest about the tradeoff in the
explainer but had picked the smaller diff for speed.

The right fix is the wrapper pattern: hide the unsafe entirely
behind a `BackedHeader<'a>` / `BackedHeaderMut<'a>` type that
carries a backing-pages witness at construction. Once you have
the wrapper, all page-addr methods are *safe*. The unsafety is
locked into the constructor: `unsafe fn backed(&self,
backing_pages: usize) -> BackedHeader<'_>`. Method resolution
makes the wrapper's `data_page_count` win over the Deref-target
on the raw header, so the wrapper's count overrides the on-disk
field for all bounds checks.

That redesign was three more rounds with Codex. The witness has
to come from trusted state — taking it from the header's own
`header_pages` field (which I did first) is circular: a corrupt
header lies about its own backing. Fixed by deriving from the
PageSetTable's registered count, which the kernel allocator wrote
at registration time and which lives in independent storage.

Then Codex caught that the wrapper *also* trusted the header's
own `count` field for the index bound. Same circular trust
problem, fixed the same way: the wrapper now takes `count` at
construction and uses it for both the logical bound and the
backing-derived safety bound. `data_page_count()` returns the
wrapper's tracked count, not the on-disk field.

The pattern that emerged: **trusted state drives behavior, on-disk
state is just a copy**. The wrapper writes count + header_pages
to the header on `set_count` for downstream consumers, but the
wrapper itself never reads from those fields for its own bounds
checks. A corrupted on-disk header can no longer truncate
operations; it just produces wrong-but-bounded results that the
wrapper's checks catch.

This is the same shape as the revocation work. Trust the table,
not the header. Trust the witness, not the self-description.

## The phase that wasn't in the plan

Phase 2.4 (the 8 MiB malloc gate) failed first time. The error
was `fread returned 0` on the user side, which traced back to
the kernel rejecting any mapping > 512 pages with
`ErrorTooManyPages`. The kernel's `validate_mapping` had a
hard-coded 512-page cap (single L2 region), and 8 MiB needs four
L2 regions worth of mapping.

The plan didn't mention this. It assumed the variable-size
header from Phase 2.K was sufficient — true on the storage side,
but the mapping primitive had its own cap. Phase 2.M was born
mid-stream as kernel pre-work for 2.4: lift the cap, rewrite
`map_pages_in_existing` to walk multiple L2 regions.

The first iteration was wrong in a way I should have seen
coming. I wrote the multi-L2 loop inline in the kernel. Two host
tests, all green. Ben pushed back: "only 2 more tests for a
change of this magnitude seems a bit light, are you doing the
fast fix or the right fix here?" Same question as Phase 2.K, two
rounds later.

Right fix: extract the L2 region slicing into a pure
`L2RegionIter` in lockjaw-types. 11 host tests pin the boundary
behavior — single page, full region, spilling by one page,
spans-many, mid-region start, max-practical-size, and the
invariants (l3_start only nonzero on first region, l2_idx
strictly increases, data_offsets accumulate).

Then Codex caught that the iterator-driven loop wasn't atomic.
A mid-loop block conflict or OOM during L3 allocation would
leave earlier regions mapped with no rollback. Fixed by making
the loop a transactional three-pass: classify (read-only),
pre-allocate (reverses on partial failure), apply (cannot fail).
Same shape as the consume_pageset_validate/apply split from
revocation.

The plan-vs-implementation gap is real and recurring. Phases as
documented underrepresent the work. Phase 2 grew a kernel
pre-work commit (2.M) that wasn't in the plan; Phase 2.K grew
into a wrapper-API redesign that wasn't in the plan; revocation
itself was unplanned pre-work that emerged from Phase 2.K. The
honest accounting: the original plan estimated the visible
features, not the architectural lift each one required.

## Where things stand

Phase 2 is done end-to-end:

- `malloc(8 MiB)` round-trips through musl → shim → posix-server
  → kernel and back, exercising every path the plan called for.
- `fopen("/HELLO.TXT") + fread + fclose` runs the existing
  Phase 1 gate through musl stdio, proving the mmap subsystem
  supports stdio (which the original Phase 1 test had to dodge
  because mmap didn't exist).
- The handle revocation infrastructure introduced as
  Phase 2.K's prerequisite is now used on every kernel-object
  consume — 53 revoke walks per integration boot, with the
  diagnostic asserted in CI.

Test counts: lockjaw-types host tests went from ~463 to **708**
across this session (plus all the bug-class lockdowns). QEMU
integration went from 83 to **87** with the new gates (revoke
diagnostic, 1 MiB malloc, 8 MiB malloc, plus alignment-rejection
guards in the dispatch layer). Both GICv2 and GICv3 pass.

The pattern of work also shifted noticeably. Earlier phases were
"build the feature, then fix bugs Codex finds." This phase was
"plan the feature with Codex until the design holds, then build
in two commits, then fix the implementation bugs Codex finds."
Probably 2-3x the planning-to-coding ratio of phases 1-15.
That's mostly because the kernel surface area has crossed a
threshold where features have non-local consequences. Revocation
touches every process's handle table; the variable-size header
touches every PageSet allocation path; multi-L2 mapping touches
every userspace map call. None of these can be added by changing
one file.

The leverage is still real — eight Codex rounds on a plan that
takes two commits to implement still beats writing the wrong
thing first. But the shape is changing. Phases 17+ will
probably look more like this: more design, less greenfield code,
more cross-cutting changes. The pattern is the same as the
mature phases of any kernel project: the architecture pays
dividends on the easy work and asks for more rigor on the hard
work.

Next phase from `docs/posix-musl-plan.md` is filesystem write,
followed by threads (sys_futex), processes (posix_spawn / wait),
pipes, and signals. Each of those will surface its own
pre-work. The handle revocation precedent suggests it's better
to plan that pre-work explicitly than to hit it mid-implementation.
