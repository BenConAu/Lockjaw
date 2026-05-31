# musl Port

Lockjaw runs musl libc 1.2.5 with two patched headers and one
shim source file. Everything else in musl is unmodified upstream
source. This doc covers what's patched and what the shim does on
each syscall.

For the posix-server side of the protocol the shim talks to, see
`posix-server` in `user/posix-server/` and the master plan in
[`../plans/posix-musl-plan.md`](../plans/posix-musl-plan.md).

## What's patched

Two files in `musl-lockjaw/patches/`:

| Patch | What it replaces | Why |
|---|---|---|
| `syscall_arch.h` | musl's aarch64 SVC stubs | Redirect every `__syscallN` to `lockjaw_syscall()` rather than `svc 0` to the Linux kernel. |
| `crt_arch.h` | musl's stack-pointer prologue at `_start` | Lockjaw sets SP to the very top of the stack allocation; the personality server writes the Linux auxv layout one page **below** the top, not at the top. The patch ADDS `sub sp, sp, #4096` before `mov x0, sp; b _start_c` so `_start_c` reads from the right place. |

Both are full-file replacements (not diff patches) installed during
musl-lockjaw/build.sh's cross-compile pass into the corresponding
slot in `musl-1.2.5/arch/aarch64/`.

## The syscall redirect

Stock musl uses `svc 0` to trap into the Linux kernel. The patched
`syscall_arch.h` (full file in `musl-lockjaw/patches/syscall_arch.h`)
replaces every `__syscallN(n, args...)` inline with a forwarding call:

```c
extern long lockjaw_syscall(long n, long a, long b, long c,
                             long d, long e, long f);

static inline long __syscall3(long n, long a, long b, long c) {
    return lockjaw_syscall(n, a, b, c, 0, 0, 0);
}
// __syscall0..__syscall6 follow the same shape.
```

`lockjaw_syscall` is a normal C function (Rust would also work; the
shim is C for its proximity to musl). It lives in
`musl-lockjaw/src/shim.c:477`. There is no SVC in the shim — the
musl binary is a userspace ELF that talks to posix-server via
IPC, not to the kernel directly.

## What the shim does

`lockjaw_syscall(n, a..f)` routes by syscall number. Three
categories.

### Locally handled (no IPC)

| Linux NR | Handler | Why local |
|---|---|---|
| `brk` | `handle_brk(a)` (`shim.c:270`) | No IPC to posix-server. Allocates and maps pages on demand via raw `LJ_SYS_ALLOC_PAGES` + `LJ_SYS_MAP_PAGES` SVCs (`:277-284`) in a `while (brk_mapped_end < new_end)` loop. |
| `mmap` | `handle_mmap(b, c, d)` (`:313`) | Local mmap tracker (`mmap_tracker[MMAP_TRACKER_SLOTS]` at `:234`) — but **also** sends an `__NR_mmap` IPC to posix-server for the PageSet allocation. The "local" part is the tracker update. |
| `munmap` | `handle_munmap(a, b)` (`:379`) | Tracker lookup first (returns EINVAL if no matching slot, no IPC); then IPC to release server-side state. |

`brk` is "local" in the sense that there's no IPC to posix-server,
but it does call directly into the kernel via SVC: `LJ_SYS_ALLOC_PAGES`
to grow the brk arena one page at a time, then `LJ_SYS_MAP_PAGES`
to install each new page at `brk_mapped_end`. The bootstrap
handshake sets `brk_current` and `brk_mapped_end` to the same
starting VA (`ensure_init` at `:444-445`) — nothing is pre-mapped;
the heap grows on demand from the first `brk(addr)` call.

### Forwarded to posix-server (IPC, no data copy)

| Linux NR | Forwarded as | Notes |
|---|---|---|
| `mprotect` | `lj_call(0, reply_handle, __NR_mprotect, a, b, c)` | Server verifies the region matches a known mmap entry. |
| `madvise` | same shape | Hints aren't load-bearing; server replies 0 unconditionally. |
| `exit_group`, `getpid`, `gettid`, etc. | per-call forward | Pure RPC. |

These syscalls take only scalar args; the shim packs them into
the four IPC message words and calls `lj_call` (`:152`), which
wraps `sys_call_ret4` to the posix-server endpoint.

### Forwarded with data via the shared buffer

| Linux NR | Data direction | Shim behavior |
|---|---|---|
| `write(fd, buf, len)` | shim -> server | `shim_memcpy(shared_buf, user_buf, len)` then forward `(fd, len, 0)` |
| `read(fd, buf, len)` | server -> shim | Forward `(fd, len, 0)`; on success `shim_memcpy_from_shared(user_buf, shared_buf, ret)` |
| `openat(dirfd, path, flags, mode)` | shim -> server | Copy null-terminated path into shared buffer; forward `(dirfd, path_len, flags)` |
| `readv` | server -> shim | Translates to a single read at the IPC level (server fills shared buffer once; shim splits into iovec entries) |

The shared buffer is one `PAGE_SIZE` page (`shared_buf` at
`:203`) that posix-server pre-mapped into the musl process at
startup. Both sides have a stable VA for it; the bootstrap
handshake establishes that VA. The buffer caps the per-syscall
transfer size at `PAGE_SIZE` — `write(fd, buf, 8192)` becomes
**one** IPC carrying 4096 bytes, returning a short count. The
caller (or musl's stdio wrapper, depending on call site) is
responsible for the retry loop; the shim does not loop.

## The bootstrap handshake

At process start, the shim's `ensure_init()` (`shim.c:421`) does
a one-time handshake with posix-server before any syscall can
run. The handshake exchanges:

- `reply_handle` — the Reply object the shim uses for every
  subsequent `sys_call`.
- `shared_buf` — the VA of the per-process shared page.
- The brk PageSet's mapping range — so `handle_brk` can grow into it.
- The mmap base VA — the per-process VA range posix-server will
  allocate from for `mmap` returns.

`initialized` (`:214`) gates the rest of `lockjaw_syscall`; every
syscall calls `ensure_init()` first. After the handshake the
shim is stateless except for the brk pointer and the mmap tracker.

## The mmap tracker

`mmap_tracker[MMAP_TRACKER_SLOTS]` (`shim.c:234`) is a fixed-size
array of `(base_va, handle, len)` records the shim maintains alongside
posix-server's own mmap_table. The tracker exists so that
`munmap(addr, len)` can validate the address+len pair against
something the shim knows is a real mmap result before forwarding
the syscall — a stray `munmap` of the brk range, or of garbage,
gets rejected without server round-trip.

This is the only persistent state the shim holds besides brk.

## What's not implemented

- **No `fork`.** Lockjaw spawns processes through
  `sys_create_process` from posix-server. The shim has no
  special handling for `clone`/`fork` — they fall through to
  the generic `lj_call` forward, and posix-server's
  `Action::Unknown` arm returns `-ENOSYS`.
- **No `execve`.** Same root cause — the posix-server path for
  process spawn doesn't exec; it spawns fresh.
- **No `signal`.** `sigaction`/`kill`/`signal` are sketched in
  [`../plans/posix-musl-plan.md`](../plans/posix-musl-plan.md)
  but not wired.
- **No file write.** `write(fd=1)` (stdout) works because
  posix-server routes it to UART; arbitrary FD writes to a
  FAT32 file aren't supported (the FS server is read-only — see
  [`fat32-server.md`](fat32-server.md)).
- **No `pipe`, `socket`, `poll`/`select`.** Phase 6+ work per the
  posix-musl-plan.

When one of these is needed, the pattern is:
1. Add a server-side handler in posix-server.
2. Decide locally-vs-forward in the shim, add the routing
   branch in `lockjaw_syscall`.
3. Update the plan doc.

## Where it lives

| Path | Role |
|---|---|
| `musl-lockjaw/musl-1.2.5/` | Unmodified upstream musl source, cross-compiled for AArch64. |
| `musl-lockjaw/patches/syscall_arch.h` | Replacement file installed at musl-1.2.5/arch/aarch64/. |
| `musl-lockjaw/patches/crt_arch.h` | Replacement file for the SP-adjust prologue. |
| `musl-lockjaw/src/shim.c` | `lockjaw_syscall`, brk handler, mmap handler, bootstrap, shared-buffer copies. |
| `musl-lockjaw/build.sh` | Cross-compile pipeline: copy patches into place, build libc.a, build `hello.c`. |
| `user/posix-server/src/main.rs` | The server side of every forwarded syscall. |
| `user/posix-hello/hello.c` | The integration-test musl binary that's the main consumer. |
