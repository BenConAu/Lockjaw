# Phase 2: Memory Management (anonymous mmap)

## Context

Phase 1 of the musl plan is met (`f929766`): a real musl program reads
a file off a FAT32 disk via the personality server. The next phase
gate is "musl `malloc` works for large allocations."

Why this matters: Phase 1's test program (`user/posix-hello/hello.c`)
deliberately avoids `fopen` / `fread` / `printf` because they all
malloc, and musl's malloc uses `mmap` for anything larger than its
small-allocation pool. That detour through direct syscalls was a
documented Phase 1 limitation. Phase 2 lifts it — once `mmap` works,
stdio works, and we can write programs that look more like normal
Linux code.

The Phase 2 gate from `posix-musl-plan.md`:

> Gate: Rust binary with Vec<u64> of 1M elements.

8 MB allocation. Comfortably above musl's small-pool threshold
(`__malloc0` switches to mmap somewhere above 16 KB). A `Vec<u64>`
push loop will trigger one mmap of ~8 MB (with growth realloc, two
or three mmaps total).

We don't actually need a Rust toolchain to hit the gate — a C
program that does `malloc(8 * 1024 * 1024)` exercises the same path.
Phase 2 verifies the gate with C; a future cleanup can add the Rust
musl target if desired.

## Scope

**In scope:**

- `mmap(NULL, len, prot, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)` —
  anonymous private mappings with server-picked VA. Covers musl's
  malloc path.
- `munmap(addr, len)` — release a previously-mmap'd region.
- `mprotect(addr, len, prot)` — stub returning 0 (no-op). musl
  occasionally calls it after mmap to flip pages writable; since
  every Phase 2 mapping is RW from the start, the stub is safe.
- `madvise(addr, len, advice)` — stub returning 0. Hints, not
  semantically required.

**Out of scope (separate phases later):**

- `MAP_FIXED` — caller-picked VA. Needs the server to honor a
  client-supplied address rather than picking from its own pool.
  musl doesn't use this for malloc, so deferred.
- File-backed mmap (`MAP_PRIVATE` against an fd from `openat`).
  musl's libc init occasionally mmaps `/proc/...`; we don't have
  procfs yet so this rarely fires. fat32-server would need to
  participate.
- `MAP_SHARED` — mappings visible to other processes.
- `MAP_GROWSDOWN` — stack mappings; we already have a fixed stack.
- Page protections beyond RW (PROT_EXEC for JIT, PROT_NONE guards).
  Phase 2 maps everything RW; future work can add a flags field.
- `mremap`, `mlock`, `mincore` — out of scope, return ENOSYS.

## Architecture

The same client-assisted-mapping shape we already use in fat32-server:
the server picks the VA and allocates the PageSet, exports it to the
client, and the client (the musl shim, on behalf of musl's malloc)
calls `sys_map_pages` itself to actually install the mapping in its
address space.

Two layers of state:

1. **Per-client VA pool** in posix-server. A bump allocator for
   each POSIX client over a dedicated mmap region in the child's VA
   space (above brk, below stack). Stays bump-only for Phase 2;
   the freed-region coalescing problem can wait.

2. **Per-client mmap table** in posix-server. Maps `(va, len)` →
   `(pageset_handle, pages)` so munmap can find the right PageSet
   to close after the client unmaps. Bounded array, like FdTable.

```
musl malloc -> mmap(NULL, 8MB, RW, ANON|PRIVATE, -1, 0)
  -> shim packs (NR_MMAP, len, prot, flags) and lj_call
  -> posix-server FileMmap action:
       - allocate PageSet of ceil(len/PAGE_SIZE) pages
       - pick VA from per-client mmap pool
       - sys_export_handle the PageSet to the client
       - record (va, len, exported_idx, pages) in mmap table
       - reply (status=0, va, exported_idx, pages)
  -> shim sys_map_pages(handle, va, 0)
  -> shim returns va to musl
  -> musl uses the memory
```

For munmap:

```
musl free -> munmap(va, len)
  -> shim packs (NR_MUNMAP, va, len, 0)
  -> posix-server FileMunmap action:
       - look up (va, len) in table; FS_ERR_INVALID if missing
       - take entry; reply (status=0, 0, 0, 0)
  -> shim sys_unmap_pages(handle? -- problem)
```

There's a wrinkle: the shim doesn't keep its own table of
exported PageSet handles per VA. Either:

A. Posix-server reply on munmap includes the handle index so the
   shim can sys_unmap + sys_close_handle.
B. The shim maintains its own per-mapping table (handle index
   stored at mmap time, looked up at munmap time).

**Option B** is the cleaner long-term shape (the shim already keeps
its own state — `shared_buf`, `brk_current`, etc.). And it lets the
shim reject double-unmap locally without IPC. For Phase 2, B.

## Pure logic vs side effects (the lockjaw rule)

Where things live:

**`lockjaw-types/src/posix.rs`** — extend with:

- New syscall numbers: `NR_MMAP = 222`, `NR_MUNMAP = 215`,
  `NR_MPROTECT = 226`, `NR_MADVISE = 233`.
- New errno: `EFAULT` (= 14), `EACCES` (= 13), `ENODEV` (= 19) for
  the mmap-flag validation arms.
- Linux mmap flags: `PROT_NONE`, `PROT_READ`, `PROT_WRITE`,
  `PROT_EXEC`; `MAP_PRIVATE`, `MAP_SHARED`, `MAP_FIXED`,
  `MAP_ANONYMOUS`. Bitmask of *unsupported* mmap flags so dispatch
  can reject up front.
- New `Action` variants: `FileMmap { len, prot, flags }`,
  `FileMunmap { addr, len }`, plus `FileMprotect { addr, len, prot }`
  and `FileMadvise { addr, len, advice }` if we want the stubs to
  flow through dispatch (they could also collapse to `Reply` arms
  that always return 0).
- Dispatch arms for each. Reject:
  - len == 0 (EINVAL)
  - len > some sane cap (e.g. 256 MB) — small Lockjaw hosts have
    tight memory budgets and a runaway alloc shouldn't OOM the kernel
  - flags missing both MAP_PRIVATE and MAP_SHARED (EINVAL)
  - flags has MAP_SHARED (ENOSYS — Phase 2 is private only)
  - flags missing MAP_ANONYMOUS (ENOSYS — file-backed deferred)
  - flags has MAP_FIXED (ENOSYS — caller-picked VA deferred)
  - prot includes PROT_EXEC (EACCES — no JIT support; future)
  - prot is PROT_NONE alone (EINVAL — at least one access mode)
- Validation goes in pure dispatch with host tests, same shape as
  the openat flag rejection.

**`user/posix-server/`** — runtime side effects:

- `MmapTable` (similar to `FdTable`): per-client array of
  `Option<MmapEntry { va, len_bytes, pageset_handle, pages }>`.
  Caller-token isolation per the existing pattern.
- `MmapVaAllocator` — bump pointer per client, advances by `len`
  rounded up to PAGE_SIZE on each successful mmap. Starts at a
  dedicated mmap_base above brk, well below USER_STACK_BASE.
- `handle_file_mmap`: validate len, allocate PageSet via
  `sys_alloc_pages`, pick VA, `sys_export_handle` to the client,
  record in MmapTable. Cleanup on every error path. Reply
  `(0, va, exported_idx, page_count)`.
- `handle_file_munmap`: look up VA in MmapTable scoped by
  `caller_token`, take entry, close server-side PageSet handle,
  reply 0 (or `-EINVAL` if not found).
- `handle_file_mprotect` / `handle_file_madvise`: reply 0
  unconditionally for Phase 2.

**`musl-lockjaw/src/shim.c`** — three new syscall handlers:

- `mmap(addr, len, prot, flags, fd, offset)` — pack
  (NR_MMAP, len, prot, flags) and call. On success the reply
  returns (va, exported_idx, page_count). Shim does
  `sys_map_pages(exported_idx, va, 0)`. Stores
  `(va, exported_idx, len)` in the shim's own per-process
  mmap-tracker so munmap can find the handle. Returns va.
- `munmap(addr, len)` — look up handle in shim tracker, call
  sys_unmap_pages locally, then call posix-server FileMunmap to
  release the server-side PageSet, then sys_close_handle on the
  exported handle.
- `mprotect`, `madvise` — pass through to posix-server (or
  return 0 locally — micro-optimization).

The shim's mmap tracker is a small fixed-size array (e.g. 16
slots). If full, mmap returns -ENOMEM. malloc tolerates this.

## VA layout

Currently `compute_va_layout` returns:

```
[ELF segments] elf_end_aligned [guard] child_shared_va [shared] brk_base [brk grows up...]
                                                                 ...
                                                                 USER_STACK_BASE [stack grows down]
```

For Phase 2, add an mmap region:

```
[ELF] elf_end [guard] child_shared_va [shared] brk_base [brk] ... [mmap_base ... mmap region grows up] ... USER_STACK_BASE
```

Where `mmap_base` is fixed somewhere above the worst-case brk.
Linux puts mmap region at a high address (mmap_base ~ stack_base /
3 historically, more variation now). For Lockjaw, we have abundant
VA: just put mmap_base at e.g. 0x10_0000_0000 (64 GB), well above
any plausible brk and well below `USER_STACK_BASE`. The bump
allocator grows up.

`compute_va_layout` extends to return a `mmap_base` field. It also
needs to verify `mmap_base + max_mmap_region < USER_STACK_BASE`.
Pure decision; host-tested.

POSIX_INIT reply gains `mmap_base` as an additional word.

## Phases

Each phase is an atomic commit, gated on `make test` green.

### Phase 2.A — VA layout extension + POSIX_INIT reply

- Extend `compute_va_layout` with `mmap_base`. Boundary tests for
  brk-and-mmap-fit-in-stack-window.
- Bump the POSIX_INIT reply shape to carry `mmap_base` in the
  unused 4th word.
- musl shim: receive `mmap_base` from POSIX_INIT, store it. Not
  used yet; just plumbed.
- posix-server: pass mmap_base into POSIX_INIT reply.

Verification: existing 83/83 passes; no behavior change.

### Phase 2.B — `lockjaw-types::posix` mmap dispatch arms

- Syscall NRs, errno constants, mmap flag constants.
- New `Action` variants: `FileMmap`, `FileMunmap`, `FileMprotect`,
  `FileMadvise`.
- Dispatch arms with all the up-front validation (flag rejection,
  len bounds, prot validation).
- ~15 host tests covering each rejection arm + happy decode.
- posix-server: stub match arms returning ENOSYS for the new
  variants. Same pattern as Phase F.1.

Verification: 83/83 + ~15 new host tests; no runtime change yet.

### Phase 2.C — posix-server mmap runtime

- `MmapTable` + `MmapVaAllocator` in `user/posix-server/src/main.rs`.
- `handle_file_mmap` / `_munmap` / `_mprotect` / `_madvise`
  handlers. Cleanup on every failure path.
- Wire into the dispatch loop's `match action`.

Verification: 83/83; runtime path exists but no client exercises
it yet.

### Phase 2.D — musl shim mmap/munmap

- Shim per-process mmap tracker (small array).
- `mmap` / `munmap` / `mprotect` / `madvise` syscall handlers in
  shim.c.
- Direct test program (`hello-mmap.c` or extend existing
  `hello.c`) that does `malloc(8MB)`, writes through the buffer,
  frees, exits.

Verification: new integration assertion `posix-hello: malloc 8MB ok`
(or similar). Total: 83 → 84.

### Phase 2.E — musl stdio gate

- Switch `hello.c` back to using `fopen`/`fread`/`printf` for the
  Phase 1 file read (or add a new test). This proves musl's stdio
  layer actually works through mmap-backed malloc.
- The Phase 1 gate assertion stays valid (the bytes still appear);
  we just route through stdio instead of direct syscalls.

Verification: 84/84 with stdio path used.

## Files to modify / create

```
lockjaw-types/src/posix.rs       — Phase 2.A: mmap_base in VaLayout
                                   Phase 2.B: NRs + Action variants
                                              + dispatch arms + tests
user/posix-server/src/main.rs    — Phase 2.A: pass mmap_base in POSIX_INIT
                                   Phase 2.B: stub ENOSYS arms
                                   Phase 2.C: MmapTable + handlers
musl-lockjaw/src/shim.c          — Phase 2.A: receive mmap_base
                                   Phase 2.D: mmap/munmap/mprotect/madvise
user/posix-hello/hello.c         — Phase 2.D: malloc test
                                   Phase 2.E: switch to stdio path
tests/qemu_integration.sh        — Phase 2.D: malloc-ok assertion
                                   Phase 2.E: stdio-path assertion
docs/posix-musl-plan.md          — mark Phase 2 done at the end
```

## Verification

- **Per-phase host tests**: `cargo test -p lockjaw-types --target
  aarch64-apple-darwin --lib`. Each sub-phase adds ~5-15 tests;
  final state should be ~660 host tests.
- **Per-phase integration**: `make test` after every phase; 83/83
  baseline plus new assertions per phase. Final integration count
  ~85-86.
- **End-to-end smoke**: `make run-blk` and observe musl's
  `posix-hello` running malloc + file read via stdio.

## Out of scope (explicit non-goals)

- `MAP_FIXED` — caller-picked VA. Needs an additional dispatch
  path that validates the address against the per-client free
  pool. musl doesn't need it for malloc.
- File-backed mmap. Requires fat32-server to participate
  (mapping a file's pages directly into the child's address
  space). Will need the same kernel capability-passing primitive
  that the FS_OPEN path needed but punted on.
- `MAP_SHARED` — multi-process shared mappings.
- `mremap`. musl's malloc uses it for in-place realloc; without
  it we just allocate a new region and copy. Slower but
  correct.
- `mlock` / `mincore` / `msync`. Hints / paging primitives we
  don't need for Phase 2.
- `PROT_EXEC` for JIT pages. No interpreter / JIT support yet.
- Freed-region coalescing in the per-client VA pool. Phase 2 is
  bump-only; munmap shrinks the table but doesn't reuse VA. The
  client can mmap and munmap a few thousand times before the VA
  pool is exhausted, which is plenty for the gate. Adding
  coalescing later is a localized change.
- mmap of `/dev/zero`, `/proc/...`. We have neither.

## Pi 4B portability check

Phase 2 is entirely userspace + the musl shim. The kernel doesn't
gain any new syscalls. The `BlockEngine` trait isn't touched. Pi
support continues to track the rest of the kernel.
