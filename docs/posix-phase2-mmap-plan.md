# Phase 2: musl mmap (revised)

## Context

The first Phase 2 plan (`docs/posix-phase2-mmap-plan.md`) had three
load-bearing assumptions that don't survive contact with the current
kernel ABI. Codex flagged each one:

1. **One PageSet ≠ 8 MB.** `MAX_PAGES_PER_SET = 510` (the page-list
   array fits a single-page header). 510 pages = ~2 MiB. The 8 MiB
   gate needs 5 PageSets per allocation.
2. **64 GiB of VA doesn't exist.** `USER_VA_END = 0x4000_0000`
   (1 GiB). My `mmap_base = 0x10_0000_0000` (64 GiB) was nonsense.
3. **Protection silently lies.** `sys_map_pages` has no PROT_*
   analog. Accepting `prot == PROT_READ` would still hand out an RW
   mapping.

This plan addresses all three. After discussion, the right
long-term fix for #1 is to make `PageSetHeader` variable-size
(headers grow only as needed for the page-addr array) rather than
chunking mmap into multiple PageSets. The header restructure is
one localized kernel change that benefits every caller (block DMA,
init segments, mmap, future caches), not a special-case mmap path.

## Scope

**In scope:**

- Variable-size `PageSetHeader`: header pages are physically
  contiguous, sized to fit `count` page addresses. Phase 2 lifts
  the 510-page cap so a single PageSet can back any allocation
  size (bounded by global page availability).
- Anonymous private mmap: `mmap(NULL, len, PROT_READ|PROT_WRITE,
  MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)`. The musl-malloc shape.
- `munmap(addr, len)` releases the mapping.
- `mprotect(addr, len, PROT_READ|PROT_WRITE)` is a no-op (matches
  current state).
- `madvise(addr, len, advice)` returns 0 unconditionally.

**Out of scope (rejected at dispatch / kernel ABI):**

- `prot` other than `PROT_READ|PROT_WRITE`. EACCES for
  PROT_EXEC, ENOSYS for read-only / write-only / PROT_NONE
  (we have no `sys_map_pages` PROT bits yet).
- `MAP_FIXED` (caller-picked VA).
- `MAP_SHARED`, file-backed mmap, `mremap`, `mlock`,
  `mincore`, `msync`. ENOSYS at dispatch.
- Coalesced VA reuse after munmap. Phase 2 is bump-only.

## Architecture

### Variable-size PageSet header (Phase 2.K — kernel pre-work)

`lockjaw_types::pageset_table::PageSetHeader` becomes a
fixed-size 16-byte metadata struct followed by an in-memory u64
array that may span multiple physically-contiguous header pages.

```rust
#[repr(C)]
pub struct PageSetHeader {
    pub count: u32,         // data pages
    pub header_pages: u32,  // contiguous header pages (>=1)
    pub refcount: u32,
    pub map_count: u32,
    pub origin: PageSetOrigin,  // M6: u64-tagged enum (Buddy=0 / DmaPool=1)
    // pages[]: count u64s starting at byte offset 24, spanning
    // into subsequent header pages as needed.
}
```

Helpers:

- `pub const fn header_pages_for(count: usize) -> usize` — pure
  calculation: `header_pages = ceil((16 + count * 8) / PAGE_SIZE)`.
  For count ≤ 510: 1 page. For count = 511: 2 pages. Etc.
- `pub fn get_page(&self, index: usize) -> Option<u64>` —
  byte-offset access via raw pointer arithmetic; cross-page
  reads are safe because header pages are physically contiguous
  and KERNEL_VA_OFFSET maps physical memory contiguously.
- `pub fn set_page(&mut self, index: usize, addr: u64)` — same
  shape for writes.
- The fixed `pages: [u64; 510]` field is removed. Existing
  callers using `header.get_page(i)` / `header.data_page_count()`
  keep working unchanged (the trait is preserved).
- The handful of direct `header.pages[..count]` slice accesses
  in `src/process.rs`, `src/syscall/handler.rs:889/948`, and
  the writes in `src/cap/pageset_table.rs:120/175` switch to
  `get_page` / `set_page` loops.

Kernel allocator (`src/cap/pageset_table.rs::alloc_pages`) now:
1. computes `header_pages = header_pages_for(count)`
2. calls `page_alloc::alloc_pages_contiguous(header_pages)` for
   the header (returns a contiguous run; on failure → ENOMEM)
3. loops `count` times to allocate data pages (non-contiguous
   for `alloc_pages`, contiguous for `alloc_pages_contiguous`)
4. initializes the header (count, header_pages, set_page for
   each data page)
5. on rollback, frees `header_pages` contiguous header pages +
   any data pages already allocated.

`free_by_header_paddr` and `consume_pageset` read
`header.header_pages` and free that many pages of header before
iterating the data pages.

The constant `MAX_PAGES_PER_SET = 510` is removed (no longer a
single-PageSet ceiling). Callers that need a sanity bound use a
new `MAX_PRACTICAL_PAGES_PER_SET = 16384` (= 64 MiB max per set,
set by what fits within `MAX_HEADER_PAGES = 33` to keep the
allocation clean — `header_pages_for(16384) = ceil(131088/4096) = 33`).
This is a soft cap to prevent runaway requests, not a hard ABI
limit.

Pure host tests in `lockjaw-types/src/pageset_table.rs` add
coverage for:
- `header_pages_for` arithmetic (boundaries at 510, 511, 1022,
  1023, 16384).
- `get_page`/`set_page` round-trips at indices that span the
  page boundary (e.g. index 510, 511, 1023, 1024).
- Existing tests still pass (no behavior change for ≤510-page
  PageSets).

### VA layout

Real numbers:

```
0x0000_0000 -- null guard / unused low VA
0x0040_0000 -- ELF image (4 MiB)
            -- shared buffer + brk region (grows up; capped at 8 MiB)
0x0080_0000 -- stack (USER_STACK_BASE; ~16 KiB for 4 pages)
0x0080_4000 -- (gap)
0x0100_0000 -- POSIX_MMAP_BASE (16 MiB) — mmap region grows up
0x4000_0000 -- USER_VA_END (1 GiB)
```

`POSIX_MMAP_BASE = 0x0100_0000` (16 MiB) — a new constant in
`lockjaw_types::constants`.

#### Why a fixed mmap_base is safe today

The fixed-base approach is correct only because of a chain of
existing invariants — `compute_va_layout` is not proving more
than it actually proves. The chain:

1. `USER_STACK_BASE = 0x0080_0000` is a fixed ABI anchor in
   `lockjaw_types::constants`. The kernel maps the stack at
   exactly this VA in `src/process.rs:175`. No process moves it.
2. The current `compute_va_layout` (Phase 0 / `f1fdff1`) already
   enforces `brk_base < USER_STACK_BASE`. Specifically it returns
   `LayoutError::OverflowsStack` if the post-shared-buffer brk
   would land at or above `USER_STACK_BASE`. So **brk is
   confined below the stack anchor**.
3. The shim's `handle_brk` (`musl-lockjaw/src/shim.c`) only
   grows brk via `sys_alloc_pages` + `sys_map_pages` at
   `brk_mapped_end`, which is initialized to `brk_current =
   brk_base` and only increases. brk never crosses
   `USER_STACK_BASE` in practice because the shim's growth
   target is bounded by the malloc demand, and any user program
   asking for more than ~6 MiB of brk would hit
   `USER_STACK_BASE` and the kernel's `sys_map_pages` would
   reject the VA (it falls in the stack range).

So `POSIX_MMAP_BASE = 16 MiB` is safe **only because brk is
confined below USER_STACK_BASE = 8 MiB**. If a future ABI change
moves USER_STACK_BASE up, or removes the brk-below-stack
invariant, POSIX_MMAP_BASE must move with it.

Phase 2.0 adds a `compute_va_layout` invariant check that pins
this dependency:

- `mmap_base > USER_STACK_BASE + max_stack_pages * PAGE_SIZE +
  PAGE_SIZE` (above the stack + one guard page).
- New `LayoutError::MmapBelowStack` for the failure case.

Both `brk_base < USER_STACK_BASE` (existing) and `mmap_base >
USER_STACK_BASE + ...` (new) must hold. If either is violated,
the layout is rejected and the personality server halts at boot
rather than letting an overlap fester.

`PosixVaLayout` extends with `mmap_base: u64` (= POSIX_MMAP_BASE
for now). Tests for the boundary added in Phase 2.0.

`POSIX_INIT` reply now carries `mmap_base` in the previously-zero
4th word.

### Protection contract

`sys_map_pages` produces RW+UXN. Phase 2's contract matches:
accept exactly that, reject everything else with the right errno.

- `prot == 0` (PROT_NONE) → EINVAL
- `prot == PROT_READ` → ENOSYS (read-only mappings deferred —
  needs `sys_map_pages` PROT extension)
- `prot == PROT_WRITE` alone → EINVAL (write-only nonsensical)
- `prot == PROT_READ | PROT_WRITE` → accepted
- `prot & PROT_EXEC` → EACCES (no JIT)

`mprotect` is scoped to known mmap regions. The server requires:

1. `prot == PROT_READ | PROT_WRITE` (matches what's actually
   installed); anything else returns ENOSYS.
2. The `(addr, len)` pair must exactly match a region currently
   in the mmap_table for this caller (`addr == entry.base_va &&
   len == entry.len_bytes`); anything else returns EINVAL.

Both checks are necessary: blanket "always 0" would let a caller
quietly mprotect arbitrary unmapped or non-mmap ranges, including
the brk region or the ELF text — claiming success while doing
nothing. The narrow rule keeps the stub truthful: mprotect is a
no-op only for mmap regions whose protection already matches.

`madvise` returns 0 unconditionally — hints aren't load-bearing
and the caller can't observe whether they happened. Unlike
mprotect, accepting madvise on non-mmap regions has no
correctness implications.

### IPC shape (now simple thanks to variable-size header)

With one PageSet per mmap, the wire is the same shape as the
block driver's `CMD_ALLOC_BUFFER`:

- **`FS_MMAP`** request `(NR_MMAP, len_bytes, prot, flags)`.
  Server validates, allocates one `ceil(len/PAGE_SIZE)`-page
  PageSet via the variable-header allocator, picks `base_va`
  from per-client bump allocator, exports the PageSet to the
  client, records it in mmap_table.
  Reply `(status, base_va, exported_pageset_handle, total_pages)`.

- **`FS_MMAP_ROLLBACK`** request `(NR_MMAP_ROLLBACK, base_va, 0, 0)`.
  Used when the client's local `sys_map_pages` fails after a
  successful FS_MMAP. Server takes the mmap_table entry by
  `(caller_token, base_va)`, frees its server-side PageSet
  handle. The exported handle still in the client's table is
  the client's to close. Reply `(status, 0, 0, 0)`.

- **`FS_MUNMAP`** request `(NR_MUNMAP, base_va, len_bytes, 0)`.
  Server looks up the entry in mmap_table by `(caller_token,
  base_va)` AND verifies `request.len_bytes ==
  entry.len_bytes`. On mismatch (caller passed the wrong length)
  → reply EINVAL. On match: drop the entry, free server-side
  PageSet handle, reply 0.

  **Phase 2 supports only exact whole-region unmap.** Partial /
  shortened / unaligned munmap is out of scope; the server
  rejects any len that doesn't match the original mmap allocation
  rather than splitting the region. A future phase can add
  splitting and trimming when malloc starts calling them.

### Failure ordering (mmap and munmap)

mmap's happy path involves two sides — server-side allocation +
export, then client-side `sys_map_pages`. Either side can fail
after the other has succeeded. The protocol pins the rollback
direction so local and remote state never diverge silently:

```
mmap happy path:
  1. shim: lj_call(NR_MMAP)
  2. server: allocate PageSet, pick base_va, sys_export_handle,
             insert mmap_table entry
  3. server: reply (0, base_va, handle, pages)
  4. shim: sys_map_pages(handle, base_va, 0)
  5. shim: record (base_va, handle, len) in shim tracker
  6. shim: return base_va to musl

mmap failure paths:
  - server step 2 fails (alloc/export): server replies errno,
    no state to clean up.
  - shim step 4 fails (sys_map_pages): shim immediately calls
    NR_MMAP_ROLLBACK(base_va). On rollback success, shim does
    sys_close_handle(handle) locally and returns -ENOMEM to musl.
    If rollback ALSO fails (transport error), shim aborts with
    lj_die — local/remote state has diverged unrecoverably and
    halting is the safe response. (Unlike Phase F.2 openat
    rollback, mmap's exported PageSet is large; deferring the
    rollback would let many MB leak per failed call. The "halt
    on rollback failure" matches Phase F.2's deferred-close
    queue-full halt.)
  - shim step 5 (table-full in shim's tracker): same as above,
    rollback-or-halt before returning ENOMEM.

munmap order: remote-first, same rule as Phase F.2 close:
  1. shim: lj_call(NR_MUNMAP, base_va, len, 0)
  2. server: lookup, take entry, close server-side PageSet,
             reply 0
  3. shim: sys_unmap_pages(handle, base_va)
  4. shim: sys_close_handle(handle)
  5. shim: drop tracker entry
  6. shim: return 0 to musl

munmap failure paths:
  - server step 2 fails (caller_token mismatch, bad va): server
    replies errno; shim leaves tracker untouched and returns
    -errno. Caller can retry.
  - shim step 3 fails (sys_unmap_pages): the server has freed
    its side, so the client's local mapping is now orphaned
    (handle still references the PageSet, but server has dropped
    its tracking). This shouldn't happen for a valid handle, but
    if it does we're in the same divergent-state class as the
    rollback-fails case — lj_die loud rather than leak.
  - shim step 4 fails (sys_close_handle): leak one local handle
    table entry. Bounded leak — log and continue.
```

The shim's responsibilities are explicit:
- After every successful FS_MMAP, the shim either succeeds the
  full sequence (steps 4-6) or executes FS_MMAP_ROLLBACK before
  returning to musl.
- Every FS_MUNMAP that returns 0 must be followed by local
  sys_unmap + sys_close_handle (or halt if either fails).
- The shim's tracker is only updated on full success.

### Pure logic vs side effects

**`lockjaw-types/src/pageset_table.rs`** (Phase 2.K):
- Restructure `PageSetHeader` to variable-size as above.
- Add `header_pages_for`, `get_page`/`set_page` helpers.
- Remove `MAX_PAGES_PER_SET`; add `MAX_PRACTICAL_PAGES_PER_SET = 16384`.
- Tests: new coverage for cross-page index access; existing tests
  preserved.

**`lockjaw-types/src/constants.rs`** (Phase 2.0):
- Add `POSIX_MMAP_BASE: u64 = 0x0100_0000` with a layout-diagram
  doc comment.

**`lockjaw-types/src/posix.rs`** (Phase 2.0 + 2.1):
- Phase 2.0: `PosixVaLayout` gains `mmap_base`. `compute_va_layout`
  adds invariant check. `LayoutError::MmapBelowStack`.
- Phase 2.1:
  - New syscall NRs: `NR_MMAP = 222`, `NR_MUNMAP = 215`,
    `NR_MPROTECT = 226`, `NR_MADVISE = 233`.
  - New shim-only sentinel `NR_MMAP_ROLLBACK = 0xFFFF_FFFF_FFFF_FF01`
    (sits one above `POSIX_INIT`'s sentinel; not a real Linux syscall
    so musl can never originate it). Used for the client-side rollback
    when `sys_map_pages` fails after a successful mmap.
  - New errnos: `EFAULT = 14`, `EACCES = 13`.
  - mmap-flag constants: `PROT_READ`, `PROT_WRITE`, `PROT_EXEC`,
    `PROT_NONE`; `MAP_PRIVATE`, `MAP_SHARED`, `MAP_FIXED`,
    `MAP_ANONYMOUS`. Required mask `MAP_PRIVATE | MAP_ANONYMOUS`.
  - Max bytes constant `MAX_MMAP_BYTES = 64 * 1024 * 1024`
    (matches MAX_PRACTICAL_PAGES_PER_SET * PAGE_SIZE).
  - New `Action` variants:
    - `FileMmap { len_bytes, prot, flags }`
    - `FileMmapRollback { base_va }`
    - `FileMunmap { base_va, len_bytes }`
    - `FileMprotect { base_va, len_bytes, prot }`
    - `FileMadvise { base_va, len_bytes, advice }`
  - Dispatch arms with up-front validation.
  - ~17 host tests (each rejection arm + happy decode + rollback).

**`user/posix-server/src/main.rs`** (Phase 2.1 + 2.2):
- Phase 2.1: stub match arms for the new actions (ENOSYS for
  Mmap/Munmap, 0 for Mprotect/Madvise per dispatch).
- Phase 2.2:
  - `MmapTable` (per-client array of `Option<MmapEntry { base_va,
    len_bytes, total_pages, server_pageset: PageSetHandle }>`).
    Caller-token isolation matching `OpenTable` shape.
  - `MmapVaAllocator { next: u64, limit: u64 }` per client (or
    per process for now). `alloc(pages) -> Option<u64>` bumps;
    returns None at limit.
  - `handle_file_mmap`: validate, allocate PageSet of N pages
    via `sys_alloc_pages(N)`, pick base_va, export handle to
    client, record entry. Cleanup on every error path *before*
    replying, so the client never sees a successful FS_MMAP for
    a half-allocated entry.
  - `handle_file_mmap_rollback`: lookup by (caller_token,
    base_va), take entry, close server-side PageSet, reply 0.
    Rejects unknown base_va with EINVAL (caller bug).
  - `handle_file_munmap`: lookup by (caller_token, base_va).
    If found, verify `request.len_bytes == entry.len_bytes`;
    EINVAL on mismatch (Phase 2 is exact-whole-region only).
    On match: take entry, close server-side PageSet, reply 0.
  - `handle_file_mprotect`: lookup by (caller_token, base_va).
    Must find an entry AND match (base_va, len_bytes) exactly;
    EINVAL otherwise. On match: reply 0 (no-op — region is
    already RW). prot validation already done in dispatch.
  - `handle_file_madvise`: reply 0 unconditionally (hints).
- Wire all five into the dispatch loop's match.

**`musl-lockjaw/src/shim.c`** (Phase 2.0 + 2.3):
- Phase 2.0: receive `mmap_base` from POSIX_INIT; stash in shim state.
- Phase 2.3: per-process mmap tracker (16-slot fixed array of
  `(base_va, handle, len)`). Updated only on full success.
  - `mmap` (failure-ordered):
    1. lj_call(NR_MMAP, len, prot, flags) → (base_va, handle, pages)
    2. sys_map_pages(handle, base_va, 0); on failure go to step 4
    3. record (base_va, handle, len) in tracker; on tracker-full
       go to step 4 too (very rare: 16-slot exhaustion)
    4. (rollback) lj_call(NR_MMAP_ROLLBACK, base_va); if reply
       errno != 0 → lj_die ("mmap rollback failed; refusing to
       leak server PageSet"). On rollback success,
       sys_close_handle(handle), return -ENOMEM.
    5. (success path completes step 3) return base_va.
  - `munmap` (remote-first, same pattern as Phase F.2 close):
    1. lookup (base_va, handle, len) in tracker; ENOENT if missing
    2. lj_call(NR_MUNMAP, base_va, len, 0); on errno return -errno
       (tracker untouched, caller can retry)
    3. sys_unmap_pages(handle, base_va); on failure → lj_die
       ("munmap local diverged from remote"). Should not happen
       for a valid handle.
    4. sys_close_handle(handle); on failure log + continue (one
       handle slot leaked locally; bounded).
    5. drop tracker entry; return 0.
  - `mprotect`/`madvise`: forward to posix-server (no local
    state to manage).

**`user/posix-hello/hello.c`** (Phase 2.4 + 2.5):
- Phase 2.4: add `malloc(8 MiB)` test, write first/middle/last
  byte, free, print "posix-hello: malloc 8MB ok".
- Phase 2.5: switch the existing direct openat/read/close back to
  `fopen + fread + fclose` (uses musl stdio = malloc = mmap).
  The Phase 1 gate assertion still passes.

## Phases

Each phase is an atomic commit, gated on `make test` green.

### Phase 2.K — Variable-size PageSet header (kernel pre-work)

Sole concern: lift the 510-page cap on a single PageSet without
changing observable behavior for any existing caller.

- Restructure `PageSetHeader` (lockjaw-types) to variable-size.
- Update kernel allocator (alloc_pages, alloc_pages_contiguous,
  free, consume) to handle multi-page headers contiguously.
- Replace direct `.pages[..]` slice access in callers with
  `get_page`-loop equivalents (4-5 sites).
- Host tests for `header_pages_for` boundaries and cross-page
  `get_page`/`set_page` round-trip.
- Integration: 83/83 unchanged. No PageSet currently allocates
  > 510 pages, so no new behavior exercised yet.

### Phase 2.0 — VA layout + POSIX_INIT extension

- `POSIX_MMAP_BASE` constant.
- `PosixVaLayout::mmap_base` + `compute_va_layout` invariant.
- POSIX_INIT reply carries `mmap_base`.
- Shim receives and stashes (not used yet).
- Host tests for the layout boundary.

Verification: 83/83 unchanged.

### Phase 2.1 — Pure dispatch arms + mprotect/madvise stubs

- Syscall NRs, errnos, mmap-flag constants.
- `Action::FileMmap` / `FileMunmap` / `FileMprotect` / `FileMadvise`.
- Dispatch arms with all up-front validation.
- ~15 host tests including each rejection arm.
- posix-server: stub ENOSYS for Mmap/Munmap; 0 for Mprotect/Madvise.

Verification: 83/83 + ~15 new host tests.

### Phase 2.2 — posix-server mmap runtime

- `MmapTable` + `MmapVaAllocator` keyed by caller_token.
- `handle_file_mmap` / `handle_file_munmap` with cleanup on
  every failure path.
- Wire into dispatch.

Verification: 83/83 (no client exercises it yet).

### Phase 2.3 — Shim mmap/munmap + small malloc test

- Shim per-process mmap tracker.
- mmap / munmap / mprotect / madvise syscall handlers.
- `posix-hello`: add `malloc(1 MiB)` test, write through, free,
  print "posix-hello: malloc 1MB ok".
- Integration: assertion for the new line.

Verification: 83 → 84.

### Phase 2.4 — 8 MB malloc gate

- `posix-hello`: add `malloc(8 MiB)` test (single-PageSet thanks
  to Phase 2.K). Write first/middle/last byte; print
  "posix-hello: malloc 8MB ok".
- Integration: assertion for the new line.

Verification: 84 → 85. Phase 2 gate met via direct `malloc`.

### Phase 2.5 — Switch hello.c to musl stdio path

- Replace the direct openat/read/close in `posix-hello/hello.c`
  with `fopen("/HELLO.TXT", "r") + fread + fclose + printf`.
- Existing Phase 1 assertion `posix-hello: hello from fat32`
  still holds — but flows through musl stdio (malloc → mmap →
  shim → posix-server → ...) instead of direct syscalls.
- Confirms Phase 1 gate still passes after stdio actually works.

Verification: 85/85.

## Files to modify / create

```
lockjaw-types/src/pageset_table.rs — Phase 2.K: variable-size header
lockjaw-types/src/constants.rs     — Phase 2.0: POSIX_MMAP_BASE
lockjaw-types/src/posix.rs         — Phase 2.0: mmap_base in VaLayout
                                     Phase 2.1: NRs/Action variants/dispatch
src/cap/pageset_table.rs           — Phase 2.K: alloc/free for multi-page hdr
src/process.rs                     — Phase 2.K: get_page loop instead of slice
src/syscall/handler.rs             — Phase 2.K: get_page loops at lines 889/948
src/arch/aarch64/vmem.rs           — Phase 2.K: ensure get_page used (already is)
user/posix-server/src/main.rs      — Phase 2.0: POSIX_INIT mmap_base
                                     Phase 2.1: stub action arms
                                     Phase 2.2: MmapTable + handlers
musl-lockjaw/src/shim.c            — Phase 2.0: receive mmap_base
                                     Phase 2.3: mmap/munmap/mprotect/madvise
user/posix-hello/hello.c           — Phase 2.3: malloc(1 MiB) test
                                     Phase 2.4: malloc(8 MiB) test
                                     Phase 2.5: stdio (fopen) test
tests/qemu_integration.sh          — Phase 2.3/2.4/2.5 assertions
docs/posix-musl-plan.md            — mark Phase 2 done at the end
docs/posix-phase2-mmap-plan.md     — replace with this plan
```

## Existing code to reuse

- `lockjaw-userlib::block::BlockClient` — exemplar of "server
  allocates buffer + exports + client maps locally" pattern.
  `FsClient` is the second instance; Phase 2's mmap is the third.
- `posix-server`'s `OpenTable` (Phase E.2) — exact shape for
  `MmapTable`: caller-token-scoped slots, alloc/lookup/remove.
- `posix-server`'s deferred-close queue (Phase F.2) — same
  rollback discipline applies if `sys_export_handle` fails after
  PageSet alloc; can reuse the queue or pattern.
- `lockjaw-userlib::handle::PageSetGuard` — RAII on the alloc
  before export/insert into table.

## Verification

- **Per-phase host tests**: `cargo test -p lockjaw-types --target
  aarch64-apple-darwin --lib`. Each sub-phase adds 5-15 tests;
  final state ~675 host tests.
- **Per-phase integration**: `make test` after every phase.
  Final integration count: 85.
- **End-to-end smoke**: `make run-blk` and observe musl's
  `posix-hello` running malloc-8MB + the stdio file read.

## Out of scope (re-stated for clarity)

- `MAX_PAGES_PER_SET`-style per-set cap — replaced with a
  practical cap of 64 MiB per PageSet (still big enough that
  Phase 2 doesn't need multi-PageSet mmap).
- `MAP_FIXED`, `MAP_SHARED`, file-backed mmap. ENOSYS at dispatch.
- PROT_READ-only, PROT_NONE, PROT_EXEC. Need a `sys_map_pages`
  ABI extension; out of this phase.
- **Partial / shortened / unaligned munmap.** Phase 2 supports
  only exact whole-region unmap (`munmap(base_va,
  original_len)`). Splitting and trimming come in a follow-up
  phase if a malloc variant ever calls them.
- **mprotect on non-mmap ranges.** Phase 2's mprotect is
  scoped to known mmap regions; brk, ELF, stack, and arbitrary
  VAs return EINVAL. The narrow rule keeps the no-op stub
  truthful.
- `mremap`, `mlock`, `mincore`, `msync`. ENOSYS.
- VA reuse after munmap (bump-only allocator).
- Per-process VA layout that varies `mmap_base`. Phase 2 uses
  the constant.

## Risks

- **Variable-size header touches every PageSet allocation path.**
  Riskiest single change in the plan. Existing integration tests
  cover allocations in all the right places (kernel, init,
  posix-server, fat32-server, blk-driver), so a regression
  surfaces quickly. **Phase 2.K is intentionally isolated** —
  ships as its own commit, with no mmap-specific behavior, so
  any test failure points squarely at the header restructure.
- **`alloc_pages_contiguous` for header**: The kernel's
  buddy allocator must hand out N contiguous pages for headers
  > 1 page. For small N (≤ 33) on a fresh system this is fine,
  but a heavily-fragmented allocator could fail. The block
  driver already uses `alloc_pages_contiguous` for DMA buffers;
  the same primitive serves header allocation.
- **POSIX_MMAP_BASE = 16 MiB is fixed.** Safe today only because
  brk is confined below `USER_STACK_BASE = 8 MiB` (see "Why a
  fixed mmap_base is safe today" above). Future ABI changes that
  move USER_STACK_BASE up or remove the brk-below-stack
  invariant must move POSIX_MMAP_BASE with them. The new
  invariant check in `compute_va_layout` rejects an invalid
  layout at boot rather than at use.
- **mmap rollback divergence**: addressed via the explicit
  failure-ordering rules in "Failure ordering (mmap and munmap)"
  above. The shim must execute FS_MMAP_ROLLBACK or halt; munmap
  is remote-first. Both modes verified during Phase 2.3
  bring-up.
