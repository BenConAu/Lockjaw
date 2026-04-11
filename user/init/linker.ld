/*
 * Lockjaw init process linker script.
 * Places code at user VA 0x0040_0000 (must match kernel expectations).
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

    /DISCARD/ : {
        *(.comment)
        *(.ARM.*)
        *(.eh_frame*)
        *(.note*)
    }
}
