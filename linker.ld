/*
 * Lockjaw Microkernel — AArch64 Linker Script
 *
 * QEMU virt machine loads the kernel via -kernel flag.
 * RAM starts at 0x4000_0000 (128 MB default). First 2 MB reserved for the
 * 1 MB DTB that QEMU places at RAM_BASE during bare-metal ELF boot.
 */

ENTRY(_start)

MEMORY
{
    RAM (rwx) : ORIGIN = 0x40200000, LENGTH = 128M - 2M
}

SECTIONS
{
    . = 0x40200000;

    /* Kernel code — _start entry point placed first */
    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    } > RAM

    /* Read-only data (string literals, constants) */
    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    } > RAM

    /* Initialized mutable data */
    .data : ALIGN(4K) {
        *(.data .data.*)
    } > RAM

    /* Zero-initialized data — boot assembly zeroes this range */
    .bss (NOLOAD) : ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    } > RAM

    . = ALIGN(4K);
    __kernel_end = .;

    /*
     * Per-CPU stacks (4 CPUs × 12 KB each = 48 KB total).
     * Each CPU gets a guard page (4 KB, unmapped) + stack (8 KB).
     * Stack budget: see docs/stack-budget.md.
     *
     * CPU 0's symbols are aliased to the legacy names so existing
     * code referencing __guard_page/__stack_bottom/__stack_top
     * continues to work without modification.
     *
     * The entire 32 KB block is 2 MB-aligned to guarantee all guard
     * pages fall within a single L2 region. setup_guard_pages() uses
     * one L3 table for the containing 2 MB block; straddling a 2 MB
     * boundary would require two L3 tables. The gap between
     * __kernel_end and __per_cpu_stacks is freed to the page
     * allocator via init_with_gap() — no physical memory is wasted.
     */
    . = ALIGN(2M);
    __per_cpu_stacks = .;

    /* CPU 0 */
    __guard_page_0 = .;
    . = . + 4K;
    __stack_bottom_0 = .;
    . = . + 8K;
    __stack_top_0 = .;

    /* CPU 1 */
    __guard_page_1 = .;
    . = . + 4K;
    __stack_bottom_1 = .;
    . = . + 8K;
    __stack_top_1 = .;

    /* CPU 2 */
    __guard_page_2 = .;
    . = . + 4K;
    __stack_bottom_2 = .;
    . = . + 8K;
    __stack_top_2 = .;

    /* CPU 3 */
    __guard_page_3 = .;
    . = . + 4K;
    __stack_bottom_3 = .;
    . = . + 8K;
    __stack_top_3 = .;

    __per_cpu_stacks_end = .;

    /* Legacy aliases for CPU 0 (boot code, existing references) */
    __guard_page = __guard_page_0;
    __stack_bottom = __stack_bottom_0;
    __stack_top = __stack_top_0;

    /DISCARD/ : {
        *(.comment)
        *(.ARM.*)
        *(.eh_frame*)
        *(.note*)
    }
}
