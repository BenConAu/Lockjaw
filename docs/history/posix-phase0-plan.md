# Phase 0: musl Hello World

## Context

First step of the POSIX personality server plan (`docs/plans/posix-musl-plan.md`).
A C program compiled with `aarch64-linux-musl-gcc -static` that calls
`puts("hello, lockjaw")` runs on Lockjaw in QEMU.

Kernel pre-work: none (TPIDR_EL0 landed in `ac09c77`).

## The SP Problem

musl's `_start` (`arch/aarch64/crt_arch.h`) does `mov x0, sp; b _start_c`
— it reads argc/argv/envp/auxv from the stack pointer. On Linux, the
kernel writes this layout below the top of the stack and sets SP to
point at argc.

Lockjaw's `create_process` (`src/process.rs:273`) hardcodes
`user_stack_top = USER_STACK_BASE + stack_pages * PAGE_SIZE` (the very
top of the stack allocation). musl would read from beyond mapped memory.

**Fix:** Patch musl's `crt_arch.h` to subtract a fixed offset:
```asm
_start:
    mov x29, #0
    mov x30, #0
    sub sp, sp, #4096       // Lockjaw: stack layout is one page below top
    mov x0, sp
    b _start_c
```

The personality server writes argc/argv/envp/auxv at
`USER_STACK_BASE + stack_pages * 4096 - 4096`. Simple, deterministic,
no kernel change.

## The Shared Buffer

`write(fd, buf, len)` passes a pointer the personality server can't
dereference (different address space).

**Constraint:** `sys_create_process` destructively transfers mappings
from parent to child (`src/process.rs:282-298`). If posix-server
includes the shared page in the child's mapping list, it loses its
own mapping. And `handle_to_copy` only copies one handle (needed for
the syscall endpoint).

**Fix:** Bootstrap-time shared buffer setup. The shim's `ensure_init`
sends a special `POSIX_INIT` message as its first IPC call on handle 0.
The posix-server recognizes this:

1. Posix-server allocates shared buffer page, maps at `server_shared_va`
   (from its own VMEM allocator)
2. `sys_export_handle(shared_ps)` → exports into the blocked child's
   handle table (returns child's handle index)
3. `sys_reply(child_handle_idx, child_shared_va, brk_base, 0)` — the
   child VA was computed from the child's ELF layout (step 6)
4. Shim receives reply, calls `lj_svc(SYS_MAP_PAGES, child_handle_idx, child_shared_va, 0)`
5. Both sides now have the same physical page mapped at different VAs:
   server reads/writes at `server_shared_va`, child at `child_shared_va`

The posix-server retains ownership of its own mapping. The child gets
an exported handle + does its own `sys_map_pages`.

**sys_export_handle verified API** (`src/syscall/handler.rs:567-630`):
- **Signature:** `sys_export_handle(x0 = handle_index_to_export)` → returns
  new handle index in the caller's table (via x1).
- **Target determination:** Implicit. Reads `current_reply_paddr` from
  the server's TCB (set by `sys_receive` when it dequeued the Call).
  Follows the Reply object's `caller_tcb_paddr` to find the blocked
  caller's process and handle table. No explicit process argument.
- **Precondition:** Server must have a bound caller (i.e., must have
  done `sys_receive` that dequeued a Call, and not yet called
  `sys_reply`). Returns `NO_CALLER` (error 6) otherwise.
- **For PageSet handles:** Increments refcount so both processes own
  the physical pages.
- **Existing usage:** Init does exactly this pattern at
  `user/init/src/main.rs:326-330`: `sys_receive` → `sys_export_handle`
  → `sys_reply(exported_idx, ...)`. The posix-server POSIX_INIT handler
  follows the identical pattern.

## The brk Problem

musl's malloc calls `brk(0)` early. No kernel brk exists.

**Fix:** The C shim handles brk locally — never sends IPC. It calls
`sys_alloc_pages` + `sys_map_pages` (direct Lockjaw SVCs) to grow
the heap upward. Self-contained, no personality server involvement.

The brk base VA is computed dynamically: after loading, the shim
receives it from the posix-server as part of the POSIX_INIT reply
(word 2). The posix-server computes it as: `elf_end_aligned + 2 *
PAGE_SIZE` (one page for the shared buffer, one page gap before that).

**brk invariants (Phase 0 personality layout contract):**
- `brk_base` is page-aligned (guaranteed by the formula above)
- Growth is monotonic — brk only moves upward, never shrinks
- The VA range `[brk_base, USER_STACK_BASE)` is exclusively owned
  by the brk shim. No other mapping (ELF, shared buffer, stack)
  will be placed in this range. Posix-server asserts this at spawn.
- The shim maps new pages at `brk_mapped_end` via `sys_alloc_pages`
  + `sys_map_pages`, then advances `brk_mapped_end` by the page count

## Output Path

For Phase 0, the personality server uses `putc()` (sys_debug_putc,
kernel UART) to emit write() output. No UART driver IPC needed. This
means posix-server doesn't need any handles from init — the bootstrap
handshake is a no-op acknowledgment.

## Components

### A. `user/posix-server/` (new Rust crate)

Follows the existing server pattern (hello, uart-driver).

**Crate setup:**
- `Cargo.toml` — depends on `lockjaw-userlib`, `lockjaw-types`
- `linker.ld` — copy from `user/hello/linker.ld`
- `build.rs` — copy from `user/hello/build.rs`
- `.cargo/config.toml` — copy from `user/hello/.cargo/config.toml`

**`src/main.rs` structure:**

1. **Bootstrap** — create Reply, call handle 0, ignore response
   (init blocks until we call, so we must). Use `puts()` for status.

2. **Load embedded POSIX binary** — `static HELLO: &[u8] = include_bytes!(...)`.
   Reuse `lockjaw_types::elf::parse_elf()`. Allocate pages per
   PT_LOAD segment, map temp, copy data, build `ProcessMapping`
   array. Same logic as `spawn_elf` in `user/init/src/main.rs:46-170`.

3. **Shared buffer** — allocated lazily on first POSIX_INIT call
   from the child (see "The Shared Buffer" above). Posix-server
   allocates 1 page, maps into its own VA, exports the PageSet
   handle into the child, and tells the child where to map it.
   The child's shim does `sys_map_pages` locally.

4. **Stack layout construction** — allocate 4 stack pages, map
   temporarily. Write at offset `3 * 4096` (= 4096 bytes below top):
   ```
   +0:   1 (argc)
   +8:   <ptr to "hello"> (argv[0])
   +16:  0 (argv terminator)
   +24:  0 (envp terminator)
   +32:  AT_PAGESZ (6)
   +40:  4096
   +48:  AT_RANDOM (25)
   +56:  <ptr to random bytes at +80>
   +64:  AT_NULL (0)
   +72:  0
   +80:  16 pseudo-random bytes (fixed seed for Phase 0)
   +96:  "hello\0"
   ```
   All pointers are in the child's VA space:
   `USER_STACK_BASE + 3 * 4096 + offset_within_layout`.

   Do NOT use `sys_create_process`'s stack_ps parameter (which
   auto-maps at USER_STACK_BASE). Instead, include stack pages in
   the regular mapping list at USER_STACK_BASE manually, and pass
   a dummy 1-page stack_ps. Actually — checking the kernel code,
   `create_process` always maps stack_ps at USER_STACK_BASE. So we
   write the layout into the stack pages before unmap, pass them as
   stack_ps, and the kernel maps them at 0x800000. SP = 0x800000 +
   4 * 4096 = 0x804000. The layout at 0x804000 - 4096 = 0x803000.

5. **Spawn** — create syscall endpoint, call `sys_create_process`
   with the endpoint as `handle_to_copy` (child gets it at index 0).

6. **Compute dynamic VAs** — layout after ELF segments:

   ```
   elf_end_aligned = align_up(max(vaddr + mem_size), PAGE_SIZE)
   shared_buf_va   = elf_end_aligned + PAGE_SIZE   // 1-page unmapped guard
   brk_base        = shared_buf_va   + PAGE_SIZE   // heap starts after shared buf
   ```

   The shared buffer occupies exactly one page. It is mapped at two
   different VAs in two different address spaces:
   - `child_shared_va` = `elf_end_aligned + PAGE_SIZE` (in the child)
   - `server_shared_va` = allocated from posix-server's VMEM (in the server)

   The brk heap starts at `brk_base` and grows upward.
   Assert `brk_base < USER_STACK_BASE`. Panic on overlap.

   `child_shared_va` and `brk_base` are computed by posix-server from
   the child's ELF layout and sent to the shim in the POSIX_INIT reply
   (words 1 and 2). `server_shared_va` is internal to posix-server.

7. **Syscall dispatch loop:**
   ```rust
   loop {
       let msg = sys_receive_ret4(syscall_ep)?;
       let nr = msg[0];
       match nr {
           POSIX_INIT => {
               // First call from child — set up shared buffer.
               // Two distinct VAs for the same physical page:
               //   server_shared_va — in posix-server's address space (from VMEM)
               //   child_shared_va  — in child's address space (from ELF layout)
               let shared_ps = sys_alloc_pages(1)?;
               server_shared_va = VMEM.alloc(1)?;
               sys_map_pages(shared_ps, server_shared_va, 0)?;
               let child_idx = sys_export_handle(shared_ps)?;
               // child_shared_va and brk_base were computed from child ELF layout
               sys_reply(child_idx, child_shared_va, brk_base, 0);
           }
           SYS_WRITEV | SYS_WRITE => handle_write(&msg),
           SYS_EXIT_GROUP => { puts("posix: exit\n"); break; }
           SYS_SET_TID_ADDRESS => sys_reply(1, 0, 0, 0),
           SYS_IOCTL => sys_reply(-ENOTTY_U64, 0, 0, 0),
           _ => sys_reply(-ENOSYS_U64, 0, 0, 0),
       }
   }
   ```

   `POSIX_INIT` is a sentinel value (e.g., `0xFFFF_FFFF_FFFF_FF00`)
   that no real Linux syscall number matches.

8. **handle_write** — Phase 0 write path with explicit capacity rules:

   **Shared buffer capacity:** 1 page = 4096 bytes. This is the hard
   limit for a single write/writev call.

   **Shim side (C):**
   - `write(fd, buf, len)`: clamp `len` to 4096, `memcpy` into shared
     buffer, send IPC with clamped length. Return value from server is
     the byte count actually written (may be < requested len).
   - `writev(fd, iov, iovcnt)`: gather iovec entries sequentially into
     shared buffer, stop when buffer is full (4096 bytes reached or all
     iovecs consumed). Send IPC with total gathered length.
   - Both paths: the shim copies at most `PAGE_SIZE` bytes into the
     shared buffer before calling. The IPC message word 1 = fd,
     word 2 = byte count in the shared buffer.

   **Server side (Rust):**
   - Read `len` bytes (from IPC word 2) from shared buffer VA.
     Assert `len <= PAGE_SIZE` (panic on violation — would mean shim
     bug).
   - Emit each byte via `putc()` (kernel UART). Reply with `len`.
   - FD 1 and 2 → UART. Other FDs → reply with `-EBADF`.

   **Phase 0 limitation:** The 4 KiB cap means writes larger than one
   page are silently truncated to a short write. This is valid POSIX
   (short writes are permitted), but libc is not obligated to retry
   transparently — callers that don't check return values will lose
   data. For Phase 0 this is acceptable: `puts("hello, lockjaw")`
   produces `writev(1, [{str, 15}, {"\n", 1}], 2)` = 16 bytes, well
   within one page. Phase 2 (mmap) adds multi-page shared buffers
   that remove this cap.

### B. musl Patches (3 files)

Kept as patch files or a snapshot in `musl-lockjaw/patches/`.

**`arch/aarch64/crt_arch.h`** — replace `_start` with SP-adjusting
version (see "The SP Problem" above).

**`arch/aarch64/syscall_arch.h`** — replace SVC stubs:
```c
extern long lockjaw_syscall(long n, long a, long b, long c,
                             long d, long e, long f);
static inline long __syscall0(long n) {
    return lockjaw_syscall(n,0,0,0,0,0,0);
}
// ... through __syscall6
```

**`src/lockjaw/shim.c`** — the IPC shim:

```c
static uint64_t reply_handle;
static volatile char *shared_buf;
static uint64_t brk_current, brk_mapped_end;
static int initialized;

static void ensure_init(void) {
    if (initialized) return;
    initialized = 1;

    // 1. Allocate Reply object via direct Lockjaw SVCs
    uint64_t ps = lj_svc(SYS_ALLOC_PAGES, 1, 0, ...);
    reply_handle = lj_svc(SYS_CREATE_REPLY, ps, ...);

    // 2. Bootstrap: send POSIX_INIT to posix-server (handle 0)
    //    Server exports shared buffer PageSet and replies with:
    //    [shared_ps_index, shared_buf_va, brk_base, 0]
    uint64_t r[4];
    lj_call_ret4(/*ep*/0, reply_handle, POSIX_INIT, 0, 0, 0, r);
    uint64_t shared_ps = r[0];
    uint64_t buf_va = r[1];
    brk_current = r[2];
    brk_mapped_end = brk_current;

    // 3. Map shared buffer locally
    lj_svc(SYS_MAP_PAGES, shared_ps, buf_va, 0);
    shared_buf = (volatile char *)buf_va;
}

long lockjaw_syscall(long n, long a, long b, ...) {
    ensure_init();
    if (n == __NR_brk) return handle_brk(a);

    // For write: clamp to PAGE_SIZE, copy into shared buffer
    if (n == __NR_write) {
        long len = c > 4096 ? 4096 : c;
        memcpy((void*)shared_buf, (void*)b, len);
        return lj_call(/*ep*/0, reply_handle, n, a, len, 0);
    }

    // For writev: gather iovecs into shared buffer, stop at PAGE_SIZE
    if (n == __NR_writev) {
        struct iovec *iov = (struct iovec *)b;
        int iovcnt = (int)c;
        long total = 0;
        for (int i = 0; i < iovcnt && total < 4096; i++) {
            long chunk = iov[i].iov_len;
            if (total + chunk > 4096) chunk = 4096 - total;
            memcpy((void*)(shared_buf + total), iov[i].iov_base, chunk);
            total += chunk;
        }
        return lj_call(/*ep*/0, reply_handle, n, a, total, 0);
    }

    // Other syscalls: pass scalar args only
    return lj_call(/*ep*/0, reply_handle, n, a, b, c);
}
```

### C. Init changes (`user/init/src/main.rs`)

1. Add `static POSIX_SERVER_ELF: &[u8] = include_bytes!("../../posix-server/target/aarch64-unknown-none/release/lockjaw-posix-server");`
2. Add `let posix_boot_ep = alloc_endpoint("posix boot");`
3. Add `spawn_elf(POSIX_SERVER_ELF, "posix-server", map_array_va, temp_base_va, scratch_ps, posix_boot_ep, 8);`
4. Add bootstrap receive + reply (no handles to export):
   ```rust
   let _ = sys_receive(posix_boot_ep);
   sys_reply(0, 0, 0, 0);
   ```

### D. Test binary

```c
#include <stdio.h>
int main() { puts("hello, lockjaw"); return 0; }
```

Cross-compiled: `aarch64-linux-musl-gcc -static -o hello hello.c`
Stored at `user/posix-hello/hello` (pre-built, checked in).
Embedded by posix-server via `include_bytes!`.

### E. Build system (`Makefile`)

Add `user/posix-server` to `USER_CRATES` (line 21).
Add `cd user/posix-server && cargo build --release` to `build-user`.

## POSIX Process Address Space

```
0x00000000 - 0x0000FFFF : null guard (unmapped)
0x00400000 - 0x004XXXXX : ELF text/data/bss (PT_LOAD segments)
elf_end_aligned          : (unmapped guard, 1 page)
elf_end_aligned+0x1000   : shared buffer (1 page, mapped at bootstrap)
elf_end_aligned+0x2000   : brk heap base (shim-managed, grows up →)
  ...                    : (free space for heap growth)
0x00800000 - 0x00803FFF : stack (4 pages, layout at 0x803000)
```

All three dynamic VAs (guard, shared buffer, brk base) are computed
by posix-server from the actual ELF layout after loading. Posix-server
asserts `brk_base < USER_STACK_BASE` before spawning the child.

SP starts at `0x804000`. Patched `_start` does `sub sp, sp, #4096`
→ SP = `0x803000` → reads argc from there.

## Syscall Execution Path

**Bootstrap (first syscall, e.g. set_tid_address):**
1. musl calls `set_tid_address()` → `lockjaw_syscall(178, ...)`
2. Shim `ensure_init()`: alloc Reply via Lockjaw SVCs
3. Shim sends `POSIX_INIT` to handle 0 (posix-server)
4. Posix-server: alloc shared page, map in own VA, export to child
5. Posix-server: `sys_reply(shared_ps_idx, shared_va, brk_base, 0)`
6. Shim: `sys_map_pages(shared_ps_idx, shared_va, 0)` — now shared
7. Shim now sends the actual `set_tid_address` via IPC
8. Posix-server replies `1` (thread ID)

**Steady state (write):**
1. musl `puts()` → `writev(1, iov, 1)`
2. `lockjaw_syscall(66, 1, iov_ptr, 1, ...)` — shim already inited
3. Shim gathers iov data into shared buffer
4. Shim: `lj_call(ep=0, reply, 66, fd=1, total_len, 0)`
5. Posix-server reads `total_len` bytes from its shared buffer VA
6. Emits via `putc()` (kernel UART)
7. `sys_reply(15, 0, 0, 0)` → shim returns 15 to musl

## Implementation Order

1. Create `user/posix-server/` crate skeleton (Cargo, linker, build)
2. Implement `_start` with bootstrap + status print — verify it boots
3. Port `spawn_elf` logic into posix-server (ELF loading + page alloc)
4. Implement shared buffer allocation + stack layout construction
5. Implement process spawning + syscall endpoint setup
6. Write the musl patches (`crt_arch.h`, `syscall_arch.h`, `shim.c`)
7. Build musl, compile hello.c, embed binary
8. Implement syscall dispatch loop (write/writev/exit_group/stubs)
9. Wire init to spawn posix-server
10. QEMU integration test

## Files to Create

```
user/posix-server/Cargo.toml
user/posix-server/.cargo/config.toml
user/posix-server/linker.ld
user/posix-server/build.rs
user/posix-server/src/main.rs
user/posix-hello/hello           (pre-built musl binary)
musl-lockjaw/patches/            (3 patch files)
```

## Files to Modify

```
user/init/src/main.rs            (spawn + bootstrap posix-server)
Makefile                         (add posix-server to USER_CRATES + build-user)
```

## Verification

1. `make test` — existing tests still pass
2. `make run` — QEMU output includes:
   - `init: posix-server spawned OK`
   - `[BOOTSTRAP] posix-server`
   - `hello, lockjaw`
   - `posix: exit`
3. QEMU integration test: `assert_contains "hello, lockjaw"`
