# POSIX Personality Server + musl Layer

Binary compatibility with `aarch64-unknown-linux-musl`. Any
statically-linked Linux binary built against musl libc runs on
Lockjaw without recompilation.

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
Lockjaw Kernel + Resource Servers (ramfs, uart-driver, etc.)
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
- **MAX_THREADS = 16**: sufficient for initial phases

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

IPC message format (4 u64 words via `sys_call` x2-x5):
- Word 0: Linux syscall number
- Words 1-3: scalar arguments (fd, len, offset, flags)
- Pointer arguments are implicit — data is in the shared buffer.

## musl Patching

Replace `arch/aarch64/syscall_arch.h` — the only file that touches
hardware. Stock musl:

```c
static inline long __syscall0(long n) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0");
    __asm__ volatile("svc 0" : "=r"(x0) : "r"(x8) : "memory", "cc");
    return x0;
}
```

Replace with:

```c
extern long lockjaw_syscall(long n, long a, long b, long c,
                             long d, long e, long f);

static inline long __syscall0(long n) {
    return lockjaw_syscall(n, 0, 0, 0, 0, 0, 0);
}
static inline long __syscall1(long n, long a) {
    return lockjaw_syscall(n, a, 0, 0, 0, 0, 0);
}
// ... through __syscall6
```

`lockjaw_syscall` (in `lockjaw_shim.c`):
1. Copy pointer arguments into shared buffer page
2. Pack syscall number + args into 4 IPC words
3. `sys_call` (real SVC #0 to Lockjaw kernel) on personality server
   endpoint
4. Unpack reply, copy data from shared buffer if needed
5. Return result (translate Lockjaw errors to Linux errno)

Everything else in musl is untouched.

Build: `./configure --target=aarch64-linux-musl --disable-shared`

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

### Phase 0: Hello World

**Gate:** `puts("hello, lockjaw")` compiled with musl runs.

Kernel pre-work: none (TPIDR_EL0 already landed).

New userspace:
- `user/posix-server/` — syscall dispatch loop, FD 0/1/2 wired to
  UART, shared buffer, Linux ELF loader with auxv
- musl fork: `syscall_arch.h` + `lockjaw_shim.c`
- `user/init/` spawns posix-server

Syscalls: `write`, `writev`, `exit_group`, `brk` (fixed value),
`set_tid_address` (stub), `ioctl(TIOCGWINSZ)` (stub)

### Phase 1: Filesystem

**Gate:** read a file from ramfs.

New userspace:
- `user/ramfs-server/` — cpio parser, inode table, IPC interface
- Personality server: VFS dispatch, FD open/close/seek

Syscalls: `openat`, `read`, `close`, `lseek`, `fstat`,
`newfstatat`, `getcwd`, `readlinkat` (stub ENOENT)

### Phase 2: Memory Management

**Gate:** Rust binary with `Vec<u64>` of 1M elements.

Personality server: mmap handler with client-assisted mapping
(export PageSet, reply with instructions, client does
`sys_map_pages`). munmap. mprotect stub.

Syscalls: `mmap`, `munmap`, `mprotect`, `madvise` (stub)

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

## New Files

```
user/posix-server/
  Cargo.toml
  src/main.rs              # bootstrap, main loop, dispatch
  src/fd_table.rs          # per-process FD table
  src/process_table.rs     # PID table, per-process state
  src/syscall/mod.rs       # dispatch table
  src/syscall/io.rs        # write, read, writev, lseek
  src/syscall/fs.rs        # open, close, stat, getdents
  src/syscall/mem.rs       # brk, mmap, munmap
  src/syscall/proc.rs      # exit, spawn, waitpid
  src/syscall/thread.rs    # clone, futex
  src/syscall/signal.rs    # sigaction, kill
  src/elf_loader.rs        # Linux ELF loader (auxv)
  src/shared_buffer.rs     # per-client shared page management

user/ramfs-server/
  Cargo.toml
  src/main.rs              # bootstrap, IPC loop
  src/cpio.rs              # cpio archive parser
  src/fs.rs                # inodes, dirents, read/write

musl-lockjaw/              # patched musl (or patch files)
  arch/aarch64/syscall_arch.h
  src/lockjaw/shim.c

tests/posix/               # C test programs
  hello.c
  test_write.c
  test_malloc.c
  test_open_read.c
  test_thread.c
  test_spawn.c
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
