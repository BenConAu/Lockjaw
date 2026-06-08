# POSIX Personality Server + musl Layer

Binary compatibility with `aarch64-unknown-linux-musl`. Any
statically-linked Linux binary built against musl libc runs on
Lockjaw without recompilation.

**Status (2026-05-05):** Phase 1 done — a real musl-built program
opens a file on a 64 MiB FAT32 virtio-blk disk, reads its contents
through three IPC layers (posix-server -> fat32-server -> virtio-blk-driver),
and prints them. Phase 0 (`23f18f1` + `b454770`) and Phase 1
(`2fb0e49` ... `f929766`) gates met. Phases 2+ still aspirational.
See [Phase 0](#phase-0-hello-world) and [Phase 1](#phase-1-filesystem)
sections below.

## Architecture

```
Statically-linked Linux ELF (Rust or C)
  |  libc calls (write, open, mmap, ...)
patched musl libc (lockjaw_syscall() replaces SVC stubs)
  |  packs syscall number + args into IPC message
IPC shim (linked into binary, uses shared buffer page)
  |  sys_call on personality server endpoint
POSIX Personality Server (userspace process)
  |  FD tables, pipes, brk, signals, VFS dispatch
  |  routes to resource servers via IPC
Lockjaw Kernel + Resource Servers (ramfs, pl011-driver, etc.)
```

The personality server is a normal Lockjaw userspace process. It
uses the existing **call/reply IPC pattern** — client does
`sys_call` on the personality server endpoint; server does
`sys_receive` + dispatch + `sys_reply`. The **caller_token**
mechanism (`sys_query_caller_token`) identifies which POSIX process
made each call.

The kernel never learns about POSIX. FDs, signals, PIDs, pipes —
all personality server. A different personality server (Plan 9,
native Lockjaw, etc.) could coexist.

## Kernel Pre-Work

Ordered by phase dependency, not front-loaded.

### ~~Blocks Phase 0 (hello world)~~ DONE

**TPIDR_EL0 save/restore.** Landed in `ac09c77`. ExceptionContext
`_pad` replaced with `tpidr_el0`, mrs/msr pairs added in
SAVE_REGS/RESTORE_REGS, zeroed in `drop_to_el0_with_ttbr0`.

### Blocks Phase 3 (time/random)

**sys_clock_gettime.** Read CNTPCT_EL0 + CNTFRQ_EL0, convert to
timespec. New kernel syscall.

**sys_getrandom.** Timer-seeded PRNG or hardware RNG if available.

### Blocks Phase 4 (threads)

**sys_futex.** musl's pthread_mutex/cond use futex(FUTEX_WAIT,
FUTEX_WAKE). Notification objects are per-object, not per-address —
futex needs address-keyed wait queues. New kernel syscall:
`sys_futex(addr, op, val, timeout)`.

### Blocks Phase 5 (processes)

**Process exit notification.** Personality server needs "process N
died" events for waitpid, FD cleanup, pipe EOF. Currently
`finish_exit` silently frees pages.

Fix: add `death_notification` field to ProcessObject. Personality
server creates a notification per POSIX process. Kernel's
`finish_exit` signals it in the `LastThread` arm.

### Already available (no changes needed)

- **TPIDR_EL0 save/restore**: landed in `ac09c77`
- **Multi-handle bootstrap**: existing call/reply bootstrap protocol
  (every child calls handle 0 at startup, parent exports handles via
  `sys_export_handle` + reply)
- **Caller identification**: `sys_query_caller_token()` with
  monotonic per-export tokens
- **IPC call/reply**: `sys_call` + `sys_reply`
- **Page allocation + mapping**: `sys_alloc_pages` + `sys_map_pages`
- **ELF loader**: `lockjaw-userlib` ET_EXEC loader
- **VA allocator**: `VirtualMemory` bitmap allocator in userlib
- **MAX_THREADS = 1024** (`lockjaw_types::scheduler::MAX_THREADS`):
  far more than POSIX phases will ever push against; sized to match
  what aarch64 hardware can comfortably schedule. Was 16 originally
  but the cap collapsed `SchedulerFull` and `OutOfMemory` into one
  syscall return — see `docs/tracking/tech-debt.md` for the full story.

## The Shared Buffer

Linux syscalls pass pointers (`write(fd, buf, len)`). The
personality server is in a different address space — it cannot
dereference client pointers.

Solution: **shared buffer page per client** (same as seL4 CAmkES).

1. At bootstrap, personality server allocates a page per client.
2. Page is mapped into both client (via exported PageSet handle +
   `sys_map_pages`) and personality server.
3. IPC shim copies user data into shared page before `sys_call`.
4. Personality server reads/writes the shared page directly.

IPC message format (4 u64 words via `sys_call`):
- Word 0: Linux syscall number
- Words 1-3: scalar arguments (fd, len, offset, flags)
- Pointer arguments are implicit — data is in the shared buffer.

The Lockjaw `sys_call` ABI is asymmetric: 4 message words go in via
`x2-x5`, but the reply lands in `x1-x4` (`x0` is the kernel transport
error code, written only on return). Phase 0a's first shim got this
wrong (read reply from `x2-x5`) and silently shifted every reply word
by one register. The shim now reads `x1-x4` correctly, and `lj_call`
treats a nonzero `x0` as a fatal transport failure (no way for libc
to recover from an unbound endpoint).

## musl Patching

Three replacement files in `musl-lockjaw/` — everything else in musl is
untouched.

### `arch/aarch64/syscall_arch.h`

Replaces stock musl's `__syscallN` inline-asm SVC wrappers with calls
to `lockjaw_syscall(n, a, b, c, d, e, f)` (defined in `shim.c`).

### `arch/aarch64/crt_arch.h`

Patched `_start` to land on the personality server's stack layout:

```asm
_start:
    mov x29, #0
    mov x30, #0
    sub sp, sp, #4096       // Lockjaw: stack layout is one page below top
    mov x0, sp
    b _start_c
```

Lockjaw's kernel sets `SP = USER_STACK_BASE + stack_pages * PAGE_SIZE`
(top of allocation). The personality server writes
argc/argv/envp/auxv at `top - PAGE_SIZE`. Patched `_start` subtracts
4096 to land there.

### `src/lockjaw/shim.c`

Implements `lockjaw_syscall`:

1. `ensure_init()` on first call — bootstrap a Reply object, send
   `POSIX_INIT` (sentinel `0xFFFF_FFFF_FFFF_FF00`) to handle 0
   (the personality-server endpoint), receive shared buffer handle
   + VA + brk base, map shared buffer locally. Every fallible step
   checked; any failure halts via `lj_die()` (prints diagnostic via
   `sys_debug_putc` and `wfi` forever — libc init has no recovery
   path for a botched bootstrap).
2. Handle `__NR_brk` locally via direct Lockjaw SVCs (`sys_alloc_pages`
   + `sys_map_pages`) — no IPC round-trip.
3. For `write`/`writev`: copy/gather user data into the shared
   buffer (clamped to `PAGE_SIZE`), then IPC.
4. For everything else: pass scalar args through, IPC, return reply.

`lj_call` treats a nonzero kernel transport return (x0) as fatal —
the personality server didn't get the call, so any value in x1 is
meaningless and would be misinterpreted as a Linux syscall result.

Build: `musl-lockjaw/build.sh` downloads musl 1.2.5, applies the three
patches, builds with `aarch64-linux-musl-gcc` (incremental — only
rebuilds libc.a when patches/shim are newer), then compiles
`hello.c` against patched musl.

## Linux ELF Loader

musl's `_start` expects the Linux initial stack layout:

```
SP + 0:   argc
SP + 8:   argv[0] ... argv[argc-1], NULL
          envp[0] ... envp[n], NULL
          auxv[0] (key, value pairs) ... AT_NULL
```

The personality server constructs this in pages it allocates before
calling `sys_create_process`. Required auxv entries:

- `AT_PAGESZ = 4096`
- `AT_RANDOM = <pointer to 16 random bytes>` (stack canary seed)
- `AT_NULL = 0`

**Static binaries only.** Reject `PT_INTERP`. No dynamic loader.

**Unaligned LOAD segments.** Musl produces tightly-packed binaries — its
second LOAD typically starts mid-page (e.g. `vaddr=0x41ffa8`,
`mem_size=0x7b8`) and crosses page boundaries. The personality server's
ELF loader (`load_elf_segments` in `posix-server/src/main.rs`) walks the
page-aligned VA range covered by each segment, places file data at the
correct in-page offset, and zeros the rest (covers BSS and pre-data
padding). The naive "one mapping per segment, vaddr is page-aligned"
approach used by `init/spawn_elf` works for Rust binaries with
`ALIGN(4K)` linker directives but is broken for musl.

## Design Decisions

| Decision | Choice | Why |
|----------|--------|-----|
| fork() | ENOSYS; posix_spawn only | No CoW, no page fault handler |
| brk | Server-side per-process heap pointer | Pure POSIX concept, no kernel brk |
| FD table | Server-side, keyed by caller_token | Correct fork/cleanup/pipe semantics |
| Signal delivery | Cooperative (check on IPC return) | Forced delivery too complex initially |
| Ramfs data | `include_bytes!` cpio in posix-server | Avoids DTB initrd parsing |
| Server threading | Single-threaded | Clients block on sys_call anyway |

## Phase Breakdown

### Phase 0: Hello World — DONE

**Gate met:** `puts("hello, lockjaw")` compiled with musl runs in
QEMU end-to-end. See commits `23f18f1` (Phase 0a: server scaffolding
+ freestanding test) and `b454770` (Phase 0b: real musl wired up).

Kernel pre-work: none (TPIDR_EL0 was already landed in `ac09c77`).

What was built:
- `user/posix-server/` — bootstrap + ELF loader + dynamic VA layout
  (ELF end → guard → shared buffer → brk base) + Linux initial
  stack construction (argc/argv/auxv with AT_PAGESZ + AT_RANDOM) +
  syscall dispatch loop. FD 1/2 → kernel UART via `sys_debug_putc`.
- `musl-lockjaw/` — three patches (`crt_arch.h`, `syscall_arch.h`)
  plus `shim.c`, a build script that builds patched musl 1.2.5 and
  compiles `hello.c` against it.
- `user/posix-hello/` — `hello.c` (the gate program), plus a
  freestanding `standalone.c` debug client (no musl dependency,
  used during shim development).
- `user/init/` updated to spawn posix-server alongside other servers.

Implemented Phase 0 syscalls: `write`, `writev`, `exit_group`,
`set_tid_address` (returns 1), `ioctl` (returns -ENOTTY).
`brk` is local-only in the shim (no IPC, uses `sys_alloc_pages` +
`sys_map_pages`); the brk base VA is computed by the personality
server from the child's ELF layout and delivered in the
POSIX_INIT reply.

#### Implementation notes (lessons learned)

- **IPC reply ABI is asymmetric** — see Shared Buffer section above.
  Cost about an hour to track down via the silent fault.
- **Every SVC return must be checked.** The original shim used
  `let _ = sys_call(...)` and `lj_svc3(...)`-without-checking
  patterns. A failed `sys_map_pages` left `shared_buf` pointing at
  an unmapped VA; the next `memcpy` faulted. Fix: `lj_die()` helper
  uses `sys_debug_putc` (kernel UART, no IPC) so unrecoverable
  errors at any layer can produce a diagnostic before halting.
- **Build with `initialized = 1` LAST.** The original `ensure_init`
  set the flag before any fallible work, which would have turned a
  bootstrap failure into a latent half-initialized state instead of
  a hard failure.
- **CRT object link order matters.** `crt1.o crti.o <user> -lc -lgcc
  crtn.o`. The first build had user objects before `crt1.o` and
  `crtn.o` before `-lc`, which would seal `.init`/`.fini` before
  libc constructors could land.
- **musl's second LOAD is unaligned.** `init/spawn_elf` assumed
  page-aligned vaddrs (works for Rust `ALIGN(4K)` linker scripts);
  it's a latent bug that the posix-server's loader had to fix.
  The same fix should land in `init/spawn_elf` if any future
  user binary is built without page-aligned segments.
- **Build script portability.** `sysctl -n hw.ncpu` is macOS-only;
  the script now falls through `nproc` → `sysctl` → `4`.

### Phase 1: Filesystem — DONE

**Gate met:** A real musl-built program opens `/HELLO.TXT` on a
FAT32 virtio-blk disk via `openat`, reads the contents via `read`,
closes the fd, and prints the bytes. The full path is:

```
musl libc -> patched shim -> posix-server (FdTable + handlers)
  -> FsClient (FS-IPC) -> fat32-server (path resolve + FAT walk)
  -> BlockClient (block-IPC) -> virtio-blk-driver (MMIO + IRQ)
  -> QEMU virtio-blk -> test.img (mformat-built FAT32).
```

Plan reframe: the original draft proposed `user/ramfs-server/` (a
cpio archive parsed in memory). We threw that out and built FAT32
on the existing virtio-blk driver instead — the block driver was
sitting in CI with no real workload, the FAT32 server is portable
to a future Pi 4B SDIO driver because both speak `BlockEngine`,
and we don't write throwaway code that ramfs would mostly become.

What was built:

- `lockjaw-types/src/fat32.rs` — pure BPB parser, FAT-chain
  decode, dirent parser, 8.3 path matching. ~70 host tests
  including adversarial geometries (FAT region exceeds volume,
  cluster count below FAT32 minimum, FAT too small for cluster
  count, forged fs_type strings).
- `lockjaw-types/src/fs.rs` — FS-IPC protocol (FS_OPEN /
  FS_READ / FS_CLOSE) with a pure dispatch decision function.
  Inline path bytes (16-byte cap) instead of capability-passing
  PageSet handles, because `sys_export_handle` only goes
  server→client and client→server import would need a new
  kernel syscall. Documented in the module header.
- `lockjaw-types/src/posix_fd.rs` — pure `FdTable<MAX_FDS=32>`
  with `with_stdio()` constructor, lowest-free-fd allocation,
  and stdio protection. No cursor field — cursor lives in
  fat32-server (Linux "open file description" semantics).
- `user/fat32-server/` — bootstraps from init, mounts the disk,
  serves FS_OPEN / FS_READ / FS_CLOSE. Open-file table scoped
  by `sys_query_caller_token` for cross-client isolation.
  Cluster scratch + FAT scratch buffers from the block driver.
- `user/fat32-test/` — ~100-line verification client that opens
  `/HELLO.TXT`, reads via FsClient, prints via UART. Independent
  of musl (proves the FS server works without the personality
  server in the loop).
- `user/lockjaw-userlib/src/fs.rs` — `FsClient` typed wrapper.
- `user/posix-server/src/main.rs` — `FsClient` integration,
  per-client FD table, FileOpen/Read/Close handlers. Open
  rejects write-touching flags (`O_WRONLY`/`O_RDWR`/`O_CREAT`/
  `O_EXCL`/`O_TRUNC`/`O_APPEND`) with `-EROFS` since Phase 1
  is read-only. Close talks remote-first; failed remote closes
  go on a small deferred-retry queue.
- `musl-lockjaw/src/shim.c` — `openat` and `read` syscall
  handlers; `close` uses the existing scalar catch-all.
- `user/posix-hello/hello.c` — extended to also openat + read
  + close. Uses *direct syscalls* (not fopen/fread) because
  musl's stdio mallocs the FILE struct and musl's malloc uses
  mmap, which Phase 2 handles.

Syscalls implemented this phase: `openat`, `read`, `close`.
Deferred to follow-on phases: `lseek`, `fstat`, `newfstatat`,
`getcwd`, `readlinkat`.

#### Implementation notes (lessons learned)

- **The FAT32 spec's discriminator is cluster count, not the
  fs_type string.** The string at offset 82 is informational; a
  forged "FAT32   " on a FAT16-sized volume should be rejected.
  `parse_bpb` enforces `cluster_count >= FAT32_MIN_CLUSTERS = 65525`.
- **Per-page PageSet allocation breaks at 32 pages.**
  `MAX_CONSUMED_HEADERS = 32` (caller-supplied storage to `dedup_add_header`). Init's
  `spawn_elf` originally allocated one PageSet per page of the
  child binary, which broke `sys_create_process` for posix-server
  (now ~42 pages because of the embedded musl hello binary).
  Fixed by allocating one PageSet of N pages with each
  `ProcessMapping` referencing it at a different `page_index`.
- **Trailing slash on a path carries semantic meaning.**
  `/file.txt/` should require the target to be a directory
  (`-ENOTDIR`), not silently open a file. Slash-normalization
  must capture the trailing-slash bit *before* dropping empty
  components in the filter.
- **Caller-token isolation is mandatory for shared servers.** The
  block server's `BufferTracker` already had this pattern; the
  fat32-server's open-file table needed the same scoping so a
  malicious client can't operate on another client's handles by
  guessing the integer.
- **musl stdio drags in malloc which drags in mmap.** A test
  program using `fopen`/`fread` won't work in Phase 1 — direct
  `openat`/`read` is required until Phase 2 lands.

### Phase 1 syscalls deferred to later

- `lseek` — needs cursor manipulation in fat32-server (cursor lives
  there per the Phase 1 design). Pure decision function in
  lockjaw-types::fs::dispatch (FS_LSEEK arm).
- `fstat` / `newfstatat` — file metadata fetch. Needs new FS-IPC
  arms (FS_FSTAT and FS_STAT) plus a Linux `struct stat` layout
  in shared bytes.
- `getcwd` / `readlinkat` — stub ENOENT initially.

### Phase 2: Memory Management — DONE

**Gate met:** musl `malloc(1MiB)` (Phase 2.3) and `malloc(8MiB)`
(Phase 2.4) via mmap; integration tests assert both.

**What landed** (commits `87149a4` → `376dedb`, Phase 2.K through
Phase 2.5): variable-size PageSet header (`pageset_table.rs`), VA
layout extension with `mmap_base` (`lockjaw-types/src/posix.rs`),
pure dispatch arms, posix-server runtime `MmapTable` /
`MmapVaAllocator` (`user/posix-server/src/main.rs`), mmap tracker in
`musl-lockjaw/src/shim.c`, fopen/fread/malloc gates in
`user/posix-hello/hello.c`. Full subphase breakdown preserved in
`docs/history/posix-phase2-mmap-plan.md`.

**Out of scope / deferred** (sketched at the bottom of the history
plan, not yet built): `MAP_FIXED`, `MAP_SHARED`, file-backed mmap,
`PROT_EXEC`, partial munmap, `mremap`, `mprotect` on non-mmap
ranges. Tracked in `docs/tracking/tech-debt.md` when promoted from
"deferred" to "needed".

### Phase 3: Time and Random

**Gate:** program prints time, sleeps, prints again.

Kernel: `sys_clock_gettime`, `sys_getrandom`

Syscalls: `clock_gettime`, `clock_nanosleep`, `getrandom`, `gettid`

### Phase 4: Threads

**Gate:** `std::thread::spawn` + join.

Kernel: `sys_futex`

Personality server: clone handler (CLONE_VM | CLONE_THREAD only).
TPIDR_EL0 writable from EL0 — thread sets its own TLS pointer.

Syscalls: `clone` (thread-only), `futex`, `set_robust_list` (stub),
`sched_yield`, `exit` (single thread)

### Phase 5: Processes

**Gate:** spawn child, wait, print exit status.

Kernel: death notification on ProcessObject.

Personality server: `posix_spawn` (load ELF from ramfs, create
process, FD inheritance), `waitpid` (death notifications), `execve`
(kill self + spawn new, same PID).

Syscalls: `fork` (ENOSYS), `execve`, `wait4`, `getpid`, `getppid`

### Phase 6: Pipes and Signals

**Gate:** pipe between two processes.

Personality server: pipe ring buffer, dup/dup2/dup3, cooperative
signal delivery (pending/blocked masks, check on IPC return).

Syscalls: `pipe2`, `dup`, `dup2`, `dup3`, `sigaction`,
`sigprocmask`, `kill`, `fcntl` (basic)

### Phase 7: Terminal

Syscalls: `ioctl` (TIOCGWINSZ, TCGETS, TCSETS), `poll`/`ppoll`,
`select`

### Phase 8: Networking (deferred)

Socket stubs return ENOSYS. Requires smoltcp or similar.

## As-built layout

The original "New Files" sketch (separate `fd_table.rs`,
`process_table.rs`, per-syscall-group modules under
`src/syscall/*`, and a `user/ramfs-server/` for cpio-backed FS) was
deliberately not built. The simpler shape that landed:

```
user/posix-server/
  Cargo.toml
  src/main.rs              # everything: bootstrap, dispatch, FD table, mmap, process state
                           # (single file by design — re-evaluate split when it exceeds
                           # what one person can hold in head, not before)

user/fat32-server/         # replaces the planned ramfs-server
user/partition-manager/    # added during the eMMC bring-up (M5)

musl-lockjaw/
  patches/                 # arch + crt + syscall_arch patches over upstream 1.2.5
  src/shim.c               # lockjaw_syscall + bootstrap handshake + local brk + mmap tracker

user/posix-hello/
  hello.c                  # musl-built test: fopen + fread + malloc(1MiB/8MiB) gates
```

## Testing Strategy

**Tier 1: Host unit tests** — personality server pure logic (FD
table, brk, signal masks, PID allocation). In `lockjaw-types` or
`posix-server/src/` with `#[cfg(test)]`. Runs with `make test`.

**Tier 2: C test programs** — one `.c` file per syscall group,
compiled with patched musl, embedded in ramfs cpio, prints "ok" on
success.

**Tier 3: QEMU integration** — extend `tests/qemu_integration.sh`.
Boot, run test binary, assert output.

**Tier 4: musl libc-test** — conformance suite (after Phase 5).
Track pass rate.

Development loop: compile test binary, boot, see ENOSYS, implement
syscall, boot again. Each syscall is a small atomic commit.

## Design Constraints

- **No POSIX in the kernel.** FDs, signals, PIDs, pipes live in the
  personality server. Kernel adds only general primitives (futex,
  clock).
- **Static binaries only.** `PT_INTERP` rejected. No dlopen.
- **Stub aggressively.** Unimplemented syscalls return ENOSYS.
- **Errno mapping must be exact.** Programs depend on specific errno
  values.
- **Single personality server process.** Don't split prematurely.

## What Success Looks Like

```
cargo build --target aarch64-unknown-linux-musl --release
```

Drop the binary into the cpio initrd. Boot Lockjaw. The binary
runs. `std::thread`, `std::fs`, `Vec`, `HashMap`, `format!` — all
work. The binary doesn't know it's not on Linux.
