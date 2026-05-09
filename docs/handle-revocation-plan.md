# Handle Revocation

## Context

The kernel has a per-consume page leak that's been live since the
PageSet model was introduced, and the existing
`consume_pageset` doc explicitly flags it:

> The header page is intentionally NOT freed. It stays allocated as a
> zeroed tombstone so that stale exported handles in other processes
> safely read count=0. Freeing would allow the page to be reused,
> making stale handles point at a live object — a use-after-repurpose
> bug. **The proper fix is handle revocation (future work).**

The leak fires every time a PageSet is consumed to create a kernel
object (endpoint / notification / reply / TCB) or to transfer
ownership during `sys_create_process`. One header page per consume,
forever. Bounded but monotonically growing.

Phase 2 mmap's variable-size `PageSetHeader` made the leak visible
because it would balloon the per-consume cost from 1 page to up to 33
pages. We stashed Phase 2.K (`stash@{0}`) and chose to land
revocation first instead of accumulating more debt on top of an
already-leaking primitive.

After revocation:

- Single-page-header consume leaks 0 pages (was 1).
- Multi-page-header consume (Phase 2.K) leaks 0 pages (was N).
- The "tombstone" pattern can be deleted from `consume_pageset`.

## Why the leak exists today

When process A calls `consume_pageset(header_paddr, A_handle_table)`:

1. The kernel removes every handle in **A's** handle table that
   references `header_paddr` (existing
   `HandleTableRef::remove_all_by_object`).
2. The kernel unlinks `header_paddr` from the global PageSet table.
3. The kernel does **not** walk other processes' handle tables.

If process B previously received an exported handle to the same
PageSet via `sys_export_handle`, B's handle still exists and still
references `header_paddr`. If we then freed the header page, the
buddy allocator could re-issue that page for unrelated data and B's
stale `lookup` would dereference whatever now lives there
(use-after-repurpose).

The current mitigation is the tombstone: zero `header_paddr`'s
contents but keep the page allocated. B's stale `lookup` reads
`count = 0` from the zeroed metadata and stops. Safe but leaks.

## Architecture

The fix is to walk every process's handle table during consume,
remove all handles to the target object, and undo any active
mappings — then it's safe to free the header normally.

### Existing infrastructure (reused)

- `HandleTableRef::remove_all_by_object(object_paddr)` —
  `src/cap/handle_table.rs:110`. Removes matching entries from one
  handle table.
- `HandleTableRef::unmap_for_object(object_paddr, cb)` — same file,
  line 83. Walks slots and invokes a callback for each slot with a
  non-zero `mapped_va_page`. Used today by `sys_create_process` for
  parent-side unmap.
- `process_obj::process_handle_table(process_paddr)` —
  `src/cap/process_obj.rs:88`-ish. Returns the handle-table paddr.
- `process_obj::process_ttbr0(process_paddr)` — same file, line 83.
  Returns the process's TTBR0 (for cross-process unmap).
- `arch::aarch64::vmem::unmap_validated` —
  `src/arch/aarch64/vmem.rs`. Validates and clears PTEs in any
  TTBR0; TLB-invalidates.
- Scheduler's `threads: [Option<PhysAddr>; MAX_THREADS = 16]` —
  `src/sched/scheduler.rs`. Each entry is a TCB paddr. TCBs carry
  `process_paddr`.

### New infrastructure

Two new top-level kernel functions, one per phase:

```rust
/// Phase 1: validate that every cross-process handle to
/// `object_paddr` is in a state where revoke can succeed. Walks
/// every live process's handle table read-only and verifies any
/// active PageSet mapping's PTEs match the kernel's expected
/// pages. Counts cleared-slot and unmapped-slot totals and
/// reconciles them against the header's snapshot refcount /
/// map_count.
///
/// **No state mutated.** If this returns Err, every handle table
/// and every page table is in exactly the state it was in at
/// entry. The caller can return an error to userspace cleanly.
///
/// Caller must hold the GKL.
pub fn revoke_validate(object_paddr: u64) -> Result<(), RevokeError>;

/// Phase 2: actually revoke. Walks every process's handle table
/// again; for each handle to `object_paddr`, clears the PTE
/// (TLB-invalidated) for any active mapping, decrements
/// map_count and refcount on the header, and clears the slot.
///
/// **MUST be called only after a successful `revoke_validate`**
/// for the same `object_paddr` within the same syscall (no GKL
/// release in between). Phase 2 cannot fail under that
/// precondition; any in-kernel error here panics — the kernel's
/// own invariants are broken.
///
/// After return: header.refcount == 0 && header.map_count == 0.
/// No handle in any process's table references `object_paddr`.
/// No PTE in any process's address space references the
/// PageSet's data pages. The caller may free the header pages.
///
/// Caller must hold the GKL throughout the validate→apply pair.
pub fn revoke_apply(object_paddr: u64);
```

`RevokeError` variants are:
- `AccountingMismatch { snapshot_refcount, snapshot_map_count, walked_refcount, walked_map_count }` — handle counts don't reconcile with the header's running counters. Indicates a missed inc_refcount somewhere or table corruption.
- `UnmapFailed { process_paddr, va }` — a cross-process PTE doesn't match the expected page. Indicates the user's page table diverged from the kernel's PageSet record.

Both errors are diagnostic: they pinpoint the offending process and address. Phase 1 returning either is recoverable.

Internally `revoke_validate` does:

1. Snapshot the PageSet header's `refcount` and `map_count`.
2. Walk `scheduler::threads()` (16-entry bounded array), dedupe
   to unique process_paddrs.
3. For each unique process, walk its handle table read-only:
   a. For each slot referencing `object_paddr`:
      - `removed_count += 1`
      - if it's a PageSet handle with `mapped_va_page != 0`:
        - call `validate_pte_match(p.ttbr0, mapped_va, header.pages_slice())`
        - on fail → return `Err(UnmapFailed { p, va })`
        - `unmapped_count += 1`
4. After the walk:
   - if `removed_count != snapshot.refcount` →
     `Err(AccountingMismatch)`
   - if `unmapped_count != snapshot.map_count` →
     `Err(AccountingMismatch)`
5. Return Ok. No state mutated.

Internally `revoke_apply` does the same walk but flips every
read into a write. Cannot return Err.

### Per-process walker

A PageSet handle is a counted reference: every `slot_insert` of a
PageSet-kind handle is balanced by a `dec_refcount()` on the
PageSet header at `sys_close_handle` time. The current bulk
helper `slot_remove_all_by_object` clears slots without any
per-slot accounting because the tombstone path means the object
header is never freed and refcount is moot.

Revocation changes that. After we revoke every cross-process
handle, we want to free the header normally — so refcount must be
maintained or the post-revoke `dec_refcount + free` path will
underflow / mis-free.

Two pieces of accounting per cleared slot:

- **map_count**: dec once per slot that had `mapped_va_page != 0`
  AND whose unmap succeeded. The unmap and the dec must travel
  together — a failed unmap leaves the page counted as mapped.
- **refcount**: dec once per slot regardless of mapping state.
  Every PageSet handle in any table contributes +1 to the
  PageSet's refcount.

The pure helper in `lockjaw-types/src/handle_ops.rs` yields a
per-slot `SlotRevokeAction` to the caller before clearing the
slot, so the kernel side can do both dec_map_count and
dec_refcount in the same loop:

```rust
/// Per-slot info yielded during a revoke walk. Lets the caller
/// take any per-kind cleanup actions (unmap from a TTBR0,
/// dec_refcount on a PageSet header, etc.) BEFORE the slot is
/// cleared.
pub struct SlotRevokeAction {
    /// True if this is a PageSet handle with an active mapping.
    pub had_mapping: bool,
    /// VA page of the active mapping (valid only when had_mapping).
    pub mapped_va_page: u32,
    /// Original handle kind — caller uses this to decide whether
    /// dec_refcount is needed (only for PageSet-kind handles in
    /// today's accounting model).
    pub kind: HandleKind,
}

/// Walk `slots`, yielding a SlotRevokeAction for each entry whose
/// `object_paddr == target`, then clear the slot. Returns the
/// number of slots cleared. The caller is responsible for the
/// per-action side effects (unmap, dec_map_count, dec_refcount).
pub fn slot_revoke_object<F>(
    slots: &mut [HandleEntry],
    target: u64,
    mut on_revoke: F,
) -> usize
where F: FnMut(&SlotRevokeAction);
```

`HandleTableRef::revoke_object` is the kernel-side wrapper that
pairs the slot scan with the kernel-side actions:

```rust
impl HandleTableRef {
    /// Revoke every handle in this table that references
    /// `object_paddr`. For each handle with an active mapping,
    /// invoke `unmap_cb(va) -> bool` (caller does the PTE work
    /// against the right TTBR0). On successful unmap, calls
    /// `dec_map_count_cb()`. On every cleared PageSet handle,
    /// calls `dec_refcount_cb()`.
    /// Returns `(unmapped_ok, removed_count)`.
    pub fn revoke_object(
        &self,
        object_paddr: u64,
        mut unmap_cb: impl FnMut(u64) -> bool,
        mut dec_map_count_cb: impl FnMut(),
        mut dec_refcount_cb: impl FnMut(),
    ) -> (usize, usize);
}
```

Counts let the top-level walker assert post-conditions: after
walking every process, the total `removed_count` must equal the
PageSet header's pre-revoke `refcount`, AND the total
`unmapped_ok` must equal the pre-revoke `map_count`. Mismatches
in the validate phase return `Err(RevokeError::AccountingMismatch)`
to the caller; consume_pageset propagates this as a syscall error
and the system stays consistent (see "Failure handling: two-phase
revocation" below).

## Failure handling: two-phase revocation

`unmap_validated` can fail if the user's page table doesn't match
the kernel's view of the PageSet (corruption, race, etc.). Today
this fires only on bug paths; the current `unmap_for_object` /
`plan.validate` chain in `sys_create_process` aborts the consume
with the parent's mappings intact.

Revocation runs against *every* live process's page table, so its
failure mode is broader. Whatever consume_pageset does on revoke
failure must NOT proceed with the ownership transfer — otherwise
some process retains a PTE that points at memory that's about to
become a different kernel object's storage, the precise
stale-cross-process-mapping bug revocation is meant to close.

A single-pass revoke (walk processes; for each, unmap-and-clear
matching slots) has the wrong failure semantics: if process N's
unmap fails, processes 0..N-1 have already been mutated (PTEs
cleared, slots cleared, refcount/map_count decremented). Even if
consume aborts after the failure, the kernel has been left in an
asymmetric state. A panic would mask the partial mutation behind
a halt, but the kernel's internal state is still inconsistent at
the moment of crash.

The right design is **two-phase revocation**, mirroring the
plan/apply split that already works in `sys_create_process`:

### Phase 1 — Validate (read-only)

Read the PageSet header's `refcount` and `map_count` snapshot.

For each unique process `p` (deduped from `scheduler::threads`):
  For each handle slot `s` in `p`'s handle table:
    if `s.object_paddr == header_paddr`:
      - `removed_count += 1`
      - if `s` is PageSet-kind with `mapped_va_page != 0`:
        - call `validate_pte_match(p.ttbr0, mapped_va, header.pages_slice())`
          (read-only walk; verifies PTEs match expected pages)
        - on fail → return `Err(UnmapFailed { p, va })`. **No
          state mutated.**
        - `unmapped_count += 1`

After the walk, check accounting:
  - if `removed_count != snapshot.refcount` → `Err(AccountingMismatch)`
  - if `unmapped_count != snapshot.map_count` → `Err(AccountingMismatch)`

Phase 1 has zero side effects. If it returns `Err`, every
process's handle table and page table is unchanged.

### Phase 2 — Apply (write)

Reached only if phase 1 returned Ok. Under GKL, the state can't
have changed between phases. Phase 2 cannot fail because phase 1
verified everything.

For each unique process `p`:
  For each handle slot `s` in `p`'s handle table:
    if `s.object_paddr == header_paddr`:
      if `s` is PageSet-kind with `mapped_va_page != 0`:
        - `clear_validated_pte(p.ttbr0, mapped_va)` (clear PTE +
          TLB invalidate)
        - `header.dec_map_count()`
      if `s` is PageSet-kind:
        - `header.dec_refcount()`
      - clear `s`

After phase 2: `refcount == 0 && map_count == 0`. The header is
unreferenced anywhere; consume_pageset can free it normally.

### Why phase 2 can't fail

Single-core under GKL: the only thing that could change between
phase 1 and phase 2 is the kernel itself (which doesn't preempt
inside a syscall). The user's page tables and handle tables are
frozen for the duration of the consume.

`clear_validated_pte` therefore CANNOT see a different PTE than
phase 1's `validate_pte_match` confirmed. The kernel-side
helpers don't allocate; they just read and write specific
addresses. No allocation failure path.

If phase 2 ever does fail (kernel bug; should be impossible):
panic. The system has invariants broken to a degree that
continuing is unsafe. But the design is structured so this is
provably-impossible state corruption rather than a routine
"unmap failed for this process" condition.

### `consume_pageset` failure surface

With two-phase, the failure surface shrinks:

- Phase 1 errors (`AccountingMismatch`, `UnmapFailed`) are
  recoverable. consume_pageset returns `Err` to its caller; the
  caller (e.g. `sys_create_endpoint`) returns a syscall error to
  userspace. The PageSet remains intact and the user can retry
  or close it.
- Phase 2 errors (only possible from kernel bugs) panic.

Making `consume_pageset` fallible does ripple to its callers,
but the change is small: each caller already has an error path
for `init_fn` failure; it adds one more error case.

For the rollback of `init_fn`'s writes: in
`create_kernel_object`, init_fn runs BEFORE consume_pageset
today. We swap the order: validate (phase 1) FIRST, then init,
then phase 2 + consume. Phase 1 doesn't observe the data page
contents, so init can safely run between. If phase 1 fails, init
never runs and there's nothing to roll back.

```rust
// New create_kernel_object flow:
1. Look up PageSet handle.
2. Run revoke phase 1 (validate). On Err, return early.
3. init_fn(page_paddr) — initialize the new object.
4. Run revoke phase 2 (apply). Cannot fail.
5. consume_by_header_paddr + dealloc header.
6. Insert new handle for the new object.
```

This makes the create flow truly transactional: nothing
externally visible commits until step 6.

## consume_pageset rewrite

`consume_pageset` becomes fallible (returns `Result`) and splits
the validate/apply pair so the caller can sequence them around
its `init_fn`:

```rust
/// Try to consume `header_paddr`. Validates that revocation
/// would succeed; returns Err without mutating state if not.
/// On Ok, the caller MUST call `consume_pageset_apply` later
/// in the same syscall (no GKL release between).
pub fn consume_pageset_validate(header_paddr: u64) -> Result<(), RevokeError> {
    revoke_validate(header_paddr)
}

/// Apply the consume. Reaches only after a matching successful
/// `consume_pageset_validate`. Cannot fail. Frees the header
/// pages and unlinks from the global PageSet table.
pub fn consume_pageset_apply(header_paddr: u64) {
    let header_pages = unsafe {
        read_header(header_paddr).header_page_count()
    };

    // Phase 2: clear PTEs, decrement refcount/map_count, clear
    // handle slots in every process. Cannot fail per the design.
    revoke_apply(header_paddr);

    // Unlink from the global PageSet table.
    consume_by_header_paddr(header_paddr);

    // Free the contiguous header block. Data pages are owned by
    // the new object (consume is the ownership-transfer path).
    page_alloc::dealloc_pages_contiguous(
        PhysPage::containing(PhysAddr::new(header_paddr)),
        header_pages,
    );
}
```

The old `consume_pageset(header_paddr, handle_table)` signature
goes away. Callers are restructured per the next section.

### `create_kernel_object` rewrite

```rust
fn create_kernel_object(
    handle: u32,
    kind: HandleKind,
    init_fn: impl FnOnce(PhysAddr) -> Result<(), CreateError>,
) -> Result<u64, SyscallError> {
    let ht = CurrentThread::handle_table();
    let entry = ht.lookup(handle, ..., ObjectType::PageSet)?;
    let header_paddr = PhysAddr::new(entry.object_paddr);
    let ps = unsafe { PageSetRef::from_header_paddr(header_paddr.as_u64()) };
    if ps.count() != 1 {
        return Err(SyscallError::INVALID_PARAMETER);
    }
    let page_paddr = ps.page(0).ok_or(SyscallError::INVALID_HANDLE)?;

    // Phase 1: validate revoke. No state mutated on failure.
    consume_pageset_validate(header_paddr.as_u64())
        .map_err(|_| SyscallError::INVALID_HANDLE)?;

    // Initialize the data page as the new kernel object. If init
    // fails, no consume has happened yet — the caller's PageSet
    // handle is still valid.
    if init_fn(page_paddr).is_err() {
        return Err(SyscallError::UNKNOWN);
    }

    // Phase 2: actually consume. Cannot fail.
    consume_pageset_apply(header_paddr.as_u64());

    // Insert the new kernel-object handle.
    ht.insert(page_paddr, ..., kind).map(|h| h as u64)
}
```

### `sys_create_process` rewrite

`sys_create_process` already had a partial validate/apply split
via `ProcessTransferPlan`, but the existing path is **not
transactional**: `src/process.rs:302` (parent-side unmap) is
destructive and happens before the still-fallible
`scheduler::add_thread` at `src/process.rs:338`. If the run
queue is full at step 338, the parent has already lost the
mappings unmapped at step 302, but the new process never
schedules and the consume never completes.

Today's behavior under that failure: parent loses mappings,
syscall returns "scheduler run queue full", parent's address
space is in a degraded state. Drop guards clean up the
new-process resources but do NOT restore the parent's mappings.

Revocation forces a real fix because the consume calls
themselves now have a validate/apply split. The destructive
parent-side unmap and the fallible scheduler add must both move
to the apply side of the same split, or one of them stops being
either destructive or fallible. The cleanest answer is to
reorder the steps so all fallible work runs in the validate
phase and only infallible commits run in the apply phase:

```rust
pub fn create_process(...) -> Result<(), &'static str> {
    // ---- Validate phase: all fallible work, no destructive mutation ----
    // 1. Look up parent handles (read-only):
    //      - stack PageSet, scratch PageSet, segment PageSets
    //        (read via the user mappings array)
    //      - parent_handle_to_copy → record (kind, object_paddr,
    //        rights). REJECT with Err if the kind is PageSet —
    //        see "PageSet kind not supported as
    //        parent_handle_to_copy" below.
    // 2. Allocate child resources with drop guards on failure:
    //      - proc page (ProcessObject)
    //      - ttbr0 builder + L1/L2/L3 page-table pages
    //      - child handle table page (init via create_handle_table —
    //        writes the CHILD's table, not the caller's)
    //      - TCB page + TCB stack page
    //    NOTE: child handle table is left empty here. The
    //    parent_handle_to_copy slot is inserted in the apply
    //    phase, AFTER all consumes are done, so revoke walks
    //    don't have to include a not-yet-scheduled process.
    // 3. Validate that every consumed PageSet header can be revoked.
    //    Build the deduplicated header set first via
    //    `ProcessTransferPlan::add_header` (preserved verbatim from
    //    today's lockjaw-types/src/process.rs:158): walking the
    //    user mappings array can produce the same header_paddr
    //    multiple times (a PageSet with N pages mapped at N
    //    contiguous VAs yields N ProcessMapping entries pointing
    //    at one header). add_header collapses duplicates and
    //    returns a unique header set, then:
    //      for each header `h` in plan.headers():
    //          consume_pageset_validate(h)?
    //    consume_pageset_validate is the same helper the kernel-
    //    object creation path uses; it wraps revoke_validate.
    //    Replaces today's destructive `unmap_for_object` walk
    //    plus `ProcessTransferPlan::validate`. add_header /
    //    headers() / MAX_CONSUMED_HEADERS bound stay; only the
    //    unmap-accounting fields (unmap_results, record_unmap,
    //    validate) are deleted.
    // 4. Validate that the scheduler can accept one more thread:
    //      if !scheduler::has_room() { return Err(...) }
    //    NEW HELPER: scheduler exposes a "would add_thread succeed
    //    right now?" check that's a precondition without mutation.

    // ---- Apply phase: all infallible commits ----
    // 5. Apply consume for each unique header in the same
    //    plan.headers() iteration order built in step 3. Each
    //    call does ONE revoke_apply (walks every process,
    //    including the parent, clearing PTEs and slots), ONE
    //    consume_by_header_paddr (unlink from the global PageSet
    //    table), and ONE dealloc_pages_contiguous (free the
    //    header pages):
    //      for each header `h` in plan.headers():
    //          consume_pageset_apply(h)
    //    No separate "parent unmap" step — revoke_apply walks
    //    the parent's table along with everyone else's and
    //    clears the PTEs as part of its work.
    // 6. Insert parent_handle_to_copy into the child's table:
    //      - If kind is Endpoint with caller_token == 0: bump
    //        endpoint.next_token (mutation in apply phase, fully
    //        transactional).
    //      - child_ht.insert(object_paddr, rights, child_kind)
    //        Cannot fail: child table was freshly allocated in
    //        step 2 and has zero entries.
    //    PageSet kind doesn't reach here (rejected in step 1),
    //    so no inc_refcount call is needed at process creation.
    // 7. Defuse drop guards (new process now owns its resources).
    // 8. scheduler::add_thread (cannot fail — has_room() returned
    //    true in step 4 and GKL has held throughout).
    Ok(())
}
```

Each unique consumed PageSet header passes through exactly one
`consume_pageset_validate` (step 3) paired with exactly one
`consume_pageset_apply` (step 5). The "exactly one" property
relies on `ProcessTransferPlan::add_header` deduplicating before
the consume loops run — without dedup, an N-page PageSet mapped
at N VAs would yield N validate/apply pairs against the same
header, which is double-consume territory (the second
revoke_validate would see refcount == 0 and the second
consume_by_header_paddr would unlink an already-unlinked entry
or panic). The dedup helper is preserved verbatim; only
`ProcessTransferPlan`'s unmap-accounting half goes away. There's
no separate `revoke_apply`/`free_header` pair to maintain — the
consume helper bundles them.

`sys_create_process` returns only a status code — there is no
caller-visible handle for the new process today (see
`src/syscall/handler.rs:374`, `src/process.rs:67`). The child's
private handle table page is allocated and initialized empty in
validate step 2 (via `create_handle_table` against the child's
table page); the existing
`process.rs:246-277` parent-handle-copy logic moves to apply
step 6 along with the Endpoint `next_token` bump. The child's
table stays empty for the entire validate phase and during the
consume_apply walks, so the not-yet-scheduled child never
participates in revoke accounting. No caller-table mutation is
required at any phase.

### PageSet kind not supported as `parent_handle_to_copy`

`process.rs:246-277` has a pre-existing bug: it inserts a copied
handle into the child's table without calling `inc_refcount()`
on the underlying object, even when the handle is a PageSet
kind. Compare with `sys_export_handle` at
`src/syscall/handler.rs:678`, which correctly bumps refcount on
every PageSet handle export. Today this bug is silent because
the only PageSet that goes through this path is whatever init
or posix-server passes as `parent_handle_to_copy` — currently
always an Endpoint handle, so refcount accounting never
matters.

Fixing the bug naively (inc_refcount on PageSet copies) creates
a different problem with the revocation design. The natural
place to put the inc is right next to the insert — but the
new child handle table doesn't appear in `scheduler::threads()`
(the source of process enumeration for revoke walks) until
`add_thread` runs in the apply phase. So during
`consume_pageset_validate` and `consume_pageset_apply`, the
child's table is invisible to the walker. Three observable
breakages:

1. If the inc lives in validate phase, the snapshot refcount
   in `consume_pageset_validate` includes the bumped count, but
   the walker doesn't see the child slot →
   `Err(AccountingMismatch)`.
2. If the inc lives at insert time in apply phase BEFORE the
   consumes, same as (1) but during apply. Apply isn't supposed
   to fail; this would assert / panic.
3. If `parent_handle_to_copy` happens to point at one of the
   consumed PageSet headers, the child ends up with a handle
   into an about-to-be-freed object — use-after-free.

For Phase 1 of revocation, **`sys_create_process` rejects
`parent_handle_to_copy` whose kind is PageSet** with an
explicit `Err("PageSet kind not supported as
parent_handle_to_copy")`. The check happens in validate step 1
before any allocation. This sidesteps the entire ordering
problem.

Today's only caller (init's `spawn_elf`) passes Endpoint
handles, so the restriction is a no-op for current code paths.
Endpoint kind continues to work via the existing token-bump
logic; the `next_token` mutation moves into apply phase
(step 6) where it's transactional with the insert.

Lifting the restriction is a future enhancement. Two viable
options:

- **Option A**: insert child PageSet handles in apply phase
  AFTER all consumes complete, with inc_refcount at insert
  time. The child is still not in `scheduler::threads()`
  during apply, but consumes are done so revoke accounting
  doesn't observe the inc. add_thread happens immediately
  after, so the window where the child has handles but isn't
  scheduled is microscopic.
- **Option B**: extend `revoke_validate` / `revoke_apply` to
  accept an optional "extra handle table" parameter — the
  child's not-yet-scheduled table — so the walk includes it.
  More general but adds an explicit parameter to the revoke
  API.

Both options work; both are out of scope for the initial
revocation commit because they're orthogonal to the
revocation core and the no-op-today restriction lets us land
the rest cleanly.

Two new pieces of infrastructure:

- **`scheduler::has_room() -> bool`** (or `Result`-typed): a
  pure check that the run queue has at least one free slot.
  Read-only; calling has_room() then add_thread() under GKL with
  no intervening syscall is guaranteed to succeed (no other
  thread can race because GKL is held).
- **Reordering the existing destructive section**: today's
  destructive parent-side unmap (`process.rs:302..332`) and the
  separate `consume_pageset` call go away as a unit. They're
  replaced by the validate-phase `consume_pageset_validate` (per
  consumed header) and the apply-phase `consume_pageset_apply`
  (per consumed header). `consume_pageset_apply` walks every
  process's table including the parent's, so the parent's
  unmaps happen as a side effect of the per-header revoke —
  not as a separate `unmap_for_object` call.
  `ProcessTransferPlan` is split: `add_header` /
  `headers()` / `MAX_CONSUMED_HEADERS` survive (still the only
  source of the deduplicated header set the consume loops walk),
  while the unmap-accounting half (`unmap_results`,
  `record_unmap`, `validate`) is deleted because
  `consume_pageset_validate` per header subsumes it.

What if `consume_pageset_validate` fails for any consumed
PageSet? Return Err, drop guards clean up the new-process
resources, parent's state is fully intact (no apply called, no
unmaps applied, no slots cleared). The user's
`sys_create_process` returns the failed errno; their original
PageSet handles still work.

What if `scheduler::has_room` returns false? Same path: drop
guards clean up new-process resources, parent's state intact,
return errno. No partial commit possible because no
`consume_pageset_apply` has been called.

This is a real refactor of `create_process` — touches ~50 lines
of reordering plus the new scheduler helper. It MUST land in
the same commit as the consume_pageset rewrite because today's
sys_create_process calls `consume_pageset` (singular). After
the rewrite it calls `consume_pageset_validate` (validate
phase) and `consume_pageset_apply` (apply phase) on every
header it consumes, with both calls bracketed around the
intermediate fallible work. A half-converted state would break
either with double-revocation (calling both old and new) or
with partial transactions (skipping the apply path's unlink).

### Why this is safe to free

After `consume_pageset_apply` returns:
- `header.refcount == 0 && header.map_count == 0` (revoke_apply
  decrements once per cleared handle / unmap, validated by
  phase 1's accounting reconciliation).
- No handle in any process's table references the header
  (revoke_apply cleared every matching slot).
- No PTE in any process's address space references the data
  pages (revoke_apply cleared every active mapping's PTE +
  TLB-invalidated).
- The header pages are the contiguous block originally returned
  by `alloc_pages_contiguous(header_pages)`; freeing with
  `dealloc_pages_contiguous(..., header_pages)` mirrors the
  allocator contract.

The data pages owned by the PageSet pre-consume are NOT freed
here — they belong to the new kernel object created via
`init_fn`.

### What happens if validate fails

`consume_pageset_validate` returning Err is an ordinary error.
The caller returns a syscall error to userspace. Nothing was
mutated:
- All handles to the PageSet still exist.
- All mappings to the PageSet's data pages still exist.
- The PageSet's refcount and map_count are unchanged.
- The user's PageSet handle is still valid; they can retry or
  close it.

The caller's syscall fails, but the system stays consistent.

## Tests

- **Pure host tests** in `lockjaw-types/src/handle_ops.rs`:
  - `revoke_yields_action_per_matching_slot` (mapped + unmapped)
  - `revoke_yields_correct_kind_for_pageset_handles`
  - `revoke_yields_correct_kind_for_endpoint_handles`
    (verifies the kind field even though endpoint-revoke isn't
    used yet; future-proof)
  - `revoke_preserves_slots_with_other_object_paddrs`
  - `revoke_with_no_matching_slots_yields_zero_actions`
  - `revoke_handles_multiple_matching_slots`
  - `revoke_action_count_matches_returned_count`
  - ~7 tests total.

- **Pure host test** in `lockjaw-types/src/pageset_table.rs`:
  - `refcount_round_trip_balances_inc_per_handle` — assert that
    N inserts via inc_refcount + N revokes via dec_refcount
    leaves refcount at 0. Sanity check that the accounting model
    is symmetric.

- **Refcount-balance test** in `src/cap/pageset_table.rs` (kernel
  side, exercised at boot):
  - On first consume_pageset of a PageSet with refcount=N,
    revoke must leave refcount=0 (asserted before free). If the
    assertion fires the kernel panics loudly rather than
    silently mis-freeing.

- **Validate-failure-leaves-no-mutation host test** (kernel
  side, exercised at boot):
  - Construct a synthetic two-process scenario where the second
    process's "page table" contents don't match the kernel's
    PageSet view (mock `validate_pte_match` returning Err for
    one slot). Verify:
    - `consume_pageset_validate` returns
      `Err(RevokeError::UnmapFailed { process_paddr, va })`.
    - All handle slots in BOTH processes are unchanged.
    - The PageSet's refcount and map_count are unchanged.
    - No PTEs in either process were modified.
  - This pins the no-side-effect-on-failure invariant.

- **Validate-then-apply round-trip**:
  - Verify that after a successful validate followed by apply,
    refcount == 0 && map_count == 0, every matching slot is
    cleared, every PTE is cleared.
  - This pins the symmetry between the two phases.

- **Bug-diagnostic completeness**: `RevokeError` variants must
  carry enough information for the kernel-warning log line to
  point at the exact problem. `UnmapFailed { process_paddr, va }`
  carries both; `AccountingMismatch` carries snapshot vs. walked
  totals.

- **Integration coverage** in `tests/qemu_integration.sh`:
  - The existing 83 assertions cover create_endpoint, create_thread,
    create_notification, create_reply, sys_create_process — every
    consume_pageset path. If revocation breaks anything in the
    happy path, these fail. **No new assertions needed for
    correctness**; revocation is a same-behavior internal change.

- Optional new assertion: log a "revoke OK: N handles in M
  processes" diagnostic at the end of `revoke_apply` and assert
  it appears for the posix-server spawn (which involves both
  kernel-object creation AND `sys_create_process`). Adds
  visibility but isn't load-bearing.

- **What's not tested** (would be nice but is non-trivial):
  - Page-allocator high-water-mark check showing the leak is gone.
    Requires page-accounting infrastructure we don't have.
  - Stress test: allocate + consume in a loop and verify steady-
    state memory. Same gap.

  These can land in a follow-up if leak regression becomes a
  concern. For now we rely on the architectural argument: every
  consume_pageset path that previously leaked one page now frees
  it via `dealloc_pages_contiguous`.

## Phasing

Two commits:

### Commit 1: pure helper

- Add `slot_revoke_object` (or similar) to
  `lockjaw-types/src/handle_ops.rs`. Same shape as
  `slot_remove_all_by_object` but yields per-slot data
  (`mapped_va_page`, object_paddr) before clearing.
- ~5 host tests as listed above.
- Integration: 83/83 unchanged (no kernel-side caller yet).

### Commit 2: kernel revocation + consume_pageset rewrite

- Add `src/cap/revoke.rs` with `revoke_validate(object_paddr)`
  and `revoke_apply(object_paddr)` top-level walkers.
  Each walks scheduler::threads, dedupes processes, calls per-
  process validate / apply.
- Add `HandleTableRef::revoke_validate` and
  `HandleTableRef::revoke_apply` thin wrappers around the new
  pure helpers.
- Replace `consume_pageset(...)` with the
  `consume_pageset_validate` + `consume_pageset_apply` pair per
  the rewrite section. On validate failure, return Err to the
  caller — no state mutated, no ownership transfer happens, and
  the user's PageSet handle is still valid.
- Restructure `create_kernel_object`: validate revoke → init_fn
  → apply revoke → free header → insert new handle. Validate
  failure short-circuits before init_fn runs.
- Restructure `sys_create_process` per the dedicated
  rewrite section: move parent-side unmap, scheduler add, and
  consume into the apply phase; validate revoke + scheduler
  has_room run as the validate phase before any destructive
  work. Add `scheduler::has_room()` helper as part of the same
  commit. Today's `process.rs:302..338` ordering (destructive
  unmap before fallible add_thread) goes away — fixing a
  pre-existing partial-failure window that revocation forces us
  to address.
- Integration: 83/83 still passes.
- Sanity: the existing posix-server bring-up exercises ~7
  consumes per boot (init's child spawns + endpoint allocations);
  each one's validate succeeds, apply succeeds, header pages are
  freed back to the buddy. No tombstone / leak under normal
  conditions. The sys_create_process restructure is exercised
  every spawn (init spawns 9 children, each calls
  sys_create_process).

## Files to modify / create

```
lockjaw-types/src/handle_ops.rs   — Commit 1: slot_revoke_validate
                                    (read-only) and slot_revoke_apply
                                    (write) pure helpers + tests
lockjaw-types/src/process.rs      — Commit 2: shrink ProcessTransferPlan.
                                    KEEP add_header (dedup), headers(),
                                    MAX_CONSUMED_HEADERS, header_count.
                                    DELETE unmap_results field,
                                    record_unmap, validate(), the
                                    UnmapFailed error variant, and
                                    HeaderIndex (no longer threaded
                                    through anywhere). Update host
                                    tests to match the smaller surface;
                                    keep the dedup test.
lockjaw-types/src/page_table.rs   — Commit 1: split unmap_validated
                                    into validate_pte_match (read-
                                    only) and clear_validated_pte
                                    (write); existing
                                    unmap_validated becomes a thin
                                    wrapper that calls both
src/arch/aarch64/vmem.rs          — Commit 1: kernel wrappers for
                                    the new pte helpers (TLB
                                    invalidation lives in the
                                    clear path)
src/cap/handle_table.rs           — Commit 2: HandleTableRef::revoke_validate
                                    and revoke_apply
src/cap/revoke.rs                 — Commit 2: NEW; revoke_validate +
                                    revoke_apply top-level walkers
src/cap/mod.rs                    — Commit 2: pub mod revoke;
src/cap/pageset_table.rs          — Commit 2: consume_pageset →
                                    consume_pageset_validate +
                                    consume_pageset_apply; remove
                                    tombstone-related comments
src/syscall/handler.rs            — Commit 2: create_kernel_object
                                    rewrite (validate → init → apply)
src/process.rs                    — Commit 2: create_process restructure
                                    per the sys_create_process rewrite
                                    section. Per consumed PageSet header:
                                    consume_pageset_validate in validate
                                    phase + consume_pageset_apply in
                                    apply phase. The old destructive
                                    `unmap_for_object` walk and the
                                    separate consume_pageset call are
                                    deleted as a unit (apply walks the
                                    parent's table along with everyone
                                    else's). Add scheduler::has_room
                                    precheck. Reject PageSet-kind
                                    parent_handle_to_copy at validate
                                    step 1. Move the child handle
                                    insert (and Endpoint next_token
                                    bump) to apply phase step 6 — the
                                    insert happens AFTER all consumes,
                                    so revoke walks don't have to
                                    include the not-yet-scheduled
                                    child. Delete the now-unused
                                    `ProcessTransferPlan::validate`,
                                    `record_unmap`, and `unmap_results`
                                    field (consume_pageset_validate
                                    subsumes the accounting); KEEP
                                    `add_header`, `headers()`, and
                                    `MAX_CONSUMED_HEADERS` as the
                                    source of the deduplicated header
                                    set the consume loops walk.
src/sched/scheduler.rs            — Commit 2: NEW pub fn has_room()
                                    -> bool. Read-only check that the
                                    16-slot run queue has at least one
                                    free entry. Bounded; under GKL the
                                    answer is stable until the next
                                    add_thread.
docs/extraction-roadmap.md        — optional: add revocation as a
                                    completed kernel item
```

## Verification

- `cargo test -p lockjaw-types --target aarch64-apple-darwin --lib`
  (host tests; +5 in commit 1, +0 in commit 2).
- `make test` (integration): 83/83 unchanged across both commits.
- After commit 2: optionally run `make run` and watch for the
  "revoke OK" diagnostic during posix-server spawn (proves the
  happy path is exercised on every boot).

## Out of scope (re-stated)

- **SMP cross-CPU TLB shootdown.** Single-core today; revocation's
  unmap step issues `tlbi vmalle1is` against the local CPU only.
  When SMP comes online with cross-process mappings, revocation
  needs IPIs to peer cores to drain TLBs. Tracked separately.
- **Per-handle revocation API.** `revoke_validate` /
  `revoke_apply` are object-keyed (every handle to a given
  paddr). A future `revoke_handle(process, handle_idx)` would
  let a process kill one specific exported capability without
  consuming the whole object. Not needed today.
- **PageSet-kind `parent_handle_to_copy`.** Rejected at
  `sys_create_process` validate time. Lifting the restriction
  needs either (a) deferring child PageSet inserts to after
  all consumes within the apply phase, or (b) extending
  revoke walks to include not-yet-scheduled child tables.
  Both are doable but orthogonal to the revocation core; out
  of scope for the initial commit.
- **Restartable revocation across kernel panic.** If a kernel
  panic interrupts the apply phase, the resulting state is
  whatever was applied so far. Single-core, no preemption inside
  apply, so no partial state is visible to userspace before the
  panic; if the kernel survives, it resumes a normal `sys_call`
  cycle. No transactional log required. (Validate phase failure
  is a normal recoverable error and not a kernel panic; see
  Failure handling.)
- **Concurrent process exit.** GKL serializes; revoke runs to
  completion before any other syscall. A process that's about to
  exit doesn't lose its handle table mid-walk because exit is
  also a syscall under GKL.
- **Revocation of mappings the kernel doesn't know about.** All
  user mappings go through `sys_map_pages`, which records
  `mapped_va_page` on the handle. Revocation reads that field; no
  extra page-table walking needed to find mappings.

## Risks

- **Refcount accounting symmetry.** The most subtle bug class.
  Today's bulk `slot_remove_all_by_object` deliberately skips
  refcount work because the existing leak-the-header-page
  pattern means the object header is never freed. Revocation
  lifts that and frees the header, so refcount must stay
  balanced. A single missed `dec_refcount` in the apply phase
  would leave the refcount artificially high; a future
  subtraction could underflow. The pure helper yielding
  `SlotRevokeAction.kind` forces the caller to see the kind for
  every cleared slot — the kernel walker's per-process apply
  loop then dec_refcounts once per PageSet-kind clear. The
  accounting-mismatch check in `revoke_validate` catches
  divergence loudly *before* any state mutation. The
  refcount-balance host test pins the invariant.
- **Cross-process `unmap_validated` failure mode is barely
  exercised today.** The only existing caller is
  `sys_create_process` doing parent-side unmap, where the parent
  IS the calling process and its address space is known-good.
  Revocation will exercise this against arbitrary other processes
  whose state may be less-tested. The two-phase design isolates
  this risk: phase 1 verifies every PTE before any mutation
  happens, and phase 2 only does the writes phase 1 already
  proved possible. A failure in phase 1 returns an error to the
  caller's syscall with the system state intact.
- **Two-walk overhead.** Phase 1 + phase 2 each walks every
  process's handle table. Worst case: 16 processes × ~512 slots
  each = 8192 slot inspections per consume, doubled to 16384.
  Bounded; consume is rare (process creation, kernel-object
  creation). The IPC-benchmark assertion will catch regressions.
- **Splitting unmap_validated.** The new
  `validate_pte_match` (read-only) and `clear_validated_pte`
  (write-only) functions are surgically extracted from
  `unmap_validated`'s existing two passes — that function
  already does validate-then-clear internally. The split is
  mechanical and host-testable.
- **`sys_create_process` reordering.** This is the largest
  single piece of touched kernel code in the plan. Today's
  flow has destructive parent-side work mid-stream and
  fallible work afterwards (a pre-existing partial-failure
  hole that revocation forces us to address). The rewrite
  moves all fallible work to the validate phase and all
  destructive commits to the apply phase. The
  `scheduler::has_room()` precheck is the only new fallibility
  primitive needed; everything else is reordering. The full
  spawn path (init's 9 child spawns + posix-server's child
  spawn) is exercised every boot, so a regression surfaces
  immediately.
- **Pre-existing refcount bug in `parent_handle_to_copy`.**
  Today's `process.rs:246` inserts the copied handle into the
  child's table without `inc_refcount()` on the underlying
  PageSet — at odds with `sys_export_handle`. Currently
  invisible because callers only ever pass Endpoint handles.
  Revocation's accounting check would turn this into a hard
  error the moment any caller passes a PageSet handle, AND
  the new child's handle table isn't in `scheduler::threads()`
  during the consume walk — making any "fix the inc_refcount"
  approach hit an accounting mismatch. The Phase 1 revocation
  commit sidesteps this by rejecting PageSet-kind
  `parent_handle_to_copy` at validate time per the dedicated
  section. Today's only caller (init's spawn_elf) passes
  Endpoint handles, so the restriction is observable as
  "ENOSUP-equivalent error if a future caller tries" rather
  than a behavior change.
- **Loop bounds**: walking up to 16 processes × up to a few
  hundred handle slots each is bounded but isn't free. Each
  consume now does this much work. Currently consume happens at
  process creation (rare) and kernel-object creation
  (per-syscall in some flows). Worth measuring in the
  IPC-benchmark assertion if anything regresses.
