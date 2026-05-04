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

/* ---------- Lockjaw syscall numbers ---------- */
#define LJ_SYS_DEBUG_PUTS    0
#define LJ_SYS_CALL          4
#define LJ_SYS_ALLOC_PAGES   6
#define LJ_SYS_MAP_PAGES     7
#define LJ_SYS_CREATE_REPLY  20

/* Sentinel: not a real Linux syscall number */
#define POSIX_INIT  0xFFFFFFFFFFFFFF00UL

#define PAGE_SIZE   4096UL

/* Linux syscall numbers (aarch64) */
#define __NR_write       64
#define __NR_writev      66
#define __NR_brk        214

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
static int initialized;

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
     *      r[3] = 0
     */
    long r[4];
    err = lj_call_ret4(0, reply_handle, POSIX_INIT, 0, 0, 0, r);
    if (err != 0) lj_die("init: POSIX_INIT call failed", err);
    long shared_ps_idx = r[0];
    long buf_va        = r[1];
    brk_current        = r[2];
    brk_mapped_end     = brk_current;

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

    /* write(fd, buf, len): clamp to PAGE_SIZE, copy into shared buffer */
    if (n == __NR_write) {
        long len = c > (long)PAGE_SIZE ? (long)PAGE_SIZE : c;
        shim_memcpy(shared_buf, (const char *)b, len);
        return lj_call(0, reply_handle, n, a, len, 0);
    }

    /* writev(fd, iov, iovcnt): gather into shared buffer, cap at PAGE_SIZE */
    if (n == __NR_writev) {
        const struct iovec *iov = (const struct iovec *)b;
        int iovcnt = (int)c;
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
