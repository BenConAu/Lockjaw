use core::arch::global_asm;

global_asm!(
    r#"
.section .text._start, "ax"         // Place in .text._start section (executable)
.global _start                       // Export _start for the linker ENTRY directive

_start:
    // Save x0 (firmware DTB pointer) before any clobber.
    // QEMU may or may not pass DTB here; Pi firmware always does.
    // Stored in x20 (callee-saved) until BSS is zeroed, then written
    // to the BOOT_DTB_PADDR global.
    mov     x20, x0                  // Preserve DTB pointer

    msr     DAIFSet, #0xf            // Mask all exceptions (Debug, Async, IRQ, FIQ)

    // --- Compute physical offset ---
    // The linker assigns symbols at fixed addresses (e.g., 0x40200000).
    // The actual load address may differ (Pi 4B loads at 0x80000).
    // phys_offset = actual_addr(_start) - linked_addr(_start)
    // All pre-MMU symbol references must add this offset.
    adr     x21, _start              // x21 = actual physical address of _start
    ldr     x22, =_start             // x22 = linker-assigned address of _start
    sub     x21, x21, x22           // x21 = phys_offset (0 on QEMU, negative/positive on Pi)

    // --- Determine current exception level ---
    mrs     x0, CurrentEL            // Read CurrentEL register
    lsr     x0, x0, #2              // Shift right to get EL number in bits [1:0]
    cmp     x0, #2                   // Are we at EL2?
    b.ne    .Lat_el1                 // If not EL2, skip the drop — already at EL1

    // --- Drop from EL2 to EL1 ---
    mov     x0, #(1 << 31)          // HCR_EL2.RW bit: EL1 executes in AArch64 mode
    msr     HCR_EL2, x0             // Write Hypervisor Configuration Register

    mov     x0, #0x3c5              // SPSR_EL2: EL1h (SP_EL1), DAIF bits all masked
    msr     SPSR_EL2, x0            // Set Saved Program Status Register for EL2

    adr     x0, .Lat_el1            // Load address of EL1 entry point (PC-relative, correct)
    msr     ELR_EL2, x0             // Set Exception Link Register — where eret jumps to

    eret                             // Return from EL2 → EL1 at .Lat_el1

.Lat_el1:
    // --- Enable FP/NEON (the compiler may generate SIMD instructions) ---
    mov     x0, #(3 << 20)          // CPACR_EL1.FPEN = 0b11: no FP/NEON trapping
    msr     CPACR_EL1, x0           // Write Coprocessor Access Control Register
    isb                              // Ensure CPACR is active before any FP use

    // --- Set up the kernel stack ---
    // Apply phys_offset to the linker-assigned stack address.
    ldr     x0, =__stack_top         // Linked address
    add     x0, x0, x21             // Adjusted to actual physical address
    mov     sp, x0                   // Initialize stack pointer (grows downward)

    // --- Zero the BSS section ---
    ldr     x0, =__bss_start         // Linked address
    add     x0, x0, x21             // Adjust to physical
    ldr     x1, =__bss_end           // Linked address
    add     x1, x1, x21             // Adjust to physical
.Lbss_loop:
    cmp     x0, x1                   // Have we reached the end?
    b.ge    .Lbss_done               // If so, stop
    str     xzr, [x0], #8           // Store zero (8 bytes), advance pointer
    b       .Lbss_loop               // Repeat

.Lbss_done:
    // Store firmware DTB pointer to global (after BSS zeroing so we
    // don't clobber it). x20 was saved from x0 at _start entry.
    ldr     x0, =BOOT_DTB_PADDR     // Linked address
    add     x0, x0, x21             // Adjust to physical
    str     x20, [x0]               // Store the saved DTB pointer

    mov     x0, x20                  // Pass DTB pointer as first arg to kmain
    bl      kmain                    // Call Rust entry point

    // --- Halt if kmain ever returns ---
.Lhalt:
    wfi                              // Wait for interrupt (low-power idle)
    b       .Lhalt                   // Loop forever

// ---------------------------------------------------------------------------
// Secondary CPU entry point — called by PSCI CPU_ON
// ---------------------------------------------------------------------------
// PSCI delivers x0 = context_id (we pass the cpu_id here).
// The secondary must: mask exceptions, drop to EL1 if at EL2, enable
// FP/NEON, set its per-CPU stack, and call secondary_main(cpu_id).
// No BSS zeroing — CPU 0 already did that.

.global _secondary_start
_secondary_start:
    msr     DAIFSet, #0xf            // Mask all exceptions

    // x0 = cpu_id (from PSCI context_id), preserve across EL2 drop
    mov     x19, x0                  // Save cpu_id in callee-saved register

    // Compute phys_offset (same as primary)
    adr     x21, _start
    ldr     x22, =_start
    sub     x21, x21, x22

    // --- EL2 → EL1 drop (same as primary) ---
    mrs     x1, CurrentEL            // Read CurrentEL
    lsr     x1, x1, #2              // EL number in bits [1:0]
    cmp     x1, #2                   // At EL2?
    b.ne    .Lsec_el1               // Already at EL1, skip

    mov     x1, #(1 << 31)          // HCR_EL2.RW = AArch64 EL1
    msr     HCR_EL2, x1
    mov     x1, #0x3c5              // SPSR_EL2: EL1h, DAIF masked
    msr     SPSR_EL2, x1
    adr     x1, .Lsec_el1
    msr     ELR_EL2, x1
    eret

.Lsec_el1:
    // --- Enable FP/NEON ---
    mov     x1, #(3 << 20)          // CPACR_EL1.FPEN = 0b11
    msr     CPACR_EL1, x1
    isb

    // --- Set per-CPU stack ---
    // Stack layout: __per_cpu_stacks + (cpu_id + 1) * 12288
    // Each CPU block: 4 KB guard + 8 KB stack = 12 KB = 12288 bytes.
    ldr     x1, =__per_cpu_stacks    // Linked address
    add     x1, x1, x21             // Adjust to physical
    add     x2, x19, #1             // cpu_id + 1
    movz    x3, #12288               // Per-CPU block stride (4K guard + 8K stack)
    mul     x2, x2, x3              // (cpu_id + 1) * 12288
    add     sp, x1, x2              // SP = stack top for this CPU

    // --- Call Rust secondary_main(cpu_id) ---
    mov     x0, x19                  // Restore cpu_id as first argument
    bl      secondary_main           // Never returns

    // --- Halt if secondary_main ever returns ---
.Lsec_halt:
    wfi
    b       .Lsec_halt

// ---------------------------------------------------------------------------
// Higher-half pivot — transitions SP, FP, and PC to TTBR1 addresses
// ---------------------------------------------------------------------------
// Called after TTBR1 is installed. Adds KERNEL_VA_OFFSET to SP, FP (x29),
// and LR (x30), then returns via `ret`. The caller resumes at its
// higher-half return address — all subsequent PC-relative references
// resolve to higher-half (TTBR1) addresses.
//
// x0 = KERNEL_VA_OFFSET (passed as first argument)
//
// After this call, the kernel runs entirely in the upper VA half.
// VBAR_EL1 (set later) gets a higher-half address automatically,
// so exception entry works even when TTBR0 holds a user page table.
.global _pivot_to_higher_half
_pivot_to_higher_half:
    add     sp, sp, x0              // SP → higher-half
    add     x29, x29, x0           // FP → higher-half (for frame walks)
    add     x30, x30, x0           // LR → higher-half (return address)
    ret                              // Return to higher-half caller
"#
);
