# Development Journal: FAT32, Three-Server Chains, and the Phase 1 Gate

Written after the FAT32 session. A real musl program now `openat`s
`/HELLO.TXT` on a virtio-blk disk, `read`s its contents, and prints
them — through six layers of Lockjaw IPC. Phase 1 of the musl plan
(`docs/posix-musl-plan.md`) is met. Originally I was going to build
a ramfs server; partway through planning we threw that out and went
straight to FAT32 on the existing virtio-blk driver.

## The plan that wasn't

The original `posix-musl-plan.md` proposed `user/ramfs-server/`: a
cpio archive parsed in memory, no block I/O, smallest possible
shape. I started writing that plan when Ben asked the question that
reset the whole approach: "We have a block device driver working,
why not get FAT32 working on that and build the file goo on that
instead of ramfs?"

He was right and the reframing took two minutes. The block driver
was sitting in CI with the assertion `blk probe returns cleanly when
no disk attached` — code we'd paid for that exercised nothing real.
A ramfs would mostly get replaced once a real FS landed. The Pi 4B
SDIO driver in our future would speak the same `BlockEngine` trait
the virtio-blk driver does, so the FAT32 server would be portable
for free. And we'd have to deal with async-shaped block I/O
eventually anyway — better to do it on the path to the gate than as
follow-up work.

I asked clarifying questions before re-planning: read-only or
read-write for the first cut? 8.3 only or with LFN? Hand-roll the
parser or vendor `fatfs`? Ben picked the smallest viable scope on
all three: read-only, 8.3 names, hand-rolled. That decision survived
intact through seven sub-phases.

## The phasing

Phase 0 of the musl plan was `puts("hello, lockjaw")`. Phase 1's gate
is "read a file." The work split into seven atomic commits, each
shippable with `make test` green before the next started:

- **A** — Attach disk to integration tests. The block driver's
  selftest path was dark; wiring up `-drive` plus a 64 MiB FAT32
  image (`mformat -F`) made it answer real reads in CI.
- **B** — Pure FAT32 BPB + FAT-chain logic in `lockjaw-types`.
  Validation gates layered: boot signature, fs_type string
  (advisory only — see Codex below), cluster-count classification,
  FAT-region-fits-volume, FAT-capacity-for-clusters.
- **C** — Pure dirent parser + 8.3 path matching. LFN entries
  silently skipped; the 0x05↔0xE5 escape decoded.
- **D** — `fat32-server` scaffold + mount. Bootstraps from init,
  reads sector 0 via `BlockClient`, calls `parse_bpb`, logs the
  geometry, sits in a stub IPC loop.
- **E.1** — FS-IPC protocol (`FS_OPEN` / `FS_READ` / `FS_CLOSE`)
  with a pure dispatch decision function.
- **E.2** — `fat32-server` open/read/close handlers: open-file
  table, cluster scratch, FAT chain walking on reads, per-handle
  buffer allocation and export.
- **E.3** — `FsClient` typed wrapper + `fat32-test` verification
  client that opens `/HELLO.TXT`, reads, prints. Single
  byte-exact integration assertion proved the entire chain.
- **F.1** — `FdTable` and Phase 1 dispatch arms (`NR_OPENAT` /
  `NR_READ` / `NR_CLOSE`) in `lockjaw-types`.
- **F.2** — `posix-server` runtime: `FsClient`, per-client FD
  table, `handle_file_open` / `handle_file_read` /
  `handle_file_close`. Init exports `fs_srv_ep` to posix-server.
- **G** — Phase 1 gate. Patched musl shim gains openat + read
  syscall handlers; `posix-hello/hello.c` extended to read
  `/HELLO.TXT` end-to-end.

## The three-server chain

Lockjaw's userspace was already a server hierarchy — devmgr,
uart-driver, ramfb-driver, virtio-blk-driver, posix-server — but
each was a single-hop client of init's bootstrap. Phase 1 needed a
two-hop chain: posix-server is a client of fat32-server, which is a
client of virtio-blk-driver. Three layers of IPC, three sets of
endpoints, three caller tokens for isolation.

The shape generalized cleanly because of the existing
`run_block_server` / `BlockEngine` pattern. Writing `FsClient`
took ~80 lines and looked exactly like `BlockClient`: typed
methods over `sys_call_ret4`, status-word decoding, RAII cleanup.
The fat32-server's IPC loop took the same shape as
`run_block_server` — receive, query caller token, dispatch, reply.
The pattern's third instance was free.

What needed actual thought was the data flow. Reading a file
involves three buffers:

1. The fat32-server's *cluster scratch* (server-internal, sized
   to the FS's cluster size).
2. The per-handle *read buffer* (allocated by fat32-server at
   open() time, mapped server-side, exported to posix-server).
3. The *client shared buffer* (allocated by posix-server at
   POSIX_INIT, mapped both in posix-server and in the musl child).

A single `read(fd, buf, count)` flows: fat32-server reads cluster
into (1) → copies the requested bytes into (2) → posix-server
sees the bytes via its mapping of (2) → copies into (3) → musl
shim copies from (3) into the user's `buf`. Two intermediate
copies, three buffer mappings, four address spaces involved. The
copies aren't free, but the isolation guarantees are.

## What Codex caught

Eight Codex passes, each one finding something. I'm listing them
because the pattern matters more than any single fix.

**The FAT32 parser was permissive.** Initial draft accepted bad
geometries that would crash later. Codex's first review caught
"layout exceeds volume" (FAT region extends past disk → underflow
in `cluster_count`); the next caught "below FAT32 minimum
clusters" (forged fs_type string on a FAT16-sized volume); the
third caught "FAT too small for cluster_count" (a tiny FAT with a
huge data region produces sector offsets past the FAT region).
Each fix added an explicit error variant + boundary tests in
`lockjaw-types/src/fat32.rs`. The parser now owns the
validity-policy layer; downstream code can trust what it returns.

The pattern in those reviews: pure code is easy to hand
adversarial inputs at host-test time. Every variant Codex flagged
became a one-line `assert_eq!` in the test module. The
review-fix-test loop tightened the validation to where the parser
is now a proper input-sanitizer for FAT32 BPBs.

**The 8.3 matcher had two semantic holes.** First: an
extension-only query like `.txt` matched a malformed on-disk entry
with all-spaces in the name field. Codex pointed out that only `.`
and `..` legitimately have empty base names. Fixed by rejecting
empty `name_part` after the dot-split. Second: `resolve_path`'s
slash-normalization (`split('/').filter(|c| !c.is_empty())`)
correctly handled `/dir//file.txt` but lost the semantic of a
trailing slash — `/file.txt/` should require the target to be a
directory. Fixed by capturing `must_be_directory = path.last() ==
Some(&b'/')` *before* the filter loop, then enforcing it after
resolution. Added `FS_ERR_NOT_DIRECTORY` to distinguish from
`FS_ERR_NOT_FOUND`.

**The IPC dispatch silently truncated 64-bit handles.** My initial
`dispatch` for `FS_READ` did `handle: req.arg1 as u32` — a
malformed `0xDEADBEEF_00000005` would become handle 5. The dispatch
module is supposed to own the request-shape contract. Fixed by
rejecting any reserved bits with `FS_ERR_INVALID`: bits above the
32-bit field width, non-zero "unused" argument words, bits above
bit 15 in the open-header packed field. The test that locked in the
truncation behavior got renamed and inverted.

**Cross-client handle isolation was missing.** fat32-server's
open-file table was keyed by a small integer with no
caller-token check. The block server already had this pattern
(`BufferTracker` scopes buffer IDs by `sys_query_caller_token()`).
Fixed by storing `caller_token` in the `OpenFile` slot and
requiring lookups/mutations/removals to match.

**The personality server's read path could write to VA 0.** Codex
caught that `handle_file_read` used `server_shared_va`
unconditionally, even though `handle_file_open` already guarded
the `== 0` case. A read before POSIX_INIT would
`copy_nonoverlapping` to VA 0. Fixed in both read and write paths
(write had the same bug, just intentionally undocumented per an
older comment).

**openat silently accepted write modes on a read-only filesystem.**
musl's `fopen("w")` would have gotten an fd it couldn't write to.
Fixed in pure dispatch: `O_WRONLY` / `O_RDWR` / `O_CREAT` /
`O_EXCL` / `O_TRUNC` / `O_APPEND` all reject up front with
`-EROFS`. Added the constants, the bitmask, and 9 boundary tests.

**close ignored remote-close failures.** Both `handle_file_close`
and the rollback paths in `handle_file_open` did `let _ =
fs.close(...)`. If the IPC failed, posix-server forgot the local
fd while fat32-server kept the remote slot live — bounded leak,
but a leak. Fixed close to call remote-first; if it fails, leave
local state intact so the caller can retry. Open rollback got a
small `MAX_DEFERRED_CLOSES = 4` retry queue; if the queue fills, the
personality server halts loud rather than silent-leaking.

The cumulative effect across eight reviews: the dispatch and
parser modules in `lockjaw-types` are stricter than I would have
made them, and the runtime modules in `posix-server` and
`fat32-server` validate their preconditions at every boundary
instead of trusting the layer above. Most of these bugs would
have been catastrophic in production but invisible in basic
testing — the pattern of "it works for the happy path" failing
under adversarial input.

## The bug I should have predicted

Phase G failed on the first integration run. `posix-server` itself
wouldn't spawn — `init: posix-server spawn FAILED`, no kernel error
message, just init's silent fail-and-continue.

Cause: `ProcessTransferPlan::MAX_CONSUMED_HEADERS = 32`. Init's
`spawn_elf` allocated **one PageSet per page** of the child binary.
With the new musl `hello.c` (now ~140 KB because it pulls in printf
and stdio), `posix-server`'s embedded copy spanned 42 pages. 42
PageSets > 32 cap. Spawn rejected.

The fix was straightforward: allocate ONE PageSet of N pages for
all segment pages, with each `ProcessMapping` indexing into it via
`page_index`. Posix-server now consumes 2 headers (segments + stack)
instead of 43.

But the lesson is the same one as ever: per-spawn O(pages) → O(1)
in PageSet headers is exactly the kind of scaling fix that "fix the
class not the instance" was written for. I'd been spawning under-32
binaries for months and never noticed. The integration test caught
it the moment the binary grew past the cap. That's the system
working — the integration tests are doing what they were designed
to do.

I also got bitten by musl stdio dragging in malloc dragging in
mmap. `fopen` allocates a `FILE` struct via `malloc`, which musl
implements via `mmap`. We don't have mmap in Phase 1. So the first
attempt at the test program (`fopen` + `fread` + `printf`) hit
`ENOSYS` on syscall 222 (mmap) and produced "fopen failed" instead
of reading the file. Fixed by switching `hello.c` to direct
`openat` + `read` syscalls, no stdio. Documented the rationale in
the file comment so the next person doesn't reach for `fopen`.

## What's actually different now

Tests: 463 → 645 host (+182), 44 → 83 integration (+39). Each
sub-phase added ~25 host tests and ~5 integration assertions; the
runaway driver was the FAT32 parser (43 tests) because Codex kept
finding adversarial geometries to validate.

Code surface:
- `lockjaw-types`: `block.rs`, `fat32.rs`, `fs.rs`, `posix_fd.rs`
  (`posix.rs` extended with `FileOpen`/`FileRead`/`FileClose`
  arms). Pure code, host-tested.
- `user/fat32-server/`: ~500 lines of side-effect glue around
  the pure types. Cluster scratch + FAT scratch buffers, open-file
  table, path resolution, multi-cluster reads.
- `user/fat32-test/`: ~100 lines of verification client.
- `user/lockjaw-userlib/src/fs.rs`: `FsClient` typed wrapper.
- `musl-lockjaw/src/shim.c`: openat + read syscall handlers added
  alongside the existing write/writev/brk.
- `user/posix-server/src/main.rs`: ~250 new lines for
  `FsClient` integration, FD table, file resource tracking,
  three new handlers, deferred-close retry queue.

The kernel itself didn't change. Phase 1 is entirely a userspace
extension — fat32-server and the FS protocol are new userspace
processes; posix-server now routes openat/read/close instead of
returning ENOSYS; init wires one extra pair of bootstrap
endpoints. The kernel continues to know nothing about
filesystems, FDs, paths, or FAT32. That's the architecture
working as designed: capability passing through PageSet exports,
caller-token isolation, and the pure-types layer let three
userspace servers compose into a Linux ABI without one kernel
syscall added.

## What I'd do differently

If I were starting Phase G over, I'd write the test program with
direct syscalls from the first version. The detour through
fopen → malloc → mmap cost ~30 minutes and a confusing error
message ("posix: unknown nr=0xde -> ENOSYS" doesn't say "your
musl program tried to mmap"). The lesson is generic: when adding
a new userspace path, exercise it with the smallest possible
client first. fopen looks innocent; it's a libc abstraction over
several syscalls.

If I were starting Phase A over, I'd put the freshness-check
logic on the test image in the integration test script, not the
Makefile. Codex's first review caught that a stale 64 MiB
non-FAT32 file would silently pass the size check and break the
new "first 3 bytes match `eb 58 90`" assertion. The fix added
both a size and signature check. But the *right* place to put
that check is the same place that defines the assertion — the
script that asserts on the bytes also knows what the bytes
should be.

If I were starting the FAT32 parser over, I would have looked up
the cluster-count threshold (`FAT32_MIN_CLUSTERS = 65525`)
*before* Codex flagged it. The Microsoft FAT spec is explicit
that cluster count is the FAT12/16/32 discriminator, not the
fs_type string. I read the spec, missed the discriminator rule,
wrote the parser, and Codex caught it on review. The cost of
reading more carefully would have been five minutes.

## What I think about the project now

Phase 0 took most of a session. Phase 1 took most of two. The
acceleration is real, and so is the ceiling: each phase pulls in
more of the kernel surface than the last. Phase 2 (mmap) requires
client-controlled VA layout decisions in the personality server
plus possibly a new kernel capability-passing primitive. Phase 4
(threads) needs sys_futex. Phase 5 (processes) needs death
notifications. The plan calls these out in advance; the question
is which one's the next bottleneck.

The eight Codex reviews on this work felt different from earlier
sessions. The pattern wasn't "I missed a class of bug." It was "I
missed a specific instance of a class I knew about." The cluster
count gate, the FAT capacity check, the trailing-slash semantic,
the caller-token isolation — these are all in the same family as
work I'd done before. The reviews are still finding things, but
the things they find are smaller. I'd take that as evidence the
patterns are doing their job.

The architecture's leverage shows up most when the change is
small. The third instance of `BlockEngine`-shaped servers cost ~80
lines of `FsClient`. The eighth dispatch arm in
`lockjaw-types::posix::Action` cost less than the first did. The
pure-types layer is paying compounding interest.

A real Linux ABI program reads a real FAT32 file off a real
virtio-blk disk and prints its contents. The next phases
(filesystem write, mmap, threads, processes, signals) are each
their own session. They will each touch parts of the kernel that
haven't been touched in a year. I'm looking forward to seeing
which of them surfaces the next class of bugs the patterns
haven't seen yet.
