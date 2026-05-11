/*
 * Lockjaw Microkernel — AArch64 Linker Script
 *
 * Kernel image is linked at a fixed higher-half VA in L0[1], decoupled
 * from the physical load address. The boot trampoline discovers the
 * actual load PA via PC-relative (`adr _start`) and maps the kernel's
 * runtime PA range at this fixed VA via 4 KB L3 page entries.
 *
 *   L0[0]   = 0xFFFF_0000_0000_0000  linear map (paddr scratch view)
 *   L0[1]   = 0xFFFF_0080_0000_0000  kernel image (this VMA)
 *   L0[256] = 0xFFFF_8000_0000_0000  KVM pool (typed kernel objects)
 *
 * The ELF program headers carry both VMA (where Rust resolves
 * symbols) and LMA (where the loader places bytes). VMA is the
 * higher-half address; LMA is the legacy QEMU load address
 * (0x40200000) so `qemu-system-aarch64 -kernel` still loads the
 * binary into RAM at a sensible PA. The runtime trampoline does not
 * depend on the LMA — it reads its own PC via `adr _start` to
 * discover where firmware actually placed it. A different LMA (e.g.,
 * Pi 4B's 0x80000) is a separate build-time concern; the runtime
 * mapping is identical.
 *
 * Pre-relink VMA was 0x40200000 (same as LMA on QEMU). Post-relink,
 * VMA and LMA are independent.
 */

KERNEL_VMA = 0xFFFF008000000000;
KERNEL_LMA = 0x40200000;

/* QEMU's ELF kernel loader sets PC = the entry-point value as written
 * in the ELF header. Pre-MMU that PC must be a physical address. _start
 * is at the start of .text whose LMA we pinned to KERNEL_LMA, so the
 * entry's PA = KERNEL_LMA + 0 = KERNEL_LMA. */
ENTRY(KERNEL_LMA)

/* Two parallel regions: VMA (where Rust resolves symbols, the L0[1]
 * kernel-image range) and LMA (where the loader places the bytes,
 * the legacy QEMU load address). Every loadable section is assigned
 * `> VMA_REGION AT> LMA_REGION` so the VMA-LMA delta is uniform
 * across the entire image — `runtime PA = symbol VMA - KERNEL_VMA +
 * KERNEL_LMA` holds for every kernel symbol, which is what the boot
 * trampoline (`mmu::init_kernel_image_map`) and `page_alloc::init_with_gap`
 * both depend on. NOLOAD sections (.bss, the per-CPU stack block)
 * carry only a VMA assignment — there are no on-disk bytes to place
 * — but their VMA continues `.` so PC-relative computation pre-MMU
 * still yields the right PAs. */
MEMORY
{
    VMA_REGION (rwx) : ORIGIN = 0xFFFF008000000000, LENGTH = 128M
    LMA_REGION (rwx) : ORIGIN = 0x40200000, LENGTH = 128M - 2M
}

SECTIONS
{
    . = KERNEL_VMA;
    __kernel_start = .;

    /* Kernel code — _start entry point placed first */
    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    } > VMA_REGION AT> LMA_REGION

    /* Read-only data (string literals, constants) */
    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    } > VMA_REGION AT> LMA_REGION

    /* Embedded user-binary blobs (include_bytes! of init.elf etc.).
     * Separate section so check-vtables can skip it: u64-aligned bytes
     * inside an embedded user ELF may coincidentally match the kernel's
     * own text address range (the embedded binary's own data tables
     * and sub-binaries) but they are opaque user data, not kernel
     * function pointers. */
    .user_elf_blob : ALIGN(4K) {
        *(.user_elf_blob .user_elf_blob.*)
    } > VMA_REGION AT> LMA_REGION

    /* Initialized mutable data */
    .data : ALIGN(4K) {
        *(.data .data.*)
    } > VMA_REGION AT> LMA_REGION

    /* Zero-initialized data — boot assembly zeroes this range. NOLOAD
     * means no on-disk bytes are placed, but the LMA still needs to
     * follow the per-section delta so the per-CPU stacks (next
     * section) get a sensible PT_LOAD physical address — without
     * `AT> LMA_REGION` here, ld defaults the LMA to the VMA, making
     * the subsequent stack PT_LOAD point at an invalid PA. */
    .bss (NOLOAD) : ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    } > VMA_REGION AT> LMA_REGION

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
     *
     * NOLOAD: stacks are not initialized by the loader; the boot
     * trampoline maps them and the per-thread create paths zero
     * them as needed. Like .bss the section carries only a VMA
     * assignment.
     */
    .per_cpu_stacks (NOLOAD) : ALIGN(2M) {
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
    } > VMA_REGION AT> LMA_REGION

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
