# AI-Native Hardening Punch List

This document is the execution companion to
[`ai-native-hardening-pass.md`](./ai-native-hardening-pass.md).

That document explains where Lockjaw still falls short in architectural
terms. This one turns that diagnosis into a subsystem-by-subsystem pass
list: what to revisit, why it still matters, what "done" should look
like, and what specific files currently carry the gap.

The ordering here is not by code size. It is by architectural risk:

1. lifecycle and ownership-transfer boundaries
2. page-table and handle-table mutation boundaries
3. syscall wrappers that still carry policy
4. active design-doc surfaces that feed implementation
5. remaining shared-layout and validation cleanups

## Severity scale

- `P0`: high-risk lifecycle boundary; easy to re-break; worth a focused
  pass even if current code is "working"
- `P1`: important wrapper or interface-width problem; not as explosive
  as `P0`, but still a recurring source of drift
- `P2`: worthwhile cleanup that tightens consistency and lowers future
  review load

## 1. Process Creation Outer Orchestration

Priority: `P0`

Primary files:

- [src/process.rs](/Users/Ben/Code/Lockjaw/src/process.rs)
- [lockjaw-types/src/process.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/process.rs)
- [docs/tracking/extraction-roadmap.md](/Users/Ben/Code/Lockjaw/docs/tracking/extraction-roadmap.md)

Why this is still a target:

- `ProcessTransferPlan`, `ProcessMapping`, and teardown planning are in
  good shape in `lockjaw-types`
- the outer `create_process()` sequence in the kernel is still long,
  imperative, and phase-boundary-sensitive
- this is exactly the kind of function where one plausible local edit
  can move a fallible step below the true commit point

What to review:

- where the real point of no return is
- whether the validate/apply split is still only comment-backed
- whether any side effect after "apply starts" is still plausibly
  fallible
- whether drop guards are carrying too much architectural meaning
- whether child-handle-table population remains impossible to reorder
  accidentally

What "done" should look like:

- the outer sequencing is itself a named pattern, not just the inner
  pieces
- the commit boundary is visible in type or function shape
- there is no latent "one more operation after commit" ambiguity
- the wrapper reads as mechanical orchestration around a smaller number
  of explicit states

## 2. PageSet Consumption and Handle Revocation

Priority: `P0`

Primary files:

- [src/cap/pageset_table.rs](/Users/Ben/Code/Lockjaw/src/cap/pageset_table.rs)
- [src/cap/handle_table.rs](/Users/Ben/Code/Lockjaw/src/cap/handle_table.rs)
- [lockjaw-types/src/handle_ops.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/handle_ops.rs)
- [lockjaw-types/src/page_table.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/page_table.rs)
- [docs/history/handle-revocation-plan.md](/Users/Ben/Code/Lockjaw/docs/history/handle-revocation-plan.md)

Why this is still a target:

- this area has already proven itself to be a multi-round review magnet
- the architecture is much better now, but the implementation pass will
  still be mechanically delicate
- this is where ownership, revoke accounting, PTE clearing, and global
  table unlinking all meet

What to review:

- whether the final code really follows the doc’s validate/apply model
- whether consume helpers are the single point of revoke/apply truth
- whether refcount/map_count adjustments happen exactly once per real
  handle or mapping removal
- whether any failure path still leaves a half-revoked or half-consumed
  object shape

What "done" should look like:

- the destructive story is uniformly routed through one narrow helper
  contract
- the wrapper cannot accidentally perform revoke twice or skip unlink
- partial-revoke reasoning is gone from steady-state code paths
- the implementation matches the final doc without "interpretive"
  freedom

## 3. Page-Table Mutation and Walk Contracts

Priority: `P0`

Primary files:

- [lockjaw-types/src/page_table.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/page_table.rs)
- [src/arch/aarch64/vmem.rs](/Users/Ben/Code/Lockjaw/src/arch/aarch64/vmem.rs)

Why this is still a target:

- this is one of the strongest current exemplars of the methodology
- it is also a good example of how the code can still lag the intended
  contract until review forces alignment
- future extensions will be tempted to add "just one more branch" at
  the wrapper layer

What to review:

- whether all wrapper-side semantic branching has actually been pushed
  down
- whether every "post-validate divergence is impossible" claim is backed
  by panic/assert behavior
- whether block/page/table distinctions are fully owned by the pure
  walker vocabulary

What "done" should look like:

- the wrapper reads as raw memory access plus TLB side effects
- all branchy interpretation of PTE meaning is in `lockjaw-types`
- impossible post-validate outcomes are loud, not forgiving

## 4. `sys_map_pages` and `sys_unmap_pages`

Priority: `P1`

Primary files:

- [src/syscall/handler.rs](/Users/Ben/Code/Lockjaw/src/syscall/handler.rs)
- [docs/tracking/extraction-roadmap.md](/Users/Ben/Code/Lockjaw/docs/tracking/extraction-roadmap.md)

Why this is still a target:

- the roadmap already names `sys_map_pages()` as a remaining inline
  decision site
- these handlers still sit at the boundary between capability state,
  VA rules, and page-table side effects
- they are exactly the sort of syscall wrappers that can quietly grow
  policy over time

What to review:

- whether mapping eligibility is returned by a pure decision vocabulary
  or still assembled inline in the handler
- whether mapped/unmapped sentinel semantics are still too implicit
- whether the unmap side is equally narrow and symmetric

What "done" should look like:

- the syscall handler does lookup, invokes a pure decision, performs the
  side effect, updates the handle state
- it no longer owns the semantic answer to "is this operation legal"

## 5. Kernel Object Creation Wrappers

Priority: `P1`

Primary files:

- [src/syscall/handler.rs](/Users/Ben/Code/Lockjaw/src/syscall/handler.rs)
- [src/cap/object.rs](/Users/Ben/Code/Lockjaw/src/cap/object.rs)
- [src/cap/reply.rs](/Users/Ben/Code/Lockjaw/src/cap/reply.rs)

Why this is still a target:

- `create_kernel_object()` is one of the better wrappers in the kernel
- it is good enough to be a model, but not yet so narrow that it
  completely disappears as a policy-bearing site
- object-header construction is still more literal and distributed than
  it needs to be

What to review:

- whether object/header creation can be further construction-safe
- whether init/consume/insert sequencing can be expressed with less
  wrapper-owned policy
- whether error translation is still wider than necessary

What "done" should look like:

- object creation wrappers are obviously "validate, initialize, consume,
  insert"
- header construction is centralized in shared constructors
- adding a new object type does not invite one-off local sequencing

## 6. PageSet Allocation and Rollback

Priority: `P1`

Primary files:

- [src/cap/pageset_table.rs](/Users/Ben/Code/Lockjaw/src/cap/pageset_table.rs)
- [lockjaw-types/src/pageset_table.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/pageset_table.rs)
- [src/mm/page_alloc.rs](/Users/Ben/Code/Lockjaw/src/mm/page_alloc.rs)

Why this is still a target:

- allocation and rollback are still closely coupled to loop structure
- the new variable-size header work increases the importance of getting
  allocation/free symmetry exactly right
- subtle allocator-shape mismatches are hard to spot locally

What to review:

- whether contiguous header allocation is always released through the
  matching allocator contract
- whether rollback is encoded as a smaller plan/state rather than a
  loop-counter convention
- whether tombstone or special-case cleanup paths still bypass the main
  allocator symmetry

What "done" should look like:

- allocation shape and cleanup shape are obviously paired
- contiguous allocation is never partially "peeled apart" by ad hoc
  deallocation
- rollback logic is explicit enough that changing the loop structure
  does not silently change cleanup semantics

## 7. Process Teardown

Priority: `P1`

Primary files:

- [lockjaw-types/src/process.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/process.rs)
- [src/cap/process_obj.rs](/Users/Ben/Code/Lockjaw/src/cap/process_obj.rs)
- relevant teardown call sites in the kernel

Why this is still a target:

- the pure teardown plan is one of the better examples of "illegal
  states made unrepresentable"
- the remaining question is whether the kernel-side execution and
  adjacent lifecycle code stay as narrow as the plan deserves

What to review:

- whether the teardown variants still fully encode the meaningful state
- whether any wrapper branches are reintroducing semantic distinctions
  outside the plan
- whether process-object layout and helper APIs are still kernel-local
  where they could be shared and pinned

What "done" should look like:

- teardown code is an obvious iteration over `TeardownStep`
- no boolean re-expansion of plan semantics occurs in the kernel
- layout-sensitive process-object facts that can live in
  `lockjaw-types` do so

## 8. Scheduler Boundary and Precheck/Commit Alignment

Priority: `P1`

Primary files:

- scheduler modules
- [src/process.rs](/Users/Ben/Code/Lockjaw/src/process.rs)
- [lockjaw-types/src/scheduler.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/scheduler.rs)

Why this is still a target:

- Lockjaw already has a strong pure scheduler model
- the risk is at the boundary: code that assumes "cannot fail after
  precheck" without making that assumption structurally obvious
- process creation and other lifecycle operations depend on scheduler
  room and commit ordering staying aligned

What to review:

- whether `has_room`-style prechecks are explicitly paired with the
  operations they justify
- whether any kernel path still relies on scheduler success after a
  destructive step
- whether any state assumptions remain comment-only

What "done" should look like:

- scheduler-facing lifecycle commits are visibly prevalidated
- later fallibility does not sit below destructive state change
- boundary assumptions are named and narrow

## 9. POSIX Personality Server

Priority: `P1`

Primary files:

- [user/posix-server/src/main.rs](/Users/Ben/Code/Lockjaw/user/posix-server/src/main.rs)
- [lockjaw-types/src/posix.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/posix.rs)
- [lockjaw-types/src/elf_loader.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/elf_loader.rs)
- [lockjaw-types/src/posix_fd.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/posix_fd.rs)

Why this is still a target:

- this area has made major progress and is one of the best userspace
  proofs of the methodology
- it is also in active growth, which is exactly when wrappers tend to
  regain semantic branching
- the outer server loop, fd forwarding, mmap work, and filesystem
  forwarding can still widen if not kept disciplined

What to review:

- whether new syscall arms go through pure `dispatch()` additions first
- whether per-client state remains in narrow pure data structures
- whether bootstrap, rollback, and deferred-close paths remain explicit
  rather than ad hoc
- whether runtime handlers are becoming mechanical glue or regaining
  local policy

What "done" should look like:

- `main.rs` keeps shrinking toward side-effect glue
- new syscall support means new pure action vocabulary first, then small
  handler glue
- per-client/process state lives in shared, host-tested shapes where
  possible

## 10. FAT32 and Filesystem Server Layers

Priority: `P1`

Primary files:

- [lockjaw-types/src/fat32.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/fat32.rs)
- [lockjaw-types/src/fs.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/fs.rs)
- [user/fat32-server/src/main.rs](/Users/Ben/Code/Lockjaw/user/fat32-server/src/main.rs)
- [user/lockjaw-userlib/src/fs.rs](/Users/Ben/Code/Lockjaw/user/lockjaw-userlib/src/fs.rs)

Why this is still a target:

- the pure FAT32 logic is in a good direction
- server-side ownership, cursor, handle isolation, and path semantics
  have already shown they are easy to get subtly wrong
- as features grow, the risk is not the parser but the live protocol and
  state ownership model

What to review:

- whether server-owned open-file state remains single-source-of-truth
- whether handle ownership and caller-token isolation remain explicit
- whether protocol validation stays strict at the wire boundary
- whether new path or directory features go through shared pure helpers
  before landing in the server

What "done" should look like:

- protocol and parser logic live in `lockjaw-types`
- server code is mostly block I/O plus open-file table mutation
- caller isolation and path normalization rules are explicit and tested

## 11. Active Architecture Docs

Priority: `P1`

Primary files:

- [docs/history/handle-revocation-plan.md](/Users/Ben/Code/Lockjaw/docs/history/handle-revocation-plan.md)
- [docs/history/posix-phase2-mmap-plan.md](/Users/Ben/Code/Lockjaw/docs/history/posix-phase2-mmap-plan.md)
- [docs/tracking/extraction-roadmap.md](/Users/Ben/Code/Lockjaw/docs/tracking/extraction-roadmap.md)
- other active subsystem plans

Why this is still a target:

- in Lockjaw, these docs are not passive notes; they are implementation
  inputs
- drift between algorithm, phasing, and summary sections has already
  produced multiple review loops
- AI-assisted implementation will happily follow the wrong section if
  the doc permits it

What to review:

- whether each doc has one canonical algorithm section
- whether phasing sections restate logic unnecessarily
- whether risks, summaries, and pseudocode still match the final model
- whether "files to modify" lists still imply a stale design

What "done" should look like:

- the docs are safe to implement from directly
- contradiction sweeps are part of document review
- summaries point back to canonical sections instead of re-describing
  them loosely

## 12. Shared Layout and Constructor Cleanups

Priority: `P2`

Primary files:

- [src/cap/object.rs](/Users/Ben/Code/Lockjaw/src/cap/object.rs)
- [src/cap/reply.rs](/Users/Ben/Code/Lockjaw/src/cap/reply.rs)
- [src/cap/process_obj.rs](/Users/Ben/Code/Lockjaw/src/cap/process_obj.rs)
- [lockjaw-types/src/thread.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/thread.rs)
- [lockjaw-types/src/process.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/process.rs)

Why this is still a target:

- these are not the highest-risk bugs, but they are the kind of
  low-grade inconsistency that makes future refactors noisier than they
  need to be
- the repo already has good examples of shared layout pinning; the
  remaining gaps are noticeable because the standard is higher now

What to review:

- which header/object constructors are still handwritten as struct
  literals
- which shared layouts remain kernel-local without a strong reason
- where "must match" comments still exist instead of shared constants or
  constructors

What "done" should look like:

- layout-sensitive shared objects are defined once
- construction-safe helpers exist for common headers
- new fields cannot be missed silently across call sites

## 13. Validation Helper Batch

Priority: `P2`

Primary files:

- [src/syscall/handler.rs](/Users/Ben/Code/Lockjaw/src/syscall/handler.rs)
- relevant `lockjaw-types` validation modules
- [docs/tracking/extraction-roadmap.md](/Users/Ben/Code/Lockjaw/docs/tracking/extraction-roadmap.md)

Why this is still a target:

- the roadmap already lists a batch of low-cost syscall validations that
  can be pulled into pure helpers
- these are not glamorous, but they reduce the amount of inline
  parameter-policy living in the syscall layer

What to review:

- thread VA validation
- allocation flag decoding
- unmap/query VA rules
- any remaining small syscall-local validators that recur in review

What "done" should look like:

- tiny validation rules no longer live as one-off inline conditionals
- syscall handlers read as lookup + pure validation + side effect

## 14. Stack Layout, Constants, and "Must Match" Pairs

Priority: `P2`

Primary files:

- stack layout code
- linker-adjacent constant definitions
- shared constants in `lockjaw-types`

Why this is still a target:

- "must match" comments are architectural debt, even when they are not
  currently failing
- this is exactly the kind of repo detail that AI tools will not infer
  reliably from prose alone

What to review:

- duplicated constants across kernel and `lockjaw-types`
- stack/layout constants that should derive from one source
- any other places where shape consistency is still maintained by human
  memory

What "done" should look like:

- one source of truth per cross-layer layout constant
- comments no longer carry the real synchronization burden

## Recommended pass order

If you do one explicit hardening sweep after the current work lands,
this is the order that will likely pay off best:

1. Process creation outer orchestration
2. PageSet consumption and handle revocation implementation
3. Page-table mutation and walk wrappers
4. `sys_map_pages` / `sys_unmap_pages`
5. Scheduler precheck/commit alignment
6. POSIX server wrapper width and rollback surfaces
7. FAT32/filesystem state ownership and protocol surfaces
8. Active architecture docs contradiction sweep
9. PageSet allocation/rollback symmetry
10. Shared layout/constructor cleanups
11. Validation helper batch
12. Constants and "must match" cleanup

## Minimum bar for closing a punch-list item

Do not mark a subsystem "done" just because the immediate bug is fixed.
Close it only when:

- the dominant pattern is obvious from the code shape
- the pure logic is in `lockjaw-types` if it can be
- the wrapper is narrow and mechanical
- the failure boundary is explicit
- the host-test surface matches the real contract
- the docs and summaries do not contradict the implementation shape

That is the real finish line.
