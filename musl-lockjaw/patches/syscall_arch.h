/*
 * Lockjaw replacement for musl arch/aarch64/syscall_arch.h
 *
 * Stock musl does `svc 0` (Linux kernel trap). We redirect all
 * syscalls to lockjaw_syscall() in shim.c, which packs arguments
 * into an IPC message and calls the POSIX personality server via
 * Lockjaw's sys_call mechanism.
 */

#define __SYSCALL_LL_E(x) (x)
#define __SYSCALL_LL_O(x) (x)

extern long lockjaw_syscall(long n, long a, long b, long c,
                             long d, long e, long f);

static inline long __syscall0(long n) {
	return lockjaw_syscall(n, 0, 0, 0, 0, 0, 0);
}
static inline long __syscall1(long n, long a) {
	return lockjaw_syscall(n, a, 0, 0, 0, 0, 0);
}
static inline long __syscall2(long n, long a, long b) {
	return lockjaw_syscall(n, a, b, 0, 0, 0, 0);
}
static inline long __syscall3(long n, long a, long b, long c) {
	return lockjaw_syscall(n, a, b, c, 0, 0, 0);
}
static inline long __syscall4(long n, long a, long b, long c, long d) {
	return lockjaw_syscall(n, a, b, c, d, 0, 0);
}
static inline long __syscall5(long n, long a, long b, long c, long d,
                               long e) {
	return lockjaw_syscall(n, a, b, c, d, e, 0);
}
static inline long __syscall6(long n, long a, long b, long c, long d,
                               long e, long f) {
	return lockjaw_syscall(n, a, b, c, d, e, f);
}
