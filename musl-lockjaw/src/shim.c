/*
 * Lockjaw POSIX shim — linked into musl binaries.
 *
 * Translates Linux syscalls into IPC messages to the POSIX personality
 * server running in a separate Lockjaw process. The personality server
 * endpoint is at handle index 0 (passed via sys_create_process).
 *
 * Data transfer uses a shared physical page mapped in both address
 * spaces (at different VAs). The shim copies user buffers into the
 * shared page before sending the IPC message.
 *
 * brk is handled locally via direct Lockjaw SVCs (no IPC round-trip).
 */

#include <stdint.h>
#include <sys/uio.h>
#include <errno.h>  /* EINVAL / ENOMEM for direct -errno returns */

/* ---------- Lockjaw syscall numbers ---------- */
#define LJ_SYS_DEBUG_PUTS    0
#define LJ_SYS_CALL          4
#define LJ_SYS_ALLOC_PAGES   6
#define LJ_SYS_MAP_PAGES     7
#define LJ_SYS_CREATE_REPLY  20
#define LJ_SYS_CLOSE_HANDLE  24
#define LJ_SYS_UNMAP_PAGES   25

/* Sentinels: not real Linux syscall numbers. Sit at the top of the
 * u64 range so they cannot collide with any musl-issued syscall. */
#define POSIX_INIT          0xFFFFFFFFFFFFFF00UL
#define NR_MMAP_ROLLBACK    0xFFFFFFFFFFFFFF01UL

#define PAGE_SIZE   4096UL

/* Linux syscall numbers (aarch64) */
#define __NR_openat      56
#define __NR_read        63
#define __NR_write       64
#define __NR_readv       65
#define __NR_writev      66
#define __NR_brk        214
#define __NR_munmap     215
#define __NR_mmap       222
#define __NR_mprotect   226
#define __NR_madvise    233


/* ---------- Diagnostic helpers (no IPC required) ---------- */

/*
 * Direct kernel UART output — works before/without the IPC bootstrap.
 * Atomic w.r.t. other threads' debug output (kernel holds GKL for the
 * whole emit). Used only by lj_die() for fatal-error diagnostics.
 */
static inline void lj_dbg_puts_n(const char *buf, long n) {
    register long x0 __asm__("x0") = (long)buf;
    register long x1 __asm__("x1") = n;
    register long x8 __asm__("x8") = LJ_SYS_DEBUG_PUTS;
    __asm__ volatile("svc #0"
        : "+r"(x0), "+r"(x1)
        : "r"(x8)
        : "memory", "cc");
}

static inline void lj_dbg_putc(char c) {
    lj_dbg_puts_n(&c, 1);
}

static void lj_dbg_print(const char *s) {
    long n = 0;
    while (s[n]) n++;
    lj_dbg_puts_n(s, n);
}

static void lj_dbg_print_hex(unsigned long v) {
    static const char hex[] = "0123456789abcdef";
    lj_dbg_putc('0'); lj_dbg_putc('x');
    int started = 0;
    for (int i = 15; i >= 0; i--) {
        unsigned nib = (v >> (i * 4)) & 0xF;
        if (nib != 0 || started || i == 0) {
            lj_dbg_putc(hex[nib]);
            started = 1;
        }
    }
}

/*
 * Hard failure for unrecoverable bootstrap or transport errors. Prints a
 * diagnostic + error code via the kernel UART (no IPC needed) and halts.
 *
 * The shim cannot recover from IPC transport failures: every Linux
 * syscall a musl binary issues funnels through here, and a corrupted
 * shared buffer or unbound endpoint would silently produce wrong
 * results. Halting is the correct response.
 */
static void __attribute__((noreturn)) lj_die(const char *msg, long err) {
    lj_dbg_print("posix-shim: ");
    lj_dbg_print(msg);
    lj_dbg_print(" err=");
    lj_dbg_print_hex((unsigned long)err);
    lj_dbg_putc('\n');
    for (;;) __asm__ volatile("wfi");
}

/* ---------- Lockjaw SVC helpers ---------- */

/*
 * Basic Lockjaw SVC. Returns x0 (error code).
 * Arguments go in x0-x2, syscall number in x8.
 */
static inline long lj_svc3(long nr, long a0, long a1, long a2) {
    register long x0 __asm__("x0") = a0;
    register long x1 __asm__("x1") = a1;
    register long x2 __asm__("x2") = a2;
    register long x8 __asm__("x8") = nr;
    __asm__ volatile("svc #0"
        : "+r"(x0), "+r"(x1), "+r"(x2)
        : "r"(x8)
        : "memory", "cc");
    return x0;
}

/*
 * Lockjaw SVC that returns both x0 (error) and x1 (result value).
 * Used for sys_alloc_pages (handle returned in x1) and
 * sys_create_reply (Reply handle in x1).
 */
static inline long lj_svc_ret1(long nr, long a0, long *out1) {
    register long x0 __asm__("x0") = a0;
    register long x1 __asm__("x1") = 0;
    register long x8 __asm__("x8") = nr;
    __asm__ volatile("svc #0"
        : "+r"(x0), "+r"(x1)
        : "r"(x8)
        : "memory", "cc");
    *out1 = x1;
    return x0;
}

/*
 * IPC call (sys_call). Lockjaw ABI:
 *   IN:  x0=ep x1=reply_handle x2-x5=msg0-msg3 x8=4
 *   OUT: x0=error_code x1-x4=reply_word_0..3
 * Note: 4 message words in (x2-x5) but reply lands in x1-x4.
 *
 * Aborts on transport failure (x0 != 0): the personality server didn't
 * receive the call, so the value in x1 is meaningless. Returning it as
 * the syscall result would let musl misinterpret a transport error as
 * a successful syscall return.
 */
static inline long lj_call(long ep, long reply_h,
                            long msg0, long msg1,
                            long msg2, long msg3) {
    register long x0 __asm__("x0") = ep;
    register long x1 __asm__("x1") = reply_h;
    register long x2 __asm__("x2") = msg0;
    register long x3 __asm__("x3") = msg1;
    register long x4 __asm__("x4") = msg2;
    register long x5 __asm__("x5") = msg3;
    register long x8 __asm__("x8") = LJ_SYS_CALL;
    __asm__ volatile("svc #0"
        : "+r"(x0), "+r"(x1), "+r"(x2),
          "+r"(x3), "+r"(x4), "+r"(x5)
        : "r"(x8)
        : "memory", "cc");
    if (x0 != 0)
        lj_die("IPC transport failure", x0);
    /* x1 = reply word 0 (the server's syscall return value) */
    return x1;
}

/*
 * IPC call returning all 4 reply words. See lj_call for ABI details.
 * Used for POSIX_INIT bootstrap.
 */
static inline long lj_call_ret4(long ep, long reply_h,
                                 long msg0, long msg1,
                                 long msg2, long msg3,
                                 long r[4]) {
    register long x0 __asm__("x0") = ep;
    register long x1 __asm__("x1") = reply_h;
    register long x2 __asm__("x2") = msg0;
    register long x3 __asm__("x3") = msg1;
    register long x4 __asm__("x4") = msg2;
    register long x5 __asm__("x5") = msg3;
    register long x8 __asm__("x8") = LJ_SYS_CALL;
    __asm__ volatile("svc #0"
        : "+r"(x0), "+r"(x1), "+r"(x2),
          "+r"(x3), "+r"(x4), "+r"(x5)
        : "r"(x8)
        : "memory", "cc");
    r[0] = x1;
    r[1] = x2;
    r[2] = x3;
    r[3] = x4;
    return x0;
}

/* ---------- Shim state ---------- */

static long reply_handle;
static volatile char *shared_buf;
static long brk_current;
static long brk_mapped_end;
/*
 * Base VA of the mmap region, sent by the personality server in
 * POSIX_INIT word 3. The shim doesn't read this directly during
 * mmap() — the server picks each base_va — but stashes it for
 * diagnostics and future use (e.g. validating that munmap targets
 * fall in the mmap region rather than brk).
 */
static long mmap_base __attribute__((unused));
static int initialized;

/* ---------- mmap region tracker (Phase 2.3) ---------- */

/*
 * Per-process tracker for live mmap regions. Phase 2.3 cap; bump
 * when programs need more. Each slot records the (base_va, handle,
 * len) tuple needed to undo the mapping.
 *
 * Updated only on full mmap success; munmap removes after the
 * remote side has acknowledged the teardown. mmap-fail-mid-flow
 * cleanup uses NR_MMAP_ROLLBACK + sys_close_handle without
 * touching the tracker (the entry was never recorded).
 */
#define MMAP_TRACKER_SLOTS 16
struct mmap_slot {
    long base_va;     /* 0 means slot is empty */
    long handle;
    long len;
};
static struct mmap_slot mmap_tracker[MMAP_TRACKER_SLOTS];

/*
 * Find an empty tracker slot. Returns its index, or -1 if full.
 */
static int mmap_tracker_find_free(void) {
    for (int i = 0; i < MMAP_TRACKER_SLOTS; i++) {
        if (mmap_tracker[i].base_va == 0)
            return i;
    }
    return -1;
}

/*
 * Locate the slot holding `base_va`. Returns its index, or -1 if
 * not found.
 */
static int mmap_tracker_find(long base_va) {
    for (int i = 0; i < MMAP_TRACKER_SLOTS; i++) {
        if (mmap_tracker[i].base_va == base_va)
            return i;
    }
    return -1;
}

/* ---------- Local brk handler ---------- */

/*
 * Handle brk locally — no IPC round-trip. Allocates physical pages
 * via direct Lockjaw SVCs and maps them into the process VA space.
 *
 * Invariants (Phase 0 personality layout contract):
 *   - brk_current starts at brk_base (page-aligned)
 *   - growth is monotonic (only moves up)
 *   - [brk_base, USER_STACK_BASE) is exclusively owned by brk
 */
static long handle_brk(long addr) {
    if (addr == 0 || addr <= brk_current)
        return brk_current;

    /* Round up to page boundary */
    long new_end = (addr + PAGE_SIZE - 1) & ~(PAGE_SIZE - 1);

    while (brk_mapped_end < new_end) {
        long ps;
        if (lj_svc_ret1(LJ_SYS_ALLOC_PAGES, 1, &ps) != 0)
            return brk_current;  /* alloc failed — return old brk */
        if (lj_svc3(LJ_SYS_MAP_PAGES, ps, brk_mapped_end, 0) != 0)
            return brk_current;  /* map failed — return old brk */
        brk_mapped_end += PAGE_SIZE;
    }

    brk_current = addr;
    return brk_current;
}

/* ---------- mmap / munmap (Phase 2.3) ---------- */

/*
 * Linux mmap(addr, len, prot, flags, fd, offset). Phase 2 supports
 * the anonymous-private subset: addr=0 (or ignored), fd=-1,
 * offset=0; flags must be MAP_PRIVATE|MAP_ANONYMOUS; prot must be
 * PROT_READ|PROT_WRITE. The dispatch layer in posix-server enforces
 * the contract and returns -errno for anything else, so the shim
 * passes the request through verbatim and trusts the reply.
 *
 * Failure-ordered handshake (the FS_MMAP_ROLLBACK protocol):
 *   1. lj_call(NR_MMAP, len, prot, flags) -> reply (status, base_va,
 *      exported_handle, total_pages).
 *   2. sys_map_pages(handle, base_va, 0). On failure: NR_MMAP_ROLLBACK,
 *      sys_close_handle, return -ENOMEM. If rollback fails: lj_die.
 *   3. mmap_tracker[i] = (base_va, handle, len). On tracker-full:
 *      treat like step 2 failure (rollback + return -ENOMEM).
 *   4. Return base_va.
 *
 * The tracker is updated only after every step succeeds; failure
 * in any post-FS_MMAP step rolls back so the server doesn't keep
 * an mmap_table entry the client can't see.
 */
static long handle_mmap(long len, long prot, long flags) {
    long r[4];
    long err = lj_call_ret4(0, reply_handle, __NR_mmap, len, prot, flags, r);
    if (err != 0) lj_die("mmap: IPC transport failure", err);
    /* r[0] = status (0 on success or -errno from dispatch).
     * r[1] = base_va (only valid when r[0] == 0).
     * r[2] = exported handle.
     * r[3] = page count. */
    if (r[0] != 0) return r[0];
    long base_va = r[1];
    long handle  = r[2];

    /* Map the exported PageSet at base_va. flags=0 (RW UXN). */
    if (lj_svc3(LJ_SYS_MAP_PAGES, handle, base_va, 0) != 0) {
        /* Local map failed AFTER successful FS_MMAP. Tell the
         * server to tear down its mmap_table entry; without this
         * the server would think the region is live. */
        long rb_err = lj_call(0, reply_handle, NR_MMAP_ROLLBACK, base_va, 0, 0);
        if (rb_err != 0) {
            /* Rollback IPC succeeded but server returned error
             * (entry missing, already rolled back). Or transport
             * failed mid-rollback. Either way the server's view
             * of this region is no longer recoverable from the
             * client side — halt rather than leak. */
            lj_die("mmap rollback failed; refusing to leak server PageSet",
                   rb_err);
        }
        /* Server's side cleaned up; close the orphan local handle. */
        (void)lj_svc3(LJ_SYS_CLOSE_HANDLE, handle, 0, 0);
        return -ENOMEM;
    }

    /* Find a tracker slot. On full, treat like map failure: the
     * region is live but we have no way to remember it for munmap,
     * so we'd leak indefinitely. Roll back to keep the leak bounded. */
    int slot = mmap_tracker_find_free();
    if (slot < 0) {
        /* sys_unmap_pages first, then NR_MMAP_ROLLBACK + close. We
         * need to undo the map even though the server's entry will
         * also be torn down — the local PTE is independent. */
        (void)lj_svc3(LJ_SYS_UNMAP_PAGES, handle, base_va, 0);
        long rb_err = lj_call(0, reply_handle, NR_MMAP_ROLLBACK, base_va, 0, 0);
        if (rb_err != 0)
            lj_die("mmap rollback failed (tracker full)", rb_err);
        (void)lj_svc3(LJ_SYS_CLOSE_HANDLE, handle, 0, 0);
        return -ENOMEM;
    }
    mmap_tracker[slot].base_va = base_va;
    mmap_tracker[slot].handle  = handle;
    mmap_tracker[slot].len     = len;
    return base_va;
}

/*
 * Linux munmap(addr, len). Remote-first teardown:
 *   1. Look up base_va in the tracker. EINVAL if missing (mirrors
 *      the server's policy and Linux convention for "not a known
 *      mapping").
 *   2. lj_call(NR_MUNMAP, base_va, len). On errno reply: return it
 *      with the tracker untouched so the caller can retry.
 *   3. sys_unmap_pages(handle, base_va). Failure here means local
 *      and remote state diverged — lj_die loud rather than leak.
 *   4. sys_close_handle(handle). Failure here is a bounded one-slot
 *      local leak; log and continue.
 *   5. Drop the tracker entry, return 0.
 */
static long handle_munmap(long base_va, long len) {
    int slot = mmap_tracker_find(base_va);
    /* Mirror the server's EINVAL-for-unknown-region policy
     * (posix-server's handle_file_munmap returns -EINVAL when the
     * (caller, base_va) lookup misses). Returning -ENOENT here
     * would split the contract — the visible errno would depend on
     * whether the miss happened in the local tracker or the server
     * table. Linux uses EINVAL for "not a known mapping" too. */
    if (slot < 0) return -EINVAL;

    long handle = mmap_tracker[slot].handle;
    long ret = lj_call(0, reply_handle, __NR_munmap, base_va, len, 0);
    if (ret != 0) {
        /* Server rejected (likely len mismatch or stale entry).
         * Leave the tracker so a corrected retry can succeed. */
        return ret;
    }
    if (lj_svc3(LJ_SYS_UNMAP_PAGES, handle, base_va, 0) != 0)
        lj_die("munmap: local sys_unmap_pages diverged from remote", 0);
    /* close failure leaks one local handle slot. Log + continue. */
    if (lj_svc3(LJ_SYS_CLOSE_HANDLE, handle, 0, 0) != 0)
        lj_dbg_print("posix-shim: munmap close_handle failed (leak)\n");

    mmap_tracker[slot].base_va = 0;
    mmap_tracker[slot].handle  = 0;
    mmap_tracker[slot].len     = 0;
    return 0;
}

/* ---------- Bootstrap ---------- */

/*
 * Called once on the first syscall. Sets up the Reply object, sends
 * POSIX_INIT to the personality server, receives the shared buffer and
 * brk base VAs, and maps the shared buffer locally.
 *
 * Every step is fallible and there is no way for libc init to recover
 * from a botched bootstrap — every subsequent syscall would dereference
 * an invalid shared_buf or talk to a nonexistent server. So any error
 * halts the process via lj_die() with a diagnostic, and `initialized`
 * is only set after every step has succeeded.
 */
static void ensure_init(void) {
    if (initialized)
        return;

    /* 1. Allocate a page and create a Reply object (direct SVCs) */
    long ps;
    long err = lj_svc_ret1(LJ_SYS_ALLOC_PAGES, 1, &ps);
    if (err != 0) lj_die("init: alloc_pages failed", err);
    err = lj_svc_ret1(LJ_SYS_CREATE_REPLY, ps, &reply_handle);
    if (err != 0) lj_die("init: create_reply failed", err);

    /* 2. Bootstrap: send POSIX_INIT to personality server (handle 0).
     *    Server replies with:
     *      r[0] = child's PageSet handle index (for shared buffer)
     *      r[1] = child_shared_va (where to map the shared buffer)
     *      r[2] = brk_base (heap start VA)
     *      r[3] = mmap_base (base VA of mmap region, Phase 2.0)
     */
    long r[4];
    err = lj_call_ret4(0, reply_handle, POSIX_INIT, 0, 0, 0, r);
    if (err != 0) lj_die("init: POSIX_INIT call failed", err);
    long shared_ps_idx = r[0];
    long buf_va        = r[1];
    brk_current        = r[2];
    brk_mapped_end     = brk_current;
    mmap_base          = r[3];

    /* 3. Map the shared buffer page locally */
    err = lj_svc3(LJ_SYS_MAP_PAGES, shared_ps_idx, buf_va, 0);
    if (err != 0) lj_die("init: map shared buffer failed", err);
    shared_buf = (volatile char *)buf_va;

    /* All bootstrap steps succeeded — only NOW mark initialized. */
    initialized = 1;
}

/* ---------- memcpy (local, no libc dependency) ---------- */

static void shim_memcpy(volatile char *dst, const char *src, long n) {
    for (long i = 0; i < n; i++)
        dst[i] = src[i];
}

/* Read direction: shared buffer (volatile) -> user buffer (plain). */
static void shim_memcpy_from_shared(char *dst, const volatile char *src, long n) {
    for (long i = 0; i < n; i++)
        dst[i] = src[i];
}

/* ---------- Main entry point ---------- */

/*
 * Called by musl's __syscallN() inlines (via syscall_arch.h).
 * Handles brk locally, copies data for write/writev into the shared
 * buffer, and forwards everything else to the personality server.
 */
long lockjaw_syscall(long n, long a, long b, long c,
                      long d, long e, long f) {
    ensure_init();

    /* brk: handled locally, no IPC */
    if (n == __NR_brk)
        return handle_brk(a);

    /* mmap(addr, len, prot, flags, fd, offset): Phase 2 supports the
     * anonymous-private subset. addr/fd/offset are ignored — server
     * picks base_va, fd=-1 + offset=0 implied by MAP_ANONYMOUS. */
    if (n == __NR_mmap)
        return handle_mmap(b, c, d);

    /* munmap(addr, len) */
    if (n == __NR_munmap)
        return handle_munmap(a, b);

    /* mprotect(addr, len, prot): forwards to posix-server which
     * verifies the region matches a known mmap entry. */
    if (n == __NR_mprotect)
        return lj_call(0, reply_handle, n, a, b, c);

    /* madvise(addr, len, advice): hints aren't load-bearing; the
     * server replies 0 unconditionally. Still forward so any
     * future per-region check can run server-side. */
    if (n == __NR_madvise)
        return lj_call(0, reply_handle, n, a, b, c);

    /* write(fd, buf, len): clamp to PAGE_SIZE, copy into shared buffer */
    if (n == __NR_write) {
        long len = c > (long)PAGE_SIZE ? (long)PAGE_SIZE : c;
        shim_memcpy(shared_buf, (const char *)b, len);
        return lj_call(0, reply_handle, n, a, len, 0);
    }

    /* openat(dirfd, path, flags, mode): copy path into shared buffer,
     * forward (dirfd, path_len, flags) to the personality server.
     * mode is ignored (Phase 1 is read-only; the dispatch layer
     * rejects O_CREAT etc. with -EROFS up front). */
    if (n == __NR_openat) {
        const char *path = (const char *)b;
        long path_len = 0;
        while (path[path_len] && path_len < (long)PAGE_SIZE)
            path_len++;
        shim_memcpy(shared_buf, path, path_len);
        return lj_call(0, reply_handle, n, a, path_len, c);
    }

    /* read(fd, buf, len): server reads file into shared buffer; on
     * success, copy from shared buffer into user buf. Cap len at
     * PAGE_SIZE (the personality server caps it the same way). */
    if (n == __NR_read) {
        long len = c > (long)PAGE_SIZE ? (long)PAGE_SIZE : c;
        long ret = lj_call(0, reply_handle, n, a, len, 0);
        if (ret > 0) {
            shim_memcpy_from_shared((char *)b, shared_buf, ret);
        }
        return ret;
    }

    /* readv(fd, iov, iovcnt): musl's __stdio_read uses readv to fill
     * the user's buffer + the FILE struct's internal buffer in one
     * syscall. Translate to a single read at the IPC level (server
     * doesn't know about iovs; it just fills the shared buffer up
     * to `total` bytes), then scatter the returned bytes back into
     * the iov entries. Cap total at PAGE_SIZE — same shared-buffer
     * limit as __NR_read. */
    if (n == __NR_readv) {
        const struct iovec *iov = (const struct iovec *)b;
        int iovcnt = (int)c;
        /* Linux returns -EINVAL for iovcnt < 0; the loop-bound
         * fallthrough would otherwise report this as EOF (return 0)
         * which musl would interpret as success. */
        if (iovcnt < 0) return -EINVAL;
        long total = 0;
        for (int i = 0; i < iovcnt; i++) {
            long chunk = (long)iov[i].iov_len;
            if (total + chunk > (long)PAGE_SIZE) {
                total = (long)PAGE_SIZE;
                break;
            }
            total += chunk;
        }
        if (total == 0) return 0;
        long ret = lj_call(0, reply_handle, __NR_read, a, total, 0);
        if (ret <= 0) return ret;
        long copied = 0;
        for (int i = 0; i < iovcnt && copied < ret; i++) {
            long chunk = (long)iov[i].iov_len;
            if (copied + chunk > ret) chunk = ret - copied;
            shim_memcpy_from_shared((char *)iov[i].iov_base,
                                     shared_buf + copied, chunk);
            copied += chunk;
        }
        return ret;
    }

    /* writev(fd, iov, iovcnt): gather into shared buffer, cap at PAGE_SIZE */
    if (n == __NR_writev) {
        const struct iovec *iov = (const struct iovec *)b;
        int iovcnt = (int)c;
        /* Same guard as readv — Linux returns -EINVAL for iovcnt < 0. */
        if (iovcnt < 0) return -EINVAL;
        long total = 0;
        for (int i = 0; i < iovcnt && total < (long)PAGE_SIZE; i++) {
            long chunk = (long)iov[i].iov_len;
            if (total + chunk > (long)PAGE_SIZE)
                chunk = (long)PAGE_SIZE - total;
            shim_memcpy(shared_buf + total,
                        (const char *)iov[i].iov_base, chunk);
            total += chunk;
        }
        return lj_call(0, reply_handle, n, a, total, 0);
    }

    /* Everything else: pass scalar args, no pointer data */
    return lj_call(0, reply_handle, n, a, b, c);
}
