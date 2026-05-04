/*
 * Lockjaw POSIX personality server linker script.
 * Places code at user VA 0x0040_0000 (same as other userspace processes —
 * each process gets its own address space so the same VA is fine).
 */

ENTRY(_start)

SECTIONS
{
    . = 0x00400000;

    .text : {
        *(.text._start)
        *(.text .text.*)
    }

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K) {
        *(.data .data.*)
    }

    .bss (NOLOAD) : ALIGN(4K) {
        *(.bss .bss.*)
    }

    .lockjaw_hash : ALIGN(8) {
        *(.lockjaw_hash)
    }

    /DISCARD/ : {
        *(.comment)
        *(.ARM.*)
        *(.eh_frame*)
        *(.note*)
    }
}
