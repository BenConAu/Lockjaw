# AI-Native Hardening Pass for Lockjaw

This document is not about what Lockjaw is already doing well. It is
about where the project still falls short of its own architectural
standard, using concrete examples from the current tree, and what a
serious second tightening pass should aim to eliminate.

Lockjaw is already ahead of most codebases in one important way:
there is a clear architectural thesis, a named pattern catalog, and a
real effort to move logic into `lockjaw-types` where it can be host
tested. That part is working.

The remaining gap is more specific:

- the methodology is not yet applied uniformly
- some core invariants are still carried by review discipline rather
  than structure
- some active subsystems are still in the awkward middle where pure
  helpers exist but the outer orchestration is still push-shaped
- the docs name the right shapes, but not every conversion is yet
  judged against a hard "done" standard

This document names those gaps directly and points at the code and
docs where they show up.

## What "AI-native hardening" means here

For Lockjaw, "AI-native" should not mean:

- generated lots of code quickly
- added more tests after the fact
- wrote large plans in natural language

It should mean:

- the architecture is shaped so an AI assistant can make correct local
  changes without needing to infer hidden sequencing rules
- policy and execution are separated sharply enough that reasoning can
  happen in pure code and side effects stay narrow
- the system makes it hard to express illegal states, partial commits,
  and cross-layer drift
- correctness is pinned by host tests and narrow interfaces, not by
  hoping future reviewers remember the same unwritten rules

Lockjaw is partway there. The rest of this document is the delta.

## The current shortfall, in one sentence

Lockjaw has a strong extraction philosophy, but it still relies too
much on human review to preserve that philosophy consistently across
kernel boundaries, long-lived plans, and evolving subsystem rewrites.

That shortfall appears in several distinct forms.

## 1. Pattern discipline is real, but not yet universal

The pattern catalog in `docs/architecture/patterns/README.md` is real and useful.
The problem is not that the patterns are weak. The problem is that not
every subsystem is yet forced into one of those shapes.

Concrete examples in the current tree:

- `lockjaw-types/src/process.rs` contains a strong pure core:
  `ProcessTransferPlan`, `build_teardown_plan`, and process-lifecycle
  decisions are in the right place.
- But `src/process.rs` still carries a large push-shaped outer shell in
  `create_process()`: user-memory reads, scratch pagination, drop-guard
  sequencing, address-space construction, TCB creation, child-table
  setup, and scheduler commit all live in one long function.
- `docs/tracking/extraction-roadmap.md` already names this exact gap under
  "Priority 1: create_process outer orchestration". That is good, but
  it also proves the subsystem is not finished yet.

Why this matters:

- AI tools are much safer when the code already sits inside one of a
  few repeated shapes
- mixed-shape subsystems invite local edits that reintroduce the exact
  bug class the extraction was supposed to eliminate
- human reviewers can often catch that drift; tooling and structure
  should do more of that work

What the hardening pass should do:

- treat `src/process.rs:create_process()` as unfinished until the outer
  sequencing is itself represented as a pattern, not just the inner
  data structures
- require new subsystem work to identify its pattern up front, not
  after the fact
- keep `docs/tracking/extraction-roadmap.md` live until the remaining
  push-shaped shells are either converted or explicitly justified

## 2. Too many invariants are still comments, not type shape

Lockjaw has improved a lot here, but not enough.

The strongest version of the architecture is:

- illegal states cannot be expressed
- partial commit phases are encoded explicitly
- impossible transitions are ruled out by return types

The weaker version, which still appears in places, is:

- comments say "must be called only after X"
- helpers rely on a matching previous validation pass
- a bool or a pair of ints stands in for a smaller, more exact state
  machine

Concrete examples in the current tree:

- `lockjaw-types/src/page_table.rs` is a strong example of the desired
  direction, but it also shows the current limit. The docs for
  `validate_pte_match()` and `clear_validated_pte()` describe a strict
  validate/apply contract. Recent review found that the code initially
  did not enforce the documented "L3 only" and "panic on post-validate
  divergence" rules. The architecture was right; the implementation was
  still relying on comments more than it should have.
- `src/process.rs:create_process()` still states its main safety rule in
  prose: "validate phase: all fallible work, no destructive mutation"
  followed by "apply phase: all infallible commits." That is better
  than nothing, but it is still a comment-level phase boundary, not a
  separate type or state token.
- `docs/architecture/01-architecture.md` explicitly argues for
  making illegal states unrepresentable. The fact that several active
  subsystems still need review to detect "this branch should be
  impossible after validate" means the principle is not yet enforced as
  hard as the docs claim.

Why this matters:

- if "must validate before apply" is mostly a comment, an AI or human
  can violate it with a locally-plausible edit
- if a kernel wrapper documents impossible outcomes but does not panic
  on them, divergence silently becomes partial success
- if a subsystem state is really an enum but modeled as loose fields,
  review has to reconstruct the state space every time

What the hardening pass should do:

- convert more comment-level "only valid after X" contracts into type
  distinctions, phase guards, or narrower result vocabularies
- treat "this cannot happen" as unfinished until it is either made
  unrepresentable or turned into a loud panic/assert
- specifically audit helpers whose documentation is more precise than
  their type signature

## 3. Validate/apply exists, but transactional boundaries are still fragile

Lockjaw has correctly converged on validate/apply for destructive
operations. That is one of the strongest parts of the architecture.

Where it still falls short is consistency at the boundary:

- some paths have a correct validate/apply split in the core, but leave
  adjacent side effects outside the split
- some operations are "mostly transactional" but still have one later
  fallible step after the destructive commit point
- some plans describe validate/apply correctly while the code around
  them is still one reorder away from reintroducing a partial-commit
  bug

Concrete examples in the current tree:

- `src/syscall/handler.rs:create_kernel_object()` is close to the gold
  standard. It does `consume_pageset_validate()`, then object init,
  then `consume_pageset_apply()`, then inserts the new handle. That
  path is narrow and readable.
- `src/process.rs:create_process()` is the counterexample. It now has a
  much better structure than before, but it is still the kind of long
  function where a future edit could move a fallible step below the
  point of no return without a type error stopping it.
- `docs/history/handle-revocation-plan.md` is the best recent example of how
  fragile this boundary still is in live design work. Multiple review
  rounds were spent forcing the plan to stop hand-waving around partial
  revoke failure, child-table timing, deduplication, and the exact
  consume helper to call. That is architectural work, but the number of
  rounds is also evidence that the transactional story is still easy to
  get subtly wrong.

Why this matters:

- the architecture has correctly identified partial-commit bugs as a
  primary enemy
- but some boundaries are still only socially enforced rather than
  mechanically closed

What the hardening pass should do:

- audit every destructive path for the exact point of no return
- make "all fallible work before this point" visible in code shape, not
  just comments
- aggressively isolate examples like `create_kernel_object()` as the
  standard and treat larger mixed functions like `create_process()` as
  still in migration

## 4. Kernel wrappers are thinner than before, but some still carry policy

The right Lockjaw shape is:

- `lockjaw-types` owns policy, transitions, classification, validation
- kernel wrappers own raw memory access, assembly, scheduler effects,
  MMIO, and TLB flushes

What still falls short:

- some wrappers still translate, fold, or reinterpret results in ways
  that subtly add policy back into the kernel
- some wrapper comments are more precise than the pure contracts they
  wrap, which is a sign the contract is not fully pushed down yet
- some "helper" functions in kernel land are still really policy
  functions with side effects attached

Concrete examples in the current tree:

- `src/arch/aarch64/vmem.rs` is trending in the right direction around
  the page-table walk and validated clear helpers. The kernel wrapper
  increasingly looks like "read raw PTE, call pure helper, perform TLB
  side effect."
- `src/syscall/handler.rs:sys_map_pages()` is explicitly called out in
  `docs/tracking/extraction-roadmap.md` as still having inline policy:
  "can this handle be mapped at this VA?" is still decided in the
  syscall handler rather than returned by a pure decision function.
- `src/syscall/handler.rs:create_kernel_object()` is a mixed example:
  it is much cleaner than older handlers, but it still decides error
  translation and sequencing in the kernel wrapper instead of exposing a
  narrower plan/result shape.

Why this matters:

- this is where drift begins: not giant handwritten logic, but small
  convenience decisions that accumulate outside the pure layer
- once wrappers start carrying semantic branches, future edits tend to
  add one more

What the hardening pass should do:

- treat wrapper boringness as an explicit quality metric
- whenever a wrapper branches for semantic rather than mechanical
  reasons, ask whether the branch belongs in `lockjaw-types`
- prioritize the wrapper-level items already named in
  `docs/tracking/extraction-roadmap.md`, especially `sys_map_pages()` and other
  syscall handlers that still mix validation and mutation inline

## 5. Documentation is strong, but doc drift is still a real architectural bug

This has shown up repeatedly in plan reviews:

- the main design section is fixed
- a later phasing section, risk note, or summary still carries the old
  model
- the contradiction is subtle enough that an implementer could follow
  the wrong section and recreate the bug

This is not a trivial docs issue. In an AI-assisted workflow, design
docs are active implementation inputs.

Concrete examples in the current tree:

- `docs/history/handle-revocation-plan.md` required multiple review rounds just
  to eliminate contradictions between the core algorithm, the
  `sys_create_process` pseudocode, and the phasing summary. The plan
  repeatedly fixed the main design while leaving old fallback or timing
  language alive further down.
- `docs/tracking/extraction-roadmap.md` is good because it is honest about what
  is done versus still push-shaped. That should be the standard: name
  the residual gap directly, not just the end state.
- The original `docs/history/ai-native-hardening-pass.md` version that prompted
  this rewrite is itself an example of the problem in a softer form: it
  claimed concrete shortfalls existed but did not cite them, which made
  it less actionable than it needed to be.

Why this matters:

- AI agents will treat prose plans as operational guidance
- contradictions in those plans are not harmless; they are a source of
  implementation bugs

What the hardening pass should do:

- require one canonical algorithm section in major design docs
- aggressively minimize logic restatement in phasing/summary sections
- add a deliberate contradiction sweep to document reviews:
  algorithm vs pseudocode vs phases vs risks vs files-to-modify

## 6. Host testing is good, but some invariants still stop at the pure layer

Lockjaw is already much better than most systems here. Still, several
important invariants remain only partially pinned:

- pure helpers are tested, but wrapper-level invariants are sometimes
  only described, not asserted
- some system-level contracts are split across multiple helpers and not
  directly exercised as a single host-testable story
- some failure-ordering guarantees rely on architecture comments rather
  than a pure model plus a narrow wrapper

Concrete examples in the current tree:

- `lockjaw-types/src/page_table.rs` now has good host tests for the
  walk/validate/clear semantics, but those tests had to be written only
  after review surfaced mismatches between the documented contract and
  the implementation.
- `lockjaw-types/src/process.rs:ProcessTransferPlan` has good host-test
  value around deduplication and header planning, but the outer
  `create_process()` sequencing that makes those plans safe is still not
  equally pinned by a pure model.
- `docs/architecture/01-architecture.md` correctly argues that the
  kernel should increasingly read like execution glue. The hardening gap
  is that some glue contracts are still only locally asserted, not yet
  lifted into a host-testable state machine or phase object.

The goal is not "move every possible line into host tests." The goal
is:

- whenever a correctness property can be decomposed into a pure model,
  do it
- whenever the kernel side remains mechanical, assert aggressively at
  the boundary

What the hardening pass should do:

- add boundary assertions wherever the pure layer has already proven a
  precondition
- prefer tests that pin "validate predicts apply" or "decision predicts
  wrapper action" rather than only unit-testing helpers in isolation
- treat a pure helper plus an unasserted wrapper as only a half-finished
  extraction

## 7. Review is still carrying too much architectural enforcement

This is the biggest cultural shortfall.

Right now, many of the best architectural wins still arrive through
review remarks like:

- this should live in `lockjaw-types`
- this apply phase still silently degrades
- this summary section contradicts the fixed algorithm above
- this child table is not included in the revoke walk

Those are good reviews. But in the strongest version of Lockjaw, fewer
of those comments should be necessary.

Concrete examples from recent work:

- the handle-revocation plan only reached a stable shape after repeated
  review findings about transactional boundaries, not-yet-scheduled
  child visibility, PageSet-kind handle copying, and deduplication
  rules
- the recent page-table validate/apply commit needed review to force
  the "L3 only" rule and loud panic on post-validate divergence to
  become real code rather than just documented intent
- the POSIX, FAT32, and mmap plan reviews repeatedly converged on the
  same meta-comments: make ownership explicit, narrow the protocol,
  remove duplicated state, and move decision logic into `lockjaw-types`

That pattern is useful data:

- the architecture is directionally right
- but review is still the main enforcement mechanism for several
  important contracts

What the hardening pass should do:

- treat recurring review comments as missing architecture, not just
  missed bugs
- when the same type of finding appears three times, create a helper,
  pattern note, checklist item, or narrower interface so the next case
  is harder to write incorrectly
- explicitly ask after major review cycles: "what structural guard
  would have made this comment unnecessary?"

## 8. Tooling does not yet encode enough of the methodology

The methodology is written down. That is good.

But Lockjaw still falls short of a fully hardened forward-engineering
environment because too little of the method is machine-checkable.

Concrete examples:

- `docs/architecture/patterns/README.md` and `docs/architecture/01-architecture.md`
  explain the right shapes, but nothing in the repo currently forces a
  new architectural doc to say which pattern it is using
- `docs/tracking/extraction-roadmap.md` names residual push-shaped shells, but
  there is no lightweight CI or checklist that prevents a converted
  subsystem from quietly regaining one
- recent document reviews had to catch internal contradictions manually;
  there is no doc-review checklist baked into the workflow

Possible future tooling directions:

- lightweight linting or grep-based CI checks for obvious pattern
  regressions
- templates for common extraction shapes
- doc checklists attached to major plan files
- a required "pattern selection" section in new architecture docs
- review macros/prompts for validate/apply boundaries

This does not need to become bureaucratic. It does need to become more
automatic.

## 9. The extraction roadmap exists, but "finished enough" is still easy to overestimate

One danger of success is that once the main architecture becomes
coherent, the remaining gaps look smaller than they are.

Typical failure mode:

- major subsystems have been converted
- the codebase feels much cleaner
- a handful of "minor" push-shaped leftovers remain
- those leftovers still sit on hot paths or lifecycle edges

Concrete examples in the current tree:

- `docs/tracking/extraction-roadmap.md` already admits that `create_process`
  outer orchestration, `sys_map_pages()`, PageSet allocation rollback,
  and page-table teardown are still active extraction targets
- these are not cosmetic leftovers; they are lifecycle and ownership
  boundaries
- that means the highest-risk remnants are exactly the ones most likely
  to generate future review-found sequencing bugs

What the hardening pass should do:

- keep the extraction roadmap live until the remaining push-shaped code
  is either eliminated or explicitly justified
- rank leftovers not by size but by bug potential:
  lifecycle, ownership transfer, teardown, scheduler boundaries,
  handle-table mutation, page-table mutation
- treat "small but central" leftovers as more urgent than "large but
  isolated" ones

## 10. Some interfaces are still wider than they need to be

A mature AI-native codebase tends to have narrow seams:

- specific enums
- exact helper return vocabularies
- small wrappers
- minimal mutation authority

Lockjaw still has some interfaces where callers can do more than they
should, then rely on comments to do the right thing.

Concrete examples in the current tree:

- `src/syscall/handler.rs:sys_map_pages()` is still wide enough that
  the handler itself decides too much about mapping eligibility instead
  of delegating that decision to a narrower pure result type
- `src/process.rs:create_process()` still exposes a broad imperative
  sequence rather than a tighter "setup state" to "commit state"
  transition
- some design docs still describe a low-level sequence of steps rather
  than a narrower helper contract. That leaves more imperative freedom
  to the eventual kernel caller than the architecture should prefer

What the hardening pass should do:

- prefer helper shapes that expose exactly the safe operation for the
  current subsystem
- only keep lower-level building blocks public when there is a real
  second caller or a strong testing reason
- narrow interfaces as soon as a repeated misuse pattern appears

## 11. AI-assisted development still needs a stronger "architecture before patch" loop

The repo is already much better than average here, but there is still a
cultural risk:

- AI tools can produce correct local patches fast
- that makes it easy to accept a series of locally-correct changes
  before checking whether the shape still matches the architecture

Concrete examples from recent work:

- several of the best outcomes in the POSIX, FAT32, revoke, and mmap
  work came from stopping and redesigning the shape before writing the
  next patch: who owns the cursor, where rollback lives, which state is
  local versus shared, what belongs in `lockjaw-types`
- the weaker moments were the ones where a plan was directionally right
  but under-specified around boundaries or ownership, and review then
  had to reconstruct the architectural contract after the fact

The remaining hardening step is to make this sequence explicit:

1. identify the pattern
2. name the point of no return if any
3. decide what belongs in `lockjaw-types`
4. decide what remains as kernel or server glue
5. then patch

What the hardening pass should do:

- ask "what is the pattern?" before large subsystem edits
- treat any inability to answer that question as design debt
- when a review finds a bug, ask whether the right fix is local or
  architectural

## 12. The project still lacks a hard "definition of done" for architectural conversions

This is a subtle but important gap.

A subsystem conversion should not be considered done merely because:

- tests pass
- the new helper exists
- the old logic mostly moved

It should be done when:

- the canonical pure logic lives in `lockjaw-types`
- the kernel wrapper is narrow and mechanical
- the old push path is actually removed
- host tests cover the pure branches and invariant edges
- docs and phasing notes are internally consistent
- adjacent failure ordering is explicitly resolved

Concrete examples in the current tree:

- `docs/tracking/extraction-roadmap.md` is valuable precisely because it refuses
  to call `create_process` finished just because `ProcessTransferPlan`,
  `AddressSpaceBuilder`, and `ScratchCursor` already exist
- the recent page-table work shows the same lesson: the extraction was
  not done when the helper existed; it was done only after the L3-only
  contract and panic-on-divergence behavior were made real and tested
- the handle-revocation plan only became implementation-ready once the
  pseudocode, phase ordering, dedup story, and failure model all agreed

What the hardening pass should do:

- define an architectural done checklist for major conversions
- use it for revocation, mmap, filesystem, SMP, teardown, and future
  object-model work
- refuse to call a subsystem "converted" while its wrapper still owns
  semantic branching or its docs still permit contradictory readings

## A practical second-pass checklist

When the current work is done, a serious hardening pass should review
each major subsystem with this checklist:

1. Pattern identification
   - Which of the four patterns does this subsystem use?
   - Is that visible from the code shape, or only obvious after
     explanation?

2. Pure/core boundary
   - What logic is still outside `lockjaw-types` that could be moved?
   - Does the kernel wrapper contain semantic branching?

3. Illegal states
   - Are any invariants still represented as comments plus loose fields?
   - Can those become enums, phase tokens, or narrower result types?

4. Transactional boundary
   - Where is the point of no return?
   - Are all fallible steps before it?
   - If not, is rollback explicit and proven?

5. Host-test surface
   - Which branches are host-tested?
   - Which invariants are only integration-tested or only reviewed?

6. Documentation consistency
   - Do algorithm, phasing, risks, and file lists agree?
   - Can an implementer follow the wrong section and write the wrong
     thing?

7. Interface width
   - Is the public helper surface narrower than the internal machinery?
   - Are callers exposed to lower-level operations they should not own?

8. Review recurrence
   - What bug classes keep showing up in review for this subsystem?
   - What structural change would make that review comment obsolete?

## Suggested focus areas for the hardening pass

These are the places where the second pass is most likely to pay off:

- remaining push-shaped lifecycle code, especially `src/process.rs`
- syscall handlers that still mix validation, mutation, and object
  policy inline, especially `sys_map_pages()`
- teardown and ownership-transfer boundaries
- scheduler-adjacent code where precheck and commit must stay aligned
- doc sets for active architectural work: `handle revocation`, `posix`,
  `mmap`, `SMP`
- wrapper APIs that still permit "technically callable but
  architecturally wrong" sequences

## What success looks like

After the hardening pass, Lockjaw should be able to claim:

- most correctness review comments are about local contract details,
  not "this logic is in the wrong layer"
- major kernel paths are obviously one of the named patterns
- validate/apply operations have explicit, narrow boundaries
- docs are consistent enough to serve as direct implementation inputs
- pure logic changes are cheap to test and hard to misuse
- the kernel increasingly reads like execution glue, not a second
  policy engine

That is the standard worth aiming at.

Lockjaw is already unusually close. The point of this document is that
"unusually close" is not the same thing as finished.
