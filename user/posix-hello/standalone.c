/*
 * Standalone Phase 0 test binary for Lockjaw POSIX personality server.
 *
 * No musl, no libc — implements the shim protocol directly.
 * Tests: stack layout parsing, POSIX_INIT handshake, write via
 * shared buffer, exit_group.
 *
 * Build: see Makefile target 'build-posix-hello'
 */

typedef unsigned long uint64_t;

#define LJ_SYS_DEBUG_PUTS    0
#define LJ_SYS_CALL          4
#define LJ_SYS_ALLOC_PAGES   6
#define LJ_SYS_MAP_PAGES     7
#define LJ_SYS_CREATE_REPLY  20

#define POSIX_INIT  0xFFFFFFFFFFFFFF00UL
#define NR_WRITEV   66
#define NR_EXIT_GROUP 94

#define PAGE_SIZE   4096UL

/* Lockjaw SVC: returns x0 (error), *out1 = x1 (result handle) */
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

/* Lockjaw SVC: 3 args, returns x0 (error) */
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

/* IPC call returning all 4 reply words.
 *
 * Lockjaw sys_call ABI:
 *   IN:  x0=ep_handle x1=reply_handle x2-x5=msg0-msg3 x8=4(SYS_CALL)
 *   OUT: x0=error_code x1-x4=reply_word_0..3
 * Note the asymmetry: 4 message words in (x2-x5) but reply lands in x1-x4.
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

/* IPC call returning first reply word (x1) — see lj_call_ret4 for ABI. */
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
    return x1;
}

/*
 * _start: mimics musl's patched _start.
 * SP points at the Linux stack layout (argc, argv, envp, auxv).
 * On Lockjaw, the kernel sets SP to stack top; the patched musl
 * _start does `sub sp, sp, #4096` to reach the layout page.
 * This standalone binary uses the same convention.
 */
void _start(void) __asm__("_start");
__attribute__((naked, noreturn))
void _start(void) {
    __asm__ volatile(
        "mov x29, #0\n"          /* Zero frame pointer */
        "mov x30, #0\n"          /* Zero link register */
        "sub sp, sp, #4096\n"    /* Lockjaw: stack layout one page below top */
        "mov x0, sp\n"           /* Pass stack pointer to main */
        "b _posix_main\n"
    );
}

static void shim_memcpy(volatile char *dst, const char *src, long n) {
    for (long i = 0; i < n; i++)
        dst[i] = src[i];
}

/* Direct kernel UART output — for diagnostics before/after IPC is
 * set up. Atomic w.r.t. other threads' debug output. */
static inline void dbg_puts_n(const char *buf, long n) {
    register long x0 __asm__("x0") = (long)buf;
    register long x1 __asm__("x1") = n;
    register long x8 __asm__("x8") = LJ_SYS_DEBUG_PUTS;
    __asm__ volatile("svc #0" : "+r"(x0), "+r"(x1) : "r"(x8) : "memory", "cc");
}

static inline void dbg_putc(char c) {
    dbg_puts_n(&c, 1);
}

static void dbg_print(const char *s) {
    long n = 0;
    while (s[n]) n++;
    dbg_puts_n(s, n);
}

static void dbg_print_hex(unsigned long v) {
    static const char hex[] = "0123456789abcdef";
    dbg_putc('0'); dbg_putc('x');
    int started = 0;
    for (int i = 15; i >= 0; i--) {
        unsigned nib = (v >> (i * 4)) & 0xF;
        if (nib != 0 || started || i == 0) {
            dbg_putc(hex[nib]);
            started = 1;
        }
    }
}

/* If err != 0, print "posix-hello: <label>=0x<err>\n" and halt forever.
 * Used to surface SVC errors that would otherwise be silently dropped. */
static void die_if_err(long err, const char *label) {
    if (err == 0) return;
    dbg_print("posix-hello: ");
    dbg_print(label);
    dbg_print(" failed err=");
    dbg_print_hex((unsigned long)err);
    dbg_putc('\n');
    for (;;) __asm__ volatile("wfi");
}

void _posix_main(uint64_t *sp) __attribute__((noreturn));
void _posix_main(uint64_t *sp) {
    /* Parse stack layout (same as musl reads argc/argv) */
    /* uint64_t argc = sp[0]; */
    /* uint64_t *argv = &sp[1]; */
    /* (Not used by this test — just validating the server wrote it) */

    dbg_print("posix-hello: starting\n");

    /* 1. Allocate a Reply object for IPC */
    long ps, reply;
    die_if_err(lj_svc_ret1(LJ_SYS_ALLOC_PAGES, 1, &ps),    "alloc_pages");
    die_if_err(lj_svc_ret1(LJ_SYS_CREATE_REPLY, ps, &reply), "create_reply");

    /* 2. Send POSIX_INIT to personality server (handle 0).
     * lj_call_ret4 returns x0 = Lockjaw error code. */
    long r[4];
    die_if_err(lj_call_ret4(0, reply, POSIX_INIT, 0, 0, 0, r), "POSIX_INIT call");
    long shared_ps_idx = r[0];
    long buf_va        = r[1];
    /* long brk_base   = r[2]; — not used in this test */

    /* 3. Map shared buffer locally */
    die_if_err(lj_svc3(LJ_SYS_MAP_PAGES, shared_ps_idx, buf_va, 0), "map shared buf");
    volatile char *shared_buf = (volatile char *)buf_va;

    /* 4. Write "hello, lockjaw\n" via the personality server.
     * lj_call returns x2 (the server's reply word 0 = byte count or errno). */
    const char *msg = "hello, lockjaw\n";
    long len = 15;
    shim_memcpy(shared_buf, msg, len);
    long write_ret = lj_call(0, reply, NR_WRITEV, 1, len, 0);
    if (write_ret != len) {
        dbg_print("posix-hello: writev short/error ret=");
        dbg_print_hex((unsigned long)write_ret);
        dbg_putc('\n');
    }

    /* 5. exit_group(0) — return is irrelevant; we halt either way. */
    lj_call(0, reply, NR_EXIT_GROUP, 0, 0, 0);

    /* Should not reach here */
    for (;;)
        __asm__ volatile("wfi");
}
