/*
 * Lockjaw replacement for musl arch/aarch64/crt_arch.h
 *
 * Stock musl's _start does `mov x0, sp; b _start_c` — it reads
 * argc/argv/envp/auxv from the stack pointer. On Linux, the kernel
 * places this layout just below the initial SP.
 *
 * Lockjaw sets SP to the very top of the stack allocation
 * (USER_STACK_BASE + stack_pages * 4096). The personality server
 * writes the Linux stack layout one page below the top. So we
 * subtract 4096 from SP before reading.
 */

__asm__(
".text \n"
".global " START "\n"
".type " START ",%function\n"
START ":\n"
"	mov x29, #0\n"          /* Zero frame pointer (ABI: base of call chain) */
"	mov x30, #0\n"          /* Zero link register (no caller to return to) */
"	sub sp, sp, #4096\n"    /* Lockjaw: stack layout is one page below top */
"	mov x0, sp\n"           /* Pass stack pointer as arg to _start_c */
"	b _start_c\n"
);
