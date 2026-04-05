/*
 * Lockjaw Microkernel — AArch64 Linker Script
 *
 * QEMU virt machine loads the kernel at 0x4008_0000 via -kernel flag.
 * RAM starts at 0x4000_0000 (128 MB default), first 512 KB reserved by QEMU firmware/DTB.
 */

ENTRY(_start)

MEMORY
{
    RAM (rwx) : ORIGIN = 0x40080000, LENGTH = 128M - 512K
}

SECTIONS
{
    . = 0x40080000;

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
     * Stack layout (grows downward on AArch64):
     *   [guard page — 4 KB, unmapped in Phase 2]
     *   [stack — 4 KB, budget: 3072 normal + 1024 interrupt]
     *
     * __stack_bottom = lowest usable stack byte
     * __stack_top    = initial SP value
     */
    . = . + 4K;
    __stack_bottom = .;
    . = . + 4K;
    __stack_top = .;

    /DISCARD/ : {
        *(.comment)
        *(.ARM.*)
        *(.eh_frame*)
        *(.note*)
    }
}
